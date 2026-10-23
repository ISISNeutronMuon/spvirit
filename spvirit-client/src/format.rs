use chrono::{TimeZone, Utc};
use serde_json::json;

use spvirit_codec::spvd_decode::{DecodedValue, extract_nt_scalar_value, format_compact_value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Json,
}

#[derive(Clone, Debug)]
pub struct RenderOptions {
    pub format: OutputFormat,
    pub include_timestamp: bool,
    pub include_units: bool,
    pub include_alarm: bool,
    pub multiline: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            format: OutputFormat::Text,
            include_timestamp: true,
            include_units: false,
            include_alarm: true,
            multiline: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AlarmInfo {
    pub severity: i32,
    pub status: i32,
    pub message: String,
}

pub fn extract_ts_units(value: &DecodedValue) -> (Option<String>, Option<String>) {
    let mut ts: Option<String> = None;
    let mut units: Option<String> = None;

    let fields = match value {
        DecodedValue::Structure(fields) => fields,
        _ => return (None, None),
    };

    if let Some((_, DecodedValue::Structure(ts_fields))) =
        fields.iter().find(|(n, _)| n == "timeStamp")
    {
        let secs = ts_fields.iter().find_map(|(n, v)| {
            if n == "secondsPastEpoch" {
                if let DecodedValue::Int64(s) = v {
                    Some(*s)
                } else {
                    None
                }
            } else {
                None
            }
        });
        let nanos = ts_fields.iter().find_map(|(n, v)| {
            if n == "nanoseconds" {
                if let DecodedValue::Int32(s) = v {
                    Some(*s as u32)
                } else {
                    None
                }
            } else {
                None
            }
        });
        if let Some(secs) = secs {
            let unix = choose_unix_epoch_seconds(secs);
            let ns = nanos.unwrap_or(0);
            if let Some(dt) = Utc.timestamp_opt(unix, ns).single() {
                ts = Some(dt.format("%Y-%m-%d %H:%M:%S%.3f").to_string());
            } else {
                ts = Some(format!("{}.{:09}", unix, ns));
            }
        }
    }

    if let Some((_, DecodedValue::Structure(d_fields))) =
        fields.iter().find(|(n, _)| n == "display")
        && let Some((_, DecodedValue::String(u))) = d_fields.iter().find(|(n, _)| n == "units")
        && !u.is_empty()
    {
        units = Some(u.clone());
    }

    (ts, units)
}

fn choose_unix_epoch_seconds(secs: i64) -> i64 {
    // EPICS base uses UNIX seconds for secondsPastEpoch. Some sources still use EPICS epoch.
    // Choose the interpretation closest to "now" to avoid 20-year skew.
    let now = Utc::now().timestamp();
    let epics_unix = secs + 631_152_000; // 1990-01-01 -> 1970-01-01
    let dist_unix = (now - secs).abs();
    let dist_epics = (now - epics_unix).abs();
    if dist_epics < dist_unix {
        epics_unix
    } else {
        secs
    }
}

pub fn extract_alarm(value: &DecodedValue) -> Option<AlarmInfo> {
    let fields = match value {
        DecodedValue::Structure(fields) => fields,
        _ => return None,
    };

    let alarm_fields = match fields.iter().find(|(n, _)| n == "alarm") {
        Some((_, DecodedValue::Structure(a))) => a,
        _ => return None,
    };

    let severity = alarm_fields.iter().find_map(|(n, v)| {
        if n == "severity"
            && let DecodedValue::Int32(s) = v
        {
            return Some(*s);
        }
        None
    })?;

    let status = alarm_fields.iter().find_map(|(n, v)| {
        if n == "status"
            && let DecodedValue::Int32(s) = v
        {
            return Some(*s);
        }
        None
    })?;

    let message = alarm_fields
        .iter()
        .find_map(|(n, v)| {
            if n == "message"
                && let DecodedValue::String(s) = v
            {
                return Some(s.clone());
            }
            None
        })
        .unwrap_or_default();

    Some(AlarmInfo {
        severity,
        status,
        message,
    })
}

pub fn severity_label(sev: i32) -> &'static str {
    match sev {
        0 => "OK",
        1 => "MINOR",
        2 => "MAJOR",
        3 => "INVALID",
        _ => "UNKNOWN",
    }
}

pub fn format_alarm(alarm: &AlarmInfo) -> String {
    let sev = severity_label(alarm.severity);
    let status_name = status_label(alarm.status);
    if !alarm.message.is_empty() {
        format!(
            "alarm={} status={}({}) msg={}",
            sev, status_name, alarm.status, alarm.message
        )
    } else {
        format!("alarm={} status={}({})", sev, status_name, alarm.status)
    }
}

