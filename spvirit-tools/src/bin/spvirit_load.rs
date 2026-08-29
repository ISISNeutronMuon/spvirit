//! spload — gateway load/benchmark driver.
//!
//! Two subcommands:
//!   spload drive  — PUT a monotonic counter (or timestamp with --stamp) to M PVs at an aggregate rate.
//!   spload watch  — open M*N monitor subscriptions, count updates, report coalescing (+latency in --stamp).
//!
//! Value model: stock ai/ao records expose only `value`, so the counter/timestamp rides in `value`.

use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use argparse::{ArgumentParser, Store, StoreTrue};
use serde::Serialize;
use spvirit_client::{client_from_opts, MonitorUpdate};
use tokio::runtime::Runtime;

// Fixed epoch offset so a millisecond timestamp fits exactly in an f64 mantissa (<2^52).
const STAMP_EPOCH_MS: u64 = 1_700_000_000_000;

fn pv_name(prefix: &str, index: usize) -> String {
    format!("{prefix}:PV{index:05}")
}

pub(crate) fn coalescing(min_seen: Option<u64>, max_seen: u64, received: u64) -> (u64, u64) {
    match min_seen {
        None => (0, 0),
        Some(min) => {
            let span = max_seen.saturating_sub(min) + 1;
            (span, span.saturating_sub(received))
        }
    }
}

pub(crate) fn percentile(sorted_ms: &[f64], p: f64) -> f64 {
    if sorted_ms.is_empty() {
        return f64::NAN;
    }
    let rank = (p * sorted_ms.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted_ms.len() - 1);
    sorted_ms[idx]
}

