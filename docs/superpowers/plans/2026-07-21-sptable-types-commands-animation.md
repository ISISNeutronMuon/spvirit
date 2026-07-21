# sptable Types / Command System / Animation — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend the `sptable` TUI IOC with two new PV kinds (NTEnum, NTTable), a vim-style `:` command line (verbs + shorthands + `:help`), bash-style pattern expansion for bulk create/delete/animate, and background animation generators that drive PV values over time.

**Architecture:** Two runtime primitives are added to `spvirit-server` (`RunningServer::add_enum`/`add_table`), mirroring the existing `add_scalar`/`add_array`. The binary `spvirit_table.rs` is split into three pure, unit-tested sibling modules (`parse`, `pattern`, `anim`) plus the async TUI shell. A server-side Tokio tick task samples animators and writes to the store independent of the UI loop.

**Tech Stack:** Rust, tokio, ratatui 0.29 (`tui` feature), argparse, spvirit-server/-types. No new crate dependencies (PRNG is hand-rolled).

## Global Constraints

- The 12 wire types are exactly the `ScalarValue`/`ScalarArrayValue` variants: `Bool, I8, I16, I32, I64, U8, U16, U32, U64, F32, F64, Str` (spvirit-types/src/lib.rs).
- `record.writable()` is **always `true`** for `NtEnum`/`NtTable` (spvirit-server/src/types.rs:303). Enum `writable` selects `Mbbo` vs `Mbbi` record type only (advertised access); table is always RW.
- Server-crate record helpers (`make_scalar_record`, `make_output_record`, `make_array_record`) are `pub(crate)` in `pva_server.rs`; new `make_enum_record`/`make_table_record` join them there. Imports already in `pva_server.rs`: `NtEnum`, `NtTable as NtTableType`, `NtTableColumn`, `ScalarArrayValue`, `ScalarValue`, `DbCommonState`, `OutputMode`, `RecordData`, `RecordInstance`, `RecordType`, `HashMap`.
- The store's general setter is `SimplePvStore::put_nt(&self, name: &str, payload: NtPayload) -> bool` (async); scalar/array use `set_value`/`set_array_value`.
- Binary submodules for `src/bin/spvirit_table.rs` live in `src/bin/spvirit_table/` and are declared `mod parse;` etc. in the bin root (standard cargo resolution; subdir files are NOT compiled as separate binaries).
- No new dependencies. Randomness uses a hand-rolled `xorshift64` seeded per animator.
- Tests: server via `cargo test -p spvirit-server`; binary unit tests via `cargo test -p spvirit-tools --bin sptable`.
- Pattern-expansion safety cap: **1000** PVs per command (a `const`).
- Default animation tick rate: **10 Hz** (`--rate` flag + `:rate` command).

---

### Task 1: `RunningServer::add_enum` and `add_table`

**Files:**
- Modify: `spvirit-server/src/pva_server.rs` (add two methods to `impl RunningServer` after `add_array` ~line 902; add two `pub(crate)` record helpers after `make_array_record`; add tests to `#[cfg(test)] mod tests`)

**Interfaces:**
- Produces:
  - `RunningServer::add_enum(&self, name: &str, choices: Vec<String>, index: i32, writable: bool)` (async)
  - `RunningServer::add_table(&self, name: &str, columns: Vec<(String, ScalarArrayValue)>)` (async)
  - `pub(crate) fn make_enum_record(name: &str, choices: Vec<String>, index: i32, writable: bool) -> RecordInstance`
  - `pub(crate) fn make_table_record(name: &str, columns: Vec<(String, ScalarArrayValue)>) -> RecordInstance`

- [ ] **Step 1: Write the failing test**