pub fn status_label(code: i32) -> &'static str {
    match code {
        0 => "NO_ALARM",
        1 => "READ",
        2 => "WRITE",
        3 => "HIHI",
        4 => "HIGH",
        5 => "LOLO",
        6 => "LOW",
        7 => "STATE",
        8 => "COS",
        9 => "COMM",
        10 => "CALC",
        11 => "SCAN",
        12 => "LINK",
        13 => "SOFT",
        14 => "BAD_SUB",
        15 => "UDF",
        16 => "DISABLE",
        17 => "SIMM",
        18 => "READ_ACCESS",
        19 => "WRITE_ACCESS",
        20 => "HWLIMIT",
        21 => "TIMEOUT",
        _ => "UNKNOWN",
    }
}

fn format_value(value: &DecodedValue) -> String {
    match value {
        DecodedValue::Float32(v) => trim_float(format!("{:.6}", v)),
        DecodedValue::Float64(v) => trim_float(format!("{:.6}", v)),
        DecodedValue::String(s) => s.clone(),
        _ => value.to_string(),
    }
}

fn trim_float(mut s: String) -> String {
    if let Some(dot) = s.find('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
        if s.is_empty() || s == "-" {
            s = "0".to_string();
        } else if dot >= s.len() {
            return s;
        }
    }
    s
}

fn format_value_with_units(value: &DecodedValue, units: Option<&str>) -> String {
    let base = format_value(value);
    if let Some(u) = units
        && !u.is_empty()
    {
        return format!("{} {}", base, u);
    }
    base
}

fn alarm_is_normal(alarm: &AlarmInfo) -> bool {
    alarm.severity == 0 && alarm.status == 0 && alarm.message.is_empty()
}

fn alarm_tokens(alarm: &AlarmInfo, include_status: bool) -> Vec<String> {
    let mut tokens = Vec::new();
    tokens.push(severity_label(alarm.severity).to_string());
    if include_status && alarm.status != 0 {
        tokens.push(status_label(alarm.status).to_string());
    }
    if !alarm.message.is_empty() && alarm.message != status_label(alarm.status) {
        tokens.push(alarm.message.clone());
    }
    tokens
}

pub fn format_output(pv: &str, value: &DecodedValue, opts: &RenderOptions) -> String {
    match opts.format {
        OutputFormat::Json => format_json_output(pv, value, opts),
        OutputFormat::Text => format_text_output(pv, value, opts),
    }
}

fn format_text_output(pv: &str, value: &DecodedValue, opts: &RenderOptions) -> String {
    if opts.multiline
        && let Some(table) = format_table_output(pv, value)
    {
        return table;
    }

    let (ts, units) = extract_ts_units(value);
    let alarm = extract_alarm(value);
    let scalar = extract_nt_scalar_value(value).unwrap_or(value);
    let units_ref = if opts.include_units {
        units.as_deref()
    } else {
        None
    };
    let val_str = format_value_with_units(scalar, units_ref);

    let mut parts: Vec<String> = Vec::new();
    parts.push(pv.to_string());
    if opts.include_timestamp
        && let Some(ts) = ts
    {
        parts.push(ts);
    }
    parts.push(format!("{:>3}", val_str));

    if opts.include_alarm
        && let Some(alarm) = alarm
        && !alarm_is_normal(&alarm)
    {
        parts.extend(alarm_tokens(&alarm, true));
    }

    parts.join(" ")
}

fn format_json_output(pv: &str, value: &DecodedValue, opts: &RenderOptions) -> String {
    let (ts, units) = extract_ts_units(value);
    let alarm = if opts.include_alarm {
        extract_alarm(value).map(|a| format_alarm(&a))
    } else {
        None
    };
    let obj = json!({
        "pv": pv,
        "value": format_compact_value(value),
        "timestamp": if opts.include_timestamp { ts } else { None },
        "units": if opts.include_units { units } else { None },
        "alarm": alarm,
    });
    obj.to_string()
}

