//! Typed PV handles — the ergonomic front door to [`SimplePvStore`].
//!
//! A [`Pv<T>`] is created *pending* (it owns a record template plus attached
//! callbacks) and becomes *bound* to a store when passed to
//! `PvaServer::serve(...)`. Handles are cheap clones; all clones observe and
//! drive the same record.
//!
//! # Hello world
//!
//! ```rust,ignore
//! use spvirit_server::{AnyPv, Pv, PvaServer};
//!
//! let temp = Pv::ai("SIM:TEMP", 22.5).units("C").prec(2);
//! let sp = Pv::ao("SIM:SETPOINT", 25.0)
//!     .on_put(|_pv, v: f64| if v.is_finite() { Ok(()) } else { Err("NaN".into()) });
//!
//! let server = PvaServer::serve([AnyPv::from(temp.clone()), AnyPv::from(sp)])
//!     .start()
//!     .await?;
//! temp.set(23.1).await?;
//! ```
//!
//! # Bulk creation
//!
//! Handles are ordinary values, so a whole bank of PVs can be built with an
//! iterator and handed to `serve` as a `Vec<AnyPv>`:
//!
//! ```rust,ignore
//! use spvirit_server::{AnyPv, Pv, PvaServer};
//!
//! let channels: Vec<Pv<f64>> = (0..8)
//!     .map(|i| Pv::ai(format!("SIM:CH{i}"), 0.0).units("C"))
//!     .collect();
//! let pvs: Vec<AnyPv> = channels.iter().cloned().map(AnyPv::from).collect();
//!
//! let server = PvaServer::serve(pvs).start().await?;
//! channels[0].set(21.3).await?;
//! ```

use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_types::{NtPayload, ScalarArrayValue, ScalarValue};

use crate::pva_server::{make_array_record, make_output_record, make_scalar_record};
use crate::simple_store::SimplePvStore;
use crate::types::{RecordInstance, RecordType};

/// Errors from typed PV handle operations.
#[derive(Debug, Clone, PartialEq)]
pub enum PvError {
    /// The handle has not been bound to a server/store yet.
    Unbound,
    /// No record with this name exists in the store.
    NotFound(String),
    /// The record's value type does not match the handle's `T`.
    TypeMismatch {
        expected: &'static str,
        actual: String,
    },
    /// A PUT was rejected by an `on_put` callback.
    PutRejected(String),
}

impl std::fmt::Display for PvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PvError::Unbound => write!(f, "PV handle is not bound to a server yet"),
            PvError::NotFound(n) => write!(f, "PV '{n}' not found"),
            PvError::TypeMismatch { expected, actual } => {
                write!(
                    f,
                    "PV value type mismatch: expected {expected}, record holds {actual}"
                )
            }
            PvError::PutRejected(msg) => write!(f, "PUT rejected: {msg}"),
        }
    }
}

impl std::error::Error for PvError {}

/// Scalar types that can back a typed [`Pv<T>`] handle.
pub trait PvScalar: Sized + Send + Sync + 'static {
    /// Human-readable type name, used in [`PvError::TypeMismatch`].
    const TYPE_NAME: &'static str;
    fn into_scalar(self) -> ScalarValue;
    fn from_scalar(v: ScalarValue) -> Option<Self>;

    /// Convert a decoded wire PUT value directly to `Self`.
    ///
    /// The default goes through [`crate::convert::decoded_to_scalar_value`],
    /// but that helper checks "is this truthy/falsy" (bool) before it checks
    /// numeric types, so *any* nonzero numeric [`DecodedValue`] resolves to
    /// `ScalarValue::Bool` first — which then fails `from_scalar` for `f64`
    /// and `i32`. Those impls override this method with a type-directed
    /// decoder (`decoded_to_f64` / `decoded_to_i32`) so ordinary numeric PUTs
    /// aren't spuriously rejected.
    fn from_decoded(dv: &DecodedValue) -> Option<Self> {
        Self::from_scalar(crate::convert::decoded_to_scalar_value(dv))
    }
}

impl PvScalar for f64 {
    const TYPE_NAME: &'static str = "f64";
    fn into_scalar(self) -> ScalarValue {
        ScalarValue::F64(self)
    }
    fn from_scalar(v: ScalarValue) -> Option<Self> {
        match v {
            ScalarValue::F64(x) => Some(x),
            ScalarValue::F32(x) => Some(x as f64),
            _ => None,
        }
    }
    fn from_decoded(dv: &DecodedValue) -> Option<Self> {
        crate::convert::decoded_to_f64(dv)
    }
}

impl PvScalar for bool {
    const TYPE_NAME: &'static str = "bool";
    fn into_scalar(self) -> ScalarValue {
        ScalarValue::Bool(self)
    }
    fn from_scalar(v: ScalarValue) -> Option<Self> {
        match v {
            ScalarValue::Bool(b) => Some(b),
            _ => None,
        }
    }
    fn from_decoded(dv: &DecodedValue) -> Option<Self> {
        crate::convert::decoded_to_bool(dv)
    }
}

impl PvScalar for i32 {
    const TYPE_NAME: &'static str = "i32";
    fn into_scalar(self) -> ScalarValue {
        ScalarValue::I32(self)
    }
    fn from_scalar(v: ScalarValue) -> Option<Self> {
        match v {
            ScalarValue::I32(x) => Some(x),
            ScalarValue::I16(x) => Some(x as i32),
            ScalarValue::I8(x) => Some(x as i32),
            _ => None,
        }
    }
    fn from_decoded(dv: &DecodedValue) -> Option<Self> {
        crate::convert::decoded_to_i32(dv)
    }
}

impl PvScalar for String {
    const TYPE_NAME: &'static str = "String";
    fn into_scalar(self) -> ScalarValue {
        ScalarValue::Str(self)
    }
    fn from_scalar(v: ScalarValue) -> Option<Self> {
        match v {
            ScalarValue::Str(s) => Some(s),
            _ => None,
        }
    }
    fn from_decoded(dv: &DecodedValue) -> Option<Self> {
        crate::convert::decoded_to_string(dv)
    }
}

impl PvScalar for ScalarValue {
    const TYPE_NAME: &'static str = "scalar";
    fn into_scalar(self) -> ScalarValue {
        self
    }
    fn from_scalar(v: ScalarValue) -> Option<Self> {
        Some(v)
    }
    /// Faithful 1:1 structural mapping — deliberately NOT the generic
    /// `decoded_to_scalar_value`, whose truthy-check-first order turns any
    /// nonzero numeric into `Bool` (see the trait doc above). Callers that
    /// need a specific wire type re-coerce the returned variant themselves.
    fn from_decoded(dv: &DecodedValue) -> Option<Self> {
        Some(match dv {
            DecodedValue::Boolean(b) => ScalarValue::Bool(*b),
            DecodedValue::Int8(n) => ScalarValue::I8(*n),
            DecodedValue::Int16(n) => ScalarValue::I16(*n),
            DecodedValue::Int32(n) => ScalarValue::I32(*n),
            DecodedValue::Int64(n) => ScalarValue::I64(*n),
            DecodedValue::UInt8(n) => ScalarValue::U8(*n),
            DecodedValue::UInt16(n) => ScalarValue::U16(*n),
            DecodedValue::UInt32(n) => ScalarValue::U32(*n),
            DecodedValue::UInt64(n) => ScalarValue::U64(*n),
            DecodedValue::Float32(f) => ScalarValue::F32(*f),
            DecodedValue::Float64(f) => ScalarValue::F64(*f),
            DecodedValue::String(s) => ScalarValue::Str(s.clone()),
            _ => return None,
        })
    }
}