Add to `#[cfg(test)] mod tests` in `pva_server.rs`:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_server_add_enum_and_table() {
    use spvirit_types::{NtPayload, ScalarArrayValue};

    let server = PvaServer::serve(Vec::<crate::pv::AnyPv>::new())
        .port(0)
        .udp_port(0)
        .start()
        .await;

    // writable enum -> mbbo, choices + index preserved
    server
        .add_enum("RT:ENUM", vec!["OFF".into(), "ON".into(), "TRIP".into()], 1, true)
        .await;
    match server.store().get_nt("RT:ENUM").await {
        Some(NtPayload::Enum(e)) => {
            assert_eq!(e.index, 1);
            assert_eq!(e.choices, vec!["OFF", "ON", "TRIP"]);
        }
        other => panic!("expected enum, got {other:?}"),
    }

    // read-only enum -> mbbi; still writable() at the store layer (documented)
    server.add_enum("RT:ENUM_RO", vec!["A".into(), "B".into()], 0, false).await;
    assert!(matches!(
        server.store().get_nt("RT:ENUM_RO").await,
        Some(NtPayload::Enum(_))
    ));

    // table with two typed columns
    server
        .add_table(
            "RT:TBL",
            vec![
                ("id".into(), ScalarArrayValue::I32(vec![1, 2, 3])),
                ("x".into(), ScalarArrayValue::F64(vec![0.5, 1.5, 2.5])),
            ],
        )
        .await;
    match server.store().get_nt("RT:TBL").await {
        Some(NtPayload::Table(t)) => {
            assert_eq!(t.labels, vec!["id", "x"]);
            assert_eq!(t.columns.len(), 2);
        }
        other => panic!("expected table, got {other:?}"),
    }

    server.abort();
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p spvirit-server running_server_add_enum_and_table`
Expected: FAIL — `no method named 'add_enum'`.

- [ ] **Step 3: Implement the record helpers**

Add after `make_array_record` in `pva_server.rs`:

```rust
pub(crate) fn make_enum_record(
    name: &str,
    choices: Vec<String>,
    index: i32,
    writable: bool,
) -> RecordInstance {
    RecordInstance {
        name: name.to_string(),
        record_type: if writable { RecordType::Mbbo } else { RecordType::Mbbi },
        common: DbCommonState::default(),
        data: RecordData::NtEnum {
            nt: NtEnum::new(index, choices),
            inp: None,
            out: None,
            omsl: OutputMode::Supervisory,
        },
        raw_fields: HashMap::new(),
    }
}

pub(crate) fn make_table_record(
    name: &str,
    columns: Vec<(String, ScalarArrayValue)>,
) -> RecordInstance {
    let labels: Vec<String> = columns.iter().map(|(n, _)| n.clone()).collect();
    let cols: Vec<NtTableColumn> = columns
        .into_iter()
        .map(|(n, v)| NtTableColumn { name: n, values: v })
        .collect();
    RecordInstance {
        name: name.to_string(),
        record_type: RecordType::NtTable,
        common: DbCommonState::default(),
        data: RecordData::NtTable {
            nt: NtTableType { labels, columns: cols, descriptor: None, alarm: None, time_stamp: None },
            inp: None,
            out: None,
            omsl: OutputMode::Supervisory,
        },
        raw_fields: HashMap::new(),
    }
}
```

- [ ] **Step 4: Implement the two `RunningServer` methods**

Add inside `impl RunningServer`, after `add_array`:

```rust
    /// Add an NTEnum record at runtime. `writable` selects an `mbbo`
    /// (output) vs `mbbi` (input) record type; note both accept client PUTs
    /// at the store layer. Replaces any existing record with the same name.
    pub async fn add_enum(&self, name: &str, choices: Vec<String>, index: i32, writable: bool) {
        let record = make_enum_record(name, choices, index, writable);
        self.store.insert(name.to_string(), record).await;
    }

    /// Add an NTTable record at runtime from named, typed columns. Tables are
    /// always writable at the store layer. Replaces any existing record with
    /// the same name.
    pub async fn add_table(&self, name: &str, columns: Vec<(String, ScalarArrayValue)>) {
        let record = make_table_record(name, columns);
        self.store.insert(name.to_string(), record).await;
    }
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p spvirit-server running_server_add_enum_and_table`
Expected: PASS.

- [ ] **Step 6: Run the full server suite**

Run: `cargo test -p spvirit-server`
Expected: PASS (no regressions).

- [ ] **Step 7: Commit**

```bash
git add spvirit-server/src/pva_server.rs
git commit -m "feat(server): RunningServer::add_enum/add_table for runtime enum + table PVs"
```

---

### Task 2: Extract the `parse` module (pure refactor)

Move the existing `WireType`, `parse_scalar`, `parse_array`, `format_scalar`, `format_array` (and their tests) out of `spvirit_table.rs` into a new sibling module, with **no behavior change**. This establishes the module layout the later tasks build on.

**Files:**
- Create: `spvirit-tools/src/bin/spvirit_table/parse.rs`
- Modify: `spvirit-tools/src/bin/spvirit_table.rs` (remove moved items; add `mod parse;` + `use`)

**Interfaces:**
- Produces (all `pub` in `parse`): `WireType` (+ `ALL`, `label`, `from_label`), `parse_scalar`, `parse_array`, `format_scalar`, `format_array`.

- [ ] **Step 1: Create `parse.rs` with the moved code**

Create `spvirit-tools/src/bin/spvirit_table/parse.rs` and move the current contents of `spvirit_table.rs` lines 16–142 (the `WireType` enum/impl, `parse_scalar`, `parse_array`, `format_scalar`, `format_array`) into it, making every item `pub`. Move the `scalar_roundtrip_and_errors`, `array_parse_and_format`, `wiretype_labels_roundtrip` tests into a `#[cfg(test)] mod tests { use super::*; ... }` at the bottom of `parse.rs`. Add the import header:

```rust
//! Pure parsing/formatting of PV wire types, values, and `:` commands.

use spvirit_types::{ScalarArrayValue, ScalarValue};
```

Make these `pub`: `pub enum WireType`, `pub const ALL`, `pub fn label`, `pub fn from_label`, `pub fn parse_scalar`, `pub fn parse_array`, `pub fn format_scalar`, `pub fn format_array`. Remove the `#[allow(dead_code)]` on `from_label` (it is used by Task 3).

- [ ] **Step 2: Wire the module into the binary**

In `spvirit_table.rs`, delete the moved lines 16–142 and the three moved tests, and add near the top (after the existing `use` block):

```rust
mod parse;
use parse::{WireType, format_array, format_scalar, parse_array, parse_scalar};
```

Keep the remaining `use spvirit_types::{ScalarArrayValue, ScalarValue};` (still used by the model/handle code).

- [ ] **Step 3: Verify it compiles and tests pass**

Run: `cargo test -p spvirit-tools --bin sptable`
Expected: PASS (3 tests, now located in `parse`).

Run: `cargo build -p spvirit-tools --bin sptable`
Expected: builds clean.

- [ ] **Step 4: Commit**

```bash
git add spvirit-tools/src/bin/spvirit_table.rs spvirit-tools/src/bin/spvirit_table/parse.rs
git commit -m "refactor(tools): extract sptable parse module"
```

---

### Task 3: typespec aliases, `SpecInput`, `Command`, and `parse_command`

Extend `parse` with alias-aware type tokens, a spec descriptor, the command AST, and the pure command parser. Patterns are left as raw strings (expanded in Task 7), so `parse` stays independent of `pattern`.

**Files:**
- Modify: `spvirit-tools/src/bin/spvirit_table/parse.rs`

**Interfaces:**
- Consumes: `WireType` (Task 2).
- Produces:
  - `WireType::from_token(s: &str) -> Option<WireType>` (accepts aliases)
  - `pub enum SpecInput { Scalar(WireType), Array(WireType), Enum, Table }`
  - `pub fn parse_typespec(tok: &str) -> Result<SpecInput, String>`
  - `pub fn coerce_scalar(raw: f64, ty: WireType) -> ScalarValue`
  - `pub enum Command { Add { pattern, spec, writable, value }, Set { pattern, value }, Del { pattern: Option<String> }, Rename { old, new }, Access { pattern, writable }, Anim { pattern, gen: String, params: Vec<(String, String)> }, Stop { pattern: Option<String> }, Rate { hz: f64 }, Source { path: String }, Help, Quit }`
  - `pub fn parse_command(line: &str) -> Result<Command, String>`

- [ ] **Step 1: Write the failing tests**

Add to `parse.rs`'s `mod tests`:

```rust
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
        Command::Anim { pattern, gen, params } => {
            assert_eq!(pattern, "SIM:X");
            assert_eq!(gen, "sine");
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p spvirit-tools --bin sptable typespec_aliases_and_kinds`
Expected: FAIL — `no function or associated item named 'from_token'`.

- [ ] **Step 3: Implement `from_token`, `parse_typespec`, `coerce_scalar`**

Add to `parse.rs` (inside `impl WireType` for `from_token`; free fns otherwise):

```rust
impl WireType {
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
```

- [ ] **Step 4: Implement `Command` + `parse_command`**

Add to `parse.rs`:

```rust
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Add { pattern: String, spec: SpecInput, writable: bool, value: String },
    Set { pattern: String, value: String },
    Del { pattern: Option<String> },
    Rename { old: String, new: String },
    Access { pattern: String, writable: bool },
    Anim { pattern: String, gen: String, params: Vec<(String, String)> },
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
            let gen = rest.get(1).ok_or("anim: missing generator")?;
            let mut params = Vec::new();
            for kv in &rest[2..] {
                let (k, v) = kv.split_once('=').ok_or_else(|| format!("anim: bad param {kv:?} (want key=value)"))?;
                params.push((k.to_string(), v.to_string()));
            }
            Ok(Command::Anim { pattern: name.to_string(), gen: gen.to_string(), params })
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
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p spvirit-tools --bin sptable`
Expected: PASS (all parse tests green).

- [ ] **Step 6: Commit**

```bash
git add spvirit-tools/src/bin/spvirit_table/parse.rs
git commit -m "feat(tools): sptable typespec aliases, Command AST + parse_command"
```

---

### Task 4: `pattern` module — bash-style brace expansion

**Files:**
- Create: `spvirit-tools/src/bin/spvirit_table/pattern.rs`
- Modify: `spvirit-tools/src/bin/spvirit_table.rs` (add `mod pattern;`)

**Interfaces:**
- Produces: `pub const EXPAND_CAP: usize = 1000;` and `pub fn expand_pattern(s: &str) -> Result<Vec<String>, String>`

- [ ] **Step 1: Write the failing tests + module skeleton**

Create `spvirit-tools/src/bin/spvirit_table/pattern.rs`:

```rust
//! Bash-style brace expansion for bulk PV names.

/// Max PVs a single pattern may expand to (safety against typos).
pub const EXPAND_CAP: usize = 1000;

// (implementation added in Step 3)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_braces_is_identity() {
        assert_eq!(expand_pattern("SIM:X").unwrap(), vec!["SIM:X"]);
    }

    #[test]
    fn numeric_range_ascending_and_descending() {
        assert_eq!(expand_pattern("N{1..3}").unwrap(), vec!["N1", "N2", "N3"]);
        assert_eq!(expand_pattern("N{3..1}").unwrap(), vec!["N3", "N2", "N1"]);
    }

    #[test]
    fn stepped_range() {
        assert_eq!(expand_pattern("N{0..10..5}").unwrap(), vec!["N0", "N5", "N10"]);
    }

    #[test]
    fn zero_padded_range() {
        assert_eq!(
            expand_pattern("BPM{08..11}").unwrap(),
            vec!["BPM08", "BPM09", "BPM10", "BPM11"]
        );
    }

    #[test]
    fn literal_list() {
        assert_eq!(expand_pattern("P:{A,B,C}").unwrap(), vec!["P:A", "P:B", "P:C"]);
    }

    #[test]
    fn cartesian_product() {
        assert_eq!(
            expand_pattern("S{1..2}:{A,B}").unwrap(),
            vec!["S1:A", "S1:B", "S2:A", "S2:B"]
        );
    }

    #[test]
    fn cap_and_malformed() {
        assert!(expand_pattern("X{1..100000}").is_err(), "over cap");
        assert!(expand_pattern("X{1..}").is_err(), "malformed range");
        assert!(expand_pattern("X{").is_err(), "unclosed brace");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p spvirit-tools --bin sptable no_braces_is_identity`
Expected: FAIL — `cannot find function 'expand_pattern'`.

Note: add `mod pattern;` to `spvirit_table.rs` (below `mod parse;`) first so the file compiles into the binary.

- [ ] **Step 3: Implement `expand_pattern`**

Add to `pattern.rs` (above the tests):

```rust
/// Expand bash-style braces. Supports `{m..n}`, `{m..n..step}`, zero-padding
/// inferred from bound widths, and `{a,b,c}` lists. Multiple braces form a
/// cartesian product. Errors on malformed braces or on exceeding `EXPAND_CAP`.
pub fn expand_pattern(s: &str) -> Result<Vec<String>, String> {
    // Find the first top-level `{...}` group.
    let open = match s.find('{') {
        Some(i) => i,
        None => {
            if s.contains('}') {
                return Err(format!("unmatched '}}' in {s:?}"));
            }
            return Ok(vec![s.to_string()]);
        }
    };
    let close = s[open..]
        .find('}')
        .map(|rel| open + rel)
        .ok_or_else(|| format!("unclosed '{{' in {s:?}"))?;

    let prefix = &s[..open];
    let inner = &s[open + 1..close];
    let suffix = &s[close + 1..];

    let alternatives = expand_group(inner)?;

    // Expand the remainder recursively, then cartesian-combine.
    let tails = expand_pattern(suffix)?;
    let mut out = Vec::new();
    for alt in &alternatives {
        for tail in &tails {
            out.push(format!("{prefix}{alt}{tail}"));
            if out.len() > EXPAND_CAP {
                return Err(format!("pattern {s:?} would create over {EXPAND_CAP} PVs"));
            }
        }
    }
    Ok(out)
}

/// Expand the text inside one `{...}` into its alternatives.
fn expand_group(inner: &str) -> Result<Vec<String>, String> {
    // Range form: m..n or m..n..step
    if let Some((lo, rest)) = inner.split_once("..") {
        let (hi, step) = match rest.split_once("..") {
            Some((h, s)) => (h, s.parse::<i64>().map_err(|_| format!("bad step in {{{inner}}}"))?),
            None => (rest, 1),
        };
        if step <= 0 {
            return Err(format!("step must be positive in {{{inner}}}"));
        }
        let start: i64 = lo.parse().map_err(|_| format!("bad range start in {{{inner}}}"))?;
        let end: i64 = hi.parse().map_err(|_| format!("bad range end in {{{inner}}}"))?;
        let width = lo.len().max(hi.len());
        let pad = lo.starts_with('0') || hi.starts_with('0');

        let mut vals = Vec::new();
        let mut i = start;
        while (start <= end && i <= end) || (start > end && i >= end) {
            let token = if pad {
                format!("{:0width$}", i, width = width)
            } else {
                i.to_string()
            };
            vals.push(token);
            if vals.len() > EXPAND_CAP {
                return Err(format!("range {{{inner}}} would create over {EXPAND_CAP} PVs"));
            }
            i += if start <= end { step } else { -step };
        }
        return Ok(vals);
    }

    // List form: a,b,c
    if inner.contains(',') {
        return Ok(inner.split(',').map(|t| t.to_string()).collect());
    }

    Err(format!("malformed brace {{{inner}}} (want m..n or a,b,c)"))
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p spvirit-tools --bin sptable`
Expected: PASS (all pattern tests green).

- [ ] **Step 5: Commit**

```bash
git add spvirit-tools/src/bin/spvirit_table.rs spvirit-tools/src/bin/spvirit_table/pattern.rs
git commit -m "feat(tools): sptable brace-pattern expansion"
```

---

### Task 5: `anim` module — generators, PRNG, sampling

**Files:**
- Create: `spvirit-tools/src/bin/spvirit_table/anim.rs`
- Modify: `spvirit-tools/src/bin/spvirit_table.rs` (add `mod anim;`)

**Interfaces:**
- Produces:
  - `pub struct Rng { state: u64 }` with `new(seed: u64)`, `next_f64(&mut self) -> f64` (uniform [0,1))
  - `pub enum Generator { Sine, Ramp, Triangle, Square, Noise, Walk, Count, Cycle }`
  - `pub struct AnimSpec { pub gen: Generator, pub p: Params }` where `Params` holds all resolved numeric fields
  - `pub struct AnimState { rng: Rng, count: f64, walk: f64, walk_init: bool }` with `new(seed: u64)`
  - `pub fn build_anim(gen: &str, params: &[(String, String)]) -> Result<AnimSpec, String>`
  - `pub fn sample(spec: &AnimSpec, st: &mut AnimState, t: f64) -> f64`
  - `pub fn is_enum_only(gen: &Generator) -> bool` (true for `Cycle`)

- [ ] **Step 1: Write the failing tests + skeleton**

Create `spvirit-tools/src/bin/spvirit_table/anim.rs`:

```rust
//! Animation generators: pure sampling of a value as a function of time.

use std::f64::consts::PI;

// (implementation added in Steps 3–4)

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(gen: &str, params: &[(&str, &str)]) -> AnimSpec {
        let p: Vec<(String, String)> =
            params.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        build_anim(gen, &p).unwrap()
    }

    #[test]
    fn sine_at_quarter_periods() {
        let s = spec("sine", &[("amp", "1"), ("offset", "0"), ("period", "4"), ("phase", "0")]);
        let mut st = AnimState::new(1);
        assert!((sample(&s, &mut st, 0.0) - 0.0).abs() < 1e-9);
        assert!((sample(&s, &mut st, 1.0) - 1.0).abs() < 1e-9); // quarter period -> peak
        assert!((sample(&s, &mut st, 2.0) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn ramp_wraps() {
        let s = spec("ramp", &[("min", "0"), ("max", "10"), ("period", "10")]);
        let mut st = AnimState::new(1);
        assert!((sample(&s, &mut st, 0.0) - 0.0).abs() < 1e-9);
        assert!((sample(&s, &mut st, 5.0) - 5.0).abs() < 1e-9);
        assert!((sample(&s, &mut st, 10.0) - 0.0).abs() < 1e-9); // wrap
    }

    #[test]
    fn square_duty() {
        let s = spec("square", &[("lo", "0"), ("hi", "1"), ("period", "10"), ("duty", "0.5")]);
        let mut st = AnimState::new(1);
        assert_eq!(sample(&s, &mut st, 1.0), 1.0);
        assert_eq!(sample(&s, &mut st, 6.0), 0.0);
    }

    #[test]
    fn noise_in_range() {
        let s = spec("noise", &[("min", "-2"), ("max", "2")]);
        let mut st = AnimState::new(42);
        for k in 0..100 {
            let v = sample(&s, &mut st, k as f64);
            assert!((-2.0..=2.0).contains(&v), "noise {v} out of range");
        }
    }

    #[test]
    fn count_advances_and_cycle_is_enum_only() {
        let s = spec("count", &[("start", "0"), ("step", "1")]);
        let mut st = AnimState::new(1);
        assert_eq!(sample(&s, &mut st, 0.0), 0.0);
        assert_eq!(sample(&s, &mut st, 0.0), 1.0);
        assert_eq!(sample(&s, &mut st, 0.0), 2.0);

        let c = spec("cycle", &[("period", "2")]);
        assert!(is_enum_only(&c.gen));
        let mut cs = AnimState::new(1);
        // index grows as floor(t/period)
        assert_eq!(sample(&c, &mut cs, 0.0), 0.0);
        assert_eq!(sample(&c, &mut cs, 2.0), 1.0);
        assert_eq!(sample(&c, &mut cs, 5.0), 2.0);
    }

    #[test]
    fn unknown_generator_and_bad_param() {
        assert!(build_anim("bogus", &[]).is_err());
        assert!(build_anim("sine", &[("amp".into(), "x".into())]).is_err());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p spvirit-tools --bin sptable sine_at_quarter_periods`
Expected: FAIL — `cannot find function 'build_anim'`.

Note: add `mod anim;` to `spvirit_table.rs` first.

- [ ] **Step 3: Implement the PRNG, types, and `build_anim`**

Add to `anim.rs` (above the tests):

```rust
/// Tiny xorshift64 PRNG — deterministic, seedable, no external dependency.
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        // Avoid the all-zero state, which xorshift cannot leave.
        Rng { state: seed | 1 }
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }
    /// Uniform in [0, 1).
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Generator {
    Sine,
    Ramp,
    Triangle,
    Square,
    Noise,
    Walk,
    Count,
    Cycle,
}

#[derive(Copy, Clone, Debug)]
pub struct Params {
    pub amp: f64,
    pub offset: f64,
    pub period: f64,
    pub phase: f64,
    pub min: f64,
    pub max: f64,
    pub lo: f64,
    pub hi: f64,
    pub duty: f64,
    pub start: f64,
    pub step: f64,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            amp: 1.0, offset: 0.0, period: 10.0, phase: 0.0,
            min: 0.0, max: 1.0, lo: 0.0, hi: 1.0, duty: 0.5,
            start: 0.0, step: 1.0,
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct AnimSpec {
    pub gen: Generator,
    pub p: Params,
}

pub struct AnimState {
    rng: Rng,
    count: f64,
    walk: f64,
    walk_init: bool,
}

impl AnimState {
    pub fn new(seed: u64) -> Self {
        AnimState { rng: Rng::new(seed), count: 0.0, walk: 0.0, walk_init: false }
    }
}

pub fn is_enum_only(gen: &Generator) -> bool {
    matches!(gen, Generator::Cycle)
}

/// Build an `AnimSpec` from a generator name and raw `key=value` params.
/// Unknown generators or unparsable params are errors; unknown param keys for
/// a generator are ignored (forward-compatible).
pub fn build_anim(gen: &str, params: &[(String, String)]) -> Result<AnimSpec, String> {
    let g = match gen {
        "sine" => Generator::Sine,
        "ramp" => Generator::Ramp,
        "triangle" => Generator::Triangle,
        "square" => Generator::Square,
        "noise" => Generator::Noise,
        "walk" => Generator::Walk,
        "count" => Generator::Count,
        "cycle" => Generator::Cycle,
        other => return Err(format!("unknown generator {other:?}")),
    };
    let mut p = Params::default();
    // `period` for cycle defaults to 1.0
    if g == Generator::Cycle {
        p.period = 1.0;
    }
    for (k, v) in params {
        let f: f64 = v.parse().map_err(|_| format!("{gen}: param {k}={v:?} is not a number"))?;
        match k.as_str() {
            "amp" => p.amp = f,
            "offset" => p.offset = f,
            "period" => p.period = f,
            "phase" => p.phase = f,
            "min" => p.min = f,
            "max" => p.max = f,
            "lo" => p.lo = f,
            "hi" => p.hi = f,
            "duty" => p.duty = f,
            "start" => p.start = f,
            "step" => p.step = f,
            _ => {} // ignore unknown keys
        }
    }
    if p.period <= 0.0 {
        return Err(format!("{gen}: period must be positive"));
    }
    Ok(AnimSpec { gen: g, p })
}
```

- [ ] **Step 4: Implement `sample`**

Add to `anim.rs`:

```rust
/// Sample the generator at time `t` (seconds since animation start). Mutates
/// `st` for stateful generators (`walk`, `count`). Returns a raw number; the
/// caller coerces to the PV's wire type (or, for `cycle`, to an enum index).
pub fn sample(spec: &AnimSpec, st: &mut AnimState, t: f64) -> f64 {
    let p = &spec.p;
    match spec.gen {
        Generator::Sine => p.offset + p.amp * (2.0 * PI * t / p.period + p.phase).sin(),
        Generator::Ramp => {
            let frac = (t / p.period).rem_euclid(1.0);
            p.min + (p.max - p.min) * frac
        }
        Generator::Triangle => {
            let frac = (t / p.period).rem_euclid(1.0);
            let tri = if frac < 0.5 { frac * 2.0 } else { 2.0 - frac * 2.0 };
            p.min + (p.max - p.min) * tri
        }
        Generator::Square => {
            let frac = (t / p.period).rem_euclid(1.0);
            if frac < p.duty { p.hi } else { p.lo }
        }
        Generator::Noise => p.min + (p.max - p.min) * st.rng.next_f64(),
        Generator::Walk => {
            if !st.walk_init {
                st.walk = p.start;
                st.walk_init = true;
            }
            let delta = (st.rng.next_f64() - 0.5) * 2.0 * p.step;
            st.walk = (st.walk + delta).clamp(p.min, p.max);
            st.walk
        }
        Generator::Count => {
            let v = p.start + st.count * p.step;
            st.count += 1.0;
            v
        }
        Generator::Cycle => (t / p.period).floor(),
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p spvirit-tools --bin sptable`
Expected: PASS (all anim tests green).

- [ ] **Step 6: Commit**

```bash
git add spvirit-tools/src/bin/spvirit_table.rs spvirit-tools/src/bin/spvirit_table/anim.rs
git commit -m "feat(tools): sptable animation generators + PRNG"
```

---

### Task 6: Model + `ServerHandle` wiring for enum/table/animation

Replace the `Kind`/`ty` model with `PvSpec`, extend `ServerHandle` with enum/table adds, enum set (via `put_nt`), a shared animator map, and the background tick task. No command dispatch yet (Task 7) — this task just makes the plumbing compile and keeps the existing scalar/array behavior working.

**Files:**
- Modify: `spvirit-tools/src/bin/spvirit_table.rs`

**Interfaces:**
- Consumes: `parse::{WireType, coerce_scalar}`, `anim::{AnimSpec, AnimState, Generator, is_enum_only, sample}`, `RunningServer::{add_enum, add_table}`, `SimplePvStore::{put_nt, set_value, set_array_value, get_nt, remove}`.
- Produces:
  - `enum PvSpec { Scalar(WireType), Array(WireType), Enum { choices: Vec<String> }, Table { columns: Vec<(String, WireType)> } }` with `fn kind_label(&self) -> &'static str` and `fn type_label(&self) -> String`
  - `struct PvRow { name, writable, display, spec }`
  - `enum Target { Scalar(WireType), Enum(Vec<String>) }`
  - `struct Live { spec: AnimSpec, state: AnimState, start: Instant, target: Target }`
  - `type Animators = Arc<Mutex<HashMap<String, Live>>>` (`std::sync::Mutex`)
  - `ServerHandle` methods: `add_enum`, `add_table`, `set_enum(name, index, choices)`, `animators()`, and the spawned tick task started in `start`.

- [ ] **Step 1: Update imports and add the model types**

At the top of `spvirit_table.rs`, extend imports:

```rust
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use spvirit_types::NtPayload;

mod anim;
use anim::{AnimSpec, AnimState, Generator, is_enum_only, sample};
```

Replace the `Kind` enum and the `PvRow` struct with:

```rust
enum PvSpec {
    Scalar(WireType),
    Array(WireType),
    Enum { choices: Vec<String> },
    Table { columns: Vec<(String, WireType)> },
}

impl PvSpec {
    fn kind_label(&self) -> &'static str {
        match self {
            PvSpec::Scalar(_) => "scalar",
            PvSpec::Array(_) => "array",
            PvSpec::Enum { .. } => "enum",
            PvSpec::Table { .. } => "table",
        }
    }
    fn type_label(&self) -> String {
        match self {
            PvSpec::Scalar(t) | PvSpec::Array(t) => t.label().to_string(),
            PvSpec::Enum { .. } => "enum".to_string(),
            PvSpec::Table { .. } => "table".to_string(),
        }
    }
}

struct PvRow {
    name: String,
    writable: bool,
    display: String,
    spec: PvSpec,
}
```

- [ ] **Step 2: Add the animation target + shared map types**

Add near the model:

```rust
/// How a sampled value is applied to a PV.
enum Target {
    Scalar(WireType),
    Enum(Vec<String>),
}

/// A running animation: spec + mutable state + wall-clock origin + target.
struct Live {
    spec: AnimSpec,
    state: AnimState,
    start: Instant,
    target: Target,
}

type Animators = Arc<Mutex<HashMap<String, Live>>>;
```

- [ ] **Step 3: Extend `App` and `Mode`**

Add a `Command` and `Help` variant to `Mode`, and give `App` a handle to the animator map plus the tick rate for display:

```rust
enum Mode {
    Browse,
    AddName { buf: String },
    AddKind { name: String },
    AddType { name: String, kind: AddKind, idx: usize },
    AddChoices { name: String, buf: String },              // enum wizard
    AddIndex { name: String, choices: Vec<String>, buf: String },
    AddAccess { name: String, spec_kind: AddKind, ty: WireType, choices: Vec<String>, index: i32, writable: bool },
    AddValue { name: String, ty: WireType, is_array: bool, writable: bool, buf: String },
    Edit { row: usize, buf: String },
    Command { buf: String },
    Help,
}
```

Note: `AddKind` is a small enum for the wizard branch selector — add it next to `Mode`:

```rust
#[derive(Copy, Clone)]
enum AddKind { Scalar, Array, Enum }
```

Extend `App`:

```rust
struct App {
    rows: Vec<PvRow>,
    table: TableState,
    mode: Mode,
    status: String,
    tcp_port: u16,
    udp_port: u16,
    animators: Animators,
    rate_hz: f64,
}
```

(The full wizard `Mode` rewiring is completed in Task 8; for this task, keep the existing scalar/array wizard arms compiling by mapping the old `Kind` to `AddKind::Scalar`/`AddKind::Array` and leaving the enum arms unused. It is acceptable for this task's build to warn about unused enum-wizard variants — Task 8 wires them.)

- [ ] **Step 4: Extend `ServerHandle` with enum/table/animation plumbing**

Modify `ServerHandle::start` to create the shared map and spawn the tick task, and add the new methods:

```rust
struct ServerHandle {
    rt: tokio::runtime::Runtime,
    server: RunningServer,
    animators: Animators,
}

impl ServerHandle {
    fn start(tcp: u16, udp: u16, rate_hz: f64) -> Result<Self, Box<dyn std::error::Error>> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let server = rt.block_on(async {
            PvaServer::serve(Vec::<AnyPv>::new())
                .port(tcp)
                .udp_port(udp)
                .listen_ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
                .start()
                .await
        });
        let animators: Animators = Arc::new(Mutex::new(HashMap::new()));

        // Background tick task: sample all animators and write to the store.
        let store = server.store().clone();
        let anim_map = animators.clone();
        let period = Duration::from_secs_f64(1.0 / rate_hz);
        rt.spawn(async move {
            let mut ticker = tokio::time::interval(period);
            loop {
                ticker.tick().await;
                // Compute under the lock; do NOT hold it across awaits.
                let updates: Vec<(String, ScalarValue, Option<Vec<String>>, i32)> = {
                    let mut map = anim_map.lock().unwrap();
                    map.iter_mut()
                        .map(|(name, live)| {
                            let t = live.start.elapsed().as_secs_f64();
                            let raw = sample(&live.spec, &mut live.state, t);
                            match &live.target {
                                Target::Scalar(ty) => {
                                    (name.clone(), coerce_scalar(raw, *ty), None, 0)
                                }
                                Target::Enum(choices) => {
                                    let n = choices.len().max(1) as i64;
                                    let idx = (raw as i64).rem_euclid(n) as i32;
                                    (name.clone(), ScalarValue::I32(idx), Some(choices.clone()), idx)
                                }
                            }
                        })
                        .collect()
                };
                for (name, sval, choices, idx) in updates {
                    match choices {
                        None => { store.set_value(&name, sval).await; }
                        Some(choices) => {
                            let nt = NtPayload::Enum(spvirit_types::NtEnum::new(idx, choices));
                            store.put_nt(&name, nt).await;
                        }
                    }
                }
            }
        });

        Ok(Self { rt, server, animators })
    }

    fn add_scalar(&self, name: &str, v: ScalarValue, writable: bool) {
        self.rt.block_on(self.server.add_scalar(name, v, writable));
    }
    fn add_array(&self, name: &str, v: ScalarArrayValue, writable: bool) {
        self.rt.block_on(self.server.add_array(name, v, writable));
    }
    fn add_enum(&self, name: &str, choices: Vec<String>, index: i32, writable: bool) {
        self.rt.block_on(self.server.add_enum(name, choices, index, writable));
    }
    fn add_table(&self, name: &str, columns: Vec<(String, ScalarArrayValue)>) {
        self.rt.block_on(self.server.add_table(name, columns));
    }
    fn set_scalar(&self, name: &str, v: ScalarValue) {
        self.rt.block_on(self.server.store().set_value(name, v));
    }
    fn set_array(&self, name: &str, v: ScalarArrayValue) {
        self.rt.block_on(self.server.store().set_array_value(name, v));
    }
    fn set_enum(&self, name: &str, index: i32, choices: Vec<String>) {
        let nt = NtPayload::Enum(spvirit_types::NtEnum::new(index, choices));
        self.rt.block_on(self.server.store().put_nt(name, nt));
    }
    fn remove(&self, name: &str) -> bool {
        self.rt.block_on(self.server.store().remove(name))
    }
    fn exists(&self, name: &str) -> bool {
        self.rt.block_on(self.server.store().get_value(name)).is_some()
    }
    fn read_scalar(&self, name: &str) -> Option<String> {
        self.rt.block_on(self.server.store().get_value(name)).map(|v| format_scalar(&v))
    }
    /// Read an enum row's current display: `index (choice)`.
    fn read_enum(&self, name: &str) -> Option<String> {
        match self.rt.block_on(self.server.store().get_nt(name)) {
            Some(NtPayload::Enum(e)) => {
                let choice = e.selected().unwrap_or("?");
                Some(format!("{} ({})", e.index, choice))
            }
            _ => None,
        }
    }
    fn animators(&self) -> &Animators {
        &self.animators
    }
    fn abort(&self) {
        self.server.abort();
    }
}
```

Add `use parse::coerce_scalar;` to the `parse` import line.

- [ ] **Step 5: Update `main` to pass the rate and seed `App.animators`**

In `main`, add a `--rate` option (default 10.0), pass it to `ServerHandle::start`, and initialise `App`:

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use argparse::{ArgumentParser, Store};
    let mut tcp_port: u16 = 5075;
    let mut udp_port: u16 = 5076;
    let mut rate_hz: f64 = 10.0;
    {
        let mut ap = ArgumentParser::new();
        ap.set_description("Interactive spreadsheet IOC — each row is a PV");
        ap.refer(&mut tcp_port).add_option(&["--port"], Store, "TCP port (default 5075)");
        ap.refer(&mut udp_port).add_option(&["--udp-port"], Store, "UDP search port (default 5076)");
        ap.refer(&mut rate_hz).add_option(&["--rate"], Store, "animation tick rate Hz (default 10)");
        ap.parse_args_or_exit();
    }
    if rate_hz <= 0.0 {
        eprintln!("--rate must be positive");
        std::process::exit(2);
    }

    let srv = ServerHandle::start(tcp_port, udp_port, rate_hz)?;

    color_eyre::install()?;
    let terminal = ratatui::init();
    let app = App {
        rows: Vec::new(),
        table: TableState::default(),
        mode: Mode::Browse,
        status: format!("serving on 127.0.0.1:{tcp_port} — 'a' add · ':' cmd · '?' help"),
        tcp_port,
        udp_port,
        animators: srv.animators().clone(),
        rate_hz,
    };
    let result = run_ui(terminal, app, &srv);
    ratatui::restore();
    srv.abort();
    result
}
```

- [ ] **Step 6: Update `refresh_scalars` to also refresh enum rows and build**

Replace `refresh_scalars` with:

```rust
fn refresh_values(app: &mut App, srv: &ServerHandle) {
    for r in app.rows.iter_mut() {
        match &r.spec {
            PvSpec::Scalar(_) => {
                if let Some(s) = srv.read_scalar(&r.name) {
                    r.display = s;
                }
            }
            PvSpec::Enum { .. } => {
                if let Some(s) = srv.read_enum(&r.name) {
                    r.display = s;
                }
            }
            _ => {}
        }
    }
}
```

Update the caller in `run_ui` from `refresh_scalars(&mut app, srv)` to `refresh_values(&mut app, srv)`.

Run: `cargo build -p spvirit-tools --bin sptable`
Expected: builds (warnings about not-yet-used enum wizard variants and command dispatch are acceptable — resolved in Tasks 7–8). Fix any hard errors by mapping the existing wizard arms onto `AddKind`.

- [ ] **Step 7: Run unit tests**

Run: `cargo test -p spvirit-tools --bin sptable`
Expected: PASS (pure-module tests unaffected).

- [ ] **Step 8: Commit**

```bash
git add spvirit-tools/src/bin/spvirit_table.rs
git commit -m "feat(tools): sptable PvSpec model + animation tick task plumbing"
```

---

### Task 7: Command execution, `:`/`?` input, enum/table rendering

Wire the command line: `:` enters `Mode::Command`, Enter runs `parse_command` → `expand_pattern` → executor; `?` opens `Mode::Help`. Render enum/table rows and the `~` animation marker.

**Files:**
- Modify: `spvirit-tools/src/bin/spvirit_table.rs`

**Interfaces:**
- Consumes: `parse::{Command, SpecInput, parse_command, parse_scalar, parse_array}`, `pattern::expand_pattern`, `anim::build_anim`, Task 6 `ServerHandle` methods + `Animators`.
- Produces: `fn exec_command(app, srv, line: &str)`, plus `Mode::Command`/`Mode::Help` handling in `on_key` and rendering updates in `draw`.

- [ ] **Step 1: Add executor helpers (spec resolution + table columns)**

Add these free functions:

```rust
use parse::{Command, SpecInput, parse_command};
use pattern::expand_pattern;
use anim::build_anim;

/// Parse a `col:type=v1,v2 ...` table value into typed columns + a WireType map
/// (for the row's PvSpec). Returns (columns, column-types).
fn parse_table_value(
    val: &str,
) -> Result<(Vec<(String, ScalarArrayValue)>, Vec<(String, WireType)>), String> {
    let mut columns = Vec::new();
    let mut types = Vec::new();
    for col in val.split_whitespace() {
        let (name, rest) = col.split_once(':').ok_or_else(|| format!("bad column {col:?} (want name:type=v,v)"))?;
        let (tytok, vals) = rest.split_once('=').ok_or_else(|| format!("bad column {col:?} (want name:type=v,v)"))?;
        let ty = WireType::from_token(tytok).ok_or_else(|| format!("unknown column type {tytok:?}"))?;
        let arr = parse_array(ty, vals)?;
        columns.push((name.to_string(), arr));
        types.push((name.to_string(), ty));
    }
    if columns.is_empty() {
        return Err("table needs at least one column".into());
    }
    Ok((columns, types))
}

/// Split an enum value field `A,B,C [index]` into choices + index.
fn parse_enum_value(val: &str) -> Result<(Vec<String>, i32), String> {
    let val = val.trim();
    if val.is_empty() {
        return Err("enum needs choices, e.g. OFF,ON,TRIP 1".into());
    }
    // Optional trailing integer index separated by whitespace.
    let (choices_part, index) = match val.rsplit_once(char::is_whitespace) {
        Some((head, tail)) if tail.parse::<i32>().is_ok() => (head, tail.parse::<i32>().unwrap()),
        _ => (val, 0),
    };
    let choices: Vec<String> = choices_part.split(',').map(|c| c.trim().to_string()).collect();
    if choices.iter().any(|c| c.is_empty()) {
        return Err("empty enum choice".into());
    }
    Ok((choices, index))
}
```

- [ ] **Step 2: Add the row helpers and the executor**

```rust
fn row_index(app: &App, name: &str) -> Option<usize> {
    app.rows.iter().position(|r| r.name == name)
}

fn select_row(app: &mut App, name: &str) {
    if let Some(i) = row_index(app, name) {
        app.table.select(Some(i));
    }
}

/// Remove an animator for `name`, if any. Returns true if one was removed.
fn stop_anim(app: &App, name: &str) -> bool {
    app.animators.lock().unwrap().remove(name).is_some()
}

fn exec_command(app: &mut App, srv: &ServerHandle, line: &str) {
    let cmd = match parse_command(line) {
        Ok(c) => c,
        Err(e) => { app.status = e; return; }
    };
    match cmd {
        Command::Quit => { /* handled by caller via a sentinel below */ }
        Command::Help => { app.mode = Mode::Help; }
        Command::Rate { .. } => {
            app.status = "rate changes require restart (--rate); tick task is fixed at startup".into();
        }
        Command::Add { pattern, spec, writable, value } => exec_add(app, srv, &pattern, spec, writable, &value),
        Command::Set { pattern, value } => exec_set(app, srv, &pattern, &value),
        Command::Del { pattern } => exec_del(app, srv, pattern),
        Command::Rename { old, new } => exec_rename(app, srv, &old, &new),
        Command::Access { pattern, writable } => exec_access(app, srv, &pattern, writable),
        Command::Anim { pattern, gen, params } => exec_anim(app, srv, &pattern, &gen, &params),
        Command::Stop { pattern } => exec_stop(app, srv, pattern),
        Command::Source { path } => exec_source(app, srv, &path),
    }
}
```

Note on `:rate` — the tick task's interval is fixed when the runtime task is spawned. Rather than add live-reconfiguration plumbing (YAGNI), `:rate` reports that the rate is set via `--rate` at startup. `App.rate_hz` is shown in the title bar. (If live rate is wanted later, the tick task can read an `Arc<AtomicU64>`.)

- [ ] **Step 3: Implement the per-verb executors**

```rust
fn exec_add(app: &mut App, srv: &ServerHandle, pattern: &str, spec: SpecInput, writable: bool, value: &str) {
    let names = match expand_pattern(pattern) {
        Ok(n) => n,
        Err(e) => { app.status = e; return; }
    };
    let (mut added, mut skipped, mut last) = (0usize, 0usize, None);
    for name in names {
        if row_index(app, &name).is_some() || srv.exists(&name) {
            skipped += 1;
            continue;
        }
        let (pvspec, display) = match &spec {
            SpecInput::Scalar(ty) => match parse_scalar(*ty, value) {
                Ok(v) => { srv.add_scalar(&name, v.clone(), writable); (PvSpec::Scalar(*ty), format_scalar(&v)) }
                Err(e) => { app.status = e; return; }
            },
            SpecInput::Array(ty) => match parse_array(*ty, value) {
                Ok(v) => { srv.add_array(&name, v.clone(), writable); (PvSpec::Array(*ty), format_array(&v)) }
                Err(e) => { app.status = e; return; }
            },
            SpecInput::Enum => match parse_enum_value(value) {
                Ok((choices, index)) => {
                    srv.add_enum(&name, choices.clone(), index, writable);
                    let disp = choices.get(index.max(0) as usize).cloned().unwrap_or_else(|| "?".into());
                    (PvSpec::Enum { choices }, format!("{index} ({disp})"))
                }
                Err(e) => { app.status = e; return; }
            },
            SpecInput::Table => match parse_table_value(value) {
                Ok((columns, types)) => {
                    let ncols = columns.len();
                    let nrows = columns.first().map(|(_, a)| a.len()).unwrap_or(0);
                    srv.add_table(&name, columns);
                    (PvSpec::Table { columns: types }, format!("{ncols} cols × {nrows} rows"))
                }
                Err(e) => { app.status = e; return; }
            },
        };
        // Tables are always RW at the store layer; reflect that in the row.
        let row_writable = if matches!(spec, SpecInput::Table) { true } else { writable };
        app.rows.push(PvRow { name: name.clone(), writable: row_writable, display, spec: pvspec });
        added += 1;
        last = Some(name);
    }
    if let Some(name) = last { select_row(app, &name); }
    app.status = format!("added {added}, skipped {skipped} (exist)");
}

fn exec_set(app: &mut App, srv: &ServerHandle, pattern: &str, value: &str) {
    let names = match expand_pattern(pattern) {
        Ok(n) => n,
        Err(e) => { app.status = e; return; }
    };
    let mut set = 0usize;
    for name in names {
        let Some(i) = row_index(app, &name) else { continue; };
        if stop_anim(app, &name) {
            app.status = format!("{name}: animation stopped by manual set");
        }
        // Reborrow row fields we need, cloning to avoid overlapping borrows.
        let spec_kind = app.rows[i].spec.kind_label();
        match spec_kind {
            "scalar" => {
                if let PvSpec::Scalar(ty) = app.rows[i].spec {
                    match parse_scalar(ty, value) {
                        Ok(v) => { srv.set_scalar(&name, v.clone()); app.rows[i].display = format_scalar(&v); set += 1; }
                        Err(e) => { app.status = e; return; }
                    }
                }
            }
            "array" => {
                if let PvSpec::Array(ty) = app.rows[i].spec {
                    match parse_array(ty, value) {
                        Ok(v) => { srv.set_array(&name, v.clone()); app.rows[i].display = format_array(&v); set += 1; }
                        Err(e) => { app.status = e; return; }
                    }
                }
            }
            "enum" => {
                if let PvSpec::Enum { choices } = &app.rows[i].spec {
                    let choices = choices.clone();
                    // value may be a choice name or an integer index
                    let index = match choices.iter().position(|c| c == value.trim()) {
                        Some(p) => p as i32,
                        None => match value.trim().parse::<i32>() {
                            Ok(n) => n,
                            Err(_) => { app.status = format!("{name}: {value:?} is not a choice or index"); return; }
                        },
                    };
                    srv.set_enum(&name, index, choices.clone());
                    let disp = choices.get(index.max(0) as usize).cloned().unwrap_or_else(|| "?".into());
                    app.rows[i].display = format!("{index} ({disp})");
                    set += 1;
                }
            }
            _ => { app.status = format!("{name}: cannot :set a table (recreate with :add)"); return; }
        }
    }
    app.status = format!("set {set}");
}

fn exec_del(app: &mut App, srv: &ServerHandle, pattern: Option<String>) {
    let names: Vec<String> = match pattern {
        Some(p) => match expand_pattern(&p) { Ok(n) => n, Err(e) => { app.status = e; return; } },
        None => match app.table.selected().map(|i| app.rows[i].name.clone()) {
            Some(n) => vec![n],
            None => { app.status = "nothing selected".into(); return; }
        },
    };
    let mut removed = 0usize;
    for name in names {
        stop_anim(app, &name);
        srv.remove(&name);
        if let Some(i) = row_index(app, &name) {
            app.rows.remove(i);
            removed += 1;
        }
    }
    if app.rows.is_empty() { app.table.select(None); }
    else {
        let i = app.table.selected().unwrap_or(0).min(app.rows.len() - 1);
        app.table.select(Some(i));
    }
    app.status = format!("removed {removed}");
}

fn exec_rename(app: &mut App, srv: &ServerHandle, old: &str, new: &str) {
    let Some(i) = row_index(app, old) else { app.status = format!("{old}: no such PV"); return; };
    if row_index(app, new).is_some() || srv.exists(new) {
        app.status = format!("{new}: already exists"); return;
    }
    // Recreate under the new name from the current spec + value, then drop old.
    stop_anim(app, old);
    let writable = app.rows[i].writable;
    match &app.rows[i].spec {
        PvSpec::Scalar(ty) => {
            let ty = *ty;
            if let Some(cur) = srv.read_scalar(old) {
                if let Ok(v) = parse_scalar(ty, &cur) { srv.add_scalar(new, v, writable); }
            }
        }
        PvSpec::Enum { choices } => {
            let choices = choices.clone();
            let index = srv.read_enum(old)
                .and_then(|d| d.split_whitespace().next().map(|s| s.to_string()))
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0);
            srv.add_enum(new, choices, index, writable);
        }
        PvSpec::Array(_) | PvSpec::Table { .. } => {
            app.status = "rename supports scalar/enum only (recreate arrays/tables with :add)".into();
            return;
        }
    }
    srv.remove(old);
    let mut row = app.rows.remove(i);
    row.name = new.to_string();
    app.rows.push(row);
    select_row(app, new);
    app.status = format!("renamed {old} -> {new}");
}

