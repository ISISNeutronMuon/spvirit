//! sptable — interactive spreadsheet IOC. Each row is one dynamically-added PV.

use spvirit_types::{ScalarArrayValue, ScalarValue};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum WireType {
    Bool, I8, I16, I32, I64, U8, U16, U32, U64, F32, F64, Str,
}

impl WireType {
    const ALL: [WireType; 12] = [
        WireType::F64, WireType::F32, WireType::I64, WireType::I32,
        WireType::I16, WireType::I8, WireType::U64, WireType::U32,
        WireType::U16, WireType::U8, WireType::Bool, WireType::Str,
    ];

    fn label(self) -> &'static str {
        match self {
            WireType::Bool => "bool", WireType::I8 => "int8",
            WireType::I16 => "int16", WireType::I32 => "int32",
            WireType::I64 => "int64", WireType::U8 => "uint8",
            WireType::U16 => "uint16", WireType::U32 => "uint32",
            WireType::U64 => "uint64", WireType::F32 => "float",
            WireType::F64 => "double", WireType::Str => "string",
        }
    }

    fn from_label(s: &str) -> Option<WireType> {
        WireType::ALL.into_iter().find(|t| t.label() == s)
    }
}

fn parse_scalar(ty: WireType, s: &str) -> Result<ScalarValue, String> {
    let s = s.trim();
    let num = |e: std::num::ParseIntError| format!("invalid {}: {e}", ty.label());
    let numf = |e: std::num::ParseFloatError| format!("invalid {}: {e}", ty.label());
    Ok(match ty {
        WireType::Bool => match s {
            "true" | "1" | "on" | "True" => ScalarValue::Bool(true),
            "false" | "0" | "off" | "False" => ScalarValue::Bool(false),
            _ => return Err(format!("invalid bool: {s:?} (use true/false)")),
        },
        WireType::I8 => ScalarValue::I8(s.parse().map_err(num)?),
        WireType::I16 => ScalarValue::I16(s.parse().map_err(num)?),
        WireType::I32 => ScalarValue::I32(s.parse().map_err(num)?),
        WireType::I64 => ScalarValue::I64(s.parse().map_err(num)?),
        WireType::U8 => ScalarValue::U8(s.parse().map_err(num)?),
        WireType::U16 => ScalarValue::U16(s.parse().map_err(num)?),
        WireType::U32 => ScalarValue::U32(s.parse().map_err(num)?),
        WireType::U64 => ScalarValue::U64(s.parse().map_err(num)?),
        WireType::F32 => ScalarValue::F32(s.parse().map_err(numf)?),
        WireType::F64 => ScalarValue::F64(s.parse().map_err(numf)?),
        WireType::Str => ScalarValue::Str(s.to_string()),
    })
}

fn parse_array(ty: WireType, s: &str) -> Result<ScalarArrayValue, String> {
    let toks: Vec<&str> = if s.trim().is_empty() {
        Vec::new()
    } else {
        s.split(',').map(|t| t.trim()).collect()
    };
    macro_rules! collect {
        ($variant:ident) => {{
            let mut out = Vec::with_capacity(toks.len());
            for t in &toks {
                match parse_scalar(ty, t)? {
                    ScalarValue::$variant(v) => out.push(v),
                    _ => unreachable!(),
                }
            }
            ScalarArrayValue::$variant(out)
        }};
    }
    Ok(match ty {
        WireType::Bool => collect!(Bool),
        WireType::I8 => collect!(I8),
        WireType::I16 => collect!(I16),
        WireType::I32 => collect!(I32),
        WireType::I64 => collect!(I64),
        WireType::U8 => collect!(U8),
        WireType::U16 => collect!(U16),
        WireType::U32 => collect!(U32),
        WireType::U64 => collect!(U64),
        WireType::F32 => collect!(F32),
        WireType::F64 => collect!(F64),
        WireType::Str => collect!(Str),
    })
}

fn format_scalar(v: &ScalarValue) -> String {
    match v {
        ScalarValue::Bool(b) => b.to_string(),
        ScalarValue::I8(n) => n.to_string(),
        ScalarValue::I16(n) => n.to_string(),
        ScalarValue::I32(n) => n.to_string(),
        ScalarValue::I64(n) => n.to_string(),
        ScalarValue::U8(n) => n.to_string(),
        ScalarValue::U16(n) => n.to_string(),
        ScalarValue::U32(n) => n.to_string(),
        ScalarValue::U64(n) => n.to_string(),
        ScalarValue::F32(n) => n.to_string(),
        ScalarValue::F64(n) => n.to_string(),
        ScalarValue::Str(s) => s.clone(),
    }
}

fn format_array(v: &ScalarArrayValue) -> String {
    macro_rules! join {
        ($vec:expr) => {
            $vec.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(", ")
        };
    }
    match v {
        ScalarArrayValue::Bool(a) => join!(a),
        ScalarArrayValue::I8(a) => join!(a),
        ScalarArrayValue::I16(a) => join!(a),
        ScalarArrayValue::I32(a) => join!(a),
        ScalarArrayValue::I64(a) => join!(a),
        ScalarArrayValue::U8(a) => join!(a),
        ScalarArrayValue::U16(a) => join!(a),
        ScalarArrayValue::U32(a) => join!(a),
        ScalarArrayValue::U64(a) => join!(a),
        ScalarArrayValue::F32(a) => join!(a),
        ScalarArrayValue::F64(a) => join!(a),
        ScalarArrayValue::Str(a) => a.join(", "),
    }
}

fn main() {
    // Replaced in Task 5.
    eprintln!("sptable: not yet implemented");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_roundtrip_and_errors() {
        assert_eq!(parse_scalar(WireType::I32, "42").unwrap(), ScalarValue::I32(42));
        assert_eq!(parse_scalar(WireType::U8, "255").unwrap(), ScalarValue::U8(255));
        assert!(parse_scalar(WireType::U8, "256").is_err(), "u8 overflow rejected");
        assert!(parse_scalar(WireType::I32, "x").is_err());
        assert_eq!(parse_scalar(WireType::Bool, "on").unwrap(), ScalarValue::Bool(true));
        assert_eq!(
            parse_scalar(WireType::Str, "hi there").unwrap(),
            ScalarValue::Str("hi there".into())
        );
    }

    #[test]
    fn array_parse_and_format() {
        let a = parse_array(WireType::F64, "1.0, 2.5, 3").unwrap();
        assert_eq!(a, ScalarArrayValue::F64(vec![1.0, 2.5, 3.0]));
        assert_eq!(format_array(&a), "1, 2.5, 3");
        assert!(parse_array(WireType::I16, "1, notanint").is_err());
        assert_eq!(parse_array(WireType::I32, "").unwrap(), ScalarArrayValue::I32(vec![]));
    }

    #[test]
    fn wiretype_labels_roundtrip() {
        for t in WireType::ALL {
            assert_eq!(WireType::from_label(t.label()), Some(t));
        }
        assert_eq!(WireType::ALL.len(), 12);
        assert!(WireType::from_label("nope").is_none());
    }
}
