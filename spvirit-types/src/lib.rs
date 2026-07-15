//! Shared Normative Type (NT) data model types.
//!
//! These types represent the PVAccess Normative Types used across the
//! codec, client tools, server, and packet capture subsystems.

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum ScalarValue {
    Bool(bool),
    I8(i8),
    I16(i16),
    I32(i32),
    I64(i64),
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    F32(f32),
    F64(f64),
    Str(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ScalarArrayValue {
    Bool(Vec<bool>),
    I8(Vec<i8>),
    I16(Vec<i16>),
    I32(Vec<i32>),
    I64(Vec<i64>),
    U8(Vec<u8>),
    U16(Vec<u16>),
    U32(Vec<u32>),
    U64(Vec<u64>),
    F32(Vec<f32>),
    F64(Vec<f64>),
    Str(Vec<String>),
}

impl ScalarArrayValue {
    pub fn len(&self) -> usize {
        match self {
            Self::Bool(v) => v.len(),
            Self::I8(v) => v.len(),
            Self::I16(v) => v.len(),
            Self::I32(v) => v.len(),
            Self::I64(v) => v.len(),
            Self::U8(v) => v.len(),
            Self::U16(v) => v.len(),
            Self::U32(v) => v.len(),
            Self::U64(v) => v.len(),
            Self::F32(v) => v.len(),
            Self::F64(v) => v.len(),
            Self::Str(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn element_size_bytes(&self) -> usize {
        match self {
            Self::Bool(_) => 1,
            Self::I8(_) => 1,
            Self::I16(_) => 2,
            Self::I32(_) => 4,
            Self::I64(_) => 8,
            Self::U8(_) => 1,
            Self::U16(_) => 2,
            Self::U32(_) => 4,
            Self::U64(_) => 8,
            Self::F32(_) => 4,
            Self::F64(_) => 8,
            Self::Str(v) => v.iter().map(|s| s.len()).sum(),
        }
    }

    pub fn type_label(&self) -> &'static str {
        match self {
            Self::Bool(_) => "boolean[]",
            Self::I8(_) => "byte[]",
            Self::I16(_) => "short[]",
            Self::I32(_) => "int[]",
            Self::I64(_) => "long[]",
            Self::U8(_) => "ubyte[]",
            Self::U16(_) => "ushort[]",
            Self::U32(_) => "uint[]",
            Self::U64(_) => "ulong[]",
            Self::F32(_) => "float[]",
            Self::F64(_) => "double[]",
            Self::Str(_) => "string[]",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NtAlarm {
    pub severity: i32,
    pub status: i32,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NtTimeStamp {
    pub seconds_past_epoch: i64,
    pub nanoseconds: i32,
    pub user_tag: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NtDisplay {
    pub limit_low: f64,
    pub limit_high: f64,
    pub description: String,
    pub units: String,
    pub precision: i32,
}

impl Default for NtDisplay {
    fn default() -> Self {
        Self {
            limit_low: 0.0,
            limit_high: 0.0,
            description: String::new(),
            units: String::new(),
            precision: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NtControl {
    pub limit_low: f64,
    pub limit_high: f64,
    pub min_step: f64,
}

impl Default for NtControl {
    fn default() -> Self {
        Self {
            limit_low: 0.0,
            limit_high: 0.0,
            min_step: 0.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NtScalar {
    pub value: ScalarValue,
    pub alarm_severity: i32,
    pub alarm_status: i32,
    pub alarm_message: String,
    pub alarm_low: Option<f64>,
    pub alarm_high: Option<f64>,
    pub alarm_lolo: Option<f64>,
    pub alarm_hihi: Option<f64>,
    pub display_low: f64,
    pub display_high: f64,
    pub display_description: String,
    pub display_precision: i32,
    pub display_form_index: i32,
    pub display_form_choices: Vec<String>,
    pub control_low: f64,
    pub control_high: f64,
    pub control_min_step: f64,
    pub units: String,
    pub value_alarm_active: bool,
    pub value_alarm_low_alarm_limit: f64,
    pub value_alarm_low_warning_limit: f64,
    pub value_alarm_high_warning_limit: f64,
    pub value_alarm_high_alarm_limit: f64,
    pub value_alarm_low_alarm_severity: i32,
    pub value_alarm_low_warning_severity: i32,
    pub value_alarm_high_warning_severity: i32,
    pub value_alarm_high_alarm_severity: i32,
    pub value_alarm_hysteresis: u8,
    /// Optional explicit timestamp for this value. When `Some`, the encoder
    /// serialises exactly this `timeStamp` (stable across encodes); when
    /// `None`, the encoder falls back to stamping `SystemTime::now()` at
    /// encode time. Set this to the time the value was actually acquired /
    /// updated so that monitor deltas flag `secondsPastEpoch` correctly and
    /// clients can detect stale data. See `with_timestamp`.
    pub time_stamp: Option<NtTimeStamp>,
}

impl NtScalar {
    pub fn from_value(value: ScalarValue) -> Self {
        Self {
            value,
            alarm_severity: 0,
            alarm_status: 0,
            alarm_message: String::new(),
            alarm_low: None,
            alarm_high: None,
            alarm_lolo: None,
            alarm_hihi: None,
            display_low: 0.0,
            display_high: 0.0,
            display_description: String::new(),
            display_precision: 0,
            display_form_index: 0,
            display_form_choices: default_form_choices(),
            control_low: 0.0,
            control_high: 0.0,
            control_min_step: 0.0,
            units: String::new(),
            value_alarm_active: false,
            value_alarm_low_alarm_limit: 0.0,
            value_alarm_low_warning_limit: 0.0,
            value_alarm_high_warning_limit: 0.0,
            value_alarm_high_alarm_limit: 0.0,
            value_alarm_low_alarm_severity: 0,
            value_alarm_low_warning_severity: 0,
            value_alarm_high_warning_severity: 0,
            value_alarm_high_alarm_severity: 0,
            value_alarm_hysteresis: 0,
            time_stamp: None,
        }
    }

    /// Set an explicit `timeStamp` (seconds/nanoseconds past the UNIX epoch)
    /// for this value. Supplying a stable, per-update timestamp lets monitor
    /// deltas report `secondsPastEpoch` changes correctly and lets clients
    /// see the real acquisition time rather than the server's encode time.
    pub fn with_timestamp(mut self, seconds_past_epoch: i64, nanoseconds: i32) -> Self {
        self.time_stamp = Some(NtTimeStamp {
            seconds_past_epoch,
            nanoseconds,
            user_tag: 0,
        });
        self
    }

    pub fn with_limits(mut self, low: f64, high: f64) -> Self {
        self.display_low = low;
        self.display_high = high;
        self.control_low = low;
        self.control_high = high;
        self
    }

    pub fn with_units(mut self, units: String) -> Self {
        self.units = units;
        self
    }

    pub fn with_description(mut self, description: String) -> Self {
        self.display_description = description;
        self
    }

    pub fn with_precision(mut self, precision: i32) -> Self {
        self.display_precision = precision;
        self
    }

    pub fn with_alarm_limits(
        mut self,
        low: Option<f64>,
        high: Option<f64>,
        lolo: Option<f64>,
        hihi: Option<f64>,
    ) -> Self {
        self.alarm_low = low;
        self.alarm_high = high;
        self.alarm_lolo = lolo;
        self.alarm_hihi = hihi;
        if let Some(v) = low {
            self.value_alarm_low_warning_limit = v;
        }
        if let Some(v) = high {
            self.value_alarm_high_warning_limit = v;
        }
        if let Some(v) = lolo {
            self.value_alarm_low_alarm_limit = v;
        }
        if let Some(v) = hihi {
            self.value_alarm_high_alarm_limit = v;
        }
        self
    }

    pub fn update_alarm_from_value(&mut self) {
        let val = match self.value {
            ScalarValue::F64(v) => v,
            ScalarValue::F32(v) => v as f64,
            ScalarValue::I8(v) => v as f64,
            ScalarValue::I16(v) => v as f64,
            ScalarValue::I32(v) => v as f64,
            ScalarValue::I64(v) => v as f64,
            ScalarValue::U8(v) => v as f64,
            ScalarValue::U16(v) => v as f64,
            ScalarValue::U32(v) => v as f64,
            ScalarValue::U64(v) => v as f64,
            _ => {
                self.alarm_severity = 0;
                self.alarm_status = 0;
                self.alarm_message.clear();
                return;
            }
        };

        let mut severity = 0;
        let mut message = String::new();

        if let Some(hihi) = self.alarm_hihi {
            if val >= hihi {
                severity = 2;
                message = "HIHI".to_string();
            }
        }
        if severity == 0 {
            if let Some(high) = self.alarm_high {
                if val >= high {
                    severity = 1;
                    message = "HIGH".to_string();
                }
            }
        }
        if severity == 0 {
            if let Some(lolo) = self.alarm_lolo {
                if val <= lolo {
                    severity = 2;
                    message = "LOLO".to_string();
                }
            }
        }
        if severity == 0 {
            if let Some(low) = self.alarm_low {
                if val <= low {
                    severity = 1;
                    message = "LOW".to_string();
                }
            }
        }

        if severity == 0 {
            self.alarm_severity = 0;
            self.alarm_status = 0;
            self.alarm_message.clear();
        } else {
            self.alarm_severity = severity;
            self.alarm_status = 1;
            self.alarm_message = message;
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NtScalarArray {
    pub value: ScalarArrayValue,
    pub alarm: NtAlarm,
    pub time_stamp: NtTimeStamp,
    pub display: NtDisplay,
    pub control: NtControl,
}

impl NtScalarArray {
    pub fn from_value(value: ScalarArrayValue) -> Self {
        Self {
            value,
            alarm: NtAlarm::default(),
            time_stamp: NtTimeStamp::default(),
            display: NtDisplay::default(),
            control: NtControl::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NtTableColumn {
    pub name: String,
    pub values: ScalarArrayValue,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NtTable {
    pub labels: Vec<String>,
    pub columns: Vec<NtTableColumn>,
    pub descriptor: Option<String>,
    pub alarm: Option<NtAlarm>,
    pub time_stamp: Option<NtTimeStamp>,
}

impl NtTable {
    pub fn validate(&self) -> Result<(), String> {
        let mut expected_len: Option<usize> = None;
        for col in &self.columns {
            let len = col.values.len();
            if let Some(expected) = expected_len {
                if expected != len {
                    return Err(format!(
                        "table column '{}' length {} does not match expected {}",
                        col.name, len, expected
                    ));
                }
            } else {
                expected_len = Some(len);
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NdCodec {
    pub name: String,
    pub parameters: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NdDimension {
    pub size: i32,
    pub offset: i32,
    pub full_size: i32,
    pub binning: i32,
    pub reverse: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NtAttribute {
    pub name: String,
    pub value: ScalarValue,
    pub descriptor: String,
    pub source_type: i32,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NtNdArray {
    pub value: ScalarArrayValue,
    pub codec: NdCodec,
    pub compressed_size: i64,
    pub uncompressed_size: i64,
    pub dimension: Vec<NdDimension>,
    pub unique_id: i32,
    pub data_time_stamp: NtTimeStamp,
    pub attribute: Vec<NtAttribute>,
    pub descriptor: Option<String>,
    pub alarm: Option<NtAlarm>,
    pub time_stamp: Option<NtTimeStamp>,
    pub display: Option<NtDisplay>,
}

impl NtNdArray {
    /// Empty NTNDArray with no data — used for type introspection before
    /// any image has been acquired.
    pub fn empty() -> Self {
        Self {
            value: ScalarArrayValue::U8(vec![]),
            codec: NdCodec {
                name: String::new(),
                parameters: Default::default(),
            },
            compressed_size: 0,
            uncompressed_size: 0,
            dimension: vec![],
            unique_id: 0,
            data_time_stamp: NtTimeStamp {
                seconds_past_epoch: 0,
                nanoseconds: 0,
                user_tag: 0,
            },
            attribute: vec![],
            descriptor: None,
            alarm: None,
            time_stamp: None,
            display: None,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self
            .attribute
            .iter()
            .any(|a| a.descriptor.trim().is_empty())
        {
            return Err("ntndarray attribute descriptor must be set".to_string());
        }
        let element_size = self.value.element_size_bytes().max(1) as i64;
        let logical_elements = self
            .dimension
            .iter()
            .map(|d| d.size.max(0) as i64)
            .product::<i64>()
            .max(0);
        let expected_uncompressed = logical_elements.saturating_mul(element_size);
        if self.uncompressed_size > 0 && self.uncompressed_size != expected_uncompressed {
            return Err(format!(
                "uncompressed_size {} does not match dimension*element_size {}",
                self.uncompressed_size, expected_uncompressed
            ));
        }
        if self.compressed_size > 0 && self.compressed_size > self.uncompressed_size {
            return Err(format!(
                "compressed_size {} cannot exceed uncompressed_size {}",
                self.compressed_size, self.uncompressed_size
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// NTEnum — EPICS Normative Type for enumerated values
// ---------------------------------------------------------------------------

/// NTEnum normative type — represents an enumerated PV value.
///
/// The `index` selects one of the `choices` strings.  The wire layout matches
/// the C++ `epics:nt/NTEnum:1.0` structure:
///
/// ```text
/// structure "epics:nt/NTEnum:1.0"
///   enum_t value
///     int index
///     string[] choices
///   alarm_t alarm
///   time_t timeStamp
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct NtEnum {
    pub index: i32,
    pub choices: Vec<String>,
    pub alarm: NtAlarm,
    pub time_stamp: NtTimeStamp,
}

impl NtEnum {
    pub fn new(index: i32, choices: Vec<String>) -> Self {
        Self {
            index,
            choices,
            alarm: NtAlarm::default(),
            time_stamp: NtTimeStamp::default(),
        }
    }

    /// Returns the currently selected choice string, or `None` if the index
    /// is out of range.
    pub fn selected(&self) -> Option<&str> {
        if self.index >= 0 {
            self.choices.get(self.index as usize).map(|s| s.as_str())
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// PvValue — recursive value tree for arbitrary PVA structures
// ---------------------------------------------------------------------------

/// A recursive value type that can represent any PVA structure without
/// depending on the codec crate.  Used by [`NtPayload::Generic`] to carry
/// group-PV composite values and other non-normative structures.
#[derive(Debug, Clone, PartialEq)]
pub enum PvValue {
    Scalar(ScalarValue),
    ScalarArray(ScalarArrayValue),
    Structure {
        struct_id: String,
        fields: Vec<(String, PvValue)>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum NtPayload {
    Scalar(NtScalar),
    ScalarArray(NtScalarArray),
    Table(NtTable),
    NdArray(NtNdArray),
    Enum(NtEnum),
    /// Arbitrary PVA structure — used for group PVs and non-normative types.
    Generic {
        struct_id: String,
        fields: Vec<(String, PvValue)>,
    },
}

pub(crate) fn default_form_choices() -> Vec<String> {
    vec![
        "Default".to_string(),
        "String".to_string(),
        "Binary".to_string(),
        "Decimal".to_string(),
        "Hex".to_string(),
        "Exponential".to_string(),
        "Engineering".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_value_has_no_timestamp_by_default() {
        let nt = NtScalar::from_value(ScalarValue::F64(1.0));
        assert_eq!(nt.time_stamp, None);
    }

    #[test]
    fn with_timestamp_sets_stored_timestamp() {
        let nt = NtScalar::from_value(ScalarValue::F64(1.0)).with_timestamp(1_700_000_000, 250);
        assert_eq!(
            nt.time_stamp,
            Some(NtTimeStamp {
                seconds_past_epoch: 1_700_000_000,
                nanoseconds: 250,
                user_tag: 0,
            })
        );
    }
}