#[derive(Serialize)]
pub(crate) struct WatchSummary {
    subscriptions: usize,
    window_s: f64,
    received_total: u64,
    span_total: u64,
    coalesced_total: u64,
    max_counter: u64,
    latency_ms_p50: Option<f64>,
    latency_ms_p99: Option<f64>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

// Extract the scalar f64 carried in an update's `value` field.
//
// Verified against `spvirit-codec/src/spvd_decode.rs` (DecodedValue enum):
// variants are Null, Boolean, Int8..Int64, UInt8..UInt64, Float32, Float64,
// String, Array, Structure(Vec<(String, DecodedValue)>), Raw. Stock ai/ao
// records decode to a Structure whose "value" field is the scalar leaf.
fn update_value_f64(u: &MonitorUpdate) -> Option<f64> {
    use spvirit_codec::spvd_decode::DecodedValue;
    fn find(v: &DecodedValue) -> Option<f64> {
        match v {
            DecodedValue::Float64(x) => Some(*x),
            DecodedValue::Float32(x) => Some(*x as f64),
            DecodedValue::Int8(x) => Some(*x as f64),
            DecodedValue::Int16(x) => Some(*x as f64),
            DecodedValue::Int32(x) => Some(*x as f64),
            DecodedValue::Int64(x) => Some(*x as f64),
            DecodedValue::UInt8(x) => Some(*x as f64),
            DecodedValue::UInt16(x) => Some(*x as f64),
            DecodedValue::UInt32(x) => Some(*x as f64),
            DecodedValue::UInt64(x) => Some(*x as f64),
            DecodedValue::Structure(fields) => {
                for (name, val) in fields {
                    if name == "value" {
                        return find(val);
                    }
                }
                None
            }
            _ => None,
        }
    }
    find(&u.value)
}

struct SubState {
    min: Option<u64>,
    max: u64,
    received: u64,
    latencies_ms: Vec<f64>,
}

async fn run_watch(
    base_opts: spvirit_client::PvGetOptions,
    prefix: String,
    npvs: usize,
    subs: usize,
    window: Duration,
    stamp: bool,
    out: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let stop = Arc::new(AtomicBool::new(false));
    let states: Vec<Arc<Mutex<SubState>>> = (0..npvs * subs)
        .map(|_| {
            Arc::new(Mutex::new(SubState {
                min: None,
                max: 0,
                received: 0,
                latencies_ms: Vec::new(),
            }))
        })
        .collect();

    let mut set = tokio::task::JoinSet::new();
    for p in 0..npvs {
        for s in 0..subs {
            let slot = p * subs + s;
            let state = states[slot].clone();
            let stop = stop.clone();
            let mut opts = base_opts.clone();
            opts.pv_name = pv_name(&prefix, p);
            set.spawn(async move {
                let client = client_from_opts(&opts);
                let cb = move |u: &MonitorUpdate| {
                    if stop.load(Ordering::Relaxed) {
                        return ControlFlow::Break(());
                    }
                    if let Some(v) = update_value_f64(u) {
                        let mut st = state.lock().unwrap();
                        st.received += 1;
                        if stamp {
                            let sent = v as u64; // ms since STAMP_EPOCH_MS
                            let recv = now_ms().saturating_sub(STAMP_EPOCH_MS);
                            st.latencies_ms.push(recv.saturating_sub(sent) as f64);
                        } else {
                            let c = v as u64;
                            st.min = Some(st.min.map_or(c, |m| m.min(c)));
                            st.max = st.max.max(c);
                        }
                    }
                    ControlFlow::Continue(())
                };
                let _ = client.pvmonitor(&opts.pv_name, cb).await;
            });
        }
    }

    tokio::time::sleep(window).await;
    stop.store(true, Ordering::Relaxed);
    set.shutdown().await;

    let mut received_total = 0u64;
    let mut span_total = 0u64;
    let mut coalesced_total = 0u64;
    let mut max_counter = 0u64;
    let mut all_lat: Vec<f64> = Vec::new();
    for st in &states {
        let st = st.lock().unwrap();
        received_total += st.received;
        let (span, coalesced) = coalescing(st.min, st.max, st.received);
        span_total += span;
        coalesced_total += coalesced;
        max_counter = max_counter.max(st.max);
        all_lat.extend_from_slice(&st.latencies_ms);
    }
    all_lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let summary = WatchSummary {
        subscriptions: npvs * subs,
        window_s: window.as_secs_f64(),
        received_total,
        span_total,
        coalesced_total,
        max_counter,
        latency_ms_p50: if stamp {
            Some(percentile(&all_lat, 0.50))
        } else {
            None
        },
        latency_ms_p99: if stamp {
            Some(percentile(&all_lat, 0.99))
        } else {
            None
        },
    };
    let json = serde_json::to_string_pretty(&summary)?;
    match out {
        Some(path) => std::fs::write(path, json + "\n")?,
        None => println!("{json}"),
    }
    Ok(())
}

pub(crate) fn puts_due(rate_hz: f64, elapsed: Duration) -> u64 {
    (rate_hz * elapsed.as_secs_f64()).floor().max(0.0) as u64
}

pub(crate) fn target_pv(seq: u64, npvs: usize) -> usize {
    (seq % npvs.max(1) as u64) as usize
}

async fn run_drive(
    base_opts: spvirit_client::PvGetOptions,
    prefix: String,
    npvs: usize,
    rate_hz: f64,
    duration: Duration,
    stamp: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = client_from_opts(&base_opts);
    // One persistent PUT channel per PV (keeps its reader task alive across puts).
    let mut channels: Vec<spvirit_client::PvaChannel> = Vec::with_capacity(npvs);
    for p in 0..npvs {
        channels.push(client.open_put_channel(&pv_name(&prefix, p)).await?);
    }

    let start = std::time::Instant::now();
    let mut issued: u64 = 0;
    // Pace by "catch up to puts_due": sleep a small tick, then issue whatever is owed.
    let tick = Duration::from_millis(1);
    while start.elapsed() < duration {
        let due = puts_due(rate_hz, start.elapsed());
        while issued < due {
            let p = target_pv(issued, npvs);
            let value: f64 = if stamp {
                now_ms().saturating_sub(STAMP_EPOCH_MS) as f64
            } else {
                // per-PV monotonic counter = issued / npvs (each PV gets a clean 0,1,2,... sequence)
                (issued / npvs.max(1) as u64) as f64
            };
            channels[p].put(value).await?;
            issued += 1;
        }
        tokio::time::sleep(tick).await;
    }
    eprintln!(
        "spload drive: issued {issued} puts over {:.1}s (target {:.0}/s)",
        duration.as_secs_f64(),
        rate_hz
    );
    Ok(())
}

async fn run_drive_cli(argv: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    use spvirit_tools::spvirit_client::cli::CommonClientArgs;
    let mut prefix = "LOAD".to_string();
    let mut npvs: usize = 1;
    let mut rate_hz: f64 = 1000.0;
    let mut duration_s: f64 = 12.0;
    let mut stamp = false;
    let mut common = CommonClientArgs::new();
    {
        let mut ap = ArgumentParser::new();
        ap.set_description(
            "spload drive: PUT a monotonic counter (or timestamp) to M PVs at an aggregate rate",
        );
        ap.refer(&mut prefix)
            .add_option(&["--prefix"], Store, "PV name prefix (default LOAD)");
        ap.refer(&mut npvs)
            .add_option(&["--pvs"], Store, "number of distinct PVs M (default 1)");
        ap.refer(&mut rate_hz)
            .add_option(&["--rate"], Store, "aggregate PUT rate Hz (default 1000)");
        ap.refer(&mut duration_s)
            .add_option(&["--duration"], Store, "drive duration seconds (default 12)");
        ap.refer(&mut stamp).add_option(
            &["--stamp"],
            StoreTrue,
            "latency mode: PUT ms timestamp instead of counter",
        );
        common.add_to_parser(&mut ap);
        match ap.parse(argv, &mut std::io::stdout(), &mut std::io::stderr()) {
            Ok(()) => {}
            Err(code) => std::process::exit(code),
        }
    }
    common.init_tracing();
    let base_opts = common.into_pv_get_options(pv_name(&prefix, 0))?;
    run_drive(
        base_opts,
        prefix,
        npvs,
        rate_hz,
        Duration::from_secs_f64(duration_s),
        stamp,
    )
    .await
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut argv: Vec<String> = std::env::args().collect();
    if argv.len() < 2 {
        eprintln!("usage: spload <drive|watch> [options]");
        std::process::exit(2);
    }
    let mode = argv.remove(1); // argv[0]=prog, argv[1]=mode; leave the rest for argparse
    let rt = Runtime::new()?;
    match mode.as_str() {
        "watch" => rt.block_on(run_watch_cli(argv)),
        "drive" => rt.block_on(run_drive_cli(argv)),
        other => {
            eprintln!("spload: unknown mode {other:?} (expected drive|watch)");
            std::process::exit(2);
        }
    }
}

async fn run_watch_cli(argv: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    use spvirit_tools::spvirit_client::cli::CommonClientArgs;
    let mut prefix = "LOAD".to_string();
    let mut npvs: usize = 1;
    let mut subs: usize = 1;
    let mut window_s: f64 = 8.0;
    let mut stamp = false;
    let mut out = String::new();
    let mut common = CommonClientArgs::new();
    {
        let mut ap = ArgumentParser::new();
        ap.set_description("spload watch: open M*N monitors, count updates, report coalescing/latency");
        ap.refer(&mut prefix)
            .add_option(&["--prefix"], Store, "PV name prefix (default LOAD)");
        ap.refer(&mut npvs)
            .add_option(&["--pvs"], Store, "number of distinct PVs M (default 1)");
        ap.refer(&mut subs)
            .add_option(&["--subs"], Store, "subscribers per PV N (default 1)");
        ap.refer(&mut window_s)
            .add_option(&["--window"], Store, "sample window seconds (default 8)");
        ap.refer(&mut stamp).add_option(
            &["--stamp"],
            StoreTrue,
            "latency mode: value carries ms timestamp",
        );
        ap.refer(&mut out)
            .add_option(&["--out"], Store, "write JSON summary to this file");
        common.add_to_parser(&mut ap);
        // argparse parses the provided argv slice (argv[0] is treated as the program name).
        match ap.parse(argv, &mut std::io::stdout(), &mut std::io::stderr()) {
            Ok(()) => {}
            Err(code) => std::process::exit(code),
        }
    }
    common.init_tracing();
    // Any PV name works to build base options; per-subscription name is overwritten in run_watch.
    let base_opts = common.into_pv_get_options(pv_name(&prefix, 0))?;
    let out_opt = if out.is_empty() { None } else { Some(out) };
    run_watch(base_opts, prefix, npvs, subs, Duration::from_secs_f64(window_s), stamp, out_opt).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalescing_counts_gaps_as_span_minus_received() {
        // saw counters 0,1,2,5 -> received 4, span 6, coalesced 2
        let (span, coalesced) = coalescing(Some(0), 5, 4);
        assert_eq!(span, 6);
        assert_eq!(coalesced, 2);
    }

    #[test]
    fn coalescing_zero_when_perfect() {
        // saw 10..=19 contiguous: received 10, span 10, coalesced 0
        let (span, coalesced) = coalescing(Some(10), 19, 10);
        assert_eq!(span, 10);
        assert_eq!(coalesced, 0);
    }

    #[test]
    fn coalescing_never_seen_is_zero() {
        let (span, coalesced) = coalescing(None, 0, 0);
        assert_eq!(span, 0);
        assert_eq!(coalesced, 0);
    }

    #[test]
    fn percentile_nearest_rank() {
        let v: Vec<f64> = (1..=100).map(|x| x as f64).collect();
        assert_eq!(percentile(&v, 0.50), 50.0);
        assert_eq!(percentile(&v, 0.99), 99.0);
        assert!(percentile(&[], 0.5).is_nan());
    }

    #[test]
    fn pv_name_matches_gen_db_format() {
        assert_eq!(pv_name("LOAD", 0), "LOAD:PV00000");
        assert_eq!(pv_name("LOAD", 1800), "LOAD:PV01800");
    }

    #[test]
    fn puts_due_is_floor_rate_times_elapsed() {
        assert_eq!(puts_due(100.0, std::time::Duration::from_millis(2500)), 250);
        assert_eq!(puts_due(1000.0, std::time::Duration::from_secs(1)), 1000);
        assert_eq!(puts_due(0.0, std::time::Duration::from_secs(5)), 0);
    }

    #[test]
    fn target_pv_round_robins() {
        assert_eq!(target_pv(0, 3), 0);
        assert_eq!(target_pv(1, 3), 1);
        assert_eq!(target_pv(2, 3), 2);
        assert_eq!(target_pv(3, 3), 0);
        assert_eq!(target_pv(7, 1), 0);
    }
}