pub(crate) struct PendingDef {
    pub(crate) record: RecordInstance,
    pub(crate) validator: Option<crate::simple_store::PutValidator>,
    pub(crate) scan: Option<(std::time::Duration, crate::simple_store::ScanCallback)>,
    pub(crate) calc: Option<(Vec<String>, crate::simple_store::LinkCallback)>,
}

pub(crate) enum PvState {
    Pending(PendingDef),
    Bound(Arc<SimplePvStore>),
}

pub(crate) struct PvShared {
    pub(crate) name: String,
    pub(crate) state: Mutex<PvState>,
}

/// Typed handle to a PV record. Cheap to clone; all clones share state.
pub struct Pv<T: PvScalar> {
    pub(crate) shared: Arc<PvShared>,
    _marker: PhantomData<fn() -> T>,
}

impl<T: PvScalar> Clone for Pv<T> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T: PvScalar> Pv<T> {
    fn from_record(record: RecordInstance) -> Self {
        Self {
            shared: Arc::new(PvShared {
                name: record.name.clone(),
                state: Mutex::new(PvState::Pending(PendingDef {
                    record,
                    validator: None,
                    scan: None,
                    calc: None,
                })),
            }),
            _marker: PhantomData,
        }
    }

    pub fn name(&self) -> &str {
        &self.shared.name
    }

    /// Clone of the pending record template; `None` once bound. Test-only —
    /// `ServeBuilder` reads the pending record via `AnyPv::take_record`.
    #[cfg(test)]
    fn pending_record(&self) -> Option<RecordInstance> {
        match &*self.shared.state.lock().unwrap() {
            PvState::Pending(def) => Some(def.record.clone()),
            PvState::Bound(_) => None,
        }
    }

    /// Mutate the pending record template. Warns and no-ops if already bound.
    fn with_record(self, f: impl FnOnce(&mut RecordInstance)) -> Self {
        {
            let mut state = self.shared.state.lock().unwrap();
            match &mut *state {
                PvState::Pending(def) => f(&mut def.record),
                PvState::Bound(_) => {
                    tracing::warn!("Pv '{}': option ignored, already bound", self.shared.name)
                }
            }
        }
        self
    }

    pub fn units(self, units: impl Into<String>) -> Self {
        let u = units.into();
        self.with_record(|r| {
            if let Some(nt) = r.nt_scalar_mut() {
                nt.units = u;
            }
        })
    }

    pub fn prec(self, prec: i32) -> Self {
        self.with_record(|r| {
            if let Some(nt) = r.nt_scalar_mut() {
                nt.display_precision = prec;
            }
        })
    }

    pub fn desc(self, desc: impl Into<String>) -> Self {
        let d = desc.into();
        self.with_record(|r| {
            r.common.desc = d.clone();
            if let Some(nt) = r.nt_scalar_mut() {
                nt.display_description = d;
            }
        })
    }

    /// Archive deadband (parsed/exposed via field access; PVA monitors use MDEL).
    pub fn adel(self, deadband: f64) -> Self {
        self.with_record(|r| {
            r.raw_fields.insert("ADEL".into(), trim_float(deadband));
        })
    }

    /// Monitor deadband — suppresses monitor posts for changes smaller than this.
    pub fn mdel(self, deadband: f64) -> Self {
        self.with_record(|r| {
            r.raw_fields.insert("MDEL".into(), trim_float(deadband));
        })
    }

    pub fn drive_limits(self, low: f64, high: f64) -> Self {
        self.with_record(|r| {
            if let Some(nt) = r.nt_scalar_mut() {
                nt.control_low = low;
                nt.control_high = high;
            }
        })
    }

    pub fn alarm_limits(self, lolo: f64, low: f64, high: f64, hihi: f64) -> Self {
        self.with_record(|r| {
            if let Some(nt) = r.nt_scalar_mut() {
                nt.value_alarm_active = true;
                nt.value_alarm_low_alarm_limit = lolo;
                nt.value_alarm_low_warning_limit = low;
                nt.value_alarm_high_warning_limit = high;
                nt.value_alarm_high_alarm_limit = hihi;
            }
        })
    }

    /// Attach a PUT handler. `Err(msg)` rejects the PUT on the wire; `Ok(())`
    /// accepts it. Called with a bound handle to this PV and the typed value.
    pub fn on_put<F>(self, f: F) -> Self
    where
        F: Fn(&Pv<T>, T) -> Result<(), String> + Send + Sync + 'static,
    {
        let handle = self.clone();
        let validator: crate::simple_store::PutValidator = Arc::new(move |_name, dv| {
            // Scalar puts may arrive wrapped as a Structure with a "value"
            // field (mirrors the bare-scalar wrapping in
            // simple_store::apply_put_to_record) — unwrap it the same way
            // before converting, or a wrapped put would fail the typed
            // conversion and be spuriously rejected.
            let scalar_dv = unwrap_value_field(dv);
            let typed = T::from_decoded(scalar_dv)
                .ok_or_else(|| format!("expected {}, got {scalar_dv:?}", T::TYPE_NAME))?;
            f(&handle, typed)
        });
        {
            let mut state = self.shared.state.lock().unwrap();
            match &mut *state {
                PvState::Pending(def) => def.validator = Some(validator),
                PvState::Bound(_) => {
                    tracing::warn!("Pv '{}': on_put ignored, already bound", self.shared.name)
                }
            }
        }
        self
    }

    /// Periodically compute and post a new value for this PV.
    pub fn scan<F>(self, period: std::time::Duration, f: F) -> Self
    where
        F: Fn(&Pv<T>) -> T + Send + Sync + 'static,
    {
        let handle = self.clone();
        let cb: crate::simple_store::ScanCallback = Arc::new(move |_name| f(&handle).into_scalar());
        {
            let mut state = self.shared.state.lock().unwrap();
            if let PvState::Pending(def) = &mut *state {
                def.scan = Some((period, cb));
            } else {
                tracing::warn!("Pv '{}': scan ignored, already bound", self.shared.name);
            }
        }
        self
    }
}

impl Pv<f64> {
    /// A derived (read-only) PV recomputed whenever any input changes.
    pub fn calc<F>(name: impl Into<String>, inputs: &[&Pv<f64>], f: F) -> Self
    where
        F: Fn(&[f64]) -> f64 + Send + Sync + 'static,
    {
        let name = name.into();
        let input_names: Vec<String> = inputs.iter().map(|p| p.shared.name.clone()).collect();
        let initial = ScalarValue::F64(0.0);
        let pv = Self::from_record(make_scalar_record(&name, RecordType::Ai, initial));
        let compute: crate::simple_store::LinkCallback = Arc::new(move |values| {
            let floats: Vec<f64> = values
                .iter()
                .map(|v| f64::from_scalar(v.clone()).unwrap_or(0.0))
                .collect();
            ScalarValue::F64(f(&floats))
        });
        if let PvState::Pending(def) = &mut *pv.shared.state.lock().unwrap() {
            def.calc = Some((input_names, compute));
        }
        pv
    }
}

