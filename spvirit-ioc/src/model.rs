//! The typed record model the engine processes.
//!
//! This is deliberately separate from `spvirit_server::types::RecordInstance`:
//! that model serves `SimplePvStore`'s direct-store semantics, while this one
//! carries the processing state (PACT, UDF, NSEV/NSTA, previous values for
//! MDEL/ADEL) that a `dbProcess` equivalent needs.

use crate::alarm::{Condition, Severity};

/// The six record types sub-project A processes. Everything else is D.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Ai,
    Ao,
    Bi,
    Bo,
    LongIn,
    LongOut,
}

impl Kind {
    pub fn from_db_name(name: &str) -> Option<Kind> {
        match name {
            "ai" => Some(Kind::Ai),
            "ao" => Some(Kind::Ao),
            "bi" => Some(Kind::Bi),
            "bo" => Some(Kind::Bo),
            "longin" => Some(Kind::LongIn),
            "longout" => Some(Kind::LongOut),
            _ => None,
        }
    }

    pub fn db_name(self) -> &'static str {
        match self {
            Kind::Ai => "ai",
            Kind::Ao => "ao",
            Kind::Bi => "bi",
            Kind::Bo => "bo",
            Kind::LongIn => "longin",
            Kind::LongOut => "longout",
        }
    }

    /// Output records take a desired value from DOL and write through OUT.
    pub fn is_output(self) -> bool {
        matches!(self, Kind::Ao | Kind::Bo | Kind::LongOut)
    }
}

/// A record's runtime value, in the record's own native type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value {
    Double(f64),
    Long(i32),
    Enum(u16),
}

impl Value {
    pub fn as_f64(self) -> f64 {
        match self {
            Value::Double(v) => v,
            Value::Long(v) => v as f64,
            Value::Enum(v) => v as f64,
        }
    }

    pub fn as_i32(self) -> i32 {
        match self {
            Value::Double(v) => v.round() as i32,
            Value::Long(v) => v,
            Value::Enum(v) => v as i32,
        }
    }

    /// Convert to the representation `kind` stores natively. Binary records
    /// treat any non-zero source as 1, as EPICS does.
    pub fn coerce_to(self, kind: Kind) -> Value {
        match kind {
            Kind::Ai | Kind::Ao => Value::Double(self.as_f64()),
            Kind::LongIn | Kind::LongOut => Value::Long(self.as_i32()),
            Kind::Bi | Kind::Bo => Value::Enum(u16::from(self.as_f64() != 0.0)),
        }
    }

    /// The zero of `kind` — the value an unresolvable link contributes.
    pub fn default_for(kind: Kind) -> Value {
        match kind {
            Kind::Ai | Kind::Ao => Value::Double(0.0),
            Kind::LongIn | Kind::LongOut => Value::Long(0),
            Kind::Bi | Kind::Bo => Value::Enum(0),
        }
    }
}

/// A field a link may address. A is limited to the fields its six record
/// types expose through links; sub-project A2 generalises this to the full
/// `.FIELD` surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Val,
    Sevr,
    Stat,
    Disa,
    Disv,
    Hihi,
    High,
    Low,
    Lolo,
    Proc,
}

impl Field {
    pub fn parse(name: &str) -> Option<Field> {
        match name.to_ascii_uppercase().as_str() {
            "VAL" => Some(Field::Val),
            "SEVR" => Some(Field::Sevr),
            "STAT" => Some(Field::Stat),
            "DISA" => Some(Field::Disa),
            "DISV" => Some(Field::Disv),
            "HIHI" => Some(Field::Hihi),
            "HIGH" => Some(Field::High),
            "LOW" => Some(Field::Low),
            "LOLO" => Some(Field::Lolo),
            "PROC" => Some(Field::Proc),
            _ => None,
        }
    }
}

/// A link field, before Task 4 resolves `target` names to `RecordId`s.
#[derive(Debug, Clone, PartialEq)]
pub enum Link {
    Constant(Value),
    Db {
        target: String,
        field: Field,
        process_passive: bool,
        maximize_severity: bool,
    },
    /// A `.db` link naming a record that does not exist. The engine treats
    /// this as a constant of the record's default and logs once at load.
    /// Sub-project C turns these into channel-access links.
    Unresolved {
        name: String,
    },
}

/// Fields every record type carries (EPICS `dbCommon`).
#[derive(Debug, Clone)]
pub struct Common {
    pub desc: String,
    /// The raw `SCAN` string. A does not act on it — sub-project B does —
    /// but the graph checks need to know whether a record is Passive.
    pub scan_raw: String,
    pub pini: bool,
    pub phas: i32,
    pub pact: bool,
    pub disa: i32,
    pub disv: i32,
    pub diss: Severity,
    pub sdis: Link,
    pub flnk: Link,
    /// Time-stamp event: 0 means "stamp at process time".
    pub tse: i32,
    /// Trace processing.
    pub tpro: bool,
    /// The record has never been given a value.
    pub udf: bool,
    /// Committed alarm state, published to clients.
    pub sevr: Severity,
    pub stat: Condition,
    /// New alarm state accumulating during the current pass.
    pub nsev: Severity,
    pub nsta: Condition,
}

impl Default for Common {
    fn default() -> Self {
        Common {
            desc: String::new(),
            scan_raw: "Passive".to_string(),
            pini: false,
            phas: 0,
            pact: false,
            disa: 0,
            // EPICS defaults DISV to 1 so DISA's default of 0 means enabled.
            disv: 1,
            diss: Severity::NoAlarm,
            sdis: Link::Constant(Value::Long(0)),
            flnk: Link::Constant(Value::Long(0)),
            tse: 0,
            tpro: false,
            udf: true,
            sevr: Severity::NoAlarm,
            stat: Condition::NoAlarm,
            nsev: Severity::NoAlarm,
            nsta: Condition::NoAlarm,
        }
    }
}

/// Alarm limits and monitor deadbands. Only the numeric record types use
/// these; binary records leave them at their defaults.
#[derive(Debug, Clone, Default)]
pub struct Limits {
    pub hihi: f64,
    pub high: f64,
    pub low: f64,
    pub lolo: f64,
    pub hhsv: Severity,
    pub hsv: Severity,
    pub lsv: Severity,
    pub llsv: Severity,
    pub hyst: f64,
    pub mdel: f64,
    pub adel: f64,
    /// True when at least one of HIHI/HIGH/LOW/LOLO was given, so the
    /// engine can tell "limit of 0.0" from "no limit configured".
    pub configured: bool,
}

/// Output mode select: where an output record takes its desired value from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Omsl {
    #[default]
    Supervisory,
    ClosedLoop,
}

/// One record, owned by exactly one lock set.
#[derive(Debug, Clone)]
pub struct Record {
    pub name: String,
    pub kind: Kind,
    pub common: Common,
    pub limits: Limits,
    pub val: Value,
    /// Last value posted as a value monitor — the MDEL reference.
    pub prev_val: Value,
    /// Last value posted as an archive monitor — the ADEL reference.
    pub prev_archive_val: Value,
    /// Last posted alarm state — a change always posts, regardless of MDEL.
    pub prev_sevr: Severity,
    pub prev_stat: Condition,
    pub inp: Link,
    pub out: Link,
    pub dol: Link,
    pub omsl: Omsl,
    /// The epoch-nanosecond timestamp of the last process pass.
    pub time_ns: u64,
}
