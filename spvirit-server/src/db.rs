use std::collections::HashMap;
use std::fs;
use std::time::Duration;

use regex::Regex;

use crate::types::{
    DbCommonState, LinkExpr, NtScalar, NtScalarArray, OutputMode, RecordData, RecordInstance,
    RecordType, ScalarArrayValue, ScalarValue, ScanMode,
};

#[derive(Debug, Clone)]
pub struct DbRecord {
    pub name: String,
    pub record_type: String,
    pub fields: HashMap<String, String>,
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn parse_f64(value: &str) -> Option<f64> {
    value.trim().parse::<f64>().ok()
}

fn parse_i32(value: &str) -> Option<i32> {
    value.trim().parse::<i32>().ok()
}

fn parse_usize(value: &str) -> Option<usize> {
    value.trim().parse::<usize>().ok()
}

fn parse_link_expr(raw: &str) -> Option<LinkExpr> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.is_empty() {
        return None;
    }

    let mut process_passive = false;
    let mut maximize_severity = false;
    let mut only_link_opts = parts.len() > 1;
    for opt in parts.iter().skip(1) {
        match opt.to_ascii_uppercase().as_str() {
            "PP" => process_passive = true,
            "NPP" => {}
            "MS" | "MSS" | "MSI" => maximize_severity = true,
            "NMS" => {}
            _ => only_link_opts = false,
        }
    }
    if only_link_opts {
        return Some(LinkExpr::DbLink {
            target: parts[0].to_string(),
            process_passive,
            maximize_severity,
        });
    }

    if parts.len() == 1 {
        if let Some(v) = parse_bool(trimmed) {
            return Some(LinkExpr::Constant(ScalarValue::Bool(v)));
        }
        if let Some(v) = parse_i32(trimmed) {
            return Some(LinkExpr::Constant(ScalarValue::I32(v)));
        }
        if let Some(v) = parse_f64(trimmed) {
            return Some(LinkExpr::Constant(ScalarValue::F64(v)));
        }
        return Some(LinkExpr::DbLink {
            target: trimmed.to_string(),
            process_passive: false,
            maximize_severity: false,
        });
    }

    Some(LinkExpr::DbLink {
        target: trimmed.to_string(),
        process_passive: false,
        maximize_severity: false,
    })
}

fn parse_scan_period(raw: &str) -> Option<Duration> {
    let first = raw.split_whitespace().next()?;
    let secs = first.parse::<f64>().ok()?;
    if secs > 0.0 {
        Some(Duration::from_secs_f64(secs))
    } else {
        None
    }
}

fn parse_scan_mode(record_name: &str, fields: &HashMap<String, String>) -> ScanMode {
    let raw = fields
        .get("SCAN")
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .unwrap_or("Passive");
    let lowered = raw.to_ascii_lowercase();
    if lowered == "passive" {
        return ScanMode::Passive;
    }
    if lowered.contains("i/o") || lowered.contains("io intr") {
        let source = fields
            .get("IOSCAN")
            .cloned()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| record_name.to_string());
        return ScanMode::IoEvent(source);
    }
    if lowered.starts_with("event") {
        let source = fields
            .get("EVNT")
            .cloned()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| raw.split_whitespace().nth(1).map(|v| v.to_string()))
            .unwrap_or_else(|| record_name.to_string());
        return ScanMode::Event(source);
    }
    if let Some(period) = parse_scan_period(raw) {
        return ScanMode::Periodic(period);
    }
    ScanMode::Passive
}

fn parse_output_mode(value: Option<&String>) -> OutputMode {
    let lowered = value
        .map(|v| v.trim().to_ascii_lowercase())
        .unwrap_or_else(|| "supervisory".to_string());
    if lowered.contains("closed") {
        OutputMode::ClosedLoop
    } else {
        OutputMode::Supervisory
    }
}