/// Mirrors `apply_put_to_record`'s bare-scalar wrapping: if `dv` is a
/// `Structure`, pull out its "value" field; otherwise treat `dv` itself as
/// the scalar value.
fn unwrap_value_field(dv: &DecodedValue) -> &DecodedValue {
    match dv {
        DecodedValue::Structure(fields) => fields
            .iter()
            .find(|(name, _)| name == "value")
            .map(|(_, v)| v)
            .unwrap_or(dv),
        other => other,
    }
}

/// Format a float like EPICS .db files do (no trailing ".0" for integers).
fn trim_float(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

impl Pv<f64> {
    pub fn ai(name: impl Into<String>, initial: f64) -> Self {
        let name = name.into();
        Self::from_record(make_scalar_record(
            &name,
            RecordType::Ai,
            ScalarValue::F64(initial),
        ))
    }
    pub fn ao(name: impl Into<String>, initial: f64) -> Self {
        let name = name.into();
        Self::from_record(make_output_record(
            &name,
            RecordType::Ao,
            ScalarValue::F64(initial),
        ))
    }
}

impl Pv<bool> {
    pub fn bi(name: impl Into<String>, initial: bool) -> Self {
        let name = name.into();
        Self::from_record(make_scalar_record(
            &name,
            RecordType::Bi,
            ScalarValue::Bool(initial),
        ))
    }
    pub fn bo(name: impl Into<String>, initial: bool) -> Self {
        let name = name.into();
        Self::from_record(make_output_record(
            &name,
            RecordType::Bo,
            ScalarValue::Bool(initial),
        ))
    }
}

impl Pv<String> {
    pub fn string_in(name: impl Into<String>, initial: impl Into<String>) -> Self {
        let name = name.into();
        Self::from_record(make_scalar_record(
            &name,
            RecordType::StringIn,
            ScalarValue::Str(initial.into()),
        ))
    }
    pub fn string_out(name: impl Into<String>, initial: impl Into<String>) -> Self {
        let name = name.into();
        Self::from_record(make_output_record(
            &name,
            RecordType::StringOut,
            ScalarValue::Str(initial.into()),
        ))
    }
}

impl Pv<i32> {
    /// `longin` — 32-bit integer input record (read-only over the wire).
    pub fn longin(name: impl Into<String>, initial: i32) -> Self {
        let name = name.into();
        Self::from_record(make_scalar_record(
            &name,
            RecordType::LongIn,
            ScalarValue::I32(initial),
        ))
    }
    /// `longout` — 32-bit integer output record (writable).
    pub fn longout(name: impl Into<String>, initial: i32) -> Self {
        let name = name.into();
        Self::from_record(make_output_record(
            &name,
            RecordType::LongOut,
            ScalarValue::I32(initial),
        ))
    }
    /// `mbbi` — multi-bit binary input (enum, read-only). Value = choice index.
    pub fn mbbi(name: impl Into<String>, choices: Vec<String>, initial: i32) -> Self {
        Self::from_enum_record(name.into(), choices, initial, RecordType::Mbbi)
    }
    /// `mbbo` — multi-bit binary output (enum, writable). Value = choice index.
    pub fn mbbo(name: impl Into<String>, choices: Vec<String>, initial: i32) -> Self {
        Self::from_enum_record(name.into(), choices, initial, RecordType::Mbbo)
    }
    fn from_enum_record(name: String, choices: Vec<String>, initial: i32, rt: RecordType) -> Self {
        // Mirror PvaServerBuilder::mbbi's record shape (pva_server.rs:360-378).
        let data = crate::types::RecordData::NtEnum {
            nt: spvirit_types::NtEnum::new(initial, choices),
            inp: None,
            out: None,
            omsl: crate::types::OutputMode::Supervisory,
        };
        Self::from_record(RecordInstance {
            name: name.clone(),
            record_type: rt,
            common: crate::types::DbCommonState::default(),
            data,
            raw_fields: std::collections::HashMap::new(),
        })
    }
}

/// Family record type for a dynamically typed scalar: the record *shape*
/// (RTYP, writability) comes from the value family, while the `NtScalar`
/// payload's `ScalarValue` variant carries the precise wire type.
pub(crate) fn scalar_family_record_type(v: &ScalarValue, writable: bool) -> RecordType {
    match (v, writable) {
        (ScalarValue::F32(_) | ScalarValue::F64(_), false) => RecordType::Ai,
        (ScalarValue::F32(_) | ScalarValue::F64(_), true) => RecordType::Ao,
        (ScalarValue::Bool(_), false) => RecordType::Bi,
        (ScalarValue::Bool(_), true) => RecordType::Bo,
        (ScalarValue::Str(_), false) => RecordType::StringIn,
        (ScalarValue::Str(_), true) => RecordType::StringOut,
        (_, false) => RecordType::LongIn,
        (_, true) => RecordType::LongOut,
    }
}

impl Pv<ScalarValue> {
    /// Dynamically typed scalar record, read-only over the wire.
    ///
    /// The wire value type is whatever `ScalarValue` variant `initial`
    /// holds — this is the route to any of the twelve NTScalar types
    /// (`boolean`, `byte`, `short`, `int`, `long`, `ubyte`, `ushort`,
    /// `uint`, `ulong`, `float`, `double`, `string`), including the eight
    /// (`byte`/`short`/`ubyte`/`ushort`/`uint`/`ulong`, plus explicit
    /// `float`/`long`) that the fixed-type constructors (`Pv::ai`, `bi`,
    /// `longin`, `string_in`, ...) cannot produce. See
    /// `Pv::<ScalarValue>::scalar_out` for the writable flavor.
    ///
    /// ```
    /// use spvirit_server::Pv;
    /// use spvirit_types::ScalarValue;
    ///
    /// let status = Pv::<ScalarValue>::scalar_in("SIM:STATUS", ScalarValue::U8(0));
    /// ```
    pub fn scalar_in(name: impl Into<String>, initial: ScalarValue) -> Self {
        let name = name.into();
        let rt = scalar_family_record_type(&initial, false);
        Self::from_record(make_scalar_record(&name, rt, initial))
    }
    /// Dynamically typed scalar record, writable over the wire.
    ///
    /// Same type coverage as [`Pv::<ScalarValue>::scalar_in`] — the wire
    /// value type is whatever `ScalarValue` variant `initial` holds, across
    /// all twelve NTScalar types.
    ///
    /// ```
    /// use spvirit_server::Pv;
    /// use spvirit_types::ScalarValue;
    ///
    /// let gain = Pv::<ScalarValue>::scalar_out("SIM:GAIN", ScalarValue::U16(1));
    /// ```
    pub fn scalar_out(name: impl Into<String>, initial: ScalarValue) -> Self {
        let name = name.into();
        let rt = scalar_family_record_type(&initial, true);
        Self::from_record(make_output_record(&name, rt, initial))
    }
}

impl<T: PvScalar> Pv<T> {
    fn store(&self) -> Result<Arc<SimplePvStore>, PvError> {
        match &*self.shared.state.lock().unwrap() {
            PvState::Bound(store) => Ok(store.clone()),
            PvState::Pending(_) => Err(PvError::Unbound),
        }
    }

    /// Write a value through the full posting pipeline (timestamp, alarms,
    /// MDEL gating, monitors, links).
    pub async fn set(&self, value: T) -> Result<(), PvError> {
        let store = self.store()?;
        if store
            .set_value(&self.shared.name, value.into_scalar())
            .await
        {
            Ok(())
        } else if store.get_value(&self.shared.name).await.is_some() {
            // Record exists — the write was a no-op (value unchanged). Records
            // CAN now be removed at runtime (`SimplePvStore::remove`), so this
            // is a genuine (benign) TOCTOU: if the record were removed between
            // the failed set and this check we would fall through to the
            // `NotFound` arm, which is the correct outcome.
            Ok(())
        } else {
            Err(PvError::NotFound(self.shared.name.clone()))
        }
    }

    /// Explicitly set the record's alarm severity/status/message, independent
    /// of its value. Alarm transitions always post (no MDEL gating, no link
    /// evaluation). A no-op re-set (unchanged alarm) is `Ok(())`.
    pub async fn set_alarm(
        &self,
        severity: i32,
        status: i32,
        message: &str,
    ) -> Result<(), PvError> {
        let store = self.store()?;
        if store
            .set_alarm(&self.shared.name, severity, status, message)
            .await
        {
            Ok(())
        } else if store.get_value(&self.shared.name).await.is_some() {
            Ok(())
        } else {
            Err(PvError::NotFound(self.shared.name.clone()))
        }
    }

    /// Read the current value, typed.
    pub async fn get(&self) -> Result<T, PvError> {
        let store = self.store()?;
        let v = store
            .get_value(&self.shared.name)
            .await
            .ok_or_else(|| PvError::NotFound(self.shared.name.clone()))?;
        let actual = format!("{v:?}");
        T::from_scalar(v).ok_or(PvError::TypeMismatch {
            expected: T::TYPE_NAME,
            actual,
        })
    }

    /// Mint a bound handle to an existing record (e.g. loaded from a `.db`).
    pub(crate) async fn attach(store: &Arc<SimplePvStore>, name: &str) -> Result<Self, PvError> {
        // `get_value` alone isn't a safe type sniff: for array-backed
        // records it returns `ScalarValue::I32(len)` (see
        // RecordInstance::current_value / types.rs), so `Pv::<i32>::attach`
        // on a waveform/aai/aao record would wrongly "match" i32. Check the
        // record's actual payload kind first and refuse array/table/ndarray
        // payloads outright; only Scalar and Enum records are valid `Pv<T>`
        // targets (enum records attach as i32 by design, see Task 2).
        match store.get_nt(name).await {
            None => return Err(PvError::NotFound(name.to_string())),
            Some(NtPayload::Scalar(_)) | Some(NtPayload::Enum(_)) => {}
            Some(other) => {
                return Err(PvError::TypeMismatch {
                    expected: T::TYPE_NAME,
                    actual: nt_payload_kind(&other).to_string(),
                });
            }
        }
        let v = store
            .get_value(name)
            .await
            .ok_or_else(|| PvError::NotFound(name.to_string()))?;
        let actual = format!("{v:?}");
        if T::from_scalar(v).is_none() {
            return Err(PvError::TypeMismatch {
                expected: T::TYPE_NAME,
                actual,
            });
        }
        Ok(Self {
            shared: Arc::new(PvShared {
                name: name.to_string(),
                state: Mutex::new(PvState::Bound(store.clone())),
            }),
            _marker: PhantomData,
        })
    }
}

/// Short label for an `NtPayload` variant, used in `PvError::TypeMismatch`.
fn nt_payload_kind(p: &NtPayload) -> &'static str {
    match p {
        NtPayload::Scalar(_) => "Scalar",
        NtPayload::ScalarArray(_) => "ScalarArray",
        NtPayload::Table(_) => "Table",
        NtPayload::NdArray(_) => "NdArray",
        NtPayload::Enum(_) => "Enum",
        NtPayload::Generic { .. } => "Generic",
    }
}

