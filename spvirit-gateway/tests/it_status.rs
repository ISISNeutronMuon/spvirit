//! Unit/integration tests for `StatusSource` (Task 12): the gateway status
//! PVs served under `<statusprefix>`, the `asTest` RPC, and the startup
//! banner listing every served PV.

use std::sync::Arc;

use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_gateway::access::AccessControl;
use spvirit_gateway::status::{StatusHandles, StatusSource, banner};
use spvirit_server::pvstore::Source;
use spvirit_types::{NtPayload, PvValue, ScalarValue};

/// Digs an `f64` out of either an `NtPayload::Scalar` (the shape
/// `StatusSource::get` returns for its live/static PVs) or an
/// `NtPayload::Generic`'s `"value"` field, matching the pattern the
/// passthrough tests use for the gateway's own NT payloads.
fn extract_f64_value(p: &NtPayload) -> f64 {
    match p {
        NtPayload::Scalar(nt) => match nt.value {
            ScalarValue::F64(x) => x,
            ref other => panic!("expected F64 scalar, got {other:?}"),
        },
        NtPayload::Generic { fields, .. } => {
            for (name, v) in fields {
                if name == "value"
                    && let PvValue::Scalar(ScalarValue::F64(x)) = v
                {
                    return *x;
                }
            }
            panic!("no scalar F64 \"value\" field in {fields:?}");
        }
        other => panic!("expected NtPayload::Scalar or Generic, got {other:?}"),
    }
}

/// Builds the `asTest` RPC argument structure: `{pv, user, host}`, all
/// strings.
fn decoded_astest_args(pv: &str, user: &str, host: &str) -> DecodedValue {
    DecodedValue::Structure(vec![
        ("pv".to_string(), DecodedValue::String(pv.to_string())),
        ("user".to_string(), DecodedValue::String(user.to_string())),
        ("host".to_string(), DecodedValue::String(host.to_string())),
    ])
}

/// True if the `asTest` RPC response reports `Put` as denied.
fn astest_put_denied(out: &NtPayload) -> bool {
    let NtPayload::Generic { fields, .. } = out else {
        panic!("expected NtPayload::Generic, got {out:?}");
    };
    for (name, v) in fields {
        if name == "put"
            && let PvValue::Scalar(ScalarValue::Str(s)) = v
        {
            return s == "deny";
        }
    }
    panic!("no \"put\" field in {fields:?}");
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
async fn static_bandwidth_pvs_read_zero() {
    let ss = StatusSource::new(
        "GW:STS:".into(),
        Arc::new(AccessControl::new(false, None, None)),
        StatusHandles::test(),
    );
    let v = ss.get("GW:STS:us:bypv:rx").await.expect("get");
    assert_eq!(extract_f64_value(&v), 0.0);
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