fn split_array_tokens(raw: &str) -> Vec<&str> {
    raw.split(|c: char| c == ',' || c.is_whitespace())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_scalar_array(raw: Option<&String>, ftvl: &str, nelm: Option<usize>) -> ScalarArrayValue {
    let tokens = raw.map_or_else(Vec::new, |v| split_array_tokens(v));
    let cap = nelm.unwrap_or(tokens.len());
    let count = if cap == 0 { tokens.len() } else { cap };
    let type_name = ftvl.trim().to_ascii_uppercase();

    let parse_bool_vec = || -> Vec<bool> {
        let mut out = Vec::new();
        for tok in &tokens {
            let lowered = tok.to_ascii_lowercase();
            let val = matches!(lowered.as_str(), "1" | "true" | "yes" | "on");
            out.push(val);
        }
        out
    };
    let parse_i8_vec = || -> Vec<i8> {
        let mut out = Vec::new();
        for tok in &tokens {
            if let Ok(v) = tok.parse::<i8>() {
                out.push(v);
            }
        }
        out
    };
    let parse_i16_vec = || -> Vec<i16> {
        let mut out = Vec::new();
        for tok in &tokens {
            if let Ok(v) = tok.parse::<i16>() {
                out.push(v);
            }
        }
        out
    };
    let parse_i32_vec = || -> Vec<i32> {
        let mut out = Vec::new();
        for tok in &tokens {
            if let Ok(v) = tok.parse::<i32>() {
                out.push(v);
            }
        }
        out
    };
    let parse_i64_vec = || -> Vec<i64> {
        let mut out = Vec::new();
        for tok in &tokens {
            if let Ok(v) = tok.parse::<i64>() {
                out.push(v);
            }
        }
        out
    };
    let parse_u8_vec = || -> Vec<u8> {
        let mut out = Vec::new();
        for tok in &tokens {
            if let Ok(v) = tok.parse::<u8>() {
                out.push(v);
            }
        }
        out
    };
    let parse_u16_vec = || -> Vec<u16> {
        let mut out = Vec::new();
        for tok in &tokens {
            if let Ok(v) = tok.parse::<u16>() {
                out.push(v);
            }
        }
        out
    };
    let parse_u32_vec = || -> Vec<u32> {
        let mut out = Vec::new();
        for tok in &tokens {
            if let Ok(v) = tok.parse::<u32>() {
                out.push(v);
            }
        }
        out
    };
    let parse_u64_vec = || -> Vec<u64> {
        let mut out = Vec::new();
        for tok in &tokens {
            if let Ok(v) = tok.parse::<u64>() {
                out.push(v);
            }
        }
        out
    };
    let parse_f32_vec = || -> Vec<f32> {
        let mut out = Vec::new();
        for tok in &tokens {
            if let Ok(v) = tok.parse::<f32>() {
                out.push(v);
            }
        }
        out
    };
    let parse_f64_vec = || -> Vec<f64> {
        let mut out = Vec::new();
        for tok in &tokens {
            if let Ok(v) = tok.parse::<f64>() {
                out.push(v);
            }
        }
        out
    };

    let mut parsed = match type_name.as_str() {
        "BOOL" | "BOOLEAN" => ScalarArrayValue::Bool(parse_bool_vec()),
        "CHAR" | "INT8" => ScalarArrayValue::I8(parse_i8_vec()),
        "SHORT" | "INT16" => ScalarArrayValue::I16(parse_i16_vec()),
        "LONG" | "INT" | "INT32" => ScalarArrayValue::I32(parse_i32_vec()),
        "INT64" => ScalarArrayValue::I64(parse_i64_vec()),
        "UCHAR" | "UINT8" => ScalarArrayValue::U8(parse_u8_vec()),
        "USHORT" | "UINT16" => ScalarArrayValue::U16(parse_u16_vec()),
        "ULONG" | "UINT32" => ScalarArrayValue::U32(parse_u32_vec()),
        "UINT64" => ScalarArrayValue::U64(parse_u64_vec()),
        "FLOAT" | "FLOAT32" => ScalarArrayValue::F32(parse_f32_vec()),
        "STRING" => ScalarArrayValue::Str(raw.map_or_else(Vec::new, |v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })),
        _ => ScalarArrayValue::F64(parse_f64_vec()),
    };

    if count > 0 {
        match &mut parsed {
            ScalarArrayValue::Bool(v) => v.truncate(count),
            ScalarArrayValue::I8(v) => v.truncate(count),
            ScalarArrayValue::I16(v) => v.truncate(count),
            ScalarArrayValue::I32(v) => v.truncate(count),
            ScalarArrayValue::I64(v) => v.truncate(count),
            ScalarArrayValue::U8(v) => v.truncate(count),
            ScalarArrayValue::U16(v) => v.truncate(count),
            ScalarArrayValue::U32(v) => v.truncate(count),
            ScalarArrayValue::U64(v) => v.truncate(count),
            ScalarArrayValue::F32(v) => v.truncate(count),
            ScalarArrayValue::F64(v) => v.truncate(count),
            ScalarArrayValue::Str(v) => v.truncate(count),
        }
    }

    parsed
}