/// Handle to an array-backed record (`waveform`/`aai`/`aao`). Unlike `Pv<T>`
/// this is untyped over the element kind — values are `ScalarArrayValue`.
/// Cheap to clone; all clones share state.
pub struct PvArray {
    pub(crate) shared: Arc<PvShared>,
}

impl Clone for PvArray {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
        }
    }
}

impl PvArray {
    fn from_record(record: RecordInstance) -> Self {
        Self {
            shared: Arc::new(PvShared {
                name: record.name.clone(),
                state: Mutex::new(PvState::Pending(PendingDef {
                    record,
                    validator: None,
                    scan: None,
                    calc: None,
                })),
            }),
        }
    }

    /// `waveform` — array record, writable over the wire.
    pub fn waveform(name: impl Into<String>, data: ScalarArrayValue) -> Self {
        let name = name.into();
        Self::from_record(make_array_record(&name, RecordType::Waveform, data))
    }

    /// `aai` — analog array input, read-only over the wire.
    pub fn aai(name: impl Into<String>, data: ScalarArrayValue) -> Self {
        let name = name.into();
        Self::from_record(make_array_record(&name, RecordType::Aai, data))
    }

    /// `aao` — analog array output, writable over the wire.
    pub fn aao(name: impl Into<String>, data: ScalarArrayValue) -> Self {
        let name = name.into();
        Self::from_record(make_array_record(&name, RecordType::Aao, data))
    }

    pub fn name(&self) -> &str {
        &self.shared.name
    }

    /// Clone of the pending record template; `None` once bound. Test-only —
    /// `ServeBuilder` reads the pending record via `AnyPv::take_record`.
    #[cfg(test)]
    fn pending_record(&self) -> Option<RecordInstance> {
        match &*self.shared.state.lock().unwrap() {
            PvState::Pending(def) => Some(def.record.clone()),
            PvState::Bound(_) => None,
        }
    }

    fn store(&self) -> Result<Arc<SimplePvStore>, PvError> {
        match &*self.shared.state.lock().unwrap() {
            PvState::Bound(store) => Ok(store.clone()),
            PvState::Pending(_) => Err(PvError::Unbound),
        }
    }

    /// Write an array value through the full posting pipeline.
    pub async fn set(&self, data: ScalarArrayValue) -> Result<(), PvError> {
        let store = self.store()?;
        if store.set_array_value(&self.shared.name, data).await {
            Ok(())
        } else {
            match store.get_nt(&self.shared.name).await {
                // Record exists and is array-backed — the write was a no-op
                // or a truncated/rejected update; either way this mirrors
                // `Pv::set`'s benign-TOCTOU existence check.
                Some(NtPayload::ScalarArray(_)) => Ok(()),
                Some(other) => Err(PvError::TypeMismatch {
                    expected: "array",
                    actual: nt_payload_kind(&other).to_string(),
                }),
                None => Err(PvError::NotFound(self.shared.name.clone())),
            }
        }
    }

