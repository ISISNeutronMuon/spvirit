use spvirit_tools::spvirit_client::cli::{finite_number, require_pv_name};

#[test]
fn an_empty_pv_name_is_rejected_with_a_useful_message() {
    let err = require_pv_name("").expect_err("empty must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("PV name"), "unhelpful message: {msg}");
    assert!(require_pv_name("MY:PV").is_ok());
    assert!(
        require_pv_name("   ").is_err(),
        "whitespace-only is empty too"
    );
}

#[test]
fn non_finite_values_are_rejected_instead_of_panicking() {
    assert!(finite_number(f64::NAN).is_err());
    assert!(finite_number(f64::INFINITY).is_err());
    assert!(finite_number(f64::NEG_INFINITY).is_err());
    let ok = finite_number(1.5).expect("1.5 is finite");
    assert_eq!(ok.as_f64(), Some(1.5));
}

#[test]
fn the_non_finite_message_names_the_offending_value() {
    let msg = finite_number(f64::NAN).expect_err("NaN").to_string();
    assert!(
        msg.contains("NaN") || msg.contains("finite"),
        "unhelpful message: {msg}"
    );
}