fn parse_simm(fields: &HashMap<String, String>) -> bool {
    let Some(raw) = fields.get("SIMM") else {
        return false;
    };
    let lowered = raw.trim().to_ascii_lowercase();
    match lowered.as_str() {
        "yes" | "true" | "on" | "1" | "raw" | "2" => true,
        "no" | "false" | "off" | "0" => false,
        _ => false,
    }
}

fn parse_ntscalar(record: &DbRecord) -> Option<NtScalar> {
    let rtype = RecordType::from_db_name(&record.record_type)?;
    let fields = &record.fields;
    let description = fields.get("DESC").cloned().unwrap_or_default();

    let nt = match rtype {
        RecordType::Ai | RecordType::Ao => {
            let val = fields.get("VAL").and_then(|v| parse_f64(v)).unwrap_or(0.0);
            NtScalar::from_value(ScalarValue::F64(val))
        }
        RecordType::Bi | RecordType::Bo => {
            let val = fields
                .get("VAL")
                .and_then(|v| parse_bool(v))
                .unwrap_or(false);
            NtScalar::from_value(ScalarValue::Bool(val))
        }
        RecordType::StringIn | RecordType::StringOut => {
            let val = fields.get("VAL").cloned().unwrap_or_default();
            NtScalar::from_value(ScalarValue::Str(val))
        }
        _ => return None,
    };

    let nt = nt.with_description(description);

    // EGU, HOPR, LOPR, PREC, and alarm limits (HIHI/HIGH/LOW/LOLO) are only
    // valid for analog record types (ai, ao) per EPICS Base specification.
    // bi/bo use ZNAM/ONAM/ZSV/OSV for state alarms; stringin/stringout have
    // no display or alarm limit fields.
    let nt = match rtype {
        RecordType::Ai | RecordType::Ao => {
            let units = fields.get("EGU").cloned().unwrap_or_default();
            let precision = fields
                .get("PREC")
                .and_then(|v| v.trim().parse::<i32>().ok())
                .unwrap_or(0);
            let low = fields.get("LOPR").and_then(|v| parse_f64(v)).unwrap_or(0.0);
            let high = fields.get("HOPR").and_then(|v| parse_f64(v)).unwrap_or(0.0);
            let alarm_low = fields.get("LOW").and_then(|v| parse_f64(v));
            let alarm_high = fields.get("HIGH").and_then(|v| parse_f64(v));
            let alarm_lolo = fields.get("LOLO").and_then(|v| parse_f64(v));
            let alarm_hihi = fields.get("HIHI").and_then(|v| parse_f64(v));
            nt.with_limits(low, high)
                .with_units(units)
                .with_precision(precision)
                .with_alarm_limits(alarm_low, alarm_high, alarm_lolo, alarm_hihi)
        }
        _ => nt,
    };

    Some(nt)
}

