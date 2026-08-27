//! Build-time record descriptions.
//!
//! A [`RecordSpec`] is what a caller assembles before there is an engine; a
//! [`crate::model::Record`] is what the engine runs. They are deliberately
//! different types — a `Record` carries mutable `val`, alarm state and links
//! already resolved to `RecordId`s, none of which a caller can supply.
//!
//! The lowering is the whole design: a `RecordSpec` becomes a
//! [`DbRecord`] — the identical struct `.db` text parses into — and then goes
//! through the identical [`crate::build::build_records`]. There is exactly one
//! field-interpretation code path in this crate, so the programmatic and `.db`
//! paths cannot drift; `tests/programmatic.rs` pins that as an observable
//! property rather than trusting the claim.

use crate::alarm::Severity;
use crate::model::Kind;
use crate::source::IocSource;
use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_server::db::DbRecord;
use spvirit_server::pvstore::Source;
use spvirit_types::{NtPayload, ScalarValue};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Errors from operating a bound [`RecordSpec`] handle.
#[derive(Debug, Clone)]
pub enum SpecError {
    /// The spec has not been bound to a running engine yet.
    Unbound,
    /// The bound engine has no record under this name.
    NotFound(String),
    /// The scalar variant cannot be written to a record's `VAL`.
    Unsupported(String),
    /// The engine refused or failed the write.
    Write(String),
}

impl std::fmt::Display for SpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpecError::Unbound => write!(f, "record spec is not bound to an engine yet"),
            SpecError::NotFound(n) => write!(f, "no record named {n}"),
            SpecError::Unsupported(v) => write!(f, "scalar {v} cannot be written to VAL"),
            SpecError::Write(e) => write!(f, "write failed: {e}"),
        }
    }
}

impl std::error::Error for SpecError {}

/// The six scalar variants `IocSource::value_of` accepts as a `VAL` write.
/// This is the private `ScalarValue → DecodedValue` bridge of Ruling 6.
fn scalar_to_decoded(value: ScalarValue) -> Result<DecodedValue, SpecError> {
    Ok(match value {
        ScalarValue::F64(x) => DecodedValue::Float64(x),
        ScalarValue::F32(x) => DecodedValue::Float32(x),
        ScalarValue::I32(x) => DecodedValue::Int32(x),
        ScalarValue::I64(x) => DecodedValue::Int64(x),
        ScalarValue::U16(x) => DecodedValue::UInt16(x),
        ScalarValue::Bool(x) => DecodedValue::Boolean(x),
        other => return Err(SpecError::Unsupported(format!("{other:?}"))),
    })
}

/// The field names in `raw` that the engine does not model, sorted.
///
/// Not an error: the `.db` path has always accepted and ignored these (a
/// `.db` carrying `DRVH` loads fine and the drive limit does nothing), and
/// the programmatic path has to match it or the two construction paths are
/// not interchangeable. Callers surface this as a warning, once, at load.
///
/// The authority for "does the engine model this field" is
/// [`crate::fields::IOC_FIELDS`] — the one table the `.FIELD` reader already
/// uses. There is deliberately no second `MODELLED_FIELDS` list: a duplicate
/// would let the two drift (Ruling 3).
pub fn unmodelled_fields(raw: &DbRecord) -> Vec<String> {
    let mut out: Vec<String> = raw
        .fields
        .keys()
        .filter(|k| !crate::fields::IOC_FIELDS.contains(&k.as_str()))
        .cloned()
        .collect();
    out.sort();
    out
}

