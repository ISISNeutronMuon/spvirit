//! Pure parsing/formatting of PV wire types, values, and `:` commands.

use spvirit_types::{ScalarArrayValue, ScalarValue};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WireType {
    Bool, I8, I16, I32, I64, U8, U16, U32, U64, F32, F64, Str,
}

impl WireType {
    pub const ALL: [WireType; 12] = [
        WireType::F64, WireType::F32, WireType::I64, WireType::I32,
        WireType::I16, WireType::I8, WireType::U64, WireType::U32,
        WireType::U16, WireType::U8, WireType::Bool, WireType::Str,
    ];

    pub fn label(self) -> &'static str {
        match self {
            WireType::Bool => "bool", WireType::I8 => "int8",
            WireType::I16 => "int16", WireType::I32 => "int32",
            WireType::I64 => "int64", WireType::U8 => "uint8",
            WireType::U16 => "uint16", WireType::U32 => "uint32",
            WireType::U64 => "uint64", WireType::F32 => "float",
            WireType::F64 => "double", WireType::Str => "string",
        }
    }

    // used only by the round-trip test; from_token is the runtime path
    #[allow(dead_code)]
    pub fn from_label(s: &str) -> Option<WireType> {
        WireType::ALL.into_iter().find(|t| t.label() == s)
    }

    /// Alias-aware token → type. Accepts short and long forms.
    pub fn from_token(s: &str) -> Option<WireType> {
        Some(match s {
            "b" | "bool" => WireType::Bool,
            "i8" | "int8" => WireType::I8,
            "i16" | "int16" => WireType::I16,
            "i32" | "int" | "int32" => WireType::I32,
            "i64" | "long" | "int64" => WireType::I64,
            "u8" | "uint8" => WireType::U8,
            "u16" | "uint16" => WireType::U16,
            "u32" | "uint32" => WireType::U32,
            "u64" | "uint64" => WireType::U64,
            "f32" | "float" => WireType::F32,
            "f64" | "double" => WireType::F64,
            "s" | "str" | "string" => WireType::Str,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpecInput {
    Scalar(WireType),
    Array(WireType),
    Enum,
    Table,
}

pub fn parse_typespec(tok: &str) -> Result<SpecInput, String> {
    if tok == "enum" {
        return Ok(SpecInput::Enum);
    }
    if tok == "table" {
        return Ok(SpecInput::Table);
    }
    if let Some(base) = tok.strip_suffix("[]") {
        return WireType::from_token(base)
            .map(SpecInput::Array)
            .ok_or_else(|| format!("unknown array type {base:?}"));
    }
    WireType::from_token(tok)
        .map(SpecInput::Scalar)
        .ok_or_else(|| format!("unknown type {tok:?}"))
}

pub fn coerce_scalar(raw: f64, ty: WireType) -> ScalarValue {
    macro_rules! clamp {
        ($t:ty, $ctor:path) => {{
            let lo = <$t>::MIN as f64;
            let hi = <$t>::MAX as f64;
            $ctor(raw.round().clamp(lo, hi) as $t)
        }};
    }
    match ty {
        WireType::Bool => ScalarValue::Bool(raw != 0.0),
        WireType::I8 => clamp!(i8, ScalarValue::I8),
        WireType::I16 => clamp!(i16, ScalarValue::I16),
        WireType::I32 => clamp!(i32, ScalarValue::I32),
        WireType::I64 => clamp!(i64, ScalarValue::I64),
        WireType::U8 => clamp!(u8, ScalarValue::U8),
        WireType::U16 => clamp!(u16, ScalarValue::U16),
        WireType::U32 => clamp!(u32, ScalarValue::U32),
        WireType::U64 => clamp!(u64, ScalarValue::U64),
        WireType::F32 => ScalarValue::F32(raw as f32),
        WireType::F64 => ScalarValue::F64(raw),
        WireType::Str => ScalarValue::Str(raw.to_string()),
    }
}

pub fn parse_scalar(ty: WireType, s: &str) -> Result<ScalarValue, String> {
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

pub fn parse_array(ty: WireType, s: &str) -> Result<ScalarArrayValue, String> {
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

pub fn format_scalar(v: &ScalarValue) -> String {
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

pub fn format_array(v: &ScalarArrayValue) -> String {
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

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Add { pattern: String, spec: SpecInput, writable: bool, value: String },
    Set { pattern: String, value: String },
    Del { pattern: Option<String> },
    Rename { old: String, new: String },
    Access { pattern: String, writable: bool },
    Anim { pattern: String, generator: String, params: Vec<(String, String)> },
    Stop { pattern: Option<String> },
    Rate { hz: f64 },
    Source { path: String },
    Help,
    Quit,
}

/// Parse a `:` command line (without the leading colon). Pattern strings are
/// returned raw; expansion happens at execution time.
pub fn parse_command(line: &str) -> Result<Command, String> {
    let line = line.trim();
    let mut it = line.split_whitespace();
    let verb = it.next().ok_or_else(|| "empty command".to_string())?;
    let rest: Vec<&str> = it.collect();

    match verb {
        "add" | "a" => {
            // <name> <typespec> [ro|rw] <value...>
            let name = rest.first().ok_or("add: missing name")?;
            let tyt = rest.get(1).ok_or("add: missing type")?;
            let spec = parse_typespec(tyt)?;
            let mut idx = 2;
            let mut writable = true;
            match rest.get(2).copied() {
                Some("ro") => { writable = false; idx = 3; }
                Some("rw") => { writable = true; idx = 3; }
                _ => {}
            }
            let value = rest.get(idx..).map(|s| s.join(" ")).unwrap_or_default();
            Ok(Command::Add { pattern: name.to_string(), spec, writable, value })
        }
        "set" | "s" => {
            let name = rest.first().ok_or("set: missing name")?;
            let value = rest.get(1..).map(|s| s.join(" ")).unwrap_or_default();
            Ok(Command::Set { pattern: name.to_string(), value })
        }
        "del" | "d" => Ok(Command::Del { pattern: rest.first().map(|s| s.to_string()) }),
        "rename" | "mv" => {
            let old = rest.first().ok_or("rename: missing old name")?;
            let new = rest.get(1).ok_or("rename: missing new name")?;
            Ok(Command::Rename { old: old.to_string(), new: new.to_string() })
        }
        "ro" | "rw" => {
            let name = rest.first().ok_or("access: missing name")?;
            Ok(Command::Access { pattern: name.to_string(), writable: verb == "rw" })
        }
        "anim" => {
            let name = rest.first().ok_or("anim: missing name")?;
            let generator = rest.get(1).ok_or("anim: missing generator")?;
            let mut params = Vec::new();
            for kv in &rest[2..] {
                let (k, v) = kv.split_once('=').ok_or_else(|| format!("anim: bad param {kv:?} (want key=value)"))?;
                params.push((k.to_string(), v.to_string()));
            }
            Ok(Command::Anim { pattern: name.to_string(), generator: generator.to_string(), params })
        }
        "stop" => Ok(Command::Stop { pattern: rest.first().map(|s| s.to_string()) }),
        "rate" => {
            let hz: f64 = rest.first().ok_or("rate: missing hz")?
                .parse().map_err(|_| "rate: hz must be a number".to_string())?;
            if hz <= 0.0 { return Err("rate: hz must be positive".into()); }
            Ok(Command::Rate { hz })
        }
        "source" | "so" => {
            let path = rest.first().ok_or("source: missing path")?;
            Ok(Command::Source { path: path.to_string() })
        }
        "help" | "h" => Ok(Command::Help),
        "quit" | "q" => Ok(Command::Quit),
        other => Err(format!("unknown command {other:?} (try :help)")),
    }
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

    #[test]
    fn typespec_aliases_and_kinds() {
        assert_eq!(WireType::from_token("i32"), Some(WireType::I32));
        assert_eq!(WireType::from_token("int"), Some(WireType::I32));
        assert_eq!(WireType::from_token("long"), Some(WireType::I64));
        assert_eq!(WireType::from_token("f64"), Some(WireType::F64));
        assert_eq!(WireType::from_token("double"), Some(WireType::F64));
        assert_eq!(WireType::from_token("s"), Some(WireType::Str));
        assert_eq!(WireType::from_token("nope"), None);

        assert!(matches!(parse_typespec("i32"), Ok(SpecInput::Scalar(WireType::I32))));
        assert!(matches!(parse_typespec("f64[]"), Ok(SpecInput::Array(WireType::F64))));
        assert!(matches!(parse_typespec("enum"), Ok(SpecInput::Enum)));
        assert!(matches!(parse_typespec("table"), Ok(SpecInput::Table)));
        assert!(parse_typespec("bogus").is_err());
        assert!(parse_typespec("bogus[]").is_err());
    }

    #[test]
    fn coerce_scalar_rounds_and_clamps() {
        assert_eq!(coerce_scalar(2.7, WireType::I32), ScalarValue::I32(3));
        assert_eq!(coerce_scalar(300.0, WireType::U8), ScalarValue::U8(255));
        assert_eq!(coerce_scalar(-5.0, WireType::U8), ScalarValue::U8(0));
        assert_eq!(coerce_scalar(0.0, WireType::Bool), ScalarValue::Bool(false));
        assert_eq!(coerce_scalar(1.0, WireType::Bool), ScalarValue::Bool(true));
        assert_eq!(coerce_scalar(1.5, WireType::F32), ScalarValue::F32(1.5));
    }

    #[test]
    fn parse_command_verbs_and_shorthands() {
        // add, full form
        match parse_command("add SIM:X i32 rw 42").unwrap() {
            Command::Add { pattern, spec, writable, value } => {
                assert_eq!(pattern, "SIM:X");
                assert!(matches!(spec, SpecInput::Scalar(WireType::I32)));
                assert!(writable);
                assert_eq!(value, "42");
            }
            _ => panic!("expected Add"),
        }
        // shorthand + default access (rw) + multi-token value
        match parse_command("a SIM:S string hello world").unwrap() {
            Command::Add { pattern, writable, value, .. } => {
                assert_eq!(pattern, "SIM:S");
                assert!(writable, "access defaults to rw");
                assert_eq!(value, "hello world");
            }
            _ => panic!("expected Add"),
        }
        // read-only
        match parse_command("a SIM:R i16 ro 3").unwrap() {
            Command::Add { writable, .. } => assert!(!writable),
            _ => panic!("expected Add"),
        }
        // set / :s
        assert!(matches!(parse_command("s SIM:X 99").unwrap(),
            Command::Set { pattern, value } if pattern == "SIM:X" && value == "99"));
        // del with and without arg
        assert!(matches!(parse_command("d SIM:X").unwrap(),
            Command::Del { pattern: Some(p) } if p == "SIM:X"));
        assert!(matches!(parse_command("d").unwrap(), Command::Del { pattern: None }));
        // rename / mv
        assert!(matches!(parse_command("mv A B").unwrap(),
            Command::Rename { old, new } if old == "A" && new == "B"));
        // access
        assert!(matches!(parse_command("ro SIM:X").unwrap(),
            Command::Access { pattern, writable } if pattern == "SIM:X" && !writable));
        assert!(matches!(parse_command("rw SIM:X").unwrap(),
            Command::Access { writable: true, .. }));
        // anim
        match parse_command("anim SIM:X sine amp=5 period=2").unwrap() {
            Command::Anim { pattern, generator, params } => {
                assert_eq!(pattern, "SIM:X");
                assert_eq!(generator, "sine");
                assert_eq!(params, vec![("amp".to_string(), "5".to_string()),
                                        ("period".to_string(), "2".to_string())]);
            }
            _ => panic!("expected Anim"),
        }
        // stop, rate, source, help, quit + shorthands
        assert!(matches!(parse_command("stop").unwrap(), Command::Stop { pattern: None }));
        assert!(matches!(parse_command("rate 20").unwrap(), Command::Rate { hz } if (hz - 20.0).abs() < 1e-9));
        assert!(matches!(parse_command("so layout.txt").unwrap(),
            Command::Source { path } if path == "layout.txt"));
        assert!(matches!(parse_command("h").unwrap(), Command::Help));
        assert!(matches!(parse_command("help").unwrap(), Command::Help));
        assert!(matches!(parse_command("q").unwrap(), Command::Quit));
        // errors
        assert!(parse_command("").is_err());
        assert!(parse_command("bogusverb x").is_err());
        assert!(parse_command("add").is_err());
        assert!(parse_command("add OnlyName").is_err());
    }
}