fn format_table_output(pv: &str, value: &DecodedValue) -> Option<String> {
    let fields = match value {
        DecodedValue::Structure(fields) => fields,
        _ => return None,
    };

    let value_fields = fields.iter().find_map(|(name, val)| {
        if name == "value"
            && let DecodedValue::Structure(cols) = val
        {
            return Some(cols);
        }
        None
    })?;

    let name_col = value_fields.iter().find_map(|(name, val)| {
        if name == "name" {
            return array_to_strings(val);
        }
        None
    })?;

    if name_col.is_empty() {
        return None;
    }

    let mut columns: Vec<(String, Vec<String>)> = Vec::new();
    for (name, val) in value_fields {
        if name == "name" {
            continue;
        }
        if let Some(col) = array_to_strings(val) {
            columns.push((name.clone(), col));
        }
    }

    if columns.is_empty() {
        return None;
    }

    let row_count = name_col.len();
    for (_, col) in &columns {
        if col.len() < row_count {
            return None;
        }
    }

    let descriptor = fields
        .iter()
        .find_map(|(name, val)| {
            if name == "descriptor"
                && let DecodedValue::String(s) = val
            {
                return Some(s.clone());
            }
            None
        })
        .or_else(|| {
            fields.iter().find_map(|(name, val)| {
                if name == "display"
                    && let DecodedValue::Structure(d_fields) = val
                {
                    return d_fields.iter().find_map(|(n, v)| {
                        if n == "description"
                            && let DecodedValue::String(s) = v
                        {
                            return Some(s.clone());
                        }
                        None
                    });
                }
                None
            })
        });

    let labels = fields.iter().find_map(|(name, val)| {
        if name == "labels" {
            return array_to_strings(val);
        }
        None
    });

    let (ts, _units) = extract_ts_units(value);
    let mut lines: Vec<String> = Vec::new();
    let header = if let Some(ts) = ts {
        format!("{} {}", pv, ts)
    } else {
        pv.to_string()
    };
    lines.push(header);
    if let Some(desc) = descriptor
        && !desc.is_empty()
    {
        lines.push(format!("     PV \"{}\"", desc));
    }

    let name_width = std::cmp::max(16, name_col.iter().map(|s| s.len()).max().unwrap_or(0));

    let (name_label, col_labels): (String, Vec<String>) = match labels {
        Some(l) if l.len() == columns.len() + 1 => (l[0].clone(), l[1..].to_vec()),
        Some(l) if l.len() == columns.len() => ("PV".to_string(), l),
        _ => (
            "PV".to_string(),
            columns.iter().map(|(n, _)| n.clone()).collect(),
        ),
    };

    let mut header = format!("{:<width$}", name_label, width = name_width);
    for (idx, _) in columns.iter().enumerate() {
        header.push(' ');
        if let Some(label) = col_labels.get(idx) {
            header.push_str(label);
        }
    }
    lines.push(header);
    for idx in 0..row_count {
        let mut line = format!("{:<width$}", name_col[idx], width = name_width);
        for (_, col) in &columns {
            line.push(' ');
            line.push_str(&col[idx]);
        }
        lines.push(line);
    }

    Some(lines.join("\n"))
}

fn array_to_strings(val: &DecodedValue) -> Option<Vec<String>> {
    match val {
        DecodedValue::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(format_value(item));
            }
            Some(out)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spvirit_codec::spvd_decode::DecodedValue;

    #[test]
    fn test_extract_ts_units() {
        let value = DecodedValue::Structure(vec![
            (
                "timeStamp".to_string(),
                DecodedValue::Structure(vec![
                    ("secondsPastEpoch".to_string(), DecodedValue::Int64(0)),
                    ("nanoseconds".to_string(), DecodedValue::Int32(0)),
                ]),
            ),
            (
                "display".to_string(),
                DecodedValue::Structure(vec![(
                    "units".to_string(),
                    DecodedValue::String("counts".to_string()),
                )]),
            ),
        ]);

        let (ts, units) = extract_ts_units(&value);
        assert!(ts.is_some());
        assert_eq!(units.as_deref(), Some("counts"));
    }

    #[test]
    fn test_extract_alarm() {
        let value = DecodedValue::Structure(vec![(
            "alarm".to_string(),
            DecodedValue::Structure(vec![
                ("severity".to_string(), DecodedValue::Int32(2)),
                ("status".to_string(), DecodedValue::Int32(7)),
                (
                    "message".to_string(),
                    DecodedValue::String("HIHI".to_string()),
                ),
            ]),
        )]);

        let alarm = extract_alarm(&value).expect("alarm");
        assert_eq!(alarm.severity, 2);
        assert_eq!(alarm.status, 7);
        assert_eq!(alarm.message, "HIHI");
        assert!(format_alarm(&alarm).contains("MAJOR"));
        assert!(format_alarm(&alarm).contains("STATE(7)"));
    }

    #[test]
    fn test_status_label() {
        assert_eq!(status_label(0), "NO_ALARM");
        assert_eq!(status_label(3), "HIHI");
        assert_eq!(status_label(99), "UNKNOWN");
    }
}
