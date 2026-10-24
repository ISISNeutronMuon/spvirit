//! A real PVA client writing to an `IocSource` over the wire.
//!
//! Every other put test in this crate calls `Source::put` with a bare
//! `DecodedValue::Float64(..)`. A real client never sends that: the server
//! decodes a PUT body into the channel's structure, so the source receives
//! `Structure([("value", ..)])` (plus whatever other members the client's
//! bitset selected). Before this file existed `IocSource::put` refused every
//! such value ("cannot write Structure(..) to a record"), so no client could
//! write to any IOC-served PV, and nothing in the suite noticed. These tests
//! drive the full path: `PvaClient::pvput` -> server PUT handler ->
//! `decode_put_body` -> `IocSource::put`.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use spvirit_client::PvaClient;
use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_ioc::IocSource;
use spvirit_ioc::scan::scanner::MAX_PERIODIC_THREADS;
use spvirit_server::pva_server::PvaServer;
use spvirit_server::pvstore::Source;

fn free_tcp_port() -> Option<u16> {
    TcpListener::bind("127.0.0.1:0")
        .ok()?
        .local_addr()
        .ok()
        .map(|a| a.port())
}

fn free_udp_port() -> Option<u16> {
    UdpSocket::bind("127.0.0.1:0")
        .ok()?
        .local_addr()
        .ok()
        .map(|a| a.port())
}

/// Start a server for `ioc` on loopback and return a client aimed straight at
/// it (`server_addr` bypasses UDP search, which CI containers do not reliably
/// route to loopback). `None` when no loopback port can be bound.
async fn serve(
    ioc: Arc<IocSource>,
) -> Option<(PvaClient, tokio::task::JoinHandle<Result<(), String>>)> {
    let (tcp, udp) = (free_tcp_port()?, free_udp_port()?);
    let server = PvaServer::builder()
        .port(tcp)
        .udp_port(udp)
        .listen_ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
        .ioc(ioc)
        .build();
    let handle = tokio::spawn(async move { server.run().await.map_err(|e| e.to_string()) });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let addr: SocketAddr = format!("127.0.0.1:{tcp}").parse().expect("loopback addr");
    let client = PvaClient::builder()
        .port(tcp)
        .udp_port(udp)
        .server_addr(addr)
        .search_addr(IpAddr::V4(Ipv4Addr::LOCALHOST))
        .bind_addr(IpAddr::V4(Ipv4Addr::LOCALHOST))
        .timeout(Duration::from_secs(5))
        .build();
    Some((client, handle))
}

/// The top-level `value` member of a decoded NT structure.
fn value_field(v: &DecodedValue) -> &DecodedValue {
    match v {
        DecodedValue::Structure(fields) => fields
            .iter()
            .find(|(name, _)| name == "value")
            .map(|(_, val)| val)
            .unwrap_or_else(|| panic!("no 'value' field in {v:?}")),
        other => panic!("expected a structure, got {other:?}"),
    }
}

async fn get_f64(client: &PvaClient, pv: &str) -> f64 {
    let got = client
        .pvget(pv)
        .await
        .unwrap_or_else(|e| panic!("pvget {pv}: {e}"));
    match value_field(&got.value) {
        DecodedValue::Float64(v) => *v,
        DecodedValue::Int32(v) => *v as f64,
        other => panic!("{pv}: expected a number, got {other:?}"),
    }
}

async fn get_string(client: &PvaClient, pv: &str) -> String {
    let got = client
        .pvget(pv)
        .await
        .unwrap_or_else(|e| panic!("pvget {pv}: {e}"));
    match value_field(&got.value) {
        DecodedValue::String(s) => s.clone(),
        other => panic!("{pv}: expected a string, got {other:?}"),
    }
}

