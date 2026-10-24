//! Server-specific record and IOC types.
//!
//! Shared Normative Type definitions (ScalarValue, NtScalar, NtPayload, etc.)
//! live in the `spvirit-types` crate and are re-exported here for convenience.

use std::collections::HashMap;
use std::time::Duration;

pub use spvirit_types::*;

/// Current wall-clock time as an [`NtTimeStamp`] (UNIX epoch).
pub(crate) fn now_nt_timestamp() -> NtTimeStamp {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    NtTimeStamp {
        seconds_past_epoch: now.as_secs() as i64,
        nanoseconds: now.subsec_nanos() as i32,
        user_tag: 0,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordType {
    Ai,
    Ao,
    Bi,
    Bo,
    StringIn,
    StringOut,
    Waveform,
    Aai,
    Aao,
    SubArray,
    NtTable,
    NtNdArray,
    Mbbi,
    Mbbo,
    Generic,
    LongIn,
    LongOut,
}

impl RecordType {
    // TODO(follow-up): .db parsing — longin/longout are not yet recognized
    // here; wiring `.db`-loaded longin/longout records needs db.rs
    // construction work (out of scope for the handle-API-only gap task).
    pub fn from_db_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "ai" => Some(Self::Ai),
            "ao" => Some(Self::Ao),
            "bi" => Some(Self::Bi),
            "bo" => Some(Self::Bo),
            "stringin" => Some(Self::StringIn),
            "stringout" => Some(Self::StringOut),
            "waveform" => Some(Self::Waveform),
            "aai" => Some(Self::Aai),
            "aao" => Some(Self::Aao),
            "subarray" => Some(Self::SubArray),
            "mbbi" | "ntenum" => Some(Self::Mbbi),
            "mbbo" => Some(Self::Mbbo),
            _ => None,
        }
    }

    pub fn is_output(&self) -> bool {
        matches!(
            self,
            Self::Ao | Self::Bo | Self::StringOut | Self::Aao | Self::Mbbo | Self::LongOut
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanMode {
    Passive,
    Periodic(Duration),
    Event(String),
    IoEvent(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum LinkExpr {
    Constant(ScalarValue),
    DbLink {
        target: String,
        process_passive: bool,
        maximize_severity: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputMode {
    Supervisory,
    ClosedLoop,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DbCommonState {
    pub desc: String,
    pub scan: ScanMode,
    pub pini: bool,
    pub phas: i32,
    pub pact: bool,
    pub disa: bool,
    pub sdis: Option<LinkExpr>,
    pub diss: i32,
    pub flnk: Option<LinkExpr>,
}

impl Default for DbCommonState {
    fn default() -> Self {
        Self {
            desc: String::new(),
            scan: ScanMode::Passive,
            pini: false,
            phas: 0,
            pact: false,
            disa: false,
            sdis: None,
            diss: 0,
            flnk: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RecordData {
    Ai {
        nt: NtScalar,
        inp: Option<LinkExpr>,
        siml: Option<LinkExpr>,
        siol: Option<LinkExpr>,
        simm: bool,
    },
    Ao {
        nt: NtScalar,
        out: Option<LinkExpr>,
        dol: Option<LinkExpr>,
        omsl: OutputMode,
        drvl: Option<f64>,
        drvh: Option<f64>,
        oroc: Option<f64>,
        siml: Option<LinkExpr>,
        siol: Option<LinkExpr>,
        simm: bool,
    },
    Bi {
        nt: NtScalar,
        inp: Option<LinkExpr>,
        znam: String,
        onam: String,
        siml: Option<LinkExpr>,
        siol: Option<LinkExpr>,
        simm: bool,
    },
    Bo {
        nt: NtScalar,
        out: Option<LinkExpr>,
        dol: Option<LinkExpr>,
        omsl: OutputMode,
        znam: String,
        onam: String,
        siml: Option<LinkExpr>,
        siol: Option<LinkExpr>,
        simm: bool,
    },
    StringIn {
        nt: NtScalar,
        inp: Option<LinkExpr>,
        siml: Option<LinkExpr>,
        siol: Option<LinkExpr>,
        simm: bool,
    },
    StringOut {
        nt: NtScalar,
        out: Option<LinkExpr>,
        dol: Option<LinkExpr>,
        omsl: OutputMode,
        siml: Option<LinkExpr>,
        siol: Option<LinkExpr>,
        simm: bool,
    },
    Waveform {
        nt: NtScalarArray,
        inp: Option<LinkExpr>,
        ftvl: String,
        nelm: usize,
        nord: usize,
    },
    Aai {
        nt: NtScalarArray,
        inp: Option<LinkExpr>,
        ftvl: String,
        nelm: usize,
        nord: usize,
    },
    Aao {
        nt: NtScalarArray,
        out: Option<LinkExpr>,
        dol: Option<LinkExpr>,
        omsl: OutputMode,
        ftvl: String,
        nelm: usize,
        nord: usize,
    },
    SubArray {
        nt: NtScalarArray,
        inp: Option<LinkExpr>,
        ftvl: String,
        malm: usize,
        nelm: usize,
        nord: usize,
        indx: usize,
    },
    NtTable {
        nt: NtTable,
        inp: Option<LinkExpr>,
        out: Option<LinkExpr>,
        omsl: OutputMode,
    },
    NtNdArray {
        nt: NtNdArray,
        inp: Option<LinkExpr>,
        out: Option<LinkExpr>,
        omsl: OutputMode,
    },
    NtEnum {
        nt: NtEnum,
        inp: Option<LinkExpr>,
        out: Option<LinkExpr>,
        omsl: OutputMode,
    },
    Generic {
        struct_id: String,
        fields: Vec<(String, PvValue)>,
        inp: Option<LinkExpr>,
        out: Option<LinkExpr>,
        omsl: OutputMode,
    },
}

/// A borrowed view of a record's payload.
///
/// Mirrors [`NtPayload`] variant-for-variant but holds references, so reading
/// one field of a record costs nothing. Use this for anything that inspects a
/// record's shape or one of its fields; use [`RecordData::payload`] only when
/// the value really has to outlive the store's read guard.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NtPayloadRef<'a> {
    Scalar(&'a NtScalar),
    ScalarArray(&'a NtScalarArray),
    Table(&'a NtTable),
    NdArray(&'a NtNdArray),
    Enum(&'a NtEnum),
    Generic {
        struct_id: &'a str,
        fields: &'a [(String, PvValue)],
    },
}

impl<'a> NtPayloadRef<'a> {
    /// Materialise an owned [`NtPayload`].
    ///
    /// This is the ONLY place the store clones a payload. Keeping the single
    /// clone site here is what stops the owned and borrowed views drifting:
    /// both this and [`RecordData::payload_ref`] are exhaustive matches over
    /// the same enums, so a new variant cannot be added to one alone.
    pub fn to_owned_payload(&self) -> NtPayload {
        match self {
            Self::Scalar(nt) => NtPayload::Scalar((*nt).clone()),
            Self::ScalarArray(nt) => NtPayload::ScalarArray((*nt).clone()),
            Self::Table(nt) => NtPayload::Table((*nt).clone()),
            Self::NdArray(nt) => NtPayload::NdArray((*nt).clone()),
            Self::Enum(nt) => NtPayload::Enum((*nt).clone()),
            Self::Generic { struct_id, fields } => NtPayload::Generic {
                struct_id: (*struct_id).to_string(),
                fields: (*fields).to_vec(),
            },
        }
    }
}

impl RecordData {
    pub fn nt_mut(&mut self) -> &mut NtScalar {
        match self {
            Self::Ai { nt, .. } => nt,
            Self::Ao { nt, .. } => nt,
            Self::Bi { nt, .. } => nt,
            Self::Bo { nt, .. } => nt,
            Self::StringIn { nt, .. } => nt,
            Self::StringOut { nt, .. } => nt,
            _ => panic!("record variant does not expose NtScalar"),
        }
    }

    /// The record's `NtScalar`, for the six variants that carry one.
    ///
    /// Unlike the old `nt()` this returns `None` rather than panicking on the
    /// eight variants that do not have one.
    pub fn nt_scalar(&self) -> Option<&NtScalar> {
        match self {
            Self::Ai { nt, .. }
            | Self::Ao { nt, .. }
            | Self::Bi { nt, .. }
            | Self::Bo { nt, .. }
            | Self::StringIn { nt, .. }
            | Self::StringOut { nt, .. } => Some(nt),
            _ => None,
        }
    }

    /// Borrowing counterpart of [`payload`](Self::payload): the same variant
    /// mapping with no clones.
    pub fn payload_ref(&self) -> NtPayloadRef<'_> {
        match self {
            Self::Ai { nt, .. }
            | Self::Ao { nt, .. }
            | Self::Bi { nt, .. }
            | Self::Bo { nt, .. }
            | Self::StringIn { nt, .. }
            | Self::StringOut { nt, .. } => NtPayloadRef::Scalar(nt),
            Self::Waveform { nt, .. }
            | Self::Aai { nt, .. }
            | Self::Aao { nt, .. }
            | Self::SubArray { nt, .. } => NtPayloadRef::ScalarArray(nt),
            Self::NtTable { nt, .. } => NtPayloadRef::Table(nt),
            Self::NtNdArray { nt, .. } => NtPayloadRef::NdArray(nt),
            Self::NtEnum { nt, .. } => NtPayloadRef::Enum(nt),
            Self::Generic {
                struct_id, fields, ..
            } => NtPayloadRef::Generic {
                struct_id,
                fields,
            },
        }
    }

    pub fn payload(&self) -> NtPayload {
        self.payload_ref().to_owned_payload()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecordInstance {
    pub name: String,
    pub record_type: RecordType,
    pub common: DbCommonState,
    pub data: RecordData,
    pub raw_fields: HashMap<String, String>,
}

impl RecordInstance {
    pub fn writable(&self) -> bool {
        if self.record_type.is_output() {
            return true;
        }
        match &self.data {
            RecordData::Ai { simm: true, .. } => true,
            RecordData::Waveform { .. } => true,
            RecordData::NtTable { .. } => true,
            RecordData::NtNdArray { .. } => true,
            // An enum is writable only as an `mbbo` (an output, caught by
            // `is_output` above). `mbbi` is an input record and deliberately
            // read-only over the wire in spvirit (EPICS Base allows VAL writes
            // to input records); host-side writes do not consult this.
            RecordData::Generic { .. } => true,
            _ => false,
        }
    }

    pub fn to_ntpayload(&self) -> NtPayload {
        self.data.payload()
    }

    /// Borrowing counterpart of [`to_ntpayload`](Self::to_ntpayload).
    pub fn payload_ref(&self) -> NtPayloadRef<'_> {
        self.data.payload_ref()
    }

    /// The record's alarm severity without materialising an `NtScalar`.
    ///
    /// Non-scalar shapes have no severity of their own; `to_ntscalar` reports
    /// 0 for them and so does this.
    pub fn alarm_severity(&self) -> i32 {
        match self.payload_ref() {
            NtPayloadRef::Scalar(nt) => nt.alarm_severity,
            _ => 0,
        }
    }

    pub fn to_ntscalar(&self) -> NtScalar {
        match self.payload_ref() {
            NtPayloadRef::Scalar(nt) => nt.clone(),
            NtPayloadRef::ScalarArray(nt) => {
                let mut scalar = NtScalar::from_value(ScalarValue::I32(nt.value.len() as i32));
                scalar.display_description = "Array length".to_string();
                scalar
            }
            NtPayloadRef::Table(nt) => {
                let mut scalar = NtScalar::from_value(ScalarValue::I32(nt.columns.len() as i32));
                scalar.display_description = "Table columns".to_string();
                scalar
            }
            NtPayloadRef::NdArray(nt) => {
                let mut scalar = NtScalar::from_value(ScalarValue::I32(nt.dimension.len() as i32));
                scalar.display_description = "NDArray dimensions".to_string();
                scalar
            }
            NtPayloadRef::Enum(nt) => {
                let mut scalar = NtScalar::from_value(ScalarValue::I32(nt.index));
                scalar.display_description = nt.selected().unwrap_or("").to_string();
                scalar
            }
            NtPayloadRef::Generic { fields, .. } => {
                let mut scalar = NtScalar::from_value(ScalarValue::I32(fields.len() as i32));
                scalar.display_description = "Generic structure".to_string();
                scalar
            }
        }
    }
    //
    pub fn nt_mut(&mut self) -> &mut NtScalar {
        self.data.nt_mut()
    }

    /// Fallible mutable access to the record's `NtScalar`, for record variants
    /// that carry one (Ai, Ao, Bi, Bo, StringIn, StringOut). `None` otherwise.
    pub(crate) fn nt_scalar_mut(&mut self) -> Option<&mut NtScalar> {
        match &mut self.data {
            RecordData::Ai { nt, .. }
            | RecordData::Ao { nt, .. }
            | RecordData::Bi { nt, .. }
            | RecordData::Bo { nt, .. }
            | RecordData::StringIn { nt, .. }
            | RecordData::StringOut { nt, .. } => Some(nt),
            _ => None,
        }
    }

    pub fn current_value(&self) -> ScalarValue {
        match self.payload_ref() {
            NtPayloadRef::Scalar(nt) => nt.value.clone(),
            NtPayloadRef::ScalarArray(nt) => ScalarValue::I32(nt.value.len() as i32),
            NtPayloadRef::Table(nt) => ScalarValue::I32(nt.columns.len() as i32),
            NtPayloadRef::NdArray(nt) => ScalarValue::I32(nt.dimension.len() as i32),
            NtPayloadRef::Enum(nt) => ScalarValue::I32(nt.index),
            NtPayloadRef::Generic { fields, .. } => ScalarValue::I32(fields.len() as i32),
        }
    }

    /// Fill in any missing (`None` or epoch-0) timestamps with the current
    /// time. Called when a record enters a store so that PVs that are never
    /// updated afterwards (e.g. a static NTTable) still serve a valid
    /// timestamp — clients like the EPICS Archiver Appliance reject epoch-0
    /// events.
    pub(crate) fn stamp_missing_timestamps(&mut self) {
        let ts = now_nt_timestamp();
        match &mut self.data {
            RecordData::Ai { nt, .. }
            | RecordData::Ao { nt, .. }
            | RecordData::Bi { nt, .. }
            | RecordData::Bo { nt, .. }
            | RecordData::StringIn { nt, .. }
            | RecordData::StringOut { nt, .. } => {
                if nt.time_stamp.is_none() {
                    nt.time_stamp = Some(ts);
                }
            }
            RecordData::Waveform { nt, .. }
            | RecordData::Aai { nt, .. }
            | RecordData::Aao { nt, .. }
            | RecordData::SubArray { nt, .. } => {
                if nt.time_stamp == NtTimeStamp::default() {
                    nt.time_stamp = ts;
                }
            }
            RecordData::NtEnum { nt, .. } => {
                if nt.time_stamp == NtTimeStamp::default() {
                    nt.time_stamp = ts;
                }
            }
            RecordData::NtTable { nt, .. } => {
                if nt.time_stamp.is_none() {
                    nt.time_stamp = Some(ts);
                }
            }
            RecordData::NtNdArray { nt, .. } => {
                if nt.time_stamp.is_none() {
                    nt.time_stamp = Some(ts.clone());
                }
                if nt.data_time_stamp == NtTimeStamp::default() {
                    nt.data_time_stamp = ts;
                }
            }
            RecordData::Generic { .. } => {}
        }
    }

    /// Overwrite the record's timestamp, whatever it currently holds.
    ///
    /// Mirrors [`RecordInstance::stamp_missing_timestamps`]'s arm structure
    /// but assigns unconditionally. Used by the PUT path, which restamps on
    /// every accepted write — EPICS Base advances TIME on every record
    /// process, value change or not.
    pub fn set_time_stamp(&mut self, ts: NtTimeStamp) {
        match &mut self.data {
            RecordData::Ai { nt, .. }
            | RecordData::Ao { nt, .. }
            | RecordData::Bi { nt, .. }
            | RecordData::Bo { nt, .. }
            | RecordData::StringIn { nt, .. }
            | RecordData::StringOut { nt, .. } => nt.time_stamp = Some(ts),
            RecordData::Waveform { nt, .. }
            | RecordData::Aai { nt, .. }
            | RecordData::Aao { nt, .. }
            | RecordData::SubArray { nt, .. } => nt.time_stamp = ts,
            RecordData::NtEnum { nt, .. } => nt.time_stamp = ts,
            RecordData::NtTable { nt, .. } => nt.time_stamp = Some(ts),
            RecordData::NtNdArray { nt, .. } => {
                nt.time_stamp = Some(ts.clone());
                nt.data_time_stamp = ts;
            }
            RecordData::Generic { .. } => {}
        }
    }

    pub fn set_scalar_value(&mut self, value: ScalarValue, compute_alarms: bool) -> bool {
        self.try_set_scalar_value(value, compute_alarms)
            .unwrap_or(false)
    }

    /// [`set_scalar_value`](Self::set_scalar_value), but telling a refusal
    /// apart from a no-op: `None` means `coerce_into` refused the value as
    /// not representable in the record's scalar type (nothing was modified);
    /// `Some(changed)` otherwise. An out-of-range `NtEnum` index keeps its
    /// existing no-op behaviour and is `Some(false)`, not a refusal.
    pub(crate) fn try_set_scalar_value(
        &mut self,
        value: ScalarValue,
        compute_alarms: bool,
    ) -> Option<bool> {
        if let RecordData::NtEnum { nt, .. } = &mut self.data {
            let idx = match &value {
                ScalarValue::I32(i) => *i,
                ScalarValue::I16(i) => *i as i32,
                ScalarValue::I8(i) => *i as i32,
                ScalarValue::I64(i) => *i as i32,
                _ => return Some(false),
            };
            if idx < 0 || (idx as usize) >= nt.choices.len() {
                return Some(false); // out-of-range index — reject, keep value
            }
            if nt.index == idx {
                return Some(false);
            }
            nt.index = idx;
            // NtEnum encodes its stored time_stamp verbatim — stamp the
            // update time or clients see epoch 0 forever.
            nt.time_stamp = now_nt_timestamp();
            return Some(true);
        }

        let nt = match &mut self.data {
            RecordData::Ai { nt, .. }
            | RecordData::Ao { nt, .. }
            | RecordData::Bi { nt, .. }
            | RecordData::Bo { nt, .. }
            | RecordData::StringIn { nt, .. }
            | RecordData::StringOut { nt, .. } => nt,
            _ => return Some(false),
        };

        // Not representable in the target type: refused.
        let next = coerce_into(&nt.value, value)?;
        if nt.value == next {
            return Some(false);
        }
        nt.value = next;
        // A `None` time_stamp makes the encoder stamp now() at *encode*
        // time. The monitor delta path re-encodes the previous and next
        // snapshots microseconds apart, so secondsPastEpoch compares
        // equal and is never flagged changed — freezing the seconds on
        // monitoring clients. Store the update time so encodes are
        // stable (see spvd_encode::encode_timestamp).
        nt.time_stamp = Some(now_nt_timestamp());
        if compute_alarms {
            nt.update_alarm_from_value();
        }
        Some(true)
    }

    pub fn set_array_value(&mut self, value: ScalarArrayValue) -> bool {
        // Unlike NtScalar (where a `None` time_stamp makes the encoder fall
        // back to now()), array payloads encode their stored time_stamp
        // verbatim — so stamp the update time here or clients see epoch 0.
        let ts = now_nt_timestamp();
        // NtNdArray keeps its dimensions; only the flat pixel data changes.
        if let RecordData::NtNdArray { nt, .. } = &mut self.data {
            if nt.value == value {
                return false;
            }
            nt.value = value;
            nt.time_stamp = Some(ts.clone());
            nt.data_time_stamp = ts;
            return true;
        }
        let (nt, nord, nelm) = match &mut self.data {
            RecordData::Waveform { nt, nord, nelm, .. }
            | RecordData::Aai { nt, nord, nelm, .. }
            | RecordData::Aao { nt, nord, nelm, .. }
            | RecordData::SubArray { nt, nord, nelm, .. } => (nt, nord, *nelm),
            _ => return false,
        };

        let mut next = value;
        truncate_scalar_array_to_nelm(&mut next, nelm);
        if nt.value == next {
            return false;
        }

        *nord = next.len();
        nt.value = next;
        nt.time_stamp = ts;
        true
    }

    pub fn set_nt_payload(&mut self, payload: NtPayload) -> bool {
        match (&mut self.data, payload) {
            (
                RecordData::Ai { nt, .. }
                | RecordData::Ao { nt, .. }
                | RecordData::Bi { nt, .. }
                | RecordData::Bo { nt, .. }
                | RecordData::StringIn { nt, .. }
                | RecordData::StringOut { nt, .. },
                NtPayload::Scalar(mut next),
            ) => {
                if *nt == next {
                    false
                } else {
                    // Respect a caller-supplied acquisition time; otherwise
                    // stamp the update time so monitor deltas stay stable
                    // (see set_scalar_value).
                    if next.time_stamp.is_none() {
                        next.time_stamp = Some(now_nt_timestamp());
                    }
                    *nt = next;
                    true
                }
            }
            (
                RecordData::Waveform { nt, nord, nelm, .. }
                | RecordData::Aai { nt, nord, nelm, .. }
                | RecordData::Aao { nt, nord, nelm, .. }
                | RecordData::SubArray { nt, nord, nelm, .. },
                NtPayload::ScalarArray(mut next),
            ) => {
                truncate_scalar_array_to_nelm(&mut next.value, *nelm);
                let next_len = next.value.len();
                if *nt == next {
                    false
                } else {
                    // Epoch 0 is the untouched default, never a real
                    // acquisition time — stamp the update time instead.
                    if next.time_stamp == NtTimeStamp::default() {
                        next.time_stamp = now_nt_timestamp();
                    }
                    *nord = next_len;
                    *nt = next;
                    true
                }
            }
            (RecordData::NtTable { nt, .. }, NtPayload::Table(mut next)) => {
                if next.validate().is_err() || *nt == next {
                    false
                } else {
                    if next.time_stamp.is_none() {
                        next.time_stamp = Some(now_nt_timestamp());
                    }
                    *nt = next;
                    true
                }
            }
            (RecordData::NtNdArray { nt, .. }, NtPayload::NdArray(mut next)) => {
                if next.validate().is_err() || *nt == next {
                    false
                } else {
                    let ts = now_nt_timestamp();
                    if next.time_stamp.is_none() {
                        next.time_stamp = Some(ts.clone());
                    }
                    if next.data_time_stamp == NtTimeStamp::default() {
                        next.data_time_stamp = ts;
                    }
                    *nt = next;
                    true
                }
            }
            (RecordData::NtEnum { nt, .. }, NtPayload::Enum(mut next)) => {
                if *nt == next {
                    false
                } else {
                    if next.time_stamp == NtTimeStamp::default() {
                        next.time_stamp = now_nt_timestamp();
                    }
                    *nt = next;
                    true
                }
            }
            (
                RecordData::Generic {
                    struct_id, fields, ..
                },
                NtPayload::Generic {
                    struct_id: next_id,
                    fields: next_fields,
                },
            ) => {
                if *struct_id == next_id && *fields == next_fields {
                    false
                } else {
                    *struct_id = next_id;
                    *fields = next_fields;
                    true
                }
            }
            _ => false,
        }
    }
}

fn truncate_scalar_array_to_nelm(value: &mut ScalarArrayValue, nelm: usize) {
    match value {
        ScalarArrayValue::Bool(v) => v.truncate(nelm),
        ScalarArrayValue::I8(v) => v.truncate(nelm),
        ScalarArrayValue::I16(v) => v.truncate(nelm),
        ScalarArrayValue::I32(v) => v.truncate(nelm),
        ScalarArrayValue::I64(v) => v.truncate(nelm),
        ScalarArrayValue::U8(v) => v.truncate(nelm),
        ScalarArrayValue::U16(v) => v.truncate(nelm),
        ScalarArrayValue::U32(v) => v.truncate(nelm),
        ScalarArrayValue::U64(v) => v.truncate(nelm),
        ScalarArrayValue::F32(v) => v.truncate(nelm),
        ScalarArrayValue::F64(v) => v.truncate(nelm),
        ScalarArrayValue::Str(v) => v.truncate(nelm),
    }
}

/// The incoming value as an exact integer, when it is one.
fn as_i128(v: &ScalarValue) -> Option<i128> {
    match v {
        ScalarValue::Bool(b) => Some(if *b { 1 } else { 0 }),
        ScalarValue::I8(x) => Some(*x as i128),
        ScalarValue::I16(x) => Some(*x as i128),
        ScalarValue::I32(x) => Some(*x as i128),
        ScalarValue::I64(x) => Some(*x as i128),
        ScalarValue::U8(x) => Some(*x as i128),
        ScalarValue::U16(x) => Some(*x as i128),
        ScalarValue::U32(x) => Some(*x as i128),
        ScalarValue::U64(x) => Some(*x as i128),
        ScalarValue::F32(_) | ScalarValue::F64(_) | ScalarValue::Str(_) => None,
    }
}

/// The incoming value as an f64, when it is numeric.
fn as_f64(v: &ScalarValue) -> Option<f64> {
    match v {
        ScalarValue::F32(x) => Some(*x as f64),
        ScalarValue::F64(x) => Some(*x),
        ScalarValue::Str(_) => None,
        other => as_i128(other).map(|i| i as f64),
    }
}

/// The incoming value rendered as a string, preserving each type's own
/// precision. Notably an `F32` is formatted AS an f32: widening it to f64
/// first turns 0.1 into "0.10000000149011612". A `Bool` renders as "1"/"0",
/// as the explicit `(Str, Bool)` arm always stored it.
fn scalar_to_string(v: &ScalarValue) -> String {
    match v {
        ScalarValue::Bool(x) => if *x { "1" } else { "0" }.to_string(),
        ScalarValue::I8(x) => x.to_string(),
        ScalarValue::I16(x) => x.to_string(),
        ScalarValue::I32(x) => x.to_string(),
        ScalarValue::I64(x) => x.to_string(),
        ScalarValue::U8(x) => x.to_string(),
        ScalarValue::U16(x) => x.to_string(),
        ScalarValue::U32(x) => x.to_string(),
        ScalarValue::U64(x) => x.to_string(),
        ScalarValue::F32(x) => x.to_string(),
        ScalarValue::F64(x) => x.to_string(),
        ScalarValue::Str(s) => s.clone(),
    }
}

/// Coerce `incoming` into `target`'s variant.
///
/// `None` means the value is not representable in the target type and the
/// write must be refused — the same rule the wire PUT path applies through
/// `crate::convert`. The two paths must not disagree.
///
/// Every refusal from THIS function is logged once at debug so a refused
/// client write is visible in logs; success and same-variant writes are not
/// logged. The rule applies to `coerce_into` only: the `NtEnum` early return
/// in `set_scalar_value` keeps its previous behaviour and refuses a
/// non-integer or out-of-range index without logging.
///
/// Behaviour changes relative to the explicit-arm implementation this
/// replaced (each pinned by a test in `mod tests`):
/// 1. `U64`/`I64` into a `Str` target is exact (no f64 round-trip).
/// 2. `F32` into a `Str` target is formatted from the f32, not a widened f64.
/// 3. An out-of-range integer into a narrower integer target is refused
///    (previously wrapped via `as`).
/// 4. A non-finite or out-of-range float into an integer target is refused
///    (previously saturated via `as`; NaN stored 0).
/// 5. An unparsable string into an `F64` target currently holding NaN is
///    refused. Previously the current NaN was "re-assigned", `NaN != NaN`
///    reported a change and the timestamp was re-stamped for a write that
///    changed nothing.
fn coerce_into(target: &ScalarValue, incoming: ScalarValue) -> Option<ScalarValue> {
    if std::mem::discriminant(target) == std::mem::discriminant(&incoming) {
        return Some(incoming);
    }
    let next = coerce_cross_variant(target, &incoming);
    if next.is_none() {
        tracing::debug!(
            target_value = ?target,
            incoming_value = ?incoming,
            "set_scalar_value: write refused, value not representable in the target type"
        );
    }
    next
}

/// The cross-variant half of [`coerce_into`]; `target` and `incoming` are
/// known to be different variants.
fn coerce_cross_variant(target: &ScalarValue, incoming: &ScalarValue) -> Option<ScalarValue> {
    use ScalarValue as S;

    // A string source is not a number. Exactly three targets parse one
    // (Bool, I32, F64); every other target refused a string before this
    // change and still does.
    if let S::Str(s) = incoming {
        return match target {
            S::Bool(_) => parse_bool_like(s).map(S::Bool),
            S::I32(_) => s.parse::<i32>().ok().map(S::I32),
            S::F64(_) => s.parse::<f64>().ok().map(S::F64),
            _ => None,
        };
    }

    // Integers convert exactly or not at all; floats are range-checked
    // BEFORE the cast, because `f as i128` saturates rather than failing.
    fn int<T: TryFrom<i128>>(v: &ScalarValue) -> Option<T> {
        match as_i128(v) {
            Some(i) => T::try_from(i).ok(),
            None => {
                // Float source. i128's range comfortably contains every
                // finite f64 that any integer target could hold, but reject
                // non-finite values explicitly rather than relying on the
                // saturating cast.
                let f = as_f64(v)?;
                if !f.is_finite() {
                    return None;
                }
                T::try_from(f as i128).ok()
            }
        }
    }
    match target {
        S::Str(_) => Some(S::Str(scalar_to_string(incoming))),
        S::Bool(_) => match as_i128(incoming) {
            Some(i) => Some(S::Bool(i != 0)),
            None => as_f64(incoming).map(|f| S::Bool(f != 0.0)),
        },
        S::F64(_) => as_f64(incoming).map(S::F64),
        S::F32(_) => as_f64(incoming).map(|f| S::F32(f as f32)),
        S::I8(_) => int::<i8>(incoming).map(S::I8),
        S::I16(_) => int::<i16>(incoming).map(S::I16),
        S::I32(_) => int::<i32>(incoming).map(S::I32),
        S::I64(_) => int::<i64>(incoming).map(S::I64),
        S::U8(_) => int::<u8>(incoming).map(S::U8),
        S::U16(_) => int::<u16>(incoming).map(S::U16),
        S::U32(_) => int::<u32>(incoming).map(S::U32),
        S::U64(_) => int::<u64>(incoming).map(S::U64),
    }
}

fn parse_bool_like(input: &str) -> Option<bool> {
    match input.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One record of every `RecordData` payload shape, for exhaustive
    /// accessor tests.
    ///
    /// No `use` statements are needed: `types.rs` does `pub use
    /// spvirit_types::*` at module level and `mod tests` does `use super::*`,
    /// so every NT type below is already in scope.
    fn one_record_per_shape() -> Vec<RecordInstance> {
        fn wrap(data: RecordData, record_type: RecordType) -> RecordInstance {
            RecordInstance {
                name: "T".to_string(),
                record_type,
                common: DbCommonState::default(),
                data,
                raw_fields: std::collections::HashMap::new(),
            }
        }

        vec![
            wrap(
                RecordData::Ai {
                    nt: NtScalar::from_value(ScalarValue::F64(1.25)),
                    inp: None,
                    siml: None,
                    siol: None,
                    simm: false,
                },
                RecordType::Ai,
            ),
            wrap(
                RecordData::Waveform {
                    nt: NtScalarArray::from_value(ScalarArrayValue::F64(vec![1.0, 2.0, 3.0])),
                    inp: None,
                    ftvl: "DOUBLE".to_string(),
                    nelm: 3,
                    nord: 3,
                },
                RecordType::Waveform,
            ),
            wrap(
                RecordData::NtTable {
                    // `NtTable` derives no `Default`, so spell it out. Same
                    // shape `simple_store.rs`'s `make_nt_table` (:847) uses.
                    nt: NtTable {
                        labels: vec!["X".to_string()],
                        columns: vec![NtTableColumn {
                            name: "x".to_string(),
                            values: ScalarArrayValue::F64(vec![1.0, 2.0]),
                        }],
                        descriptor: Some("table".to_string()),
                        alarm: None,
                        time_stamp: None,
                    },
                    inp: None,
                    out: None,
                    omsl: OutputMode::Supervisory,
                },
                RecordType::NtTable,
            ),
            wrap(
                RecordData::NtNdArray {
                    // `NtNdArray` derives no `Default`; `empty()` is the
                    // constructor the crate provides.
                    nt: NtNdArray::empty(),
                    inp: None,
                    out: None,
                    omsl: OutputMode::Supervisory,
                },
                RecordType::NtNdArray,
            ),
            wrap(
                RecordData::NtEnum {
                    // `NtEnum` derives no `Default`; `NtEnum::new(index,
                    // choices)` is the constructor.
                    nt: NtEnum::new(1, vec!["OFF".to_string(), "ON".to_string()]),
                    inp: None,
                    out: None,
                    omsl: OutputMode::Supervisory,
                },
                // NOTE: `RecordType` has NO `NtEnum` variant. An ntenum record
                // is typed `Mbbi` (`RecordType::from_db_name` maps
                // `"mbbi" | "ntenum" => Mbbi`, types.rs:59).
                RecordType::Mbbi,
            ),
            wrap(
                RecordData::Generic {
                    struct_id: "epics:nt/NTScalar:1.0".to_string(),
                    fields: vec![("value".to_string(), PvValue::Scalar(ScalarValue::I32(3)))],
                    inp: None,
                    out: None,
                    omsl: OutputMode::Supervisory,
                },
                RecordType::Generic,
            ),
        ]
    }

    fn ai_record(val: f64) -> RecordInstance {
        RecordInstance {
            name: "T".to_string(),
            record_type: RecordType::Ai,
            common: DbCommonState::default(),
            data: RecordData::Ai {
                nt: NtScalar::from_value(ScalarValue::F64(val)),
                inp: None,
                siml: None,
                siol: None,
                simm: false,
            },
            raw_fields: std::collections::HashMap::new(),
        }
    }

    /// `ai_record` with an arbitrary scalar variant in place of F64.
    fn ai_record_of(value: ScalarValue) -> RecordInstance {
        let mut rec = ai_record(0.0);
        if let RecordData::Ai { nt, .. } = &mut rec.data {
            nt.value = value;
        }
        rec
    }

    #[test]
    fn set_scalar_value_accepts_a_small_magnitude_f64_change() {
        let mut rec = ai_record(0.0);
        assert!(
            rec.set_scalar_value(ScalarValue::F64(1e-18), false),
            "a write of 1e-18 over 0.0 is a real change and must be accepted"
        );
        assert_eq!(rec.current_value(), ScalarValue::F64(1e-18));
    }

    #[test]
    fn set_scalar_value_accepts_a_small_magnitude_f32_change() {
        let mut rec = ai_record_of(ScalarValue::F32(0.0));
        assert!(
            rec.set_scalar_value(ScalarValue::F32(1e-9), false),
            "f32::EPSILON is 1.19e-7; a write of 1e-9 must not be swallowed"
        );
        assert_eq!(rec.current_value(), ScalarValue::F32(1e-9));
    }

    #[test]
    fn set_scalar_value_accepts_a_small_cross_variant_float_change() {
        // Exercises the catch-all coercion arms, not just the same-variant ones.
        let mut rec = ai_record_of(ScalarValue::F32(0.0));
        assert!(
            rec.set_scalar_value(ScalarValue::F64(1e-9), false),
            "cross-variant writes must use the same exact comparison"
        );
    }

    #[test]
    fn set_scalar_value_still_reports_an_identical_float_write_as_unchanged() {
        let mut rec = ai_record(2.5);
        assert!(
            !rec.set_scalar_value(ScalarValue::F64(2.5), false),
            "writing the same value is still a no-op"
        );
    }

    #[test]
    fn set_time_stamp_overwrites_an_existing_stamp() {
        let mut rec = ai_record(1.0);
        rec.set_time_stamp(NtTimeStamp {
            seconds_past_epoch: 100,
            nanoseconds: 0,
            user_tag: 0,
        });
        let replacement = NtTimeStamp {
            seconds_past_epoch: 200,
            nanoseconds: 7,
            user_tag: 3,
        };
        rec.set_time_stamp(replacement.clone());

        let RecordData::Ai { nt, .. } = &rec.data else {
            panic!("expected Ai");
        };
        assert_eq!(nt.time_stamp, Some(replacement));
    }

    #[test]
    fn set_time_stamp_on_generic_record_is_a_no_op() {
        let mut rec = RecordInstance {
            name: "G".to_string(),
            record_type: RecordType::Generic,
            common: DbCommonState::default(),
            data: RecordData::Generic {
                struct_id: "epics:nt/NTScalar:1.0".to_string(),
                fields: vec![],
                inp: None,
                out: None,
                omsl: OutputMode::Supervisory,
            },
            raw_fields: std::collections::HashMap::new(),
        };
        // Must not panic and must leave the record untouched.
        rec.set_time_stamp(now_nt_timestamp());
        let RecordData::Generic { fields, .. } = &rec.data else {
            panic!("expected Generic");
        };
        assert!(fields.is_empty());
    }

    #[test]
    fn payload_ref_materialises_exactly_what_payload_returns() {
        // Since `payload()` is `payload_ref().to_owned_payload()`, this is a
        // consistency check only; the real guard is the frozen
        // `reference_payload` oracle in
        // `payload_matches_the_frozen_reference_for_every_variant`.
        for rec in one_record_per_shape() {
            assert_eq!(
                rec.data.payload_ref().to_owned_payload(),
                rec.data.payload(),
                "borrowed and owned payload views disagree for {:?}",
                rec.record_type
            );
        }
    }

    /// Frozen copy of `RecordData::payload()` as it was before it was
    /// re-expressed over `NtPayloadRef` (body verbatim from commit 22181ad,
    /// with `Self::` spelled `RecordData::`). Do not "simplify" this into a
    /// call to `payload()`/`payload_ref()`: its independence is what makes it
    /// an oracle.
    fn reference_payload(data: &RecordData) -> NtPayload {
        match data {
            RecordData::Ai { nt, .. }
            | RecordData::Ao { nt, .. }
            | RecordData::Bi { nt, .. }
            | RecordData::Bo { nt, .. }
            | RecordData::StringIn { nt, .. }
            | RecordData::StringOut { nt, .. } => NtPayload::Scalar(nt.clone()),
            RecordData::Waveform { nt, .. }
            | RecordData::Aai { nt, .. }
            | RecordData::Aao { nt, .. }
            | RecordData::SubArray { nt, .. } => NtPayload::ScalarArray(nt.clone()),
            RecordData::NtTable { nt, .. } => NtPayload::Table(nt.clone()),
            RecordData::NtNdArray { nt, .. } => NtPayload::NdArray(nt.clone()),
            RecordData::NtEnum { nt, .. } => NtPayload::Enum(nt.clone()),
            RecordData::Generic {
                struct_id, fields, ..
            } => NtPayload::Generic {
                struct_id: struct_id.clone(),
                fields: fields.clone(),
            },
        }
    }

    /// One record for each of the 14 `RecordData` variants, each with a
    /// distinct value so a wrong arm mapping cannot pass by coincidence.
    /// Sibling of `one_record_per_shape()`, which other tests depend on.
    fn one_record_per_variant() -> Vec<RecordInstance> {
        fn wrap(data: RecordData, record_type: RecordType) -> RecordInstance {
            RecordInstance {
                name: "T".to_string(),
                record_type,
                common: DbCommonState::default(),
                data,
                raw_fields: std::collections::HashMap::new(),
            }
        }
        // Each scalar-carrying record gets a distinct NON-ZERO severity so an
        // accessor reading the wrong arm cannot pass on 0 == 0.
        fn scalar(v: ScalarValue, severity: i32) -> NtScalar {
            let mut nt = NtScalar::from_value(v);
            nt.alarm_severity = severity;
            nt
        }
        fn array(v: ScalarArrayValue) -> NtScalarArray {
            NtScalarArray::from_value(v)
        }

        vec![
            wrap(
                RecordData::Ai {
                    nt: scalar(ScalarValue::F64(1.25), 1),
                    inp: None,
                    siml: None,
                    siol: None,
                    simm: false,
                },
                RecordType::Ai,
            ),
            wrap(
                RecordData::Ao {
                    nt: scalar(ScalarValue::F64(2.5), 2),
                    out: None,
                    dol: None,
                    omsl: OutputMode::Supervisory,
                    drvl: None,
                    drvh: None,
                    oroc: None,
                    siml: None,
                    siol: None,
                    simm: false,
                },
                RecordType::Ao,
            ),
            wrap(
                RecordData::Bi {
                    nt: scalar(ScalarValue::Bool(true), 3),
                    inp: None,
                    znam: "Low".to_string(),
                    onam: "High".to_string(),
                    siml: None,
                    siol: None,
                    simm: false,
                },
                RecordType::Bi,
            ),
            wrap(
                RecordData::Bo {
                    nt: scalar(ScalarValue::Bool(false), 4),
                    out: None,
                    dol: None,
                    omsl: OutputMode::Supervisory,
                    znam: "Off".to_string(),
                    onam: "On".to_string(),
                    siml: None,
                    siol: None,
                    simm: false,
                },
                RecordType::Bo,
            ),
            wrap(
                RecordData::StringIn {
                    nt: scalar(ScalarValue::Str("in".to_string()), 5),
                    inp: None,
                    siml: None,
                    siol: None,
                    simm: false,
                },
                RecordType::StringIn,
            ),
            wrap(
                RecordData::StringOut {
                    nt: scalar(ScalarValue::Str("out".to_string()), 6),
                    out: None,
                    dol: None,
                    omsl: OutputMode::Supervisory,
                    siml: None,
                    siol: None,
                    simm: false,
                },
                RecordType::StringOut,
            ),
            wrap(
                RecordData::Waveform {
                    nt: array(ScalarArrayValue::F64(vec![1.0, 2.0, 3.0])),
                    inp: None,
                    ftvl: "DOUBLE".to_string(),
                    nelm: 3,
                    nord: 3,
                },
                RecordType::Waveform,
            ),
            wrap(
                RecordData::Aai {
                    nt: array(ScalarArrayValue::I32(vec![4, 5])),
                    inp: None,
                    ftvl: "LONG".to_string(),
                    nelm: 2,
                    nord: 2,
                },
                RecordType::Aai,
            ),
            wrap(
                RecordData::Aao {
                    nt: array(ScalarArrayValue::Str(vec!["a".to_string()])),
                    out: None,
                    dol: None,
                    omsl: OutputMode::Supervisory,
                    ftvl: "STRING".to_string(),
                    nelm: 1,
                    nord: 1,
                },
                RecordType::Aao,
            ),
            wrap(
                RecordData::SubArray {
                    nt: array(ScalarArrayValue::F64(vec![7.0, 8.0])),
                    inp: None,
                    ftvl: "DOUBLE".to_string(),
                    malm: 4,
                    nelm: 2,
                    nord: 2,
                    indx: 1,
                },
                RecordType::SubArray,
            ),
            wrap(
                RecordData::NtTable {
                    nt: NtTable {
                        labels: vec!["X".to_string()],
                        columns: vec![NtTableColumn {
                            name: "x".to_string(),
                            values: ScalarArrayValue::F64(vec![1.0, 2.0]),
                        }],
                        descriptor: Some("table".to_string()),
                        alarm: None,
                        time_stamp: None,
                    },
                    inp: None,
                    out: None,
                    omsl: OutputMode::Supervisory,
                },
                RecordType::NtTable,
            ),
            wrap(
                RecordData::NtNdArray {
                    nt: NtNdArray::empty(),
                    inp: None,
                    out: None,
                    omsl: OutputMode::Supervisory,
                },
                RecordType::NtNdArray,
            ),
            wrap(
                RecordData::NtEnum {
                    nt: NtEnum::new(1, vec!["OFF".to_string(), "ON".to_string()]),
                    inp: None,
                    out: None,
                    omsl: OutputMode::Supervisory,
                },
                // `RecordType` has no `NtEnum` variant; ntenum is `Mbbi`.
                RecordType::Mbbi,
            ),
            wrap(
                RecordData::Generic {
                    struct_id: "epics:nt/NTScalar:1.0".to_string(),
                    fields: vec![("value".to_string(), PvValue::Scalar(ScalarValue::I32(3)))],
                    inp: None,
                    out: None,
                    omsl: OutputMode::Supervisory,
                },
                RecordType::Generic,
            ),
        ]
    }

    #[test]
    fn payload_matches_the_frozen_reference_for_every_variant() {
        let mut records = one_record_per_variant();
        assert_eq!(records.len(), 14, "one record per RecordData variant");
        records.extend(one_record_per_shape());
        for rec in &records {
            assert_eq!(
                rec.data.payload(),
                reference_payload(&rec.data),
                "payload() drifted from the frozen reference for {:?}",
                rec.record_type
            );
        }
    }

    /// Frozen copy of `RecordInstance::to_ntscalar()` before Task 7 moved it
    /// onto the borrowed view (body verbatim from commit f7380f7, with
    /// `self.to_ntpayload()` spelled `reference_payload(&rec.data)`).
    fn reference_to_ntscalar(rec: &RecordInstance) -> NtScalar {
        match reference_payload(&rec.data) {
            NtPayload::Scalar(nt) => nt,
            NtPayload::ScalarArray(nt) => {
                let mut scalar = NtScalar::from_value(ScalarValue::I32(nt.value.len() as i32));
                scalar.display_description = "Array length".to_string();
                scalar
            }
            NtPayload::Table(nt) => {
                let mut scalar = NtScalar::from_value(ScalarValue::I32(nt.columns.len() as i32));
                scalar.display_description = "Table columns".to_string();
                scalar
            }
            NtPayload::NdArray(nt) => {
                let mut scalar = NtScalar::from_value(ScalarValue::I32(nt.dimension.len() as i32));
                scalar.display_description = "NDArray dimensions".to_string();
                scalar
            }
            NtPayload::Enum(nt) => {
                let mut scalar = NtScalar::from_value(ScalarValue::I32(nt.index));
                scalar.display_description = nt.selected().unwrap_or("").to_string();
                scalar
            }
            NtPayload::Generic { fields, .. } => {
                let mut scalar = NtScalar::from_value(ScalarValue::I32(fields.len() as i32));
                scalar.display_description = "Generic structure".to_string();
                scalar
            }
        }
    }

    /// Frozen copy of `RecordInstance::current_value()` before Task 7 (body
    /// verbatim from commit f7380f7, same substitution as above).
    fn reference_current_value(rec: &RecordInstance) -> ScalarValue {
        match reference_payload(&rec.data) {
            NtPayload::Scalar(nt) => nt.value,
            NtPayload::ScalarArray(nt) => ScalarValue::I32(nt.value.len() as i32),
            NtPayload::Table(nt) => ScalarValue::I32(nt.columns.len() as i32),
            NtPayload::NdArray(nt) => ScalarValue::I32(nt.dimension.len() as i32),
            NtPayload::Enum(nt) => ScalarValue::I32(nt.index),
            NtPayload::Generic { fields, .. } => ScalarValue::I32(fields.len() as i32),
        }
    }

    #[test]
    fn task7_accessors_match_the_frozen_references_for_every_variant() {
        for rec in one_record_per_variant() {
            let reference = reference_payload(&rec.data);
            let reference_scalar = match &reference {
                NtPayload::Scalar(nt) => Some(nt),
                _ => None,
            };
            assert_eq!(
                rec.data.nt_scalar(),
                reference_scalar,
                "nt_scalar disagrees with the frozen reference for {:?}",
                rec.record_type
            );
            assert_eq!(
                rec.alarm_severity(),
                reference_scalar.map_or(0, |nt| nt.alarm_severity),
                "alarm_severity disagrees with the frozen reference for {:?}",
                rec.record_type
            );
            assert_eq!(
                rec.to_ntscalar(),
                reference_to_ntscalar(&rec),
                "to_ntscalar disagrees with the frozen reference for {:?}",
                rec.record_type
            );
            assert_eq!(
                rec.current_value(),
                reference_current_value(&rec),
                "current_value disagrees with the frozen reference for {:?}",
                rec.record_type
            );
        }
    }

    #[test]
    fn scalar_fixtures_carry_distinct_non_zero_severities() {
        let severities: Vec<i32> = one_record_per_variant()
            .iter()
            .filter_map(|rec| match reference_payload(&rec.data) {
                NtPayload::Scalar(nt) => Some(nt.alarm_severity),
                _ => None,
            })
            .collect();
        assert_eq!(severities, vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn alarm_severity_matches_the_owned_accessor_for_every_shape() {
        for rec in one_record_per_shape() {
            assert_eq!(
                rec.alarm_severity(),
                rec.to_ntscalar().alarm_severity,
                "alarm_severity disagrees with to_ntscalar for {:?}",
                rec.record_type
            );
        }
    }

    #[test]
    fn alarm_severity_reads_a_real_scalar_severity() {
        let mut rec = ai_record(1.0);
        if let RecordData::Ai { nt, .. } = &mut rec.data {
            nt.alarm_severity = 2;
        }
        assert_eq!(rec.alarm_severity(), 2);
    }

    #[test]
    fn nt_scalar_is_none_where_nt_would_have_panicked() {
        for rec in one_record_per_shape() {
            let is_scalar_shape = matches!(rec.payload_ref(), NtPayloadRef::Scalar(_));
            assert_eq!(
                rec.data.nt_scalar().is_some(),
                is_scalar_shape,
                "nt_scalar disagrees with the payload shape for {:?}",
                rec.record_type
            );
        }
    }

    const ALL_SCALARS: &[fn() -> ScalarValue] = &[
        || ScalarValue::Bool(true),
        || ScalarValue::I8(3),
        || ScalarValue::I16(3),
        || ScalarValue::I32(3),
        || ScalarValue::I64(3),
        || ScalarValue::U8(3),
        || ScalarValue::U16(3),
        || ScalarValue::U32(3),
        || ScalarValue::U64(3),
        || ScalarValue::F32(3.0),
        || ScalarValue::F64(3.0),
        || ScalarValue::Str("3".to_string()),
    ];

    #[test]
    fn set_scalar_value_same_variant_round_trips_for_every_type() {
        for make in ALL_SCALARS {
            let v = make();
            let mut rec = ai_record_of(ScalarValue::I32(0));
            if let RecordData::Ai { nt, .. } = &mut rec.data {
                nt.value = v.clone();
            }
            // Writing the same value back is a no-op...
            assert!(
                !rec.set_scalar_value(v.clone(), false),
                "same-variant identical write must report unchanged for {v:?}"
            );
            assert_eq!(rec.current_value(), v, "no-op write must not mutate");
        }
    }

    #[test]
    fn set_scalar_value_always_preserves_the_target_variant() {
        for target in ALL_SCALARS {
            for incoming in ALL_SCALARS {
                let t = target();
                let i = incoming();
                let mut rec = ai_record_of(t.clone());
                rec.set_scalar_value(i.clone(), false);
                assert_eq!(
                    std::mem::discriminant(&rec.current_value()),
                    std::mem::discriminant(&t),
                    "target {t:?} changed variant after a write of {i:?}"
                );
            }
        }
    }

    #[test]
    fn set_scalar_value_integer_to_integer_is_exact() {
        let mut rec = ai_record_of(ScalarValue::U64(0));
        let big: u64 = 9_007_199_254_740_993; // 2^53 + 1
        assert!(rec.set_scalar_value(ScalarValue::U64(big), false));
        assert_eq!(rec.current_value(), ScalarValue::U64(big));
    }

    #[test]
    fn set_scalar_value_still_parses_strings_into_bool_i32_and_f64_targets() {
        // These three arms exist today (types.rs:667, :676, :685) and must
        // survive the collapse. Every OTHER target refuses a string source,
        // and that must survive too.
        let mut b = ai_record_of(ScalarValue::Bool(false));
        assert!(b.set_scalar_value(ScalarValue::Str("on".to_string()), false));
        assert_eq!(b.current_value(), ScalarValue::Bool(true));

        let mut i = ai_record_of(ScalarValue::I32(0));
        assert!(i.set_scalar_value(ScalarValue::Str("-42".to_string()), false));
        assert_eq!(i.current_value(), ScalarValue::I32(-42));

        let mut f = ai_record_of(ScalarValue::F64(0.0));
        assert!(f.set_scalar_value(ScalarValue::Str("1.5".to_string()), false));
        assert_eq!(f.current_value(), ScalarValue::F64(1.5));

        // Unparsable text is a no-op, not a zeroing write.
        let mut bad = ai_record_of(ScalarValue::I32(7));
        assert!(!bad.set_scalar_value(ScalarValue::Str("banana".to_string()), false));
        assert_eq!(bad.current_value(), ScalarValue::I32(7));

        // A target with no string arm refuses.
        let mut u = ai_record_of(ScalarValue::U16(3));
        assert!(!u.set_scalar_value(ScalarValue::Str("5".to_string()), false));
        assert_eq!(u.current_value(), ScalarValue::U16(3));
    }

    // --- deliberate behaviour changes (see `coerce_into`'s doc) ---
    // 1. U64/I64 into Str is exact; 2. F32 into Str formats as f32;
    // 3. out-of-range integer into a narrower integer is refused;
    // 4. non-finite / out-of-range float into an integer is refused;
    // 5. unparsable string into an F64 holding NaN is refused, not a
    //    spurious change report. A refusal returns false and logs at debug.

    #[test]
    fn a_large_integer_written_to_a_string_record_is_exact() {
        let mut rec = ai_record_of(ScalarValue::Str(String::new()));
        let big: u64 = 9_007_199_254_740_993;
        assert!(rec.set_scalar_value(ScalarValue::U64(big), false));
        assert_eq!(
            rec.current_value(),
            ScalarValue::Str("9007199254740993".to_string()),
            "a string record must not round a u64 through f64"
        );
    }

    #[test]
    fn an_f32_written_to_a_string_record_formats_as_an_f32() {
        let mut rec = ai_record_of(ScalarValue::Str(String::new()));
        assert!(rec.set_scalar_value(ScalarValue::F32(0.1), false));
        assert_eq!(
            rec.current_value(),
            ScalarValue::Str("0.1".to_string()),
            "widening to f64 before formatting leaks f32 representation error"
        );
    }

    #[test]
    fn an_out_of_range_integer_into_a_narrower_integer_is_refused() {
        // Change 3: the old code wrapped these via `as`.
        let cases = [
            (ScalarValue::U8(7), ScalarValue::I32(300)),
            (ScalarValue::I8(7), ScalarValue::I64(-200)),
            (ScalarValue::U16(7), ScalarValue::I32(-1)),
        ];
        for (target, incoming) in cases {
            let mut rec = ai_record_of(target.clone());
            assert!(
                !rec.set_scalar_value(incoming.clone(), false),
                "{incoming:?} into {target:?} must be refused"
            );
            assert_eq!(rec.current_value(), target, "refused write must not mutate");
        }
    }

    #[test]
    fn a_non_finite_or_out_of_range_float_into_an_integer_is_refused() {
        // Change 4: the old code saturated these via `as` (NaN stored 0).
        let cases = [
            (ScalarValue::I32(7), ScalarValue::F64(f64::NAN)),
            (ScalarValue::I32(7), ScalarValue::F64(f64::INFINITY)),
            (ScalarValue::I32(7), ScalarValue::F64(1e10)),
            (ScalarValue::U8(7), ScalarValue::F64(-1.0)),
        ];
        for (target, incoming) in cases {
            let mut rec = ai_record_of(target.clone());
            assert!(
                !rec.set_scalar_value(incoming.clone(), false),
                "{incoming:?} into {target:?} must be refused"
            );
            assert_eq!(rec.current_value(), target, "refused write must not mutate");
        }
    }

    #[test]
    fn an_unparsable_string_into_an_f64_holding_nan_is_refused() {
        // Change 5: the old code re-assigned the current NaN, `NaN != NaN`
        // reported a change, and the timestamp was re-stamped.
        let mut rec = ai_record_of(ScalarValue::F64(f64::NAN));
        let stamp_before = match &rec.data {
            RecordData::Ai { nt, .. } => nt.time_stamp.clone(),
            _ => unreachable!(),
        };
        assert!(!rec.set_scalar_value(ScalarValue::Str("abc".to_string()), false));
        match rec.current_value() {
            ScalarValue::F64(x) => assert!(x.is_nan(), "value must still be NaN, got {x}"),
            other => panic!("target variant changed: {other:?}"),
        }
        let stamp_after = match &rec.data {
            RecordData::Ai { nt, .. } => nt.time_stamp.clone(),
            _ => unreachable!(),
        };
        assert_eq!(stamp_after, stamp_before, "a refused write must not re-stamp");
    }

    #[test]
    fn a_refused_coercion_is_logged_once_at_debug_and_success_is_not() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let buf = Buf::default();
        let writer = buf.clone();
        let sub = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .without_time()
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(sub, || {
            let mut ok = ai_record_of(ScalarValue::I32(0));
            assert!(ok.set_scalar_value(ScalarValue::F64(5.0), false));
            let mut refused = ai_record_of(ScalarValue::U8(7));
            assert!(!refused.set_scalar_value(ScalarValue::I32(300), false));
        });

        let out = String::from_utf8(buf.0.lock().unwrap().clone()).expect("utf8");
        let lines: Vec<&str> = out.lines().filter(|l| l.contains("write refused")).collect();
        assert_eq!(lines.len(), 1, "exactly one refusal line expected; got:
{out}");
        assert!(lines[0].contains("DEBUG"), "refusal must log at debug: {}", lines[0]);
        assert!(lines[0].contains("U8(7)"), "log must name the target: {}", lines[0]);
        assert!(lines[0].contains("I32(300)"), "log must name the incoming value: {}", lines[0]);
    }

    // --- preserved behaviour ---

    #[test]
    fn a_bool_written_to_a_string_record_stores_one_or_zero() {
        let mut rec = ai_record_of(ScalarValue::Str(String::new()));
        assert!(rec.set_scalar_value(ScalarValue::Bool(true), false));
        assert_eq!(rec.current_value(), ScalarValue::Str("1".to_string()));
        assert!(rec.set_scalar_value(ScalarValue::Bool(false), false));
        assert_eq!(rec.current_value(), ScalarValue::Str("0".to_string()));
    }
}
