# `sptable` Interactive Spreadsheet IOC — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A ratatui TUI binary `sptable` that spawns a PVAccess server and lets the user dynamically add PVs (all 12 scalar wire types + arrays), edit their values live, and delete them — each row is one PV.

**Architecture:** Two server-crate additions provide the runtime primitives (`RunningServer::add_scalar`/`add_array`, `SimplePvStore::remove`). A new single-file binary in `spvirit-tools` wraps a `RunningServer` in a thin `ServerHandle`, parses user input into `ScalarValue`/`ScalarArrayValue`, and drives a ratatui grid. Async store calls run on a `block_on` Tokio runtime, matching the other tools.

**Tech Stack:** Rust, tokio, ratatui 0.29 (`tui` feature), `argparse`, spvirit-server/-client/-types/-codec.

## Global Constraints

- Binaries are `src/bin/spvirit_<x>.rs` compiled to `sp<x>` via the `[[bin]]` table; `required-features` must be declared. Copy the convention verbatim.
- Server-crate additions live in the same crate as their private helpers (`scalar_family_record_type`, `make_scalar_record`, `make_output_record`, `make_array_record` are `pub(crate)` in `pva_server.rs`; `Pv::attach`/`PvArray::attach` are `pub(crate)` in `pv.rs`) — new methods that call them must be defined in `spvirit-server`.
- The 12 wire types are exactly the `ScalarValue`/`ScalarArrayValue` variants: `Bool, I8, I16, I32, I64, U8, U16, U32, U64, F32, F64, Str` (spvirit-types/src/lib.rs:9-38).
- Tests: server crate via `cargo test -p spvirit-server`; tool integration via `cargo test -p spvirit-tools --test <name>`; binary unit tests via `cargo test -p spvirit-tools --bin sptable`.
- In-memory only (no persistence). The `Clients`/subscriber-count column is **dropped** from v1 — `SimplePvStore` exposes no subscriber count and adding one is out of scope. Grid columns are: Name, Kind, Type, R/W, Value.

---

### Task 1: `SimplePvStore::remove` + retire the "never removed" invariant

**Files:**
- Modify: `spvirit-server/src/simple_store.rs` (add `remove`, add test)
- Modify: `spvirit-server/src/pv.rs:588-596` and `spvirit-server/src/pv.rs:762-766` (comment fixes)

**Interfaces:**
- Produces: `SimplePvStore::remove(&self, name: &str) -> bool` (async) — `true` if a record was removed, `false` if absent. Dropping the `PvEntry` drops its subscriber `mpsc::Sender`s, closing any monitor channels.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `spvirit-server/src/simple_store.rs`:

```rust
#[tokio::test]
async fn remove_deletes_record_and_is_idempotent() {
    let mut records = std::collections::HashMap::new();
    records.insert(
        "T:GONE".to_string(),
        crate::pva_server::make_scalar_record("T:GONE", RecordType::Ai, ScalarValue::F64(1.0)),
    );
    let store = SimplePvStore::new(records, Default::default(), Vec::new(), false);

    assert!(store.get_value("T:GONE").await.is_some());
    assert!(store.remove("T:GONE").await, "first remove returns true");
    assert!(store.get_value("T:GONE").await.is_none(), "record is gone");
    assert!(!store.remove("T:GONE").await, "second remove returns false");
    assert!(store.claim("T:GONE").await.is_none(), "claim no longer matches");
}
```