const CHAIN: &str = "\
record(ao, \"W:SET\") {
    field(OUT, \"W:MID.VAL\")
    field(FLNK, \"W:MID\")
}
record(ai, \"W:MID\") {
    field(INP, \"W:MID.VAL NPP\")
    field(FLNK, \"W:OUT\")
}
record(ai, \"W:OUT\") {
    field(INP, \"W:MID PP\")
}
";

/// A client `pvput` of a double to an `ao` record is accepted, reads back, and
/// processes the record: the forward-linked chain downstream updates too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_put_to_a_record_is_accepted_and_processes_it() {
    let ioc = Arc::new(IocSource::from_db_str(CHAIN).expect("the database loads"));
    let Some((client, handle)) = serve(ioc).await else {
        eprintln!("skipping: cannot bind a loopback port in this environment");
        return;
    };

    client
        .pvput("W:SET", 7.5)
        .await
        .expect("a client PUT of a double to an ao record must be accepted");
    assert_eq!(
        get_f64(&client, "W:SET").await,
        7.5,
        "W:SET reads back the put value"
    );
    assert_eq!(
        get_f64(&client, "W:OUT").await,
        7.5,
        "the put processed W:SET, so the linked chain carried the value to W:OUT"
    );

    handle.abort();
}

const FIELDS: &str = "\
record(ai, \"F:A\") {
    field(EGU, \"C\")
}
";

/// The writable field PVs (`EVNT` a string, `PHAS` an integer) accept a client
/// PUT and read back; an ordinary field PV such as `EGU` is read-only on the
/// IOC tier and the client is told so rather than left hanging.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_put_to_a_writable_field_pv_is_accepted_and_reads_back() {
    let ioc = Arc::new(IocSource::from_db_str(FIELDS).expect("the database loads"));
    let Some((client, handle)) = serve(ioc).await else {
        eprintln!("skipping: cannot bind a loopback port in this environment");
        return;
    };

    client
        .pvput("F:A.EVNT", "tick")
        .await
        .expect("a client PUT of a string to F:A.EVNT must be accepted");
    assert_eq!(get_string(&client, "F:A.EVNT").await, "tick");

    client
        .pvput("F:A.PHAS", 3)
        .await
        .expect("a client PUT of an integer to F:A.PHAS must be accepted");
    assert_eq!(get_f64(&client, "F:A.PHAS").await, 3.0);

    client
        .pvput("F:A.EGU", "K")
        .await
        .expect_err("EGU is read-only on the IOC tier; the client must get an error");
    assert_eq!(
        get_string(&client, "F:A.EGU").await,
        "C",
        "a refused put changes nothing"
    );

    handle.abort();
}

const SCAN_DB: &str = "\
record(ai, \"S:SRC\") {
    field(INP, \"42\")
}
record(ai, \"S:SELF\") {
    field(INP, \"S:SRC\")
}
";

/// `SCAN = "1 second"` written by a client is accepted and the record starts
/// scanning: `S:SELF` links `S:SRC` (42) but is passive and undefined (0)
/// until processed, so only the scan thread can move it to 42. A period
/// outside the accepted range is refused, and the refusal reaches the client.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_scan_put_starts_scanning_and_a_bad_period_is_refused_on_the_wire() {
    let ioc = Arc::new(IocSource::from_db_str(SCAN_DB).expect("the database loads"));
    let Some((client, handle)) = serve(ioc).await else {
        eprintln!("skipping: cannot bind a loopback port in this environment");
        return;
    };

    assert_eq!(
        get_f64(&client, "S:SELF").await,
        0.0,
        "passive and unprocessed"
    );

    let err = client
        .pvput("S:SELF.SCAN", "2e9 second")
        .await
        .expect_err("a period above MAX_SCAN_PERIOD_SECS must be refused over the wire");
    assert!(
        err.to_string().contains("2e9 second"),
        "the refusal names the value: {err}"
    );
    assert_eq!(
        get_string(&client, "S:SELF.SCAN").await,
        "Passive",
        "a refused SCAN changes nothing"
    );

    client
        .pvput("S:SELF.SCAN", "1 second")
        .await
        .expect("a client PUT of SCAN = \"1 second\" must be accepted");
    assert_eq!(get_string(&client, "S:SELF.SCAN").await, "1 second");

    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        if get_f64(&client, "S:SELF").await == 42.0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "S:SELF never picked up its linked value: the SCAN put did not start scanning"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    handle.abort();
}

/// The periodic-thread cap's refusal (`TooManyPeriods`) reaches a real client.
/// `MAX_PERIODIC_THREADS` distinct periods are occupied through the source
/// directly (long periods, so those threads only sleep); the next new period,
/// written over the wire, must come back to the client as an error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_periodic_thread_cap_refusal_reaches_the_client() {
    let mut db = String::new();
    for i in 0..=MAX_PERIODIC_THREADS {
        db.push_str(&format!("record(ai, \"CAP:{i}\") {{\n}}\n"));
    }
    let ioc = Arc::new(IocSource::from_db_str(&db).expect("the database loads"));
    for i in 0..MAX_PERIODIC_THREADS {
        let period = DecodedValue::String(format!("{} second", 100_000 + i));
        Source::put(&*ioc, &format!("CAP:{i}.SCAN"), &period)
            .await
            .unwrap_or_else(|e| panic!("period {i} must fit under the cap: {e}"));
    }
    let Some((client, handle)) = serve(ioc).await else {
        eprintln!("skipping: cannot bind a loopback port in this environment");
        return;
    };

    let last = format!("CAP:{MAX_PERIODIC_THREADS}.SCAN");
    let err = client
        .pvput(&last, "200000 second")
        .await
        .expect_err("a new period past MAX_PERIODIC_THREADS must be refused over the wire");
    assert!(
        err.to_string().contains("too many distinct scan periods"),
        "the client must see the cap refusal, got: {err}"
    );
    assert_eq!(
        get_string(&client, &last).await,
        "Passive",
        "a refused SCAN changes nothing"
    );

    handle.abort();
}
