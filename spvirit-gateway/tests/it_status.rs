//! Unit/integration tests for `StatusSource` (Task 12): the gateway status
//! PVs served under `<statusprefix>`, the `asTest` RPC, and the startup
//! banner listing every served PV.

use std::sync::Arc;

use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_gateway::access::AccessControl;
use spvirit_gateway::status::{StatusHandles, StatusSource, banner};
use spvirit_server::pvstore::Source;
use spvirit_types::{NtPayload, PvValue, ScalarArrayValue, ScalarValue};

/// Builds the `asTest` RPC argument structure: `{pv, user, host}`, all
/// strings.
fn decoded_astest_args(pv: &str, user: &str, host: &str) -> DecodedValue {
    DecodedValue::Structure(vec![
        ("pv".to_string(), DecodedValue::String(pv.to_string())),
        ("user".to_string(), DecodedValue::String(user.to_string())),
        ("host".to_string(), DecodedValue::String(host.to_string())),
    ])
}

/// True if the `asTest` RPC response (p4p `epics:p2p/Permission:1.0`) reports
/// `put` as denied in its nested `permission` sub-structure.
fn astest_put_denied(out: &NtPayload) -> bool {
    let NtPayload::Generic { fields, .. } = out else {
        panic!("expected NtPayload::Generic, got {out:?}");
    };
    let perm = fields
        .iter()
        .find(|(n, _)| n == "permission")
        .map(|(_, v)| v)
        .expect("permission sub-structure");
    let PvValue::Structure { fields: pf, .. } = perm else {
        panic!("permission must be a sub-structure, got {perm:?}");
    };
    for (name, v) in pf {
        if name == "put"
            && let PvValue::Scalar(ScalarValue::Bool(b)) = v
        {
            return !*b;
        }
    }
    panic!("no \"put\" field in {pf:?}");
}

#[tokio::test]
async fn status_source_claims_prefixed_pvs() {
    let ss = StatusSource::new(
        "GW:STS:".into(),
        Arc::new(AccessControl::new(false, None, None)),
        StatusHandles::test(),
    );
    assert!(ss.claim("GW:STS:clients").await.is_some());
    assert!(ss.claim("GW:STS:ds:bypv:rx").await.is_some());
    assert!(ss.claim("UNRELATED:PV").await.is_none());
}

#[tokio::test]
async fn static_bandwidth_pvs_are_empty_tables() {
    let ss = StatusSource::new(
        "GW:STS:".into(),
        Arc::new(AccessControl::new(false, None, None)),
        StatusHandles::test(),
    );
    let v = ss.get("GW:STS:us:bypv:rx").await.expect("get");
    let NtPayload::Table(t) = v else {
        panic!("bandwidth PV must be an NTTable, got {v:?}");
    };
    assert_eq!(t.labels, vec!["PV", "RX (B/s)"]);
    // Static in M1: no byte accounting, so zero rows.
    for col in &t.columns {
        assert_eq!(col.values.len(), 0, "column {} must have no rows", col.name);
    }
}

#[tokio::test]
async fn clients_pv_is_a_string_array() {
    let ss = StatusSource::new(
        "GW:STS:".into(),
        Arc::new(AccessControl::new(false, None, None)),
        StatusHandles::test(),
    );
    let v = ss.get("GW:STS:clients").await.expect("get");
    let NtPayload::ScalarArray(a) = v else {
        panic!("clients must be an NTScalarArray, got {v:?}");
    };
    assert!(matches!(a.value, ScalarArrayValue::Str(_)));
}

#[tokio::test]
async fn astest_matches_decide() {
    let ac = Arc::new(AccessControl::new(true, None, None)); // readOnly
    let ss = StatusSource::new("GW:STS:".into(), ac, StatusHandles::test());
    // asTest(pv, user, host) as RPC args, NOT caller identity.
    let args = decoded_astest_args("SOMEPV", "alice", "10.0.0.1");
    let out = ss.rpc("GW:STS:asTest", &args).await.expect("rpc");
    // readOnly -> PUT denied; asserts the reported verdict is "deny" for Put.
    assert!(astest_put_denied(&out));
}

#[tokio::test]
async fn banner_lists_every_served_pv() {
    let lines = banner::status_pv_lines("GW:STS:");
    assert!(lines.iter().any(|l| l == "Status PV: GW:STS:clients"));
    assert!(lines.iter().any(|l| l == "Status PV: GW:STS:asTest"));
    // 6 live (clients,cache,refs,threads,stats,poke) + 8 static bandwidth
    // counters + 1 RPC (asTest) = 15.
    assert_eq!(lines.len(), 15);
}