/// Render a float the way a `.db` file writes one: `100`, not `100.0`.
///
/// `< 1e15` (rather than `<=` or `==`) is the guard against `as i64`
/// silently saturating on a value so large the cast can't represent it —
/// 1e15 is comfortably inside f64's exact-integer range (2^53 ≈ 9.007e15),
/// so every whole number this branch actually reaches round-trips through
/// `as i64` and through `Display` identically. That makes `==` and `<=` at
/// this comparison equivalent mutants under `cargo mutants`: they can only
/// disagree with `<` at the single point `v.abs() == 1e15`, and at that
/// point both branches render the same digits, so no value-based test can
/// distinguish them. `>` is not equivalent — it is caught by
/// `f_does_not_apply_the_terse_form_far_above_the_threshold` below, which
/// exercises a magnitude where `as i64` actually saturates.
fn f(v: f64) -> String {
    if v.fract() == 0.0 && v.is_finite() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// The shared inner state of a [`RecordSpec`].
///
/// `name` and `kind` are fixed at construction; `fields` is what the builder
/// setters accumulate; `source` is the binding slot that lets the same
/// handle address the record once the engine is built. A `RecordSpec` is a
/// build-time description that also carries this binding slot once bound.
struct SpecShared {
    name: String,
    kind: Kind,
    fields: Mutex<HashMap<String, String>>,
    /// `None` until [`RecordSpec::bind`] attaches the built engine. Once
    /// `Some`, the builder setters warn-and-no-op and (Task 5) `get`/`set`
    /// reach the engine instead of returning `Unbound`.
    source: Mutex<Option<Arc<IocSource>>>,
}

/// A record described but not yet built.
///
/// See the module docs for why this is not [`crate::model::Record`]. A
/// `RecordSpec` is an `Arc` handle: cloning it is a cheap refcount bump and
/// every clone names the same record, so a builder call on any clone is seen
/// by all of them. Task 3 binds it to a built engine and Task 5 gives it
/// `get`/`set` — the same pending→bound shape `Pv` already uses in the server
/// crate (Ruling 6). Interior mutability (the `Mutex`) is what lets the
/// fluent `self`-consuming builders mutate shared state behind the `Arc`.
#[derive(Clone)]
pub struct RecordSpec(Arc<SpecShared>);

macro_rules! setter_str {
    ($($m:ident => $F:literal),* $(,)?) => {$(
        #[doc = concat!("Set the `", $F, "` field.")]
        pub fn $m(self, value: &str) -> RecordSpec { self.field($F, value) }
    )*};
}

macro_rules! setter_f64 {
    ($($m:ident => $F:literal),* $(,)?) => {$(
        #[doc = concat!("Set the `", $F, "` field.")]
        pub fn $m(self, value: f64) -> RecordSpec { self.field($F, f(value)) }
    )*};
}

macro_rules! setter_i32 {
    ($($m:ident => $F:literal),* $(,)?) => {$(
        #[doc = concat!("Set the `", $F, "` field.")]
        pub fn $m(self, value: i32) -> RecordSpec { self.field($F, value.to_string()) }
    )*};
}

macro_rules! setter_sev {
    ($($m:ident => $F:literal),* $(,)?) => {$(
        #[doc = concat!("Set the `", $F, "` severity field.")]
        pub fn $m(self, value: Severity) -> RecordSpec {
            self.field($F, value.epics_string())
        }
    )*};
}

impl RecordSpec {
    pub fn new(kind: Kind, name: impl Into<String>) -> RecordSpec {
        RecordSpec(Arc::new(SpecShared {
            name: name.into(),
            kind,
            fields: Mutex::new(HashMap::new()),
            source: Mutex::new(None),
        }))
    }

    pub fn ai(name: impl Into<String>) -> RecordSpec { Self::new(Kind::Ai, name) }
    pub fn ao(name: impl Into<String>) -> RecordSpec { Self::new(Kind::Ao, name) }
    pub fn bi(name: impl Into<String>) -> RecordSpec { Self::new(Kind::Bi, name) }
    pub fn bo(name: impl Into<String>) -> RecordSpec { Self::new(Kind::Bo, name) }
    pub fn longin(name: impl Into<String>) -> RecordSpec { Self::new(Kind::LongIn, name) }
    pub fn longout(name: impl Into<String>) -> RecordSpec { Self::new(Kind::LongOut, name) }

    pub fn name(&self) -> &str {
        &self.0.name
    }

    pub fn kind(&self) -> Kind {
        self.0.kind
    }

    /// Set any field by its verbatim EPICS name.
    ///
    /// The escape hatch for fields with no typed setter — including the ones
    /// the engine does not model, such as `DRVH`, which are carried and then
    /// dropped by the builder exactly as they are on the `.db` path. The name
    /// is uppercased, so `.field("egu", …)` sets `EGU` rather than adding a
    /// second key nothing reads.
    pub fn field(self, name: &str, value: impl Into<String>) -> RecordSpec {
        if self.0.source.lock().unwrap().is_some() {
            tracing::warn!(
                target: "spvirit_ioc",
                record = %self.0.name,
                field = name,
                "RecordSpec field set after the record was built is ignored"
            );
            return self;
        }
        self.0
            .fields
            .lock()
            .unwrap()
            .insert(name.to_ascii_uppercase(), value.into());
        self
    }

    setter_str! {
        desc => "DESC", egu => "EGU", inp => "INP", out => "OUT", dol => "DOL",
        flnk => "FLNK", sdis => "SDIS", scan => "SCAN", omsl => "OMSL",
    }

    setter_f64! {
        val => "VAL", hihi => "HIHI", high => "HIGH", low => "LOW", lolo => "LOLO",
        hyst => "HYST", mdel => "MDEL", adel => "ADEL",
    }

    setter_i32! { phas => "PHAS", disa => "DISA", disv => "DISV", tse => "TSE" }

    setter_sev! {
        diss => "DISS", hhsv => "HHSV", hsv => "HSV", lsv => "LSV", llsv => "LLSV",
    }

    /// Set `PINI`. EPICS spells the menu `YES`/`NO`.
    pub fn pini(self, yes: bool) -> RecordSpec {
        self.field("PINI", if yes { "YES" } else { "NO" })
    }

    /// Set `TPRO`, the per-record process trace flag.
    pub fn tpro(self, on: bool) -> RecordSpec {
        self.field("TPRO", if on { "1" } else { "0" })
    }

    /// Lower to the struct the `.db` parser produces.
    ///
    /// Non-consuming (`&self`), so the handle stays alive for Task 3 to bind
    /// after the engine is built. `from_records` calls this on every spec,
    /// feeds the results through the identical [`crate::build::build_records`],
    /// then binds each spec to the result.
    pub fn to_db_record(&self) -> DbRecord {
        DbRecord {
            name: self.0.name.clone(),
            record_type: self.0.kind.db_name().to_string(),
            fields: self.0.fields.lock().unwrap().clone(),
        }
    }

    /// Attach the built engine, flipping this handle from pending to bound.
    ///
    /// Called by [`crate::IocSource::from_records`] once the engine exists —
    /// the tier-3 analogue of `ServeBuilder::build` binding its `Pv`s
    /// (`pva_server.rs:1140-1142`). Every clone shares the slot, so binding
    /// here binds them all.
    pub(crate) fn bind(&self, source: &Arc<IocSource>) {
        *self.0.source.lock().unwrap() = Some(Arc::clone(source));
    }

    /// The bound engine, or `Err(Unbound)` while the spec is still pending.
    fn bound(&self) -> Result<Arc<IocSource>, SpecError> {
        self.0
            .source
            .lock()
            .unwrap()
            .clone()
            .ok_or(SpecError::Unbound)
    }

    /// Whether the binding slot is filled — i.e. this spec (or a clone that
    /// shares its slot) has been built into an engine. Python's `Ioc(...)`
    /// reads this to keep a spec to one engine without a Python-side flag.
    pub fn is_bound(&self) -> bool {
        self.0.source.lock().unwrap().is_some()
    }

    /// Read this record's current scalar `VAL`.
    ///
    /// `Err(SpecError::Unbound)` before bind — the tier-3 analogue of tier 2's
    /// `Pv::store()` on a pending handle (`pv.rs:598-603`).
    pub async fn get(&self) -> Result<ScalarValue, SpecError> {
        let source = self.bound()?;
        let payload = source
            .get(&self.0.name)
            .await
            .ok_or_else(|| SpecError::NotFound(self.0.name.clone()))?;
        match payload {
            NtPayload::Scalar(s) => Ok(s.value),
            other => Err(SpecError::Unsupported(format!("{other:?}"))),
        }
    }

    /// Write this record's `VAL` from host code and process, publishing the
    /// result exactly as a client PUT would.
    ///
    /// Delegates to [`IocSource::set_value`] so the write goes through the one
    /// path that publishes host-side writes (see Task 5's publication trap).
    /// `Err(SpecError::Unbound)` before bind.
    pub async fn set(&self, value: ScalarValue) -> Result<(), SpecError> {
        let source = self.bound()?;
        let decoded = scalar_to_decoded(value)?;
        source
            .set_value(&self.0.name, decoded)
            .await
            .map_err(SpecError::Write)?;
        Ok(())
    }

    /// Read one of this record's fields by its verbatim EPICS name (e.g.
    /// `"EGU"`). The binding slot lives only here, so the Python `rec["EGU"]`
    /// handle delegates to this rather than reaching for the engine itself.
    ///
    /// `Err(SpecError::Unbound)` before bind; `Err(SpecError::NotFound)` for a
    /// field the record does not carry. Field *writes* are sub-project B.
    pub async fn get_field(&self, field: &str) -> Result<ScalarValue, SpecError> {
        let source = self.bound()?;
        let pv = format!("{}.{}", self.0.name, field.to_ascii_uppercase());
        let payload = source.get(&pv).await.ok_or(SpecError::NotFound(pv))?;
        match payload {
            NtPayload::Scalar(s) => Ok(s.value),
            other => Err(SpecError::Unsupported(format!("{other:?}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alarm::Severity;

    #[test]
    fn a_spec_lowers_to_the_same_db_record_the_parser_would_produce() {
        let spec = RecordSpec::ai("RIG:RBV")
            .inp("RIG:SP PP")
            .egu("C")
            .hihi(100.0)
            .hhsv(Severity::Major)
            .mdel(0.1);
        let raw = spec.to_db_record();

        assert_eq!(raw.name, "RIG:RBV");
        assert_eq!(raw.record_type, "ai");
        assert_eq!(raw.fields.get("INP").map(String::as_str), Some("RIG:SP PP"));
        assert_eq!(raw.fields.get("EGU").map(String::as_str), Some("C"));
        assert_eq!(raw.fields.get("HIHI").map(String::as_str), Some("100"));
        assert_eq!(raw.fields.get("HHSV").map(String::as_str), Some("MAJOR"));
        assert_eq!(raw.fields.get("MDEL").map(String::as_str), Some("0.1"));
    }

    /// Field names are verbatim EPICS and case-insensitive on input, because
    /// a caller typing `.field("egu", …)` means `EGU` and silently getting a
    /// second, ignored key would be the worst outcome.
    #[test]
    fn the_escape_hatch_uppercases_the_field_name() {
        let raw = RecordSpec::ao("X").field("egu", "mm").to_db_record();
        assert_eq!(raw.fields.get("EGU").map(String::as_str), Some("mm"));
        assert!(!raw.fields.contains_key("egu"));
    }

    /// Ruling 3: DRVH is not modelled by the engine, but it must still be
    /// accepted and carried, because the `.db` path accepts and ignores it
    /// and the two paths have to behave identically.
    #[test]
    fn an_unmodelled_field_is_carried_not_rejected() {
        let raw = RecordSpec::ao("X").field("DRVH", "100").to_db_record();
        assert_eq!(raw.fields.get("DRVH").map(String::as_str), Some("100"));
        assert_eq!(unmodelled_fields(&raw), vec!["DRVH".to_string()]);
    }

    #[test]
    fn a_fully_modelled_record_reports_no_unmodelled_fields() {
        let raw = RecordSpec::ai("X").egu("C").hihi(1.0).to_db_record();
        assert!(unmodelled_fields(&raw).is_empty());
    }

    /// The last write wins, so a typed setter and the escape hatch naming the
    /// same field do not both survive into the map.
    #[test]
    fn setting_a_field_twice_keeps_the_last_value() {
        let raw = RecordSpec::ai("X").egu("C").field("EGU", "K").to_db_record();
        assert_eq!(raw.fields.get("EGU").map(String::as_str), Some("K"));
    }

    /// Floats render without a trailing `.0` so the lowered text matches what
    /// a human writes in a `.db` file. `build.rs` parses either, but the
    /// round-trip test compares field *text* on both paths.
    #[test]
    fn whole_floats_render_without_a_decimal_point() {
        let raw = RecordSpec::ai("X").hihi(100.0).hyst(0.5).to_db_record();
        assert_eq!(raw.fields.get("HIHI").map(String::as_str), Some("100"));
        assert_eq!(raw.fields.get("HYST").map(String::as_str), Some("0.5"));
    }

    #[test]
    fn pini_and_tpro_render_as_the_epics_menu_strings() {
        let yes = RecordSpec::ai("X").pini(true).tpro(true).to_db_record();
        assert_eq!(yes.fields.get("PINI").map(String::as_str), Some("YES"));
        assert_eq!(yes.fields.get("TPRO").map(String::as_str), Some("1"));
        let no = RecordSpec::ai("Y").pini(false).to_db_record();
        assert_eq!(no.fields.get("PINI").map(String::as_str), Some("NO"));
    }

    /// The `Display` text is what a host sees in an error message, so its
    /// exact wording is observable behaviour, not an implementation detail
    /// `Debug` already covers.
    #[test]
    fn spec_error_display_messages_name_the_problem() {
        assert_eq!(
            SpecError::Unbound.to_string(),
            "record spec is not bound to an engine yet"
        );
        assert_eq!(SpecError::NotFound("X".into()).to_string(), "no record named X");
        assert_eq!(
            SpecError::Unsupported("Bool(true)".into()).to_string(),
            "scalar Bool(true) cannot be written to VAL"
        );
        assert_eq!(
            SpecError::Write("boom".into()).to_string(),
            "write failed: boom"
        );
    }

    /// A record's own name is what the engine looks it up by, and what a
    /// host reads back to identify which handle it holds — it must be the
    /// name given at construction, not a placeholder.
    #[test]
    fn a_specs_name_is_the_one_it_was_constructed_with() {
        assert_eq!(RecordSpec::ai("RIG:RBV").name(), "RIG:RBV");
        assert_eq!(RecordSpec::ao("OTHER:PV").name(), "OTHER:PV");
    }

    /// `is_bound` is what Python's `Ioc(...)` relies on to keep a spec to
    /// one engine (Ruling: "a RecordSpec belongs to one Ioc") — it must
    /// read `false` before `bind` and `true` after, for real.
    #[test]
    fn is_bound_reflects_whether_bind_was_ever_called() {
        let spec = RecordSpec::ai("REG:BOUND");
        assert!(!spec.is_bound(), "a fresh spec is not yet bound");
        let _ioc = crate::IocSource::from_records(vec![spec.clone()]).expect("must build");
        assert!(spec.is_bound(), "building the engine must bind the spec");
    }

    /// `f`'s `< 1e15` guard exists so a value far too large for the terse
    /// `as i64` form falls back to `f64`'s own `Display` instead of
    /// silently saturating. Replacing `<` with `>` at that comparison would
    /// send a huge value through the `as i64` cast, which saturates to
    /// `i64::MAX` — a visibly wrong render this test catches. (`==` and
    /// `<=` at the same spot are equivalent mutants — see the comment on
    /// `f` above.)
    #[test]
    fn f_does_not_apply_the_terse_form_far_above_the_threshold() {
        assert_eq!(f(1e20), format!("{}", 1e20_f64));
    }

    #[test]
    fn every_record_kind_has_a_constructor() {
        for (spec, want) in [
            (RecordSpec::ai("A"), "ai"),
            (RecordSpec::ao("A"), "ao"),
            (RecordSpec::bi("A"), "bi"),
            (RecordSpec::bo("A"), "bo"),
            (RecordSpec::longin("A"), "longin"),
            (RecordSpec::longout("A"), "longout"),
        ] {
            assert_eq!(spec.to_db_record().record_type, want);
        }
    }

    /// Once the engine is built, the spec is a live handle, not a builder;
    /// a late setter must not silently rewrite a record that already exists.
    #[test]
    fn a_builder_call_after_bind_is_ignored() {
        let spec = RecordSpec::ai("REG:X").egu("C");
        let _ioc = crate::IocSource::from_records(vec![spec.clone()])
            .expect("must build");
        let before = spec.to_db_record();
        let _ = spec.clone().egu("K"); // ignored: REG:X is already built
        assert_eq!(
            spec.to_db_record().fields.get("EGU"),
            before.fields.get("EGU"),
            "a field set after bind must not change the spec"
        );
    }
}
