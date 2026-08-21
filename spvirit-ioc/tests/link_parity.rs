//! Link fields read the same on tier 1 and tier 2, from the same `.db`.
//!
//! `SimplePvStore` (tier 1) keeps the raw `.db` strings; `IocSource`
//! (tier 2) threw them away for a parsed link model. Before this test the
//! two served different text for the same input — `PV:B PP` against
//! `PV:B.VAL PP NMS` — which is a client-visible way to tell the tiers
//! apart, the one thing the `.FIELD` parity claim rules out. Both now render
//! through `record_fields::render_link_text`.
//!
//! The comparison is tier against tier, not tier against a literal: the
//! literals below are a second, independent statement of what Base prints,
//! and the assertion that matters is that the two stores agree.
//!
//! It lives in `spvirit-ioc`'s test tree because `spvirit-ioc` depends on
//! `spvirit-server` and never the reverse, so this is the only side that can
//! see both stores.

use spvirit_ioc::IocSource;
use spvirit_server::field_provider::resolve_field_payload;
use spvirit_server::pva_server::PvaServer;
use spvirit_server::pvstore::Source;
use spvirit_types::{NtPayload, ScalarValue};

/// Every link field, in both a terse and an explicit spelling, plus a
/// constant link and a link to a non-`VAL` field.
const DB: &str = "record(ai, \"PV:A\") {
    field(INP, \"PV:B PP\")
    field(SDIS, \"PV:B.SEVR MS\")
    field(FLNK, \"PV:B\")
}
record(ao, \"PV:C\") {
    field(OUT, \"PV:B.VAL\")
    field(DOL, \"PV:B NPP MS\")
    field(FLNK, \"PV:B.PROC\")
}
record(ai, \"PV:B\") {
    field(INP, \"7\")
}
";

fn scalar(payload: &NtPayload) -> ScalarValue {
    match payload {
        NtPayload::Scalar(s) => s.value.clone(),
        other => panic!("expected a scalar, got {other:?}"),
    }
}

#[tokio::test]
async fn the_two_stores_render_link_fields_identically() {
    let ioc = IocSource::from_db_str(DB).expect("the engine loads the database");
    let builtin = PvaServer::builder().db_string(DB).build().store().clone();

    // What Base prints: the target verbatim (never a synthesized `.VAL`),
    // then both modifiers, always spelled out. A forward link addresses a
    // record, so it carries no field at all.
    let expected = [
        ("PV:A.INP", "PV:B PP NMS"),
        ("PV:A.SDIS", "PV:B.SEVR NPP MS"),
        ("PV:A.FLNK", "PV:B NPP NMS"),
        ("PV:C.OUT", "PV:B NPP NMS"),
        ("PV:C.DOL", "PV:B NPP MS"),
        ("PV:C.FLNK", "PV:B NPP NMS"),
    ];

    for (name, base_text) in expected {
        let from_ioc = ioc.get(name).await.map(|p| scalar(&p));
        let from_builtin = resolve_field_payload(&*builtin, name)
            .await
            .map(|p| scalar(&p));
        assert_eq!(
            from_ioc, from_builtin,
            "{name}: tier 1 and tier 2 must serve the same link text"
        );
        assert_eq!(
            from_ioc,
            Some(ScalarValue::Str(base_text.to_string())),
            "{name}: and it must be the text EPICS Base would print"
        );
    }
}

/// A constant link is not a link: both tiers serve the constant.
#[tokio::test]
async fn a_constant_link_is_served_as_the_constant_on_both_tiers() {
    let ioc = IocSource::from_db_str(DB).expect("the engine loads the database");
    let builtin = PvaServer::builder().db_string(DB).build().store().clone();
    let from_ioc = ioc.get("PV:B.INP").await.map(|p| scalar(&p));
    let from_builtin = resolve_field_payload(&*builtin, "PV:B.INP")
        .await
        .map(|p| scalar(&p));
    assert_eq!(from_ioc, from_builtin, "PV:B.INP");
    assert_eq!(from_ioc, Some(ScalarValue::Str("7".into())));
}