fn exec_access(app: &mut App, srv: &ServerHandle, pattern: &str, writable: bool) {
    let names = match expand_pattern(pattern) { Ok(n) => n, Err(e) => { app.status = e; return; } };
    let mut changed = 0usize;
    for name in names {
        let Some(i) = row_index(app, &name) else { continue; };
        // Only scalar/enum have a meaningful access recreate.
        match app.rows[i].spec {
            PvSpec::Scalar(ty) => {
                if let Some(cur) = srv.read_scalar(&name) {
                    if let Ok(v) = parse_scalar(ty, &cur) {
                        srv.add_scalar(&name, v, writable);
                        app.rows[i].writable = writable;
                        changed += 1;
                    }
                }
            }
            PvSpec::Enum { .. } => {
                if let PvSpec::Enum { choices } = &app.rows[i].spec {
                    let choices = choices.clone();
                    let index = srv.read_enum(&name)
                        .and_then(|d| d.split_whitespace().next().map(str::to_string))
                        .and_then(|s| s.parse::<i32>().ok()).unwrap_or(0);
                    srv.add_enum(&name, choices, index, writable);
                    app.rows[i].writable = writable;
                    changed += 1;
                }
            }
            _ => {}
        }
    }
    app.status = format!("access changed on {changed}");
}

