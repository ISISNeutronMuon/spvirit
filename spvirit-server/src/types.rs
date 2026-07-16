//! Server-specific record and IOC types.
//!
//! Shared Normative Type definitions (ScalarValue, NtScalar, NtPayload, etc.)
//! live in the `spvirit-types` crate and are re-exported here for convenience.

use std::collections::HashMap;
use std::time::Duration;

pub use spvirit_types::*;

/// Current wall-clock time as an [`NtTimeStamp`] (UNIX epoch).
fn now_nt_timestamp() -> NtTimeStamp {
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

impl RecordData {
    pub fn nt(&self) -> &NtScalar {
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

    pub fn payload(&self) -> NtPayload {
        match self {
            Self::Ai { nt, .. }
            | Self::Ao { nt, .. }
            | Self::Bi { nt, .. }
            | Self::Bo { nt, .. }
            | Self::StringIn { nt, .. }
            | Self::StringOut { nt, .. } => NtPayload::Scalar(nt.clone()),
            Self::Waveform { nt, .. }
            | Self::Aai { nt, .. }
            | Self::Aao { nt, .. }
            | Self::SubArray { nt, .. } => NtPayload::ScalarArray(nt.clone()),
            Self::NtTable { nt, .. } => NtPayload::Table(nt.clone()),
            Self::NtNdArray { nt, .. } => NtPayload::NdArray(nt.clone()),
            Self::NtEnum { nt, .. } => NtPayload::Enum(nt.clone()),
            Self::Generic {
                struct_id, fields, ..
            } => NtPayload::Generic {
                struct_id: struct_id.clone(),
                fields: fields.clone(),
            },
        }
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
            RecordData::NtEnum { .. } => true,
            RecordData::Generic { .. } => true,
            _ => false,
        }
    }

    pub fn to_ntpayload(&self) -> NtPayload {
        self.data.payload()
    }

    pub fn to_ntscalar(&self) -> NtScalar {
        match self.to_ntpayload() {
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
        match self.to_ntpayload() {
            NtPayload::Scalar(nt) => nt.value,
            NtPayload::ScalarArray(nt) => ScalarValue::I32(nt.value.len() as i32),
            NtPayload::Table(nt) => ScalarValue::I32(nt.columns.len() as i32),
            NtPayload::NdArray(nt) => ScalarValue::I32(nt.dimension.len() as i32),
            NtPayload::Enum(nt) => ScalarValue::I32(nt.index),
            NtPayload::Generic { fields, .. } => ScalarValue::I32(fields.len() as i32),
        }
    }

    pub fn set_scalar_value(&mut self, value: ScalarValue, compute_alarms: bool) -> bool {
        if let RecordData::NtEnum { nt, .. } = &mut self.data {
            let idx = match &value {
                ScalarValue::I32(i) => *i,
                ScalarValue::I16(i) => *i as i32,
                ScalarValue::I8(i) => *i as i32,
                ScalarValue::I64(i) => *i as i32,
                _ => return false,
            };
            if idx < 0 || (idx as usize) >= nt.choices.len() {
                return false; // out-of-range index — reject, keep value
            }
            if nt.index == idx {
                return false;
            }
            nt.index = idx;
            return true;
        }

        let nt = match &mut self.data {
            RecordData::Ai { nt, .. }
            | RecordData::Ao { nt, .. }
            | RecordData::Bi { nt, .. }
            | RecordData::Bo { nt, .. }
            | RecordData::StringIn { nt, .. }
            | RecordData::StringOut { nt, .. } => nt,
            _ => return false,
        };

        let changed = match (&mut nt.value, value) {
            (ScalarValue::Bool(current), ScalarValue::Bool(v)) => {
                if *current == v {
                    false
                } else {
                    *current = v;
                    true
                }
            }
            (ScalarValue::I32(current), ScalarValue::I32(v)) => {
                if *current == v {
                    false
                } else {
                    *current = v;
                    true
                }
            }
            (ScalarValue::F64(current), ScalarValue::F64(v)) => {
                if (*current - v).abs() < f64::EPSILON {
                    false
                } else {
                    *current = v;
                    true
                }
            }
            (ScalarValue::Str(current), ScalarValue::Str(v)) => {
                if *current == v {
                    false
                } else {
                    *current = v;
                    true
                }
            }
            (ScalarValue::Bool(current), ScalarValue::I32(v)) => {
                let next = v != 0;
                if *current == next {
                    false
                } else {
                    *current = next;
                    true
                }
            }
            (ScalarValue::Bool(current), ScalarValue::F64(v)) => {
                let next = v != 0.0;
                if *current == next {
                    false
                } else {
                    *current = next;
                    true
                }
            }
            (ScalarValue::I32(current), ScalarValue::Bool(v)) => {
                let next = if v { 1 } else { 0 };
                if *current == next {
                    false
                } else {
                    *current = next;
                    true
                }
            }
            (ScalarValue::I32(current), ScalarValue::F64(v)) => {
                let next = v as i32;
                if *current == next {
                    false
                } else {
                    *current = next;
                    true
                }
            }
            (ScalarValue::F64(current), ScalarValue::Bool(v)) => {
                let next = if v { 1.0 } else { 0.0 };
                if (*current - next).abs() < f64::EPSILON {
                    false
                } else {
                    *current = next;
                    true
                }
            }
            (ScalarValue::F64(current), ScalarValue::I32(v)) => {
                let next = v as f64;
                if (*current - next).abs() < f64::EPSILON {
                    false
                } else {
                    *current = next;
                    true
                }
            }
            (ScalarValue::Str(current), ScalarValue::Bool(v)) => {
                let next = if v { "1" } else { "0" }.to_string();
                if *current == next {
                    false
                } else {
                    *current = next;
                    true
                }
            }
            (ScalarValue::Str(current), ScalarValue::I32(v)) => {
                let next = v.to_string();
                if *current == next {
                    false
                } else {
                    *current = next;
                    true
                }
            }
            (ScalarValue::Str(current), ScalarValue::F64(v)) => {
                let next = v.to_string();
                if *current == next {
                    false
                } else {
                    *current = next;
                    true
                }
            }
            (ScalarValue::Bool(current), ScalarValue::Str(v)) => {
                let next = parse_bool_like(&v).unwrap_or(*current);
                if *current == next {
                    false
                } else {
                    *current = next;
                    true
                }
            }
            (ScalarValue::I32(current), ScalarValue::Str(v)) => {
                let next = v.parse::<i32>().unwrap_or(*current);
                if *current == next {
                    false
                } else {
                    *current = next;
                    true
                }
            }
            (ScalarValue::F64(current), ScalarValue::Str(v)) => {
                let next = v.parse::<f64>().unwrap_or(*current);
                if (*current - next).abs() < f64::EPSILON {
                    false
                } else {
                    *current = next;
                    true
                }
            }
            // Handle all remaining numeric ScalarValue variants by coercing to
            // the target's type via f64.
            (target, other) => {
                let as_f64 = match &other {
                    ScalarValue::I8(v) => *v as f64,
                    ScalarValue::I16(v) => *v as f64,
                    ScalarValue::I64(v) => *v as f64,
                    ScalarValue::U8(v) => *v as f64,
                    ScalarValue::U16(v) => *v as f64,
                    ScalarValue::U32(v) => *v as f64,
                    ScalarValue::U64(v) => *v as f64,
                    ScalarValue::F32(v) => *v as f64,
                    _ => return false,
                };
                match target {
                    ScalarValue::Bool(current) => {
                        let next = as_f64 != 0.0;
                        if *current == next {
                            false
                        } else {
                            *current = next;
                            true
                        }
                    }
                    ScalarValue::I32(current) => {
                        let next = as_f64 as i32;
                        if *current == next {
                            false
                        } else {
                            *current = next;
                            true
                        }
                    }
                    ScalarValue::F64(current) => {
                        if (*current - as_f64).abs() < f64::EPSILON {
                            false
                        } else {
                            *current = as_f64;
                            true
                        }
                    }
                    ScalarValue::Str(current) => {
                        let next = as_f64.to_string();
                        if *current == next {
                            false
                        } else {
                            *current = next;
                            true
                        }
                    }
                    _ => false,
                }
            }
        };
        if changed && compute_alarms {
            nt.update_alarm_from_value();
        }
        changed
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
                NtPayload::Scalar(next),
            ) => {
                if *nt == next {
                    false
                } else {
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
                    *nord = next_len;
                    *nt = next;
                    true
                }
            }
            (RecordData::NtTable { nt, .. }, NtPayload::Table(next)) => {
                if next.validate().is_err() || *nt == next {
                    false
                } else {
                    *nt = next;
                    true
                }
            }
            (RecordData::NtNdArray { nt, .. }, NtPayload::NdArray(next)) => {
                if next.validate().is_err() || *nt == next {
                    false
                } else {
                    *nt = next;
                    true
                }
            }
            (RecordData::NtEnum { nt, .. }, NtPayload::Enum(next)) => {
                if *nt == next {
                    false
                } else {
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

fn parse_bool_like(input: &str) -> Option<bool> {
    match input.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}