    /// Explicitly set the record's alarm severity/status/message, independent
    /// of its value. Alarm transitions always post (no MDEL gating, no link
    /// evaluation). A no-op re-set (unchanged alarm) is `Ok(())`.
    pub async fn set_alarm(
        &self,
        severity: i32,
        status: i32,
        message: &str,
    ) -> Result<(), PvError> {
        let store = self.store()?;
        if store
            .set_alarm(&self.shared.name, severity, status, message)
            .await
        {
            Ok(())
        } else if store.get_nt(&self.shared.name).await.is_some() {
            Ok(())
        } else {
            Err(PvError::NotFound(self.shared.name.clone()))
        }
    }

    /// Read the current array value.
    pub async fn get(&self) -> Result<ScalarArrayValue, PvError> {
        let store = self.store()?;
        match store.get_nt(&self.shared.name).await {
            Some(NtPayload::ScalarArray(nt)) => Ok(nt.value),
            Some(other) => Err(PvError::TypeMismatch {
                expected: "array",
                actual: nt_payload_kind(&other).to_string(),
            }),
            None => Err(PvError::NotFound(self.shared.name.clone())),
        }
    }

    /// Mint a bound handle to an existing array-backed record.
    pub(crate) async fn attach(store: &Arc<SimplePvStore>, name: &str) -> Result<Self, PvError> {
        match store.get_nt(name).await {
            Some(NtPayload::ScalarArray(_)) => Ok(Self {
                shared: Arc::new(PvShared {
                    name: name.to_string(),
                    state: Mutex::new(PvState::Bound(store.clone())),
                }),
            }),
            Some(other) => Err(PvError::TypeMismatch {
                expected: "array",
                actual: nt_payload_kind(&other).to_string(),
            }),
            None => Err(PvError::NotFound(name.to_string())),
        }
    }
}

impl From<PvArray> for AnyPv {
    fn from(pv: PvArray) -> Self {
        Self { shared: pv.shared }
    }
}

/// Type-erased PV, as accepted by `PvaServer::serve` / `.pvs(...)`.
pub struct AnyPv {
    pub(crate) shared: Arc<PvShared>,
}

impl<T: PvScalar> From<Pv<T>> for AnyPv {
    fn from(pv: Pv<T>) -> Self {
        Self { shared: pv.shared }
    }
}

impl AnyPv {
    /// Clone the pending record template. Returns `None` if already bound.
    pub(crate) fn take_record(&self) -> Option<RecordInstance> {
        let state = self.shared.state.lock().unwrap();
        match &*state {
            PvState::Pending(def) => Some(def.record.clone()),
            PvState::Bound(_) => None,
        }
    }

    /// Flip the handle (and every clone sharing this state) to bound.
    pub(crate) fn bind(&self, store: &Arc<SimplePvStore>) {
        *self.shared.state.lock().unwrap() = PvState::Bound(store.clone());
    }

    pub fn name(&self) -> &str {
        &self.shared.name
    }

    /// Take the pending PUT validator, if any. `None` once bound.
    pub(crate) fn take_validator(&self) -> Option<crate::simple_store::PutValidator> {
        let mut state = self.shared.state.lock().unwrap();
        match &mut *state {
            PvState::Pending(def) => def.validator.take(),
            PvState::Bound(_) => None,
        }
    }

    /// Take the pending scan definition, if any. `None` once bound.
    pub(crate) fn take_scan(
        &self,
    ) -> Option<(std::time::Duration, crate::simple_store::ScanCallback)> {
        let mut state = self.shared.state.lock().unwrap();
        match &mut *state {
            PvState::Pending(def) => def.scan.take(),
            PvState::Bound(_) => None,
        }
    }