fn exec_anim(app: &mut App, srv: &ServerHandle, pattern: &str, gen: &str, params: &[(String, String)]) {
    let spec = match build_anim(gen, params) { Ok(s) => s, Err(e) => { app.status = e; return; } };
    let names = match expand_pattern(pattern) { Ok(n) => n, Err(e) => { app.status = e; return; } };
    let mut on = 0usize;
    let mut seed: u64 = 0x9E3779B97F4A7C15;
    for name in names {
        let Some(i) = row_index(app, &name) else { continue; };
        let target = match &app.rows[i].spec {
            PvSpec::Scalar(ty) if *ty != WireType::Str => Target::Scalar(*ty),
            PvSpec::Enum { choices } if is_enum_only(&spec.gen) => Target::Enum(choices.clone()),
            PvSpec::Enum { .. } => { app.status = format!("{name}: enum takes only the 'cycle' generator"); return; }
            _ => { app.status = format!("{name}: only numeric scalars (and enum+cycle) are animatable"); return; }
        };
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(i as u64 + 1);
        let live = Live { spec, state: AnimState::new(seed), start: Instant::now(), target };
        app.animators.lock().unwrap().insert(name, live);
        on += 1;
    }
    app.status = format!("animating {on} ({gen})");
}

fn exec_stop(app: &mut App, _srv: &ServerHandle, pattern: Option<String>) {
    let names: Vec<String> = match pattern {
        Some(p) => match expand_pattern(&p) { Ok(n) => n, Err(e) => { app.status = e; return; } },
        None => match app.table.selected().map(|i| app.rows[i].name.clone()) {
            Some(n) => vec![n],
            None => { app.status = "nothing selected".into(); return; }
        },
    };
    let mut stopped = 0usize;
    for name in names { if stop_anim(app, &name) { stopped += 1; } }
    app.status = format!("stopped {stopped}");
}

