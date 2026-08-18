//! EPICS alarm severity and condition, and their mapping to the PVA alarm
//! fields carried by [`spvirit_types::NtScalar`].

/// EPICS `epicsAlarmSeverity`. Ordering is meaningful: `recGblSetSevr` raises
/// severity only, so `Ord` is the comparison it uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Severity {
    #[default]
    NoAlarm = 0,
    Minor = 1,
    Major = 2,
    Invalid = 3,
}

impl Severity {
    /// Raise to `other` if it is more severe. Returns whether this changed.
    ///
    /// This is the `recGblSetSevr` primitive: severity only ever increases
    /// within one processing pass.
    pub fn raise(&mut self, other: Severity) -> bool {
        if other > *self {
            *self = other;
            true
        } else {
            false
        }
    }

    /// The EPICS `menuAlarmSevr` entry, as `SEVR` reports it to a client.
    pub fn epics_string(self) -> &'static str {
        match self {
            Severity::NoAlarm => "NO_ALARM",
            Severity::Minor => "MINOR",
            Severity::Major => "MAJOR",
            Severity::Invalid => "INVALID",
        }
    }
}

/// EPICS `epicsAlarmCondition`, in the canonical `alarmString` order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Condition {
    #[default]
    NoAlarm = 0,
    Read = 1,
    Write = 2,
    HiHi = 3,
    High = 4,
    LoLo = 5,
    Low = 6,
    State = 7,
    Cos = 8,
    Comm = 9,
    Timeout = 10,
    HwLimit = 11,
    Calc = 12,
    Scan = 13,
    Link = 14,
    Soft = 15,
    BadSub = 16,
    Udf = 17,
    Disable = 18,
    Simm = 19,
    ReadAccess = 20,
    WriteAccess = 21,
}

impl Condition {
    /// Every condition, in declaration order. Used by the exhaustiveness test
    /// and by diagnostics that render the whole table.
    pub const ALL: [Condition; 22] = [
        Condition::NoAlarm,
        Condition::Read,
        Condition::Write,
        Condition::HiHi,
        Condition::High,
        Condition::LoLo,
        Condition::Low,
        Condition::State,
        Condition::Cos,
        Condition::Comm,
        Condition::Timeout,
        Condition::HwLimit,
        Condition::Calc,
        Condition::Scan,
        Condition::Link,
        Condition::Soft,
        Condition::BadSub,
        Condition::Udf,
        Condition::Disable,
        Condition::Simm,
        Condition::ReadAccess,
        Condition::WriteAccess,
    ];

    /// The EPICS `alarmString[]` entry. This is what a client sees in
    /// `alarm.message`.
    pub fn epics_string(self) -> &'static str {
        match self {
            Condition::NoAlarm => "NO_ALARM",
            Condition::Read => "READ",
            Condition::Write => "WRITE",
            Condition::HiHi => "HIHI",
            Condition::High => "HIGH",
            Condition::LoLo => "LOLO",
            Condition::Low => "LOW",
            Condition::State => "STATE",
            Condition::Cos => "COS",
            Condition::Comm => "COMM",
            Condition::Timeout => "TIMEOUT",
            Condition::HwLimit => "HWLIMIT",
            Condition::Calc => "CALC",
            Condition::Scan => "SCAN",
            Condition::Link => "LINK",
            Condition::Soft => "SOFT",
            Condition::BadSub => "BAD_SUB",
            Condition::Udf => "UDF",
            Condition::Disable => "DISABLE",
            Condition::Simm => "SIMM",
            Condition::ReadAccess => "READ_ACCESS",
            Condition::WriteAccess => "WRITE_ACCESS",
        }
    }

    /// The PVA `alarm.status` category this condition reports as.
    ///
    /// PVA's `statusEnum` is coarse — `NONE=0, DEVICE=1, DRIVER=2, RECORD=3,
    /// DB=4, CONF=5, UNDEFINED=6, CLIENT=7` — so several EPICS conditions
    /// share a category and the precise cause travels in `alarm.message`.
    /// Limit alarms deliberately report `DEVICE` to stay consistent with
    /// `SimplePvStore` (`spvirit-types/src/lib.rs:345`).
    ///
    /// Sub-project E validates this table against `softIoc` + QSRV.
    pub fn pva_status(self) -> i32 {
        match self {
            Condition::NoAlarm => 0,
            Condition::Read
            | Condition::Write
            | Condition::HiHi
            | Condition::High
            | Condition::LoLo
            | Condition::Low
            | Condition::State
            | Condition::Cos
            | Condition::Comm
            | Condition::Timeout
            | Condition::HwLimit => 1,
            Condition::Calc
            | Condition::Scan
            | Condition::Link
            | Condition::Soft
            | Condition::BadSub
            | Condition::Disable
            | Condition::Simm => 3,
            Condition::Udf => 6,
            Condition::ReadAccess | Condition::WriteAccess => 7,
        }
    }
}

/// Map an engine-internal alarm to the `(alarm_severity, alarm_status,
/// alarm_message)` triple carried by `NtScalar`.
///
/// `NO_ALARM` maps to an empty message so a healthy record does not carry
/// stale text, matching `spvirit-types/src/lib.rs:342`.
pub fn to_nt_alarm(sev: Severity, cond: Condition) -> (i32, i32, String) {
    if cond == Condition::NoAlarm {
        return (sev as i32, 0, String::new());
    }
    (
        sev as i32,
        cond.pva_status(),
        cond.epics_string().to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_renders_the_epics_menu_strings() {
        assert_eq!(Severity::NoAlarm.epics_string(), "NO_ALARM");
        assert_eq!(Severity::Minor.epics_string(), "MINOR");
        assert_eq!(Severity::Major.epics_string(), "MAJOR");
        assert_eq!(Severity::Invalid.epics_string(), "INVALID");
    }

    #[test]
    fn severity_raises_but_never_lowers() {
        let mut sev = Severity::Minor;
        assert!(
            !sev.raise(Severity::NoAlarm),
            "lowering must not report a change"
        );
        assert_eq!(sev, Severity::Minor);
        assert!(sev.raise(Severity::Major), "raising must report a change");
        assert_eq!(sev, Severity::Major);
    }

    #[test]
    fn no_alarm_maps_to_none_with_empty_message() {
        assert_eq!(
            to_nt_alarm(Severity::NoAlarm, Condition::NoAlarm),
            (0, 0, String::new())
        );
    }

    #[test]
    fn limit_alarms_match_the_simple_store_convention() {
        // spvirit-types/src/lib.rs:305-347 posts severity 1/2, status 1,
        // message "HIHI"/"HIGH"/"LOLO"/"LOW". The engine must agree.
        assert_eq!(
            to_nt_alarm(Severity::Major, Condition::HiHi),
            (2, 1, "HIHI".to_string())
        );
        assert_eq!(
            to_nt_alarm(Severity::Minor, Condition::Low),
            (1, 1, "LOW".to_string())
        );
    }

    #[test]
    fn undefined_records_map_to_the_undefined_status() {
        assert_eq!(
            to_nt_alarm(Severity::Invalid, Condition::Udf),
            (3, 6, "UDF".to_string())
        );
    }

    #[test]
    fn every_condition_has_a_distinct_epics_string() {
        let all = Condition::ALL;
        let mut seen = std::collections::HashSet::new();
        for cond in all {
            assert!(
                seen.insert(cond.epics_string()),
                "duplicate alarm string for {cond:?}"
            );
        }
        assert_eq!(seen.len(), 22, "EPICS defines 22 alarm conditions");
    }
}