fn to_record(record: &DbRecord) -> Option<RecordInstance> {
    let record_type = RecordType::from_db_name(&record.record_type)?;
    let fields = &record.fields;

    let common = DbCommonState {
        desc: fields.get("DESC").cloned().unwrap_or_default(),
        scan: parse_scan_mode(&record.name, fields),
        pini: fields
            .get("PINI")
            .and_then(|v| parse_bool(v))
            .unwrap_or(false),
        phas: fields.get("PHAS").and_then(|v| parse_i32(v)).unwrap_or(0),
        pact: false,
        disa: fields
            .get("DISA")
            .and_then(|v| parse_bool(v))
            .unwrap_or(false),
        sdis: fields.get("SDIS").and_then(|v| parse_link_expr(v)),
        diss: fields.get("DISS").and_then(|v| parse_i32(v)).unwrap_or(0),
        flnk: fields.get("FLNK").and_then(|v| parse_link_expr(v)),
    };

    let simm = parse_simm(fields);
    let siml = fields.get("SIML").and_then(|v| parse_link_expr(v));
    let siol = fields.get("SIOL").and_then(|v| parse_link_expr(v));

    let data = match record_type {
        RecordType::Ai => RecordData::Ai {
            nt: parse_ntscalar(record)?,
            inp: fields.get("INP").and_then(|v| parse_link_expr(v)),
            siml,
            siol,
            simm,
        },
        RecordType::Ao => RecordData::Ao {
            nt: parse_ntscalar(record)?,
            out: fields.get("OUT").and_then(|v| parse_link_expr(v)),
            dol: fields.get("DOL").and_then(|v| parse_link_expr(v)),
            omsl: parse_output_mode(fields.get("OMSL")),
            drvl: fields.get("DRVL").and_then(|v| parse_f64(v)),
            drvh: fields.get("DRVH").and_then(|v| parse_f64(v)),
            oroc: fields.get("OROC").and_then(|v| parse_f64(v)),
            siml,
            siol,
            simm,
        },
        RecordType::Bi => RecordData::Bi {
            nt: parse_ntscalar(record)?,
            inp: fields.get("INP").and_then(|v| parse_link_expr(v)),
            znam: fields
                .get("ZNAM")
                .cloned()
                .unwrap_or_else(|| "OFF".to_string()),
            onam: fields
                .get("ONAM")
                .cloned()
                .unwrap_or_else(|| "ON".to_string()),
            siml,
            siol,
            simm,
        },
        RecordType::Bo => RecordData::Bo {
            nt: parse_ntscalar(record)?,
            out: fields.get("OUT").and_then(|v| parse_link_expr(v)),
            dol: fields.get("DOL").and_then(|v| parse_link_expr(v)),
            omsl: parse_output_mode(fields.get("OMSL")),
            znam: fields
                .get("ZNAM")
                .cloned()
                .unwrap_or_else(|| "OFF".to_string()),
            onam: fields
                .get("ONAM")
                .cloned()
                .unwrap_or_else(|| "ON".to_string()),
            siml,
            siol,
            simm,
        },
        RecordType::StringIn => RecordData::StringIn {
            nt: parse_ntscalar(record)?,
            inp: fields.get("INP").and_then(|v| parse_link_expr(v)),
            siml,
            siol,
            simm,
        },
        RecordType::StringOut => RecordData::StringOut {
            nt: parse_ntscalar(record)?,
            out: fields.get("OUT").and_then(|v| parse_link_expr(v)),
            dol: fields.get("DOL").and_then(|v| parse_link_expr(v)),
            omsl: parse_output_mode(fields.get("OMSL")),
            siml,
            siol,
            simm,
        },
        RecordType::Waveform => {
            let ftvl = fields
                .get("FTVL")
                .cloned()
                .unwrap_or_else(|| "DOUBLE".to_string());
            let nelm = fields.get("NELM").and_then(|v| parse_usize(v));
            let array = parse_scalar_array(fields.get("VAL"), &ftvl, nelm);
            RecordData::Waveform {
                nt: NtScalarArray::from_value(array),
                inp: fields.get("INP").and_then(|v| parse_link_expr(v)),
                ftvl,
                nelm: nelm.unwrap_or(0),
                nord: fields
                    .get("NORD")
                    .and_then(|v| parse_usize(v))
                    .unwrap_or_else(|| {
                        fields.get("NELM").and_then(|v| parse_usize(v)).unwrap_or(0)
                    }),
            }
        }
        RecordType::Aai => {
            let ftvl = fields
                .get("FTVL")
                .cloned()
                .unwrap_or_else(|| "DOUBLE".to_string());
            let nelm = fields.get("NELM").and_then(|v| parse_usize(v));
            let array = parse_scalar_array(fields.get("VAL"), &ftvl, nelm);
            RecordData::Aai {
                nt: NtScalarArray::from_value(array),
                inp: fields.get("INP").and_then(|v| parse_link_expr(v)),
                ftvl,
                nelm: nelm.unwrap_or(0),
                nord: fields
                    .get("NORD")
                    .and_then(|v| parse_usize(v))
                    .unwrap_or_else(|| {
                        fields.get("NELM").and_then(|v| parse_usize(v)).unwrap_or(0)
                    }),
            }
        }
        RecordType::Aao => {
            let ftvl = fields
                .get("FTVL")
                .cloned()
                .unwrap_or_else(|| "DOUBLE".to_string());
            let nelm = fields.get("NELM").and_then(|v| parse_usize(v));
            let array = parse_scalar_array(fields.get("VAL"), &ftvl, nelm);
            RecordData::Aao {
                nt: NtScalarArray::from_value(array),
                out: fields.get("OUT").and_then(|v| parse_link_expr(v)),
                dol: fields.get("DOL").and_then(|v| parse_link_expr(v)),
                omsl: parse_output_mode(fields.get("OMSL")),
                ftvl,
                nelm: nelm.unwrap_or(0),
                nord: fields
                    .get("NORD")
                    .and_then(|v| parse_usize(v))
                    .unwrap_or_else(|| {
                        fields.get("NELM").and_then(|v| parse_usize(v)).unwrap_or(0)
                    }),
            }
        }
        RecordType::SubArray => {
            let ftvl = fields
                .get("FTVL")
                .cloned()
                .unwrap_or_else(|| "DOUBLE".to_string());
            let nelm = fields.get("NELM").and_then(|v| parse_usize(v));
            let array = parse_scalar_array(fields.get("VAL"), &ftvl, nelm);
            RecordData::SubArray {
                nt: NtScalarArray::from_value(array),
                inp: fields.get("INP").and_then(|v| parse_link_expr(v)),
                ftvl,
                malm: fields.get("MALM").and_then(|v| parse_usize(v)).unwrap_or(0),
                nelm: nelm.unwrap_or(0),
                nord: fields
                    .get("NORD")
                    .and_then(|v| parse_usize(v))
                    .unwrap_or_else(|| {
                        fields.get("NELM").and_then(|v| parse_usize(v)).unwrap_or(0)
                    }),
                indx: fields.get("INDX").and_then(|v| parse_usize(v)).unwrap_or(0),
            }
        }
        // TODO(follow-up): .db parsing — from_db_name never yields LongIn/
        // LongOut today, so these arms are unreachable; they're only here to
        // satisfy exhaustiveness. Wiring longin/longout .db loading is
        // tracked as follow-up work.
        RecordType::NtTable
        | RecordType::NtNdArray
        | RecordType::Mbbi
        | RecordType::Mbbo
        | RecordType::Generic
        | RecordType::LongIn
        | RecordType::LongOut => {
            eprintln!(
                "Record '{}': type '{}' is not a standard EPICS Base record type and cannot be loaded from .db files",
                record.name, record.record_type
            );
            return None;
        }
    };

    Some(RecordInstance {
        name: record.name.clone(),
        record_type,
        common,
        data,
        raw_fields: record.fields.clone(),
    })
}