fn exec_source(app: &mut App, srv: &ServerHandle, path: &str) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => { app.status = format!("source {path}: {e}"); return; }
    };
    let (mut n, mut errs) = (0usize, Vec::new());
    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        let line = line.strip_prefix(':').unwrap_or(line);
        // Reuse exec_command but capture parse errors per line.
        match parse_command(line) {
            Ok(_) => { exec_command(app, srv, line); n += 1; }
            Err(e) => errs.push(format!("line {}: {e}", lineno + 1)),
        }
    }
    app.status = if errs.is_empty() {
        format!("sourced {n} commands")
    } else {
        format!("sourced {n} commands, {} errors: {}", errs.len(), errs.join("; "))
    };
}
```

- [ ] **Step 4: Handle `:` and `?` in Browse; add Command/Help input arms**

In `on_key`, in the `Mode::Browse` match, add:

```rust
            KeyCode::Char(':') => app.mode = Mode::Command { buf: String::new() },
            KeyCode::Char('?') => app.mode = Mode::Help,
```

Add the two new arms to the outer `match std::mem::replace(&mut app.mode, Mode::Browse)`:

```rust
        Mode::Command { mut buf } => match code {
            KeyCode::Esc => {}
            KeyCode::Enter => {
                if parse_command(buf.trim()) == Ok(Command::Quit) {
                    return true;
                }
                exec_command(app, srv, buf.trim());
            }
            KeyCode::Char(c) => { buf.push(c); app.mode = Mode::Command { buf }; }
            KeyCode::Backspace => { buf.pop(); app.mode = Mode::Command { buf }; }
            _ => app.mode = Mode::Command { buf },
        },
        Mode::Help => match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {}
            _ => app.mode = Mode::Help,
        },