    /// Take the pending calc definition, if any. `None` once bound.
    pub(crate) fn take_calc(&self) -> Option<(Vec<String>, crate::simple_store::LinkCallback)> {
        let mut state = self.shared.state.lock().unwrap();
        match &mut *state {
            PvState::Pending(def) => def.calc.take(),
            PvState::Bound(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spvirit_types::NtScalar;

    #[test]
    fn pvscalar_roundtrip_f64() {
        assert_eq!(f64::from_scalar(ScalarValue::F64(1.5)), Some(1.5));
        assert!(matches!(1.5f64.into_scalar(), ScalarValue::F64(x) if x == 1.5));
    }

    #[test]
    fn pvscalar_f64_accepts_f32_widening() {
        assert_eq!(f64::from_scalar(ScalarValue::F32(2.0)), Some(2.0));
    }

    #[test]
    fn pvscalar_rejects_wrong_variant() {
        assert_eq!(f64::from_scalar(ScalarValue::Str("x".into())), None);
        assert_eq!(bool::from_scalar(ScalarValue::F64(1.0)), None);
        assert_eq!(i32::from_scalar(ScalarValue::Str("1".into())), None);
        assert_eq!(String::from_scalar(ScalarValue::Bool(true)), None);
    }

    #[test]
    fn pverror_display() {
        let e = PvError::TypeMismatch {
            expected: "f64",
            actual: "Str".into(),
        };
        assert!(e.to_string().contains("f64"));
        assert!(PvError::Unbound.to_string().contains("not bound"));
    }

    #[test]
    fn ai_constructor_builds_record_template() {
        let pv = Pv::ai("SIM:TEMP", 22.5).units("C").prec(2).desc("Temp");
        let rec = pv.pending_record().expect("still pending");
        assert_eq!(rec.name, "SIM:TEMP");
        let nt = rec.to_ntscalar();
        assert_eq!(nt.value, ScalarValue::F64(22.5));
        assert_eq!(nt.units, "C");
        assert_eq!(nt.display_precision, 2);
        assert_eq!(rec.common.desc, "Temp");
        assert!(!rec.writable(), "ai is read-only over the wire");
        assert_eq!(pv.name(), "SIM:TEMP");
    }

    #[test]
    fn ao_is_writable_with_drive_limits() {
        let pv = Pv::ao("SIM:SP", 25.0).drive_limits(0.0, 100.0);
        let rec = pv.pending_record().unwrap();
        assert!(rec.writable());
        let nt = rec.to_ntscalar();
        assert_eq!(nt.control_low, 0.0);
        assert_eq!(nt.control_high, 100.0);
    }

    #[test]
    fn mdel_adel_go_to_raw_fields() {
        let pv = Pv::ai("SIM:X", 0.0).mdel(0.5).adel(1.0);
        let rec = pv.pending_record().unwrap();
        assert_eq!(rec.raw_fields.get("MDEL").map(String::as_str), Some("0.5"));
        assert_eq!(rec.raw_fields.get("ADEL").map(String::as_str), Some("1"));
    }

    #[test]
    fn alarm_limits_set_value_alarm_block() {
        let pv = Pv::ao("SIM:A", 0.0).alarm_limits(-10.0, -5.0, 5.0, 10.0);
        let nt = pv.pending_record().unwrap().to_ntscalar();
        assert_eq!(nt.value_alarm_low_alarm_limit, -10.0);
        assert_eq!(nt.value_alarm_low_warning_limit, -5.0);
        assert_eq!(nt.value_alarm_high_warning_limit, 5.0);
        assert_eq!(nt.value_alarm_high_alarm_limit, 10.0);
        assert!(nt.value_alarm_active);
    }

    #[test]
    fn bool_and_string_constructors() {
        assert!(Pv::bo("B", true).pending_record().unwrap().writable());
        assert!(!Pv::bi("B2", false).pending_record().unwrap().writable());
        let s = Pv::string_in("S", "hello").pending_record().unwrap();
        assert_eq!(s.to_ntscalar().value, ScalarValue::Str("hello".into()));
    }

    #[test]
    fn longin_longout_constructors() {
        let li = Pv::longin("L:IN", 42);
        let rec = li.pending_record().unwrap();
        assert_eq!(rec.record_type, crate::types::RecordType::LongIn);
        assert_eq!(rec.to_ntscalar().value, ScalarValue::I32(42));
        assert!(!rec.writable());

        let lo = Pv::longout("L:OUT", 7).drive_limits(0.0, 1000.0);
        let rec = lo.pending_record().unwrap();
        assert_eq!(rec.record_type, crate::types::RecordType::LongOut);
        assert!(rec.writable());
        assert_eq!(rec.to_ntscalar().control_high, 1000.0);
    }

    #[tokio::test]
    async fn longout_set_get_roundtrip() {
        let store = empty_store();
        let pv = Pv::longout("L:RT", 1);
        let any: AnyPv = pv.clone().into();
        let rec = any.take_record().unwrap();
        store.insert(rec.name.clone(), rec).await;
        any.bind(&store);
        pv.set(99).await.unwrap();
        assert_eq!(pv.get().await, Ok(99));
    }

    #[test]
    fn mbbi_mbbo_constructors() {
        let m = Pv::mbbi("M:I", vec!["Off".into(), "On".into(), "Auto".into()], 1);
        let rec = m.pending_record().unwrap();
        assert_eq!(rec.record_type, crate::types::RecordType::Mbbi);
        assert_eq!(rec.current_value(), ScalarValue::I32(1));

        let o = Pv::mbbo("M:O", vec!["A".into(), "B".into()], 0);
        assert!(o.pending_record().unwrap().writable());
    }

    #[tokio::test]
    async fn mbbo_set_get_index_with_bounds() {
        let store = empty_store();
        let pv = Pv::mbbo("M:RT", vec!["Stop".into(), "Run".into(), "Fault".into()], 0);
        let any: AnyPv = pv.clone().into();
        let rec = any.take_record().unwrap();
        store.insert(rec.name.clone(), rec).await;
        any.bind(&store);

        pv.set(2).await.unwrap();
        assert_eq!(pv.get().await, Ok(2));
        // out-of-range index is rejected (value unchanged); set() maps the
        // store's `false` to Ok-if-exists, so verify via get()
        let _ = pv.set(7).await;
        assert_eq!(pv.get().await, Ok(2));
    }

    #[test]
    fn set_scalar_value_on_raw_nt_enum_record() {
        let mut rec = Pv::mbbo("M:RAW", vec!["Off".into(), "On".into(), "Auto".into()], 0)
            .pending_record()
            .unwrap();
        // In-range change succeeds.
        assert!(rec.set_scalar_value(ScalarValue::I32(2), true));
        assert_eq!(rec.current_value(), ScalarValue::I32(2));
        // Same-index is a no-op.
        assert!(!rec.set_scalar_value(ScalarValue::I32(2), true));
        // Out-of-range index is rejected; value unchanged.
        assert!(!rec.set_scalar_value(ScalarValue::I32(7), true));
        assert_eq!(rec.current_value(), ScalarValue::I32(2));
        assert!(!rec.set_scalar_value(ScalarValue::I32(-1), true));
        assert_eq!(rec.current_value(), ScalarValue::I32(2));
    }

    #[test]
    fn set_scalar_value_stamps_timestamp() {
        let mut rec = Pv::ao("A:TS", 1.0).pending_record().unwrap();
        assert!(rec.set_scalar_value(ScalarValue::F64(2.0), false));
        match rec.to_ntpayload() {
            NtPayload::Scalar(nt) => {
                let ts = nt.time_stamp.expect("scalar update must store a timestamp");
                assert!(ts.seconds_past_epoch > 0);
            }
            other => panic!("expected scalar payload, got {other:?}"),
        }
        // Unchanged value: no post, timestamp keeps the last update time.
        assert!(!rec.set_scalar_value(ScalarValue::F64(2.0), false));
    }

    #[test]
    fn set_scalar_value_stamps_enum_timestamp() {
        let mut rec = Pv::mbbo("M:TS", vec!["Off".into(), "On".into()], 0)
            .pending_record()
            .unwrap();
        assert!(rec.set_scalar_value(ScalarValue::I32(1), false));
        match rec.to_ntpayload() {
            NtPayload::Enum(nt) => {
                assert!(nt.time_stamp.seconds_past_epoch > 0);
            }
            other => panic!("expected enum payload, got {other:?}"),
        }
    }

    #[test]
    fn set_nt_payload_stamps_missing_timestamp_but_keeps_explicit_one() {
        let mut rec = Pv::ao("A:NT", 1.0).pending_record().unwrap();

        // No caller timestamp: server stamps the update time.
        let nt = NtScalar::from_value(ScalarValue::F64(2.0));
        assert!(rec.set_nt_payload(NtPayload::Scalar(nt)));
        match rec.to_ntpayload() {
            NtPayload::Scalar(nt) => {
                let ts = nt.time_stamp.expect("put_nt must store a timestamp");
                assert!(ts.seconds_past_epoch > 0);
            }
            other => panic!("expected scalar payload, got {other:?}"),
        }

        // Explicit caller timestamp is preserved verbatim.
        let nt = NtScalar::from_value(ScalarValue::F64(3.0)).with_timestamp(1_700_000_000, 42);
        assert!(rec.set_nt_payload(NtPayload::Scalar(nt)));
        match rec.to_ntpayload() {
            NtPayload::Scalar(nt) => {
                let ts = nt.time_stamp.unwrap();
                assert_eq!(ts.seconds_past_epoch, 1_700_000_000);
                assert_eq!(ts.nanoseconds, 42);
            }
            other => panic!("expected scalar payload, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pv_array_roundtrip_and_serve() {
        let wf = PvArray::waveform("W:1", ScalarArrayValue::F64(vec![1.0, 2.0, 3.0]));
        let server = crate::pva_server::PvaServer::serve([AnyPv::from(wf.clone())])
            .build()
            .await;
        assert_eq!(
            wf.get().await,
            Ok(ScalarArrayValue::F64(vec![1.0, 2.0, 3.0]))
        );
        wf.set(ScalarArrayValue::F64(vec![4.0, 5.0])).await.unwrap();
        match wf.get().await.unwrap() {
            ScalarArrayValue::F64(v) => assert_eq!(v, vec![4.0, 5.0]),
            other => panic!("wrong kind: {other:?}"),
        }
        // typed attach via the server
        let h = server.array_pv("W:1").await.unwrap();
        assert!(matches!(h.get().await.unwrap(), ScalarArrayValue::F64(_)));
        // scalar attach to an array record must type-mismatch
        assert!(matches!(
            server.pv::<f64>("W:1").await,
            Err(PvError::TypeMismatch { .. })
        ));
        // array attach to a scalar record must type-mismatch
        let t = Pv::ai("W:S", 1.0);
        let s2 = crate::pva_server::PvaServer::serve([AnyPv::from(t)])
            .build()
            .await;
        assert!(matches!(
            s2.array_pv("W:S").await,
            Err(PvError::TypeMismatch { .. })
        ));
    }

    #[test]
    fn aai_read_only_aao_writable() {
        let a = PvArray::aai("W:AI", ScalarArrayValue::I32(vec![1]));
        assert!(a.pending_record().is_some());
        assert!(!AnyPv::from(a).take_record().unwrap().writable());
        let b = PvArray::aao("W:AO", ScalarArrayValue::I32(vec![1]));
        assert!(AnyPv::from(b).take_record().unwrap().writable());
    }

    #[tokio::test]
    async fn scalar_attach_rejects_array_backed_record() {
        // Regression: Pv::attach used to sniff via get_value, which returns
        // I32(len) for array records — so Pv::<i32>::attach would WRONGLY
        // succeed on a waveform/aai/aao record. The payload-kind guard must
        // reject it as TypeMismatch instead.
        let store = empty_store();
        let wf = PvArray::waveform("W:GUARD", ScalarArrayValue::I32(vec![1, 2, 3]));
        let any: AnyPv = wf.into();
        let rec = any.take_record().unwrap();
        store.insert(rec.name.clone(), rec).await;

        let bad = Pv::<i32>::attach(&store, "W:GUARD").await;
        assert!(matches!(bad, Err(PvError::TypeMismatch { .. })));
    }

    fn empty_store() -> Arc<SimplePvStore> {
        Arc::new(SimplePvStore::new(
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
            Vec::new(),
            false,
        ))
    }

    #[tokio::test]
    async fn set_get_before_bind_errors() {
        let pv = Pv::ai("SIM:X", 1.0);
        assert_eq!(pv.set(2.0).await, Err(PvError::Unbound));
        assert_eq!(pv.get().await, Err(PvError::Unbound));
    }

    #[tokio::test]
    async fn bind_then_set_get_roundtrip() {
        let store = empty_store();
        let pv = Pv::ai("SIM:X", 1.0).units("mm");
        let any: AnyPv = pv.clone().into();
        let rec = any.take_record().expect("pending record");
        store.insert(rec.name.clone(), rec).await;
        any.bind(&store);

        assert_eq!(pv.get().await, Ok(1.0));
        pv.set(2.5).await.unwrap();
        assert_eq!(pv.get().await, Ok(2.5));
        // clone sees the same record
        assert_eq!(pv.clone().get().await, Ok(2.5));
    }

    #[tokio::test]
    async fn attach_mints_typed_handle_and_checks_type() {
        let store = empty_store();
        let src = Pv::ai("SIM:Y", 3.0);
        let any: AnyPv = src.into();
        let rec = any.take_record().unwrap();
        store.insert(rec.name.clone(), rec).await;

        let h: Pv<f64> = Pv::attach(&store, "SIM:Y").await.unwrap();
        assert_eq!(h.get().await, Ok(3.0));

        let bad = Pv::<bool>::attach(&store, "SIM:Y").await;
        assert!(matches!(bad, Err(PvError::TypeMismatch { .. })));
        let missing = Pv::<f64>::attach(&store, "NOPE").await;
        assert!(matches!(missing, Err(PvError::NotFound(ref n)) if n == "NOPE"));
    }

    #[tokio::test]
    async fn set_same_value_is_ok_not_not_found() {
        let store = empty_store();
        let pv = Pv::ai("SIM:Z", 1.0);
        let any: AnyPv = pv.clone().into();
        let rec = any.take_record().unwrap();
        store.insert(rec.name.clone(), rec).await;
        any.bind(&store);

        assert_eq!(pv.set(2.5).await, Ok(()));
        // Second write of the same value is a no-op, not NotFound.
        assert_eq!(pv.set(2.5).await, Ok(()));
        assert_eq!(pv.get().await, Ok(2.5));
    }

    #[tokio::test]
    async fn set_alarm_posts_and_reads_back() {
        let store = empty_store();
        let pv = Pv::ai("A:1", 1.0);
        let any: AnyPv = pv.clone().into();
        let rec = any.take_record().unwrap();
        store.insert(rec.name.clone(), rec).await;
        any.bind(&store);

        // subscribe like a monitor client
        let mut rx = crate::pvstore::Source::subscribe(&*store, "A:1")
            .await
            .unwrap();

        pv.set_alarm(2, 3, "sensor dead").await.unwrap();
        let rec = store.get_record("A:1").await.unwrap();
        let nt = rec.to_ntscalar();
        assert_eq!(nt.alarm_severity, 2);
        assert_eq!(nt.alarm_status, 3);
        assert_eq!(nt.alarm_message, "sensor dead");
        // a payload was posted to the subscriber
        let posted = rx.try_recv().expect("alarm change must post");
        drop(posted);
        // idempotent re-set posts nothing
        pv.set_alarm(2, 3, "sensor dead").await.unwrap();
        assert!(rx.try_recv().is_err());
        // unbound / missing paths
        let ghost = Pv::ai("A:GHOST", 0.0);
        assert_eq!(ghost.set_alarm(1, 0, "x").await, Err(PvError::Unbound));

        let missing: Pv<f64> = Pv::attach(&store, "A:1").await.unwrap();
        // sanity: attach roundtrip still works after alarm writes
        assert_eq!(missing.get().await, Ok(1.0));
    }

    #[tokio::test]
    async fn set_alarm_missing_record_is_not_found() {
        let store = empty_store();
        let pv = Pv::ai("A:MISSING", 0.0);
        let any: AnyPv = pv.clone().into();
        any.bind(&store);
        assert_eq!(
            pv.set_alarm(1, 1, "x").await,
            Err(PvError::NotFound("A:MISSING".into()))
        );
    }

    #[tokio::test]
    async fn set_alarm_on_enum_and_array_records() {
        let store = empty_store();

        let mbbo = Pv::mbbo("M:ALM", vec!["Stop".into(), "Run".into()], 0);
        let any: AnyPv = mbbo.clone().into();
        let rec = any.take_record().unwrap();
        store.insert(rec.name.clone(), rec).await;
        any.bind(&store);

        let wf = PvArray::waveform("W:ALM", ScalarArrayValue::F64(vec![1.0, 2.0]));
        let any: AnyPv = wf.clone().into();
        let rec = any.take_record().unwrap();
        store.insert(rec.name.clone(), rec).await;
        any.bind(&store);

        assert!(store.set_alarm("M:ALM", 2, 5, "enum fault").await);
        assert!(store.set_alarm("W:ALM", 1, 4, "array fault").await);

        match store.get_nt("M:ALM").await.unwrap() {
            NtPayload::Enum(nt) => {
                assert_eq!(nt.alarm.severity, 2);
                assert_eq!(nt.alarm.status, 5);
                assert_eq!(nt.alarm.message, "enum fault");
            }
            other => panic!("expected Enum, got {other:?}"),
        }
        match store.get_nt("W:ALM").await.unwrap() {
            NtPayload::ScalarArray(nt) => {
                assert_eq!(nt.alarm.severity, 1);
                assert_eq!(nt.alarm.status, 4);
                assert_eq!(nt.alarm.message, "array fault");
            }
            other => panic!("expected ScalarArray, got {other:?}"),
        }

        // idempotent re-set posts nothing / returns false
        assert!(!store.set_alarm("M:ALM", 2, 5, "enum fault").await);
    }

    #[tokio::test]
    async fn on_put_callback_travels_to_store() {
        let rejected = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let r2 = rejected.clone();
        let pv = Pv::ao("SIM:SP", 1.0).on_put(move |_pv, v: f64| {
            if v > 100.0 {
                r2.store(true, std::sync::atomic::Ordering::SeqCst);
                Err("over limit".into())
            } else {
                Ok(())
            }
        });
        let any: AnyPv = pv.clone().into();
        assert!(any.take_validator().is_some());
    }

    #[tokio::test]
    async fn on_put_wrapper_unwraps_structure_wrapped_scalar_put() {
        // Real puts to scalar records arrive as a Structure with a "value"
        // field (see apply_put_to_record's bare-scalar wrapping in
        // simple_store.rs). The typed on_put wrapper must unwrap that the
        // same way before calling convert::decoded_to_scalar_value, or a
        // wrapped put would fail the typed conversion and spuriously reject.
        let store = empty_store();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let seen2 = seen.clone();
        let pv = Pv::ao("SIM:WRAP", 1.0).on_put(move |_pv, v: f64| {
            *seen2.lock().unwrap() = Some(v);
            Ok(())
        });
        let any: AnyPv = pv.clone().into();
        let rec = any.take_record().unwrap();
        store.insert(rec.name.clone(), rec).await;
        let validator = any.take_validator().expect("validator attached");
        any.bind(&store);

        let dv = spvirit_codec::spvd_decode::DecodedValue::Structure(vec![(
            "value".to_string(),
            spvirit_codec::spvd_decode::DecodedValue::Float64(42.0),
        )]);
        let res = validator("SIM:WRAP", &dv);
        assert_eq!(res, Ok(()));
        assert_eq!(*seen.lock().unwrap(), Some(42.0));
    }

    #[test]
    fn scalar_value_handle_constructors() {
        let p = Pv::<ScalarValue>::scalar_out("S:U16", ScalarValue::U16(7));
        let rec = p.pending_record().unwrap();
        assert_eq!(rec.record_type, crate::types::RecordType::LongOut);
        assert_eq!(rec.current_value(), ScalarValue::U16(7));
        assert!(rec.writable());

        let q = Pv::<ScalarValue>::scalar_in("S:F32", ScalarValue::F32(1.5));
        let rec = q.pending_record().unwrap();
        assert_eq!(rec.record_type, crate::types::RecordType::Ai);
        assert!(!rec.writable());

        let s = Pv::<ScalarValue>::scalar_in("S:STR", ScalarValue::Str("x".into()));
        assert_eq!(
            s.pending_record().unwrap().record_type,
            crate::types::RecordType::StringIn
        );

        let b = Pv::<ScalarValue>::scalar_out("S:B", ScalarValue::Bool(true));
        assert_eq!(
            b.pending_record().unwrap().record_type,
            crate::types::RecordType::Bo
        );
    }

    #[tokio::test]
    async fn scalar_value_handle_set_get_preserves_variant() {
        let store = empty_store();
        let pv = Pv::<ScalarValue>::scalar_out("S:U64", ScalarValue::U64(1));
        let any: AnyPv = pv.clone().into();
        let rec = any.take_record().unwrap();
        store.insert(rec.name.clone(), rec).await;
        any.bind(&store);

        pv.set(ScalarValue::U64(u64::MAX)).await.unwrap();
        assert_eq!(pv.get().await, Ok(ScalarValue::U64(u64::MAX)));
    }

    #[test]
    fn set_scalar_value_same_variant_u64_is_exact() {
        let mut rec = make_output_record("S:U64", RecordType::LongOut, ScalarValue::U64(1));
        let changed = rec.set_scalar_value(ScalarValue::U64(u64::MAX), true);
        assert!(changed);
        assert_eq!(rec.current_value(), ScalarValue::U64(u64::MAX));
    }

    #[test]
    fn set_scalar_value_same_variant_f32_is_exact() {
        let mut rec = make_output_record("S:F32", RecordType::LongOut, ScalarValue::F32(1.5));
        let changed = rec.set_scalar_value(ScalarValue::F32(2.5), true);
        assert!(changed);
        assert_eq!(rec.current_value(), ScalarValue::F32(2.5));
    }

    #[test]
    fn set_scalar_value_same_variant_i64_is_exact() {
        let mut rec = make_output_record("S:I64", RecordType::LongOut, ScalarValue::I64(1));
        let changed = rec.set_scalar_value(ScalarValue::I64(i64::MIN), true);
        assert!(changed);
        assert_eq!(rec.current_value(), ScalarValue::I64(i64::MIN));
    }

    #[test]
    fn set_scalar_value_cross_variant_preserves_target_variant_from_i32() {
        let mut rec = make_output_record("S:U16", RecordType::LongOut, ScalarValue::U16(5));
        let changed = rec.set_scalar_value(ScalarValue::I32(42), true);
        assert!(changed);
        assert_eq!(rec.current_value(), ScalarValue::U16(42));
    }

    #[test]
    fn set_scalar_value_cross_variant_preserves_target_variant_from_f64() {
        let mut rec = make_output_record("S:U16", RecordType::LongOut, ScalarValue::U16(5));
        let changed = rec.set_scalar_value(ScalarValue::F64(7.0), true);
        assert!(changed);
        assert_eq!(rec.current_value(), ScalarValue::U16(7));
    }

    #[test]
    fn set_scalar_value_unchanged_u64_returns_false() {
        let mut rec = make_output_record("S:U64B", RecordType::LongOut, ScalarValue::U64(5));
        let changed = rec.set_scalar_value(ScalarValue::U64(5), true);
        assert!(!changed);
        assert_eq!(rec.current_value(), ScalarValue::U64(5));
    }

    #[test]
    fn scalar_value_from_decoded_maps_one_to_one() {
        assert_eq!(
            ScalarValue::from_decoded(&DecodedValue::UInt32(7)),
            Some(ScalarValue::U32(7))
        );
        assert_eq!(
            ScalarValue::from_decoded(&DecodedValue::Int8(-3)),
            Some(ScalarValue::I8(-3))
        );
        assert_eq!(
            ScalarValue::from_decoded(&DecodedValue::Boolean(true)),
            Some(ScalarValue::Bool(true))
        );
        assert_eq!(
            ScalarValue::from_decoded(&DecodedValue::String("hi".into())),
            Some(ScalarValue::Str("hi".into()))
        );
        assert!(ScalarValue::from_decoded(&DecodedValue::Null).is_none());
    }
}