/// A `.db` parse failure, naming the file and the line that caused it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbParseError {
    pub path: String,
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for DbParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.path, self.line, self.message)
    }
}

impl std::error::Error for DbParseError {}

/// Expand `$(NAME)` and `${NAME}` from `macros`.
///
/// An undefined macro is an error: silently expanding to the empty string is
/// how a typo becomes a record named `""` that no client can ever find.
fn expand_macros(
    raw: &str,
    macros: &HashMap<String, String>,
    path: &str,
    line: usize,
) -> Result<String, DbParseError> {
    let mut out = String::with_capacity(raw.len());
    let bytes: Vec<char> = raw.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '$' && i + 1 < bytes.len() && (bytes[i + 1] == '(' || bytes[i + 1] == '{') {
            let close = if bytes[i + 1] == '(' { ')' } else { '}' };
            let start = i + 2;
            let Some(end) = (start..bytes.len()).find(|&j| bytes[j] == close) else {
                return Err(DbParseError {
                    path: path.to_string(),
                    line,
                    message: format!("unterminated macro reference starting at column {}", i + 1),
                });
            };
            let name: String = bytes[start..end].iter().collect();
            let Some(value) = macros.get(&name) else {
                return Err(DbParseError {
                    path: path.to_string(),
                    line,
                    message: format!("undefined macro '{name}'"),
                });
            };
            out.push_str(value);
            i = end + 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Ok(out)
}

/// Join physical lines into logical ones so a quoted value may span newlines.
///
/// Also strips trailing inline comments: an unquoted `#` runs to the end of
/// the physical line. A `#` inside a quoted value is not a comment marker
/// and is preserved untouched.
///
/// Returns `(logical_line_text, first_physical_line_number)` pairs.
fn logical_lines(content: &str, path: &str) -> Result<Vec<(String, usize)>, DbParseError> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut start_line = 0usize;
    let mut in_quote = false;

    for (idx, physical) in content.lines().enumerate() {
        let lineno = idx + 1;
        if !in_quote {
            start_line = lineno;
            buf.clear();
        } else {
            buf.push('\n');
        }
        for ch in physical.chars() {
            if !in_quote && ch == '#' {
                break;
            }
            if ch == '"' {
                in_quote = !in_quote;
            }
            buf.push(ch);
        }
        if in_quote {
            continue;
        }
        out.push((buf.clone(), start_line));
    }

    if in_quote {
        return Err(DbParseError {
            path: path.to_string(),
            line: start_line,
            message: "unterminated quoted value".to_string(),
        });
    }
    Ok(out)
}

/// Parse `content` into untyped records, expanding `include` and macros.
///
/// `path` is used only for error messages and for resolving relative
/// `include` paths; it need not exist on disk.
pub fn parse_db_records(
    content: &str,
    path: &str,
    macros: &HashMap<String, String>,
) -> Result<Vec<DbRecord>, DbParseError> {
    let mut visiting = Vec::new();
    parse_db_inner(content, path, macros, &mut visiting)
}