```

Note: `Command` and `parse_command` are already imported in Step 1. The `Quit` sentinel is checked here (not in `exec_command`) so the loop can return `true`.

- [ ] **Step 5: Render enum/table rows, the `~` marker, and the Command/Help overlays**

In `draw`, replace the body-row builder so the Value column shows a `~` prefix for animated rows, and update the title hint:

```rust
    let animated = app.animators.lock().unwrap();
    let body = app.rows.iter().map(|r| {
        let mark = if animated.contains_key(&r.name) { "~" } else { "" };
        Row::new([
            Cell::from(r.name.clone()),
            Cell::from(r.spec.kind_label()),
            Cell::from(r.spec.type_label()),
            Cell::from(if r.writable { "RW" } else { "RO" }),
            Cell::from(format!("{mark}{}", r.display)),
        ])
    }).collect::<Vec<_>>();
    drop(animated);
```

(Change the `Table::new(body, widths)` call to take the collected `Vec`.) Update the title format string to end with `(a add · : cmd · ? help · q quit)` and include `rate {rate_hz}Hz`.

Extend `prompt_text` for the two new modes:

```rust
        Mode::Command { buf } => Some((" :command (Enter run · Esc cancel) ", format!(":{buf}"))),
        Mode::Help => Some((" help (Esc to close) ", help_text())),