Note: `make_scalar_record` is `pub(crate)` in `pva_server.rs` — reachable from this in-crate test via `crate::pva_server::make_scalar_record`. If the test module lacks `use super::*;` imports for `RecordType`/`ScalarValue`, add them (they are already used elsewhere in this test module).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p spvirit-server remove_deletes_record_and_is_idempotent`
Expected: FAIL — `no method named 'remove' found for struct 'SimplePvStore'`.

- [ ] **Step 3: Implement `remove`**

Add to `impl SimplePvStore` in `simple_store.rs`, immediately after the `insert` method (around line 120):

```rust
    /// Remove a PV record at runtime. Returns `true` if a record was removed,
    /// `false` if no record with that name existed. Dropping the entry drops
    /// its subscriber senders, which closes any active monitor channels for
    /// that PV.
    pub async fn remove(&self, name: &str) -> bool {
        self.pvs.write().await.remove(name).is_some()
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p spvirit-server remove_deletes_record_and_is_idempotent`
Expected: PASS.

- [ ] **Step 5: Fix the "never removed" comments**

In `spvirit-server/src/pv.rs`, the `Pv::set` no-op branch (around line 588-593) currently reads:

```rust
            // Record exists — the write was a no-op (value unchanged). This
            // existence check is a benign TOCTOU: records are never removed
            // from SimplePvStore, so a `Some` here can't go stale by the time
            // we return `Ok`.
```

Replace with:

```rust
            // Record exists — the write was a no-op (value unchanged). Records
            // CAN now be removed at runtime (`SimplePvStore::remove`), so this
            // is a genuine (benign) TOCTOU: if the record were removed between
            // the failed set and this check we would fall through to the
            // `NotFound` arm, which is the correct outcome.
```

In `PvArray::set` (around line 762-765) the comment reads `// or a truncated/rejected update; either way this mirrors // `Pv::set`'s benign-TOCTOU existence check.` — leave the wording (it references `Pv::set`, now corrected). No code change.

- [ ] **Step 6: Run the full server suite**

Run: `cargo test -p spvirit-server`
Expected: PASS (no regressions).

- [ ] **Step 7: Commit**

```bash
git add spvirit-server/src/simple_store.rs spvirit-server/src/pv.rs
git commit -m "feat(server): SimplePvStore::remove for runtime PV deletion"
```

---

### Task 2: `RunningServer::add_scalar` and `add_array`

**Files:**
- Modify: `spvirit-server/src/pva_server.rs` (add two methods to `impl RunningServer` at ~line 844-865; add tests to the `#[cfg(test)] mod tests` at ~line 1027)

**Interfaces:**
- Consumes: `SimplePvStore::insert` (existing), `scalar_family_record_type`, `make_scalar_record`, `make_output_record`, `make_array_record` (all in this module), `Pv::attach`/`PvArray::attach`.
- Produces:
  - `RunningServer::add_scalar(&self, name: &str, value: ScalarValue, writable: bool) -> Pv<ScalarValue>` (async)
  - `RunningServer::add_array(&self, name: &str, value: ScalarArrayValue, writable: bool) -> PvArray` (async)

- [ ] **Step 1: Write the failing test**

Add to `#[cfg(test)] mod tests` in `pva_server.rs`:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_server_add_scalar_and_array() {
    use spvirit_types::{ScalarArrayValue, ScalarValue};

    let server = PvaServer::serve(Vec::<crate::pv::AnyPv>::new())
        .port(0)
        .udp_port(0)
        .start()
        .await;

    // add a writable u32 scalar
    let h = server.add_scalar("RT:U32", ScalarValue::U32(7), true).await;
    assert_eq!(h.get().await.unwrap(), ScalarValue::U32(7));
    // exact wire type preserved
    assert!(matches!(
        server.store().get_value("RT:U32").await,
        Some(ScalarValue::U32(7))
    ));

    // add a read-only i16 scalar; family maps to an input record
    let _ = server.add_scalar("RT:I16", ScalarValue::I16(-3), false).await;
    assert!(matches!(
        server.store().get_value("RT:I16").await,
        Some(ScalarValue::I16(-3))
    ));

    // add a writable f64 array
    let a = server
        .add_array("RT:ARR", ScalarArrayValue::F64(vec![1.0, 2.0, 3.0]), true)
        .await;
    a.set(ScalarArrayValue::F64(vec![4.0, 5.0])).await.unwrap();
    assert!(matches!(
        server.store().get_nt("RT:ARR").await,
        Some(spvirit_types::NtPayload::ScalarArray(_))
    ));

    server.abort();
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p spvirit-server running_server_add_scalar_and_array`
Expected: FAIL — `no method named 'add_scalar'`.

- [ ] **Step 3: Implement the two methods**

Add inside `impl RunningServer` in `pva_server.rs` (after `array_pv`, before `store`):

```rust
    /// Add a scalar record to the running server at runtime. The wire type is
    /// taken from the `ScalarValue` variant; `writable` selects an output
    /// record family (client PUTs allowed) vs an input family (read-only).
    /// Returns a bound handle to the new record. Replaces any existing record
    /// with the same name.
    pub async fn add_scalar(
        &self,
        name: &str,
        value: ScalarValue,
        writable: bool,
    ) -> crate::pv::Pv<ScalarValue> {
        let rt = scalar_family_record_type(&value, writable);
        let record = if writable {
            make_output_record(name, rt, value)
        } else {
            make_scalar_record(name, rt, value)
        };
        self.store.insert(name.to_string(), record).await;
        crate::pv::Pv::attach(&self.store, name)
            .await
            .expect("record just inserted")
    }

    /// Add an array record to the running server at runtime. `writable`
    /// selects `aao` (client PUTs allowed) vs `aai` (read-only). Element type
    /// comes from the `ScalarArrayValue` variant. Returns a bound handle.
    /// Replaces any existing record with the same name.
    pub async fn add_array(
        &self,
        name: &str,
        value: ScalarArrayValue,
        writable: bool,
    ) -> crate::pv::PvArray {
        let rt = if writable {
            RecordType::Aao
        } else {
            RecordType::Aai
        };
        let record = make_array_record(name, rt, value);
        self.store.insert(name.to_string(), record).await;
        crate::pv::PvArray::attach(&self.store, name)
            .await
            .expect("record just inserted")
    }
```

Note: `scalar_family_record_type` is a free `fn` in `pv.rs`. Verify it is `pub(crate)` (or reachable). If it is private to `pv.rs`, either change it to `pub(crate)` or move the family-selection inline. The grep in planning showed it as a module-private `fn` in `pv.rs`; make it `pub(crate) fn scalar_family_record_type(...)` and add `use crate::pv::scalar_family_record_type;` at the top of `pva_server.rs` if not already imported. `RecordType`, `ScalarValue`, `ScalarArrayValue` are already imported in `pva_server.rs` (used by the record helpers).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p spvirit-server running_server_add_scalar_and_array`
Expected: PASS.

- [ ] **Step 5: Run the full server suite**

Run: `cargo test -p spvirit-server`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add spvirit-server/src/pva_server.rs spvirit-server/src/pv.rs
git commit -m "feat(server): RunningServer::add_scalar/add_array for runtime PVs"
```

---

### Task 3: Over-the-wire integration test for dynamic add/edit/remove

Proves the crux risk: a PV added *after* the server is running is discoverable and gettable by a real client, edits post, and removal makes it unresolvable. Lives in `spvirit-tools` so it uses the re-exported server + the real `pvget` client.

**Files:**
- Create: `spvirit-tools/tests/sptable_dynamic.rs`

**Interfaces:**
- Consumes: `RunningServer::add_scalar`/`add_array`/`remove` (via `server.store().remove`), `spvirit_client::pvget`, `PvGetOptions`.

- [ ] **Step 1: Write the test**

Create `spvirit-tools/tests/sptable_dynamic.rs`:

```rust
//! Dynamic add / edit / remove of PVs against a running server, over the wire.

use std::net::{IpAddr, Ipv4Addr, TcpListener, UdpSocket};
use std::time::Duration;

use spvirit_client::pvget;
use spvirit_codec::spvd_decode::format_compact_value;
use spvirit_tools::spvirit_client::types::PvGetOptions;
use spvirit_tools::spvirit_server::pv::AnyPv;
use spvirit_tools::spvirit_server::pva_server::PvaServer;
use spvirit_types::ScalarValue;

fn free_tcp_port() -> Option<u16> {
    TcpListener::bind("127.0.0.1:0").ok()?.local_addr().ok().map(|a| a.port())
}
fn free_udp_port() -> Option<u16> {
    UdpSocket::bind("127.0.0.1:0").ok()?.local_addr().ok().map(|a| a.port())
}
fn opts(pv: &str, tcp: u16, udp: u16, timeout: Duration) -> PvGetOptions {
    let mut o = PvGetOptions::new(pv.to_string());
    o.tcp_port = tcp;
    o.udp_port = udp;
    o.timeout = timeout;
    o.search_addr = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));
    o.bind_addr = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));
    o
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_add_edit_remove_over_wire() {
    let (Some(tcp), Some(udp)) = (free_tcp_port(), free_udp_port()) else {
        eprintln!("Skipping: cannot bind ports");
        return;
    };

    let server = PvaServer::serve(Vec::<AnyPv>::new())
        .port(tcp)
        .udp_port(udp)
        .listen_ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
        .start()
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // ADD: writable i32 PV appears and is gettable.
    let _h = server.add_scalar("DYN:VAL", ScalarValue::I32(42), true).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let r = pvget(&opts("DYN:VAL", tcp, udp, Duration::from_secs(5)))
        .await
        .expect("pvget after add");
    assert!(
        format_compact_value(&r.value).contains("42"),
        "expected 42, got {}",
        format_compact_value(&r.value)
    );

    // EDIT: value updates over the wire.
    server.store().set_value("DYN:VAL", ScalarValue::I32(99)).await;
    let r = pvget(&opts("DYN:VAL", tcp, udp, Duration::from_secs(5)))
        .await
        .expect("pvget after edit");
    assert!(
        format_compact_value(&r.value).contains("99"),
        "expected 99, got {}",
        format_compact_value(&r.value)
    );

    // REMOVE: PV no longer resolves (short timeout — negative lookup).
    assert!(server.store().remove("DYN:VAL").await);
    let res = pvget(&opts("DYN:VAL", tcp, udp, Duration::from_millis(800))).await;
    assert!(res.is_err(), "removed PV should not resolve, got {res:?}");

    server.abort();
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p spvirit-tools --test sptable_dynamic`
Expected: PASS. (If loopback UDP discovery is unavailable in the environment the add/edit asserts still work because `search_addr`/`bind_addr` force loopback, matching `pv_handle_api.rs`.)

- [ ] **Step 3: Commit**

```bash
git add spvirit-tools/tests/sptable_dynamic.rs
git commit -m "test(tools): wire test for dynamic add/edit/remove of PVs"
```

---

### Task 4: Wire-type + value parsing module (in the binary)

Pure functions that turn user text into typed values. Defined in the new binary file with inline unit tests so the TUI task can build on them.

**Files:**
- Create: `spvirit-tools/src/bin/spvirit_table.rs` (this task adds only the parsing section + its tests + a `fn main` stub so it compiles)
- Modify: `spvirit-tools/Cargo.toml` (add the `[[bin]]` entry)

**Interfaces:**
- Produces:
  - `enum WireType { Bool, I8, I16, I32, I64, U8, U16, U32, U64, F32, F64, Str }`
  - `WireType::label(self) -> &'static str` and `WireType::from_label(&str) -> Option<WireType>` and `WireType::ALL: [WireType; 12]`
  - `fn parse_scalar(ty: WireType, s: &str) -> Result<ScalarValue, String>`
  - `fn parse_array(ty: WireType, s: &str) -> Result<ScalarArrayValue, String>`
  - `fn format_scalar(v: &ScalarValue) -> String` and `fn format_array(v: &ScalarArrayValue) -> String` (for grid display)

- [ ] **Step 1: Add the `[[bin]]` entry**

Append to `spvirit-tools/Cargo.toml` after the last `[[bin]]` block:

```toml
[[bin]]
name = "sptable"
path = "src/bin/spvirit_table.rs"
required-features = ["server", "tui"]
```

- [ ] **Step 2: Write the failing tests + minimal file**

Create `spvirit-tools/src/bin/spvirit_table.rs`:

```rust
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
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p spvirit-tools --bin sptable`
Expected: PASS (3 tests). This also confirms the `[[bin]]` entry and feature gating compile.

- [ ] **Step 4: Commit**

```bash
git add spvirit-tools/Cargo.toml spvirit-tools/src/bin/spvirit_table.rs
git commit -m "feat(tools): sptable wire-type + value parsing"
```

---

### Task 5: The ratatui TUI (grid, add flow, edit, delete, refresh)

Replaces the `main` stub. Not unit-tested (consistent with `spexplore`/`spsearch`); verified manually. Uses `ratatui::init()`/`restore()` and a `block_on` Tokio runtime to drive the async `RunningServer`.

**Files:**
- Modify: `spvirit-tools/src/bin/spvirit_table.rs` (replace `fn main`, add app/model/render/input code above it; keep the parsing section and its tests)

**Interfaces:**
- Consumes: `WireType`, `parse_scalar`, `parse_array`, `format_scalar`, `format_array` (Task 4); `RunningServer::add_scalar`/`add_array`/`remove`; `PvaServer::serve`.

- [ ] **Step 1: Add imports and the row/app model**

At the top of `spvirit-tools/src/bin/spvirit_table.rs`, extend imports:

```rust
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState};
use ratatui::DefaultTerminal;

use spvirit_tools::spvirit_server::pv::AnyPv;
use spvirit_tools::spvirit_server::pva_server::{PvaServer, RunningServer};
```

Add the model types (below the parsing section):

```rust
#[derive(Copy, Clone, PartialEq, Eq)]
enum Kind { Scalar, Array }

impl Kind {
    fn label(self) -> &'static str {
        match self { Kind::Scalar => "scalar", Kind::Array => "array" }
    }
}

struct PvRow {
    name: String,
    kind: Kind,
    ty: WireType,
    writable: bool,
    display: String, // last known value, formatted
}

/// Modal input state for the multi-step "add row" flow and inline edit.
enum Mode {
    Browse,
    AddName { buf: String },
    AddKind { name: String },
    AddType { name: String, kind: Kind, idx: usize },
    AddAccess { name: String, kind: Kind, ty: WireType, writable: bool },
    AddValue { name: String, kind: Kind, ty: WireType, writable: bool, buf: String },
    Edit { row: usize, buf: String },
}

struct App {
    rows: Vec<PvRow>,
    table: TableState,
    mode: Mode,
    status: String,
    tcp_port: u16,
    udp_port: u16,
}
```

- [ ] **Step 2: Add the `ServerHandle` wrapper**

```rust
/// Owns the runtime + running server; all async store calls go through here.
struct ServerHandle {
    rt: tokio::runtime::Runtime,
    server: RunningServer,
}

impl ServerHandle {
    fn start(tcp: u16, udp: u16) -> Result<Self, Box<dyn std::error::Error>> {
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
        Ok(Self { rt, server })
    }

    fn add_scalar(&self, name: &str, v: ScalarValue, writable: bool) {
        self.rt.block_on(self.server.add_scalar(name, v, writable));
    }
    fn add_array(&self, name: &str, v: ScalarArrayValue, writable: bool) {
        self.rt.block_on(self.server.add_array(name, v, writable));
    }
    fn set_scalar(&self, name: &str, v: ScalarValue) {
        self.rt.block_on(self.server.store().set_value(name, v));
    }
    fn set_array(&self, name: &str, v: ScalarArrayValue) {
        self.rt.block_on(self.server.store().set_array_value(name, v));
    }
    fn remove(&self, name: &str) -> bool {
        self.rt.block_on(self.server.store().remove(name))
    }
    fn exists(&self, name: &str) -> bool {
        self.rt.block_on(self.server.store().get_value(name)).is_some()
    }
    /// Read the current formatted value for a scalar row (for refresh).
    fn read_scalar(&self, name: &str) -> Option<String> {
        self.rt
            .block_on(self.server.store().get_value(name))
            .map(|v| format_scalar(&v))
    }
    fn abort(&self) { self.server.abort(); }
}
```

Note: for array-row refresh we keep the last-set `display` string rather than reading back (arrays return `ScalarValue::I32(len)` from `get_value` — see pv.rs:637). Scalars refresh from the store so external PUTs show; arrays show the last locally-set value.

- [ ] **Step 3: Add rendering**

```rust
fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(frame.area());

    let header = Row::new(["Name", "Kind", "Type", "R/W", "Value"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let body = app.rows.iter().map(|r| {
        Row::new([
            Cell::from(r.name.clone()),
            Cell::from(r.kind.label()),
            Cell::from(r.ty.label()),
            Cell::from(if r.writable { "RW" } else { "RO" }),
            Cell::from(r.display.clone()),
        ])
    });
    let widths = [
        Constraint::Percentage(34), Constraint::Length(7),
        Constraint::Length(8), Constraint::Length(4),
        Constraint::Percentage(34),
    ];
    let table = Table::new(body, widths)
        .header(header)
        .row_highlight_style(Style::default().bg(Color::DarkGray))
        .block(Block::default().borders(Borders::ALL).title(format!(
            " sptable — {} PVs @ 127.0.0.1:{} (a add · e edit · d del · q quit) ",
            app.rows.len(), app.tcp_port
        )));
    let mut ts = app.table.clone();
    frame.render_stateful_widget(table, chunks[0], &mut ts);

    let status = Paragraph::new(app.status.clone())
        .block(Block::default().borders(Borders::ALL).title(" status "));
    frame.render_widget(status, chunks[1]);

    // Modal prompt overlay for add/edit.
    if let Some((title, content)) = prompt_text(&app.mode) {
        let area = centered(60, 20, frame.area());
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(content)
                .block(Block::default().borders(Borders::ALL).title(title)),
            area,
        );
    }
}

fn prompt_text(mode: &Mode) -> Option<(&'static str, String)> {
    match mode {
        Mode::Browse => None,
        Mode::AddName { buf } => Some((" new PV name (Enter) ", buf.clone())),
        Mode::AddKind { .. } => Some((" kind: [s]calar / [a]rray ", String::new())),
        Mode::AddType { idx, .. } => Some((
            " type (←/→, Enter) ",
            WireType::ALL[*idx].label().to_string(),
        )),
        Mode::AddAccess { writable, .. } => Some((
            " access: [r]ead-only / [w]ritable (Enter) ",
            if *writable { "writable" } else { "read-only" }.to_string(),
        )),
        Mode::AddValue { buf, ty, kind, .. } => Some((
            match kind { Kind::Array => " values, comma-separated (Enter) ",
                         Kind::Scalar => " initial value (Enter) " },
            format!("[{}] {}", ty.label(), buf),
        )),
        Mode::Edit { buf, .. } => Some((" new value (Enter) ", buf.clone())),
    }
}

fn centered(pct_w: u16, pct_h: u16, area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_h) / 2),
            Constraint::Percentage(pct_h),
            Constraint::Percentage((100 - pct_h) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_w) / 2),
            Constraint::Percentage(pct_w),
            Constraint::Percentage((100 - pct_w) / 2),
        ])
        .split(v[1])[1]
}
```

- [ ] **Step 4: Add input handling + the event loop**

```rust
fn commit_add(app: &mut App, srv: &ServerHandle,
              name: String, kind: Kind, ty: WireType, writable: bool, val: &str) {
    if app.rows.iter().any(|r| r.name == name) || srv.exists(&name) {
        app.status = format!("name {name:?} already exists");
        return;
    }
    let display;
    match kind {
        Kind::Scalar => match parse_scalar(ty, val) {
            Ok(v) => { display = format_scalar(&v); srv.add_scalar(&name, v, writable); }
            Err(e) => { app.status = e; return; }
        },
        Kind::Array => match parse_array(ty, val) {
            Ok(v) => { display = format_array(&v); srv.add_array(&name, v, writable); }
            Err(e) => { app.status = e; return; }
        },
    }
    app.status = format!("added {name}");
    app.rows.push(PvRow { name, kind, ty, writable, display });
    app.table.select(Some(app.rows.len() - 1));
}

fn on_key(app: &mut App, srv: &ServerHandle, code: KeyCode) -> bool {
    // returns true to quit
    match std::mem::replace(&mut app.mode, Mode::Browse) {
        Mode::Browse => match code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Char('a') => app.mode = Mode::AddName { buf: String::new() },
            KeyCode::Char('e') | KeyCode::Enter => {
                if let Some(i) = app.table.selected() {
                    app.mode = Mode::Edit { row: i, buf: String::new() };
                }
            }
            KeyCode::Char('d') => {
                if let Some(i) = app.table.selected() {
                    let name = app.rows[i].name.clone();
                    srv.remove(&name);
                    app.rows.remove(i);
                    app.status = format!("removed {name}");
                    if app.rows.is_empty() { app.table.select(None); }
                    else { app.table.select(Some(i.min(app.rows.len() - 1))); }
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let n = app.rows.len();
                if n > 0 {
                    let i = app.table.selected().map_or(0, |i| (i + 1) % n);
                    app.table.select(Some(i));
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let n = app.rows.len();
                if n > 0 {
                    let i = app.table.selected().map_or(0, |i| (i + n - 1) % n);
                    app.table.select(Some(i));
                }
            }
            _ => {}
        },
        Mode::AddName { mut buf } => match code {
            KeyCode::Esc => {}
            KeyCode::Enter if !buf.trim().is_empty() =>
                app.mode = Mode::AddKind { name: buf.trim().to_string() },
            KeyCode::Char(c) => { buf.push(c); app.mode = Mode::AddName { buf }; }
            KeyCode::Backspace => { buf.pop(); app.mode = Mode::AddName { buf }; }
            _ => app.mode = Mode::AddName { buf },
        },
        Mode::AddKind { name } => match code {
            KeyCode::Esc => {}
            KeyCode::Char('s') => app.mode = Mode::AddType { name, kind: Kind::Scalar, idx: 0 },
            KeyCode::Char('a') => app.mode = Mode::AddType { name, kind: Kind::Array, idx: 0 },
            _ => app.mode = Mode::AddKind { name },
        },
        Mode::AddType { name, kind, mut idx } => match code {
            KeyCode::Esc => {}
            KeyCode::Left => { idx = (idx + WireType::ALL.len() - 1) % WireType::ALL.len();
                               app.mode = Mode::AddType { name, kind, idx }; }
            KeyCode::Right => { idx = (idx + 1) % WireType::ALL.len();
                                app.mode = Mode::AddType { name, kind, idx }; }
            KeyCode::Enter => app.mode = Mode::AddAccess {
                name, kind, ty: WireType::ALL[idx], writable: true },
            _ => app.mode = Mode::AddType { name, kind, idx },
        },
        Mode::AddAccess { name, kind, ty, mut writable } => match code {
            KeyCode::Esc => {}
            KeyCode::Char('r') => { writable = false;
                app.mode = Mode::AddAccess { name, kind, ty, writable }; }
            KeyCode::Char('w') => { writable = true;
                app.mode = Mode::AddAccess { name, kind, ty, writable }; }
            KeyCode::Enter => app.mode = Mode::AddValue {
                name, kind, ty, writable, buf: String::new() },
            _ => app.mode = Mode::AddAccess { name, kind, ty, writable },
        },
        Mode::AddValue { name, kind, ty, writable, mut buf } => match code {
            KeyCode::Esc => {}
            KeyCode::Enter => commit_add(app, srv, name, kind, ty, writable, &buf),
            KeyCode::Char(c) => { buf.push(c);
                app.mode = Mode::AddValue { name, kind, ty, writable, buf }; }
            KeyCode::Backspace => { buf.pop();
                app.mode = Mode::AddValue { name, kind, ty, writable, buf }; }
            _ => app.mode = Mode::AddValue { name, kind, ty, writable, buf },
        },
        Mode::Edit { row, mut buf } => match code {
            KeyCode::Esc => {}
            KeyCode::Enter => {
                let r = &app.rows[row];
                match r.kind {
                    Kind::Scalar => match parse_scalar(r.ty, &buf) {
                        Ok(v) => { srv.set_scalar(&r.name, v.clone());
                                   app.rows[row].display = format_scalar(&v);
                                   app.status = format!("set {}", r.name); }
                        Err(e) => app.status = e,
                    },
                    Kind::Array => match parse_array(r.ty, &buf) {
                        Ok(v) => { srv.set_array(&r.name, v.clone());
                                   app.rows[row].display = format_array(&v);
                                   app.status = format!("set {}", r.name); }
                        Err(e) => app.status = e,
                    },
                }
            }
            KeyCode::Char(c) => { buf.push(c); app.mode = Mode::Edit { row, buf }; }
            KeyCode::Backspace => { buf.pop(); app.mode = Mode::Edit { row, buf }; }
            _ => app.mode = Mode::Edit { row, buf },
        },
    }
    false
}

fn refresh_scalars(app: &mut App, srv: &ServerHandle) {
    for r in app.rows.iter_mut() {
        if r.kind == Kind::Scalar {
            if let Some(s) = srv.read_scalar(&r.name) { r.display = s; }
        }
    }
}

fn run_ui(mut terminal: DefaultTerminal, mut app: App, srv: &ServerHandle)
    -> Result<(), Box<dyn std::error::Error>> {
    loop {
        // Reflect external PUTs into scalar rows each tick.
        if matches!(app.mode, Mode::Browse) { refresh_scalars(&mut app, srv); }
        terminal.draw(|f| draw(f, &app))?;
        if event::poll(Duration::from_millis(500))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press && on_key(&mut app, srv, k.code) {
                    return Ok(());
                }
            }
        }
    }
}
```

- [ ] **Step 5: Replace `main`**

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use argparse::{ArgumentParser, Store};
    let mut tcp_port: u16 = 5075;
    let mut udp_port: u16 = 5076;
    {
        let mut ap = ArgumentParser::new();
        ap.set_description("Interactive spreadsheet IOC — each row is a PV");
        ap.refer(&mut tcp_port).add_option(&["--port"], Store, "TCP port (default 5075)");
        ap.refer(&mut udp_port).add_option(&["--udp-port"], Store, "UDP search port (default 5076)");
        ap.parse_args_or_exit();
    }

    let srv = ServerHandle::start(tcp_port, udp_port)?;

    color_eyre::install()?;
    let terminal = ratatui::init();
    let app = App {
        rows: Vec::new(),
        table: TableState::default(),
        mode: Mode::Browse,
        status: format!("serving on 127.0.0.1:{tcp_port} — press 'a' to add a PV"),
        tcp_port,
        udp_port,
    };
    let result = run_ui(terminal, app, &srv);
    ratatui::restore();
    srv.abort();
    result
}
```

- [ ] **Step 6: Verify it compiles and unit tests still pass**

Run: `cargo build -p spvirit-tools --bin sptable`
Expected: builds clean (no warnings-as-errors expected; fix any unused-import warnings).

Run: `cargo test -p spvirit-tools --bin sptable`
Expected: PASS (parsing tests from Task 4 still green).

- [ ] **Step 7: Manual smoke test**

In one terminal: `cargo run -p spvirit-tools --bin sptable -- --port 5099 --udp-port 5100`
- Press `a`, type `SIM:X`, Enter; press `s` (scalar); ←/→ to `int32`, Enter; press `w` (writable), Enter; type `42`, Enter. Row appears.
In another terminal:
`cargo run -p spvirit-tools --bin spget -- --server 127.0.0.1:5099 SIM:X` → shows 42.
`cargo run -p spvirit-tools --bin spput -- --server 127.0.0.1:5099 SIM:X 7` → back in the TUI within ~0.5s the Value column shows 7.
- In the TUI press `d` on the row → PV removed; a fresh `spget` fails to resolve.
Confirm, then `q` to quit.

- [ ] **Step 8: Commit**

```bash
git add spvirit-tools/src/bin/spvirit_table.rs
git commit -m "feat(tools): sptable ratatui interactive spreadsheet IOC"
```

---

### Task 6: Document the tool

**Files:**
- Modify: `docs/dev-guide/04-client-and-tools.md` (add `sptable` to the tools table)

- [ ] **Step 1: Add the table row**

In the `spvirit-tools` binary table in `docs/dev-guide/04-client-and-tools.md`, add a row (after the `spdodeca` row):

```markdown
| `sptable` | ~600 | ratatui TUI: spreadsheet IOC — each row is a dynamically added PV (12 scalar types + arrays), served live via `RunningServer::add_scalar`/`add_array`; `a` add / `e` edit / `d` delete; reflects external PUTs on scalar rows |
```

- [ ] **Step 2: Commit**

```bash
git add docs/dev-guide/04-client-and-tools.md
git commit -m "docs: document sptable in the dev-guide tools table"
```

---

## Self-Review

**Spec coverage:**
- Spawn server + dynamic add → Task 2 (`add_scalar`/`add_array`) + Task 5 (`ServerHandle::start`). ✓
- All 12 scalar types + arrays → Task 4 (`WireType`, parse) + Task 2 (variant-carried wire type). ✓
- Edit live, monitors update → Task 5 (`set_scalar`/`set_array` via store) + Task 3 (wire edit assert). ✓
- Delete at runtime → Task 1 (`remove`) + Task 5 (`d` key) + Task 3 (wire remove assert). ✓
- Reflect external writes → Task 5 (`refresh_scalars` tick). Documented limitation: arrays show last-set value (get_value returns length for arrays, pv.rs:637). ✓
- In-memory only, no persistence → honored (no file I/O). ✓
- `Clients` column dropped → recorded in Global Constraints (no public subscriber count). ✓
- "records never removed" invariant retired → Task 1 Step 5. ✓
- Testing (server units, tool integration, ratatui untested) → Tasks 1/2 (units), 3 (integration), 5 (manual). ✓

**Placeholder scan:** No TBD/TODO; every code step shows full code. The one flagged risk (`scalar_family_record_type` visibility) has an explicit remediation in Task 2 Step 3.

**Type consistency:** `add_scalar(&self, name: &str, value: ScalarValue, writable: bool) -> Pv<ScalarValue>` and `add_array(...ariant...) -> PvArray` are used identically in Tasks 2, 3, 5. `remove(&self, name: &str) -> bool` consistent across Tasks 1, 3, 5. `WireType`/`parse_scalar`/`parse_array`/`format_scalar`/`format_array` signatures defined in Task 4 and consumed unchanged in Task 5.