/// Read `path` and parse it. `include` targets resolve relative to the
/// including file's directory, as EPICS `dbLoadRecords` does.
pub fn load_db_records(
    path: &str,
    macros: &HashMap<String, String>,
) -> Result<Vec<DbRecord>, DbParseError> {
    let mut visiting = Vec::new();
    load_db_inner(path, macros, &mut visiting)
}

fn load_db_inner(
    path: &str,
    macros: &HashMap<String, String>,
    visiting: &mut Vec<String>,
) -> Result<Vec<DbRecord>, DbParseError> {
    let canonical = fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string());
    if visiting.contains(&canonical) {
        return Err(DbParseError {
            path: path.to_string(),
            line: 0,
            message: format!("include cycle: '{path}' is already being parsed"),
        });
    }
    let content = fs::read_to_string(path).map_err(|e| DbParseError {
        path: path.to_string(),
        line: 0,
        message: e.to_string(),
    })?;
    visiting.push(canonical);
    let result = parse_db_inner(&content, path, macros, visiting);
    visiting.pop();
    result
}

fn parse_db_inner(
    content: &str,
    path: &str,
    macros: &HashMap<String, String>,
    visiting: &mut Vec<String>,
) -> Result<Vec<DbRecord>, DbParseError> {
    let record_re = Regex::new(r#"^record\s*\(\s*([A-Za-z0-9_]+)\s*,\s*"([^"]*)"\s*\)\s*\{?$"#)
        .expect("static record regex");
    let field_re = Regex::new(r#"^field\s*\(\s*([A-Za-z0-9_]+)\s*,\s*"([\s\S]*)"\s*\)$"#)
        .expect("static field regex");
    let include_re = Regex::new(r#"^include\s+"?([^"]+)"?$"#).expect("static include regex");

    let mut records: Vec<DbRecord> = Vec::new();
    let mut current: Option<DbRecord> = None;

    for (raw_line, lineno) in logical_lines(content, path)? {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some(caps) = include_re.captures(line) {
            let target = expand_macros(&caps[1], macros, path, lineno)?;
            let base = std::path::Path::new(path)
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."));
            let resolved = base.join(&target);
            let nested = load_db_inner(resolved.to_str().unwrap_or(&target), macros, visiting)?;
            records.extend(nested);
            continue;
        }

        let line = expand_macros(line, macros, path, lineno)?;
        let line = line.trim();

        if let Some(caps) = record_re.captures(line) {
            let name = caps[2].to_string();
            if name.is_empty() {
                return Err(DbParseError {
                    path: path.to_string(),
                    line: lineno,
                    message: "record name must not be empty".to_string(),
                });
            }
            if let Some(rec) = current.take() {
                records.push(rec);
            }
            current = Some(DbRecord {
                name,
                record_type: caps[1].to_string(),
                fields: HashMap::new(),
            });
            continue;
        }
        if line == "{" {
            continue;
        }
        if line.starts_with('}') {
            if let Some(rec) = current.take() {
                records.push(rec);
            }
            continue;
        }
        if let Some(caps) = field_re.captures(line) {
            let Some(rec) = current.as_mut() else {
                return Err(DbParseError {
                    path: path.to_string(),
                    line: lineno,
                    message: "field() outside any record() block".to_string(),
                });
            };
            rec.fields.insert(caps[1].to_string(), caps[2].to_string());
            continue;
        }

        return Err(DbParseError {
            path: path.to_string(),
            line: lineno,
            message: format!("unrecognised line: {line}"),
        });
    }

    if let Some(rec) = current.take() {
        records.push(rec);
    }
    Ok(records)
}

pub fn load_db(path: &str) -> Result<HashMap<String, RecordInstance>, String> {
    let raw = load_db_records(path, &HashMap::new()).map_err(|e| e.to_string())?;
    typed_from_raw(&raw)
}

pub fn parse_db(content: &str) -> Result<HashMap<String, RecordInstance>, String> {
    let raw = parse_db_records(content, "<memory>", &HashMap::new()).map_err(|e| e.to_string())?;
    typed_from_raw(&raw)
}

/// Build the `SimplePvStore` typed model. A record the typed layer cannot
/// represent is now an error rather than a silent omission.
fn typed_from_raw(raw: &[DbRecord]) -> Result<HashMap<String, RecordInstance>, String> {
    let mut map = HashMap::new();
    for rec in raw {
        let Some(parsed) = to_record(rec) else {
            return Err(format!(
                "record '{}' of type '{}' is not supported",
                rec.name, rec.record_type
            ));
        };
        map.insert(parsed.name.clone(), parsed);
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_supported_records() {
        let input = r#"
            record(ai, "PV:AI") {
                field(VAL, "1.25")
                field(EGU, "mA")
                field(HOPR, "10")
                field(LOPR, "-10")
                field(SIMM, "RAW")
                field(INP, "PV:RAW PP MS")
            }
            record(ao, "PV:AO") {
                field(VAL, "2")
                field(OMSL, "closed_loop")
                field(DOL, "PV:SET NPP NMS")
                field(OUT, "PV:RAW")
            }
            record(bi, "PV:BI") {
                field(VAL, "1")
            }
            record(bo, "PV:BO") {
                field(VAL, "0")
            }
            record(stringin, "PV:STRIN") {
                field(VAL, "hello")
            }
            record(stringout, "PV:STROUT") {
                field(VAL, "world")
            }
        "#;
        let map = parse_db(input).expect("parse");
        assert!(map.contains_key("PV:AI"));
        assert!(map.contains_key("PV:AO"));
        assert!(map.contains_key("PV:BI"));
        assert!(map.contains_key("PV:BO"));
        assert!(map.contains_key("PV:STRIN"));
        assert!(map.contains_key("PV:STROUT"));

        let ai = map.get("PV:AI").unwrap();
        assert_eq!(ai.record_type, RecordType::Ai);
        match &ai.data {
            RecordData::Ai { inp, simm, .. } => {
                assert!(*simm);
                match inp {
                    Some(LinkExpr::DbLink {
                        target,
                        process_passive,
                        maximize_severity,
                    }) => {
                        assert_eq!(target, "PV:RAW");
                        assert!(*process_passive);
                        assert!(*maximize_severity);
                    }
                    _ => panic!("expected ai inp db link"),
                }
            }
            _ => panic!("expected ai data"),
        }
    }

    #[test]
    fn parse_scan_modes() {
        let input = r#"
            record(ai, "PV:PERIODIC") {
                field(SCAN, "0.5 second")
            }
            record(ai, "PV:EVENT") {
                field(SCAN, "Event")
                field(EVNT, "MY_EVT")
            }
            record(ai, "PV:IO") {
                field(SCAN, "I/O Intr")
                field(IOSCAN, "ADC0")
            }
        "#;
        let map = parse_db(input).expect("parse");
        let periodic = map.get("PV:PERIODIC").unwrap();
        assert!(matches!(periodic.common.scan, ScanMode::Periodic(_)));
        let event = map.get("PV:EVENT").unwrap();
        assert_eq!(event.common.scan, ScanMode::Event("MY_EVT".to_string()));
        let io = map.get("PV:IO").unwrap();
        assert_eq!(io.common.scan, ScanMode::IoEvent("ADC0".to_string()));
    }

    #[test]
    fn parse_error_names_the_file_and_line() {
        let input = "record(ai, \"PV:A\") {\n    field(VAL \"1\")\n}\n";
        let err = parse_db_records(input, "bad.db", &HashMap::new())
            .expect_err("a malformed field line must abort the load");
        assert_eq!(err.path, "bad.db");
        assert_eq!(err.line, 2, "the error must name the offending line");
        assert!(
            err.to_string().contains("bad.db:2"),
            "Display must render path:line, got {err}"
        );
    }

    #[test]
    fn unknown_record_type_is_no_longer_silently_dropped() {
        let input = "record(nosuchtype, \"PV:X\") {\n    field(VAL, \"1\")\n}\n";
        // parse_db_records is untyped and keeps it; to_record is what rejects.
        let raw = parse_db_records(input, "x.db", &HashMap::new()).expect("raw parse succeeds");
        assert_eq!(raw.len(), 1);
        let err = parse_db(input).expect_err("the typed load must abort");
        assert!(
            err.contains("PV:X"),
            "the error must name the record, got {err}"
        );
    }

    #[test]
    fn macro_substitution_expands_dollar_braces_and_parens() {
        let mut macros = HashMap::new();
        macros.insert("P".to_string(), "SYS:".to_string());
        macros.insert("N".to_string(), "7".to_string());
        let input = "record(ai, \"${P}AI$(N)\") {\n    field(VAL, \"$(N)\")\n}\n";
        let recs = parse_db_records(input, "m.db", &macros).expect("expansion succeeds");
        assert_eq!(recs[0].name, "SYS:AI7");
        assert_eq!(recs[0].fields.get("VAL").map(String::as_str), Some("7"));
    }

    #[test]
    fn undefined_macro_is_an_error_naming_the_macro() {
        let input = "record(ai, \"$(NOPE)\") {\n}\n";
        let err = parse_db_records(input, "m.db", &HashMap::new())
            .expect_err("an undefined macro must abort the load");
        assert!(err.message.contains("NOPE"), "got {}", err.message);
    }

    #[test]
    fn quoted_field_values_may_continue_across_lines() {
        let input = "record(ai, \"PV:A\") {\n    field(DESC, \"one\ntwo\")\n}\n";
        let recs = parse_db_records(input, "c.db", &HashMap::new()).expect("continuation parses");
        assert_eq!(
            recs[0].fields.get("DESC").map(String::as_str),
            Some("one\ntwo")
        );
    }

    #[test]
    fn unterminated_quote_reports_the_opening_line() {
        let input = "record(ai, \"PV:A\") {\n    field(DESC, \"never closed\n}\n";
        let err = parse_db_records(input, "c.db", &HashMap::new())
            .expect_err("an unterminated quote must abort the load");
        assert_eq!(err.line, 2);
    }

    #[test]
    fn include_pulls_in_a_sibling_file() {
        let dir = std::env::temp_dir().join("spvirit_db_include_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        fs::write(dir.join("inner.db"), "record(ai, \"PV:INNER\") {\n}\n").expect("write inner");
        let outer = dir.join("outer.db");
        fs::write(
            &outer,
            "include \"inner.db\"\nrecord(ai, \"PV:OUTER\") {\n}\n",
        )
        .expect("write outer");

        let recs = load_db_records(outer.to_str().expect("utf8 path"), &HashMap::new())
            .expect("include resolves relative to the including file");
        let names: Vec<&str> = recs.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["PV:INNER", "PV:OUTER"]);

        fs::remove_dir_all(&dir).expect("clean up");
    }

    #[test]
    fn include_cycle_is_reported_rather_than_hanging() {
        let dir = std::env::temp_dir().join("spvirit_db_cycle_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        fs::write(dir.join("a.db"), "include \"b.db\"\n").expect("write a");
        fs::write(dir.join("b.db"), "include \"a.db\"\n").expect("write b");

        let err = load_db_records(
            dir.join("a.db").to_str().expect("utf8 path"),
            &HashMap::new(),
        )
        .expect_err("an include cycle must be an error, not a hang");
        assert!(err.message.contains("cycle"), "got {}", err.message);

        fs::remove_dir_all(&dir).expect("clean up");
    }

    #[test]
    fn trailing_inline_comments_after_a_field_value_are_stripped() {
        let input = "record(ai, \"PV:A\") {\n    field(LOPR, \"0\")      # display low\n}\n";
        let recs = parse_db_records(input, "t.db", &HashMap::new())
            .expect("a trailing comment after a field must not abort the load");
        assert_eq!(recs[0].fields.get("LOPR").map(String::as_str), Some("0"));
    }

    #[test]
    fn hash_inside_a_quoted_value_is_not_treated_as_a_comment() {
        let input = "record(ai, \"PV:A\") {\n    field(DESC, \"50% #1 unit\")\n}\n";
        let recs =
            parse_db_records(input, "t.db", &HashMap::new()).expect("a quoted # must survive");
        assert_eq!(
            recs[0].fields.get("DESC").map(String::as_str),
            Some("50% #1 unit")
        );
    }

    #[test]
    fn empty_record_name_is_rejected() {
        let input = "record(ai, \"\") {\n}\n";
        let err = parse_db_records(input, "e.db", &HashMap::new())
            .expect_err("an empty record name must abort the load");
        assert_eq!(err.line, 1);
        assert!(err.message.contains("empty"), "got {}", err.message);
    }

    #[test]
    fn shipped_example_db_parses_and_yields_expected_records() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("examples")
            .join("example.db");
        let recs = load_db_records(path.to_str().expect("utf8 path"), &HashMap::new())
            .expect("the shipped example.db must parse, trailing comments and all");
        let names: Vec<&str> = recs.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["DEMO:TEMP", "DEMO:SETPOINT", "DEMO:ENABLE", "DEMO:SPECTRUM",]
        );
        // The typed loader must also accept it end-to-end.
        load_db(path.to_str().expect("utf8 path")).expect("example.db must load as typed records");
    }

    #[test]
    fn archiver_demo_db_parses_and_yields_expected_record_count() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("examples")
            .join("archiver_demo.db");
        let recs = load_db_records(path.to_str().expect("utf8 path"), &HashMap::new())
            .expect("examples/archiver_demo.db must parse");
        assert_eq!(
            recs.len(),
            14,
            "unexpected record count in archiver_demo.db"
        );
        load_db(path.to_str().expect("utf8 path"))
            .expect("archiver_demo.db must load as typed records");
    }
}