```

Add the help text function:

```rust
fn help_text() -> String {
    "\
Commands (prefix :) — shorthands in ( )
  add|a  <name> <type> [ro|rw] <value>   add PV(s)
  set|s  <name> <value>                  set value (choice name or index for enum)
  del|d  [name]                          delete (blank = selected row)
  rename|mv <old> <new>                  rename (scalar/enum)
  ro|rw  <name>                          set advertised access
  anim   <name> <gen> [k=v ...]          animate
  stop   [name]                          stop animation (blank = selected)
  source|so <file>                       run a file of commands
  rate   <hz>                            (set at startup via --rate)
  help|h    quit|q

Types: bool int8 int16 int32(int) int64(long) uint8 uint16 uint32 uint64
       float(f32) double(f64) string(s) ; arrays: int32[]  ; enum ; table
Patterns: {1..8} {8..1} {0..100..10} {01..12} {A,B,C}  and products S{1..4}:{A,B}
Generators: sine ramp triangle square noise walk count  (enum: cycle)
  e.g. :anim RING:BPM{01..99} noise min=-1 max=1
Value forms: enum -> OFF,ON,TRIP 1   table -> id:i32=1,2,3 x:f64=0.5,1.5"
        .to_string()
}
```

Also widen the help modal: in `draw`, when `matches!(app.mode, Mode::Help)`, use `centered(80, 70, frame.area())` instead of the default modal size.

- [ ] **Step 6: Build and run unit tests**

Run: `cargo build -p spvirit-tools --bin sptable`
Expected: builds clean (resolve any remaining unused-import warnings).

Run: `cargo test -p spvirit-tools --bin sptable`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add spvirit-tools/src/bin/spvirit_table.rs
git commit -m "feat(tools): sptable command line, patterns, animation, enum/table rendering"
```

---

### Task 8: Wizard enum branch + title hint

Extend the modal `a` wizard with an `[e]num` branch (choices → index → access) and finalize the `AddKind` wiring left open in Task 6. Table stays command-line-only.

**Files:**
- Modify: `spvirit-tools/src/bin/spvirit_table.rs`

- [ ] **Step 1: Update `AddKind` prompt and branch**

In `prompt_text`, update the kind prompt:

```rust
        Mode::AddKind { .. } => Some((" kind: [s]calar / [a]rray / [e]num (table via :add) ", String::new())),
```

In `on_key`, the `Mode::AddKind` arm:

```rust
        Mode::AddKind { name } => match code {
            KeyCode::Esc => {}
            KeyCode::Char('s') => app.mode = Mode::AddType { name, kind: AddKind::Scalar, idx: 0 },
            KeyCode::Char('a') => app.mode = Mode::AddType { name, kind: AddKind::Array, idx: 0 },
            KeyCode::Char('e') => app.mode = Mode::AddChoices { name, buf: String::new() },
            _ => app.mode = Mode::AddKind { name },
        },
```

- [ ] **Step 2: Add the enum wizard arms**

```rust
        Mode::AddChoices { name, mut buf } => match code {
            KeyCode::Esc => {}
            KeyCode::Enter if !buf.trim().is_empty() => {
                let choices: Vec<String> = buf.split(',').map(|c| c.trim().to_string()).collect();
                app.mode = Mode::AddIndex { name, choices, buf: String::new() };
            }
            KeyCode::Char(c) => { buf.push(c); app.mode = Mode::AddChoices { name, buf }; }
            KeyCode::Backspace => { buf.pop(); app.mode = Mode::AddChoices { name, buf }; }
            _ => app.mode = Mode::AddChoices { name, buf },
        },
        Mode::AddIndex { name, choices, mut buf } => match code {
            KeyCode::Esc => {}
            KeyCode::Enter => {
                let index: i32 = buf.trim().parse().unwrap_or(0);
                app.mode = Mode::AddAccess {
                    name, spec_kind: AddKind::Enum, ty: WireType::I32, choices, index, writable: true,
                };
            }
            KeyCode::Char(c) => { buf.push(c); app.mode = Mode::AddIndex { name, choices, buf }; }
            KeyCode::Backspace => { buf.pop(); app.mode = Mode::AddIndex { name, choices, buf }; }
            _ => app.mode = Mode::AddIndex { name, choices, buf },
        },
```

- [ ] **Step 3: Rework `Mode::AddAccess` and `Mode::AddValue` to carry `AddKind`/enum data**

Replace the existing `AddAccess`/`AddValue` arms so that on `Enter` at access, an **enum** finalizes immediately (choices+index already known) while scalar/array proceed to a value prompt:

```rust
        Mode::AddAccess { name, spec_kind, ty, choices, index, mut writable } => match code {
            KeyCode::Esc => {}
            KeyCode::Char('r') => { writable = false; app.mode = Mode::AddAccess { name, spec_kind, ty, choices, index, writable }; }
            KeyCode::Char('w') => { writable = true; app.mode = Mode::AddAccess { name, spec_kind, ty, choices, index, writable }; }
            KeyCode::Enter => match spec_kind {
                AddKind::Enum => {
                    if row_index(app, &name).is_some() || srv.exists(&name) {
                        app.status = format!("name {name:?} already exists");
                    } else {
                        srv.add_enum(&name, choices.clone(), index, writable);
                        let disp = choices.get(index.max(0) as usize).cloned().unwrap_or_else(|| "?".into());
                        app.rows.push(PvRow {
                            name: name.clone(), writable, display: format!("{index} ({disp})"),
                            spec: PvSpec::Enum { choices },
                        });
                        app.table.select(Some(app.rows.len() - 1));
                        app.status = format!("added {name}");
                    }
                }
                AddKind::Scalar | AddKind::Array => {
                    let is_array = matches!(spec_kind, AddKind::Array);
                    app.mode = Mode::AddValue { name, ty, is_array, writable, buf: String::new() };
                }
            },
            _ => app.mode = Mode::AddAccess { name, spec_kind, ty, choices, index, writable },
        },
        Mode::AddValue { name, ty, is_array, writable, mut buf } => match code {
            KeyCode::Esc => {}
            KeyCode::Enter => {
                let spec = if is_array { SpecInput::Array(ty) } else { SpecInput::Scalar(ty) };
                // Reuse exec_add's single-name path via a one-shot pattern.
                let before = app.rows.len();
                exec_add(app, srv, &name, spec, writable, &buf);
                if app.rows.len() == before {
                    // add failed (parse error) — keep the value prompt open
                    app.mode = Mode::AddValue { name, ty, is_array, writable, buf };
                }
            }
            KeyCode::Char(c) => { buf.push(c); app.mode = Mode::AddValue { name, ty, is_array, writable, buf }; }
            KeyCode::Backspace => { buf.pop(); app.mode = Mode::AddValue { name, ty, is_array, writable, buf }; }
            _ => app.mode = Mode::AddValue { name, ty, is_array, writable, buf },
        },
```

Update `Mode::AddType`'s `Enter` arm to route into the reworked `AddAccess`:

```rust
            KeyCode::Enter => {
                app.mode = Mode::AddAccess {
                    name, spec_kind: kind, ty: WireType::ALL[idx], choices: Vec::new(), index: 0, writable: true,
                };
            }
```

And `prompt_text` arms for the new modes:

```rust
        Mode::AddChoices { buf, .. } => Some((" enum choices, comma-separated (Enter) ", buf.clone())),
        Mode::AddIndex { buf, .. } => Some((" initial index (Enter, default 0) ", buf.clone())),
        Mode::AddValue { buf, ty, is_array, .. } => Some((
            if *is_array { " values, comma-separated (Enter) " } else { " initial value (Enter) " },
            format!("[{}] {buf}", ty.label()),
        )),
```

(The old `AddValue`/`AddAccess` prompt arms are replaced by these.)

- [ ] **Step 4: Build and smoke the wizard compiles**

Run: `cargo build -p spvirit-tools --bin sptable`
Expected: builds clean.

Run: `cargo test -p spvirit-tools --bin sptable`
Expected: PASS.

- [ ] **Step 5: Manual smoke test**

Start: `cargo run -p spvirit-tools --bin sptable -- --port 5099 --udp-port 5100 --rate 10`

1. Press `a`, type `SIM:E`, Enter; press `e`; type `OFF,ON,TRIP`, Enter; type `1`, Enter; press `w`, Enter. Enum row shows `1 (ON)`.
2. Press `:`, type `add RING:BPM{01..04} f64 rw 0`, Enter. Four rows appear; status `added 4, skipped 0`.
3. Press `:`, type `anim RING:BPM{01..04} sine amp=5 period=2`, Enter. The four Value cells show live `~`-prefixed values changing.
4. In another terminal: `cargo run -p spvirit-tools --bin spget -- --server 127.0.0.1:5099 RING:BPM01` → a moving value.
5. Press `:`, `stop RING:BPM{01..04}`, Enter → values freeze, `~` gone.
6. Select the enum row, press `:`, `set SIM:E TRIP`, Enter → shows `2 (TRIP)`.
7. Press `?` → help modal; Esc closes.
8. `q` to quit.

Confirm each step, then continue.

- [ ] **Step 6: Commit**

```bash
git add spvirit-tools/src/bin/spvirit_table.rs
git commit -m "feat(tools): sptable enum wizard branch + help/command hints"
```

---

### Task 9: Docs + optional wire test

**Files:**
- Modify: `docs/dev-guide/04-client-and-tools.md`
- Modify: `spvirit-tools/tests/sptable_dynamic.rs` (extend)

- [ ] **Step 1: Update the tools table row**

In `docs/dev-guide/04-client-and-tools.md`, replace the existing `sptable` row with:

```markdown
| `sptable` | ~1200 | ratatui TUI spreadsheet IOC. Rows are dynamically added PVs: 12 scalar types, arrays, **NTEnum**, **NTTable**. Modal `a` wizard **plus a vim-style `:` command line** (`:add/:set/:del/:mv/:ro/:rw/:anim/:stop/:source`, shorthands, `:help`). Bash-style **pattern expansion** (`RING:BPM{01..99}`, products) for bulk ops. **Animation** generators (sine/ramp/triangle/square/noise/walk/count, enum `cycle`) driven by a server-side tick (`--rate`, default 10 Hz). |
```

- [ ] **Step 2: Add a short usage subsection**

Below the table, add a `#### sptable command reference` subsection documenting: the verb table, typespec aliases, pattern forms, enum/table value syntax, and generator list (mirror `help_text()` in the binary so they stay in sync). Keep it under ~30 lines.

- [ ] **Step 3: Extend the wire test with an enum add + set-by-choice**

Append to `spvirit-tools/tests/sptable_dynamic.rs` a new test:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_enum_add_and_set_over_wire() {
    let (Some(tcp), Some(udp)) = (free_tcp_port(), free_udp_port()) else {
        eprintln!("Skipping: cannot bind ports");
        return;
    };
    let server = PvaServer::serve(Vec::<AnyPv>::new())
        .port(tcp).udp_port(udp)
        .listen_ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
        .start().await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    server.add_enum("DYN:ENUM", vec!["OFF".into(), "ON".into(), "TRIP".into()], 0, true).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let r = pvget(&opts("DYN:ENUM", tcp, udp, Duration::from_secs(5))).await.expect("pvget enum");
    assert!(format_compact_value(&r.value).contains("OFF"), "got {}", format_compact_value(&r.value));

    // set index 2 via put_nt
    server.store().put_nt("DYN:ENUM",
        spvirit_types::NtPayload::Enum(spvirit_types::NtEnum::new(2, vec!["OFF".into(), "ON".into(), "TRIP".into()]))).await;
    let r = pvget(&opts("DYN:ENUM", tcp, udp, Duration::from_secs(5))).await.expect("pvget enum 2");
    assert!(format_compact_value(&r.value).contains("TRIP"), "got {}", format_compact_value(&r.value));

    server.abort();
}
```

Note: if `format_compact_value` renders the enum index rather than the choice string, assert on the index (`"2"`) instead — adjust after seeing the first run.

- [ ] **Step 4: Run the wire test**

Run: `cargo test -p spvirit-tools --test sptable_dynamic`
Expected: PASS (both tests).

- [ ] **Step 5: Commit**

```bash
git add docs/dev-guide/04-client-and-tools.md spvirit-tools/tests/sptable_dynamic.rs
git commit -m "docs+test: document sptable types/commands/animation; wire enum test"
```

---

## Self-Review

**Spec coverage:**
- Enum + Table kinds → Task 1 (server primitives) + Task 6 (`PvSpec`) + Task 7 (`:add` enum/table paths, rendering) + Task 8 (enum wizard). ✓
- `:` command system, verbs + shorthands → Task 3 (`parse_command`) + Task 7 (executor, input). ✓
- `:help` → Task 7 (`help_text`, `Mode::Help`). ✓
- Typespec aliases → Task 3 (`from_token`, `parse_typespec`). ✓
- Pattern expansion (ranges/pad/list/product/cap) → Task 4 (`expand_pattern`) + Task 7 (used in every name verb). ✓
- Animation generators + server-side tick + rate → Task 5 (`sample`) + Task 6 (tick task, `--rate`) + Task 7 (`:anim`/`:stop`). ✓
- `~` marker + selected-row status → Task 7 (render). ✓
- Edit/`:set` stops animator → Task 7 (`exec_set` calls `stop_anim`). ✓
- Manual `:set` by choice name or index → Task 7 (`exec_set` enum branch). ✓
- Table CLI-only, always RW → Task 1 (no writable param) + Task 7 (`row_writable`) + Task 8 (wizard offers no table). ✓
- Module split → Tasks 2/4/5 create `parse`/`pattern`/`anim`. ✓
- Docs + wire test → Task 9. ✓

**Placeholder scan:** No TBD/TODO. Every code step shows complete code. The `:rate` live-reconfig limitation is called out explicitly (reports startup-only) rather than left vague. The wire-test assertion note (index vs choice) is a real, bounded adjust-after-first-run, not a placeholder.

**Type consistency:**
- `expand_pattern(&str) -> Result<Vec<String>, String>` and `EXPAND_CAP` consistent across Tasks 4/7.
- `parse_command(&str) -> Result<Command, String>` and the `Command` variants (`Add{pattern,spec,writable,value}`, `Set{pattern,value}`, `Del{pattern:Option}`, `Rename{old,new}`, `Access{pattern,writable}`, `Anim{pattern,gen,params}`, `Stop{pattern:Option}`, `Rate{hz}`, `Source{path}`, `Help`, `Quit`) defined in Task 3, consumed unchanged in Task 7.
- `build_anim(gen:&str, params:&[(String,String)]) -> Result<AnimSpec,String>` and `sample(&AnimSpec, &mut AnimState, f64) -> f64` defined Task 5, used Tasks 6/7.
- `coerce_scalar(f64, WireType) -> ScalarValue` defined Task 3, used in Task 6 tick task.
- `ServerHandle` methods (`add_enum`, `add_table`, `set_enum`, `read_enum`, `animators`) defined Task 6, used Tasks 7/8.
- `PvSpec` / `AddKind` / `Target` / `Live` / `Animators` defined Task 6, used Tasks 7/8.
- `RunningServer::add_enum(name, choices, index, writable)` / `add_table(name, columns)` defined Task 1, wrapped in Task 6.

Consistent throughout.
