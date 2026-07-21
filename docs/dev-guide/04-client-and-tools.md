# spvirit-client & spvirit-tools — Client Library and CLI Tools

## spvirit-client

Library-only crate (~4,900 LOC). Deps: `spvirit-types`, `spvirit-codec`,
`tokio`, `serde_json`, `dns-lookup`, `get_if_addrs`, `socket2`, `chrono`.
**No TLS crate — TLS is the biggest known gap.**

| File | ~LOC | Purpose |
|---|---|---|
| `pva_client.rs` | 970 | High-level API: `PvaClient`, `PvaClientBuilder`, `PvaChannel` (streaming PUT), monitor loop, pvinfo |
| `search.rs` | 1233 | UDP broadcast/multicast search + discovery, TCP name-server search, `resolve_pv_server`, EPICS env vars |
| `pvlist.rs` | 766 | PV-name listing with 4 fallback strategies (`__pvlist`, GET_FIELD, server RPC, server GET) |
| `put_encode.rs` | 673 | JSON→PVD PUT payload encoder (bitset partial encoding, scalars/arrays/unions/variants) |
| `client.rs` | 311 | Low-level channel lifecycle: `establish_channel`, `pvget`/`pvget_fields` |
| `format.rs` | 521 | Output rendering (text/JSON), NT metadata extraction, NTTable formatting |
| `types.rs` | 88 | `PvOptions`, `PvGetResult`, `PvMonitorEvent`, `PvGetError` |
| `transport.rs` | 61 | `read_packet` / `read_until` framed-packet readers |
| `auth.rs` | 25 | AuthNZ user/host resolution (options → env → "unknown") |

### Discovery and search (`search.rs`)

`resolve_pv_server` (search.rs:749) is the entry point: an explicit
`server_addr` short-circuits; otherwise all strategies run concurrently in a
`JoinSet` (one TCP name-server search per configured server + one UDP search),
first success wins, rest aborted.

- Targets: explicit `--search-addr` overrides everything; otherwise
  `EPICS_PVA_ADDR_LIST` entries + auto-broadcast targets (per-interface
  directed broadcast + IPv4 multicast `224.0.0.128` + IPv6 `ff02::42:1` —
  multicast added because Docker overlay networks may block broadcast).
- Retransmit schedule: 100/500/1000/2000 ms within the overall timeout.
- **Ephemeral source port is intentional** (search.rs:376): binding the client
  to 5076 with SO_REUSEPORT loops packets back to the sender on Linux. Do not
  "fix" this.
- **SO_REUSEADDR is Unix-only** (search.rs:321): deliberately skipped on
  Windows where the semantics are unsafe. Also do not "fix".

### Operations

- **Channel setup** (`establish_channel`, client.rs:56): TCP connect → learn
  version + byte order from the first packets → client validation (authnz
  "ca") → wait for ConnectionValidated → CREATE_CHANNEL (**cid hardcoded to
  1** — fine for one-shot connections, a hazard if you ever multiplex).
- **GET**: GET INIT (subcmd 0x08, fixed 6-byte "all fields" pvRequest or
  `encode_pv_request(fields)`) → introspection from INIT → GET DATA →
  `decode_with_field_desc`. Result carries `value: DecodedValue` plus raw PVA
  and PVD bytes and the introspection.
- **PUT**: defaults to field `["value"]`; value is `impl Into<serde_json::Value>`,
  encoded by `put_encode.rs` (which does support nested structures, unions,
  variants, structure-arrays — the README's "structured put payloads"
  caveat means *not fully surfaced/battle-tested*, not absent).
  `open_put_channel` returns a `PvaChannel` for streaming puts (reuses
  introspection, echo keepalive after 10 s idle, background reader aborted on
  Drop).
- **MONITOR**: INIT → START = `0x44` (START|GET). Callback
  `FnMut(&DecodedValue) -> ControlFlow<()>`; `Break` sends best-effort
  DESTROY. Read timeouts are non-fatal (`continue`); 10 s echo keepalive.
  Pipelining (`MonitorOptions::pipelined(n)`) encodes
  `pipeline=true,queueSize=N` in the pvRequest *and* appends queueSize to the
  INIT body *and* sets bit 0x80 on the INIT subcmd; ACKs at queueSize/2.
  **Critical invariant** (pva_client.rs:548–552): never set 0x80 on the START
  message — on non-INIT monitor messages 0x80 means "ACK with u32 body", and
  getting this wrong makes pvxs/Java drop the TCP connection.
- **INFO**: GET_FIELD (cmd 0x11) → `StructureDesc`.

### Environment variables

| Variable | Effect |
|---|---|
| `EPICS_PVA_ADDR_LIST` | extra search targets |
| `EPICS_PVA_AUTO_ADDR_LIST` | YES/NO auto-broadcast (default on) |
| `EPICS_PVA_NAME_SERVERS` | TCP name servers (host[:port], port defaults 5075) |
| `EPICS_PVA_ENABLE_GET_FIELD_FALLBACK` | opt-in pvlist GET_FIELD strategy |
| `PVA_AUTHNZ_USER` / `USER` / `LOGNAME` / `USERNAME` | authnz user |
| `PVA_AUTHNZ_HOST` / `HOSTNAME` / `HOST` / `COMPUTERNAME` | authnz host |

Note: standard `EPICS_PVA_SERVER_PORT` / `EPICS_PVA_BROADCAST_PORT` are **not**
read by the client — ports come from builder/CLI defaults (5075/5076).

### Client gotchas

- **Epoch disambiguation heuristic** (format.rs:98): `secondsPastEpoch` is
  interpreted as UNIX or EPICS-1990 epoch by picking whichever is closer to
  "now" — fragile for historical or far-future timestamps.
- pvlist's server-GET strategy scrapes ASCII candidates from raw bytes with a
  denylist filter — heuristic, can false-positive.
- `spinfo` works around servers that crash on an empty GET_FIELD field name
  by retrying without the wire field, then falling back to pvget.
- No automatic reconnect anywhere; a single `timeout` (default 5 s) applies
  to connect and every read.

## spvirit-tools

Binaries + a thin lib re-exporting the client and server crates. Features:
`default = ["client", "server", "tui"]`; `tui` pulls ratatui 0.29 +
color-eyre. Source files are named `spvirit_*.rs` but the installed binaries
are the `sp*` names (see the `[[bin]]` table in Cargo.toml).

All client tools share `CommonClientArgs` (src/spvirit_client/cli.rs:25) —
timeout/server/search-addr/name-server/ports/debug/authnz/fields flags — via
the `argparse` crate, and `block_on` a manually built tokio runtime.

| Binary | LOC | What it is / what it exercises |
|---|---|---|
| `spget` | 56 | One-shot GET; `format_output`, `--raw` hex dumps |
| `spput` | 316 | PUT; default "EPICS-base-style" full flow (GET → PUT INIT → get-put probe → PUT → DESTROY_REQUEST → GET), auto-fallback to simple flow; `PV=VALUE` syntax for negative numbers |
| `spmonitor` | 278 | Multi-PV monitor (JoinSet); `--raw`, `--json`, `--pipeline N` |
| `spinfo` | 192 | Introspection with the 3-level fallback chain |
| `splist` | 112 | Server discovery (GUID + addr) or PV listing via `pvlist_with_fallback` |
| `spsine` | 102 | Streaming-PUT sine generator (`open_put_channel` showcase) |
| `spget_compare` | 324 | Offline diagnostic: byte-compares captured frames against local encoder output; no network |
| `spexplore` | 1414 | ratatui TUI: servers→PVs→details panes, chart view, background worker thread over `std::sync::mpsc` |
| `spsearch` | 1094 | ratatui TUI: **passive** search-traffic sniffer; decodes frames directly with `PvaPacket` |
| `spserver` | 4185 | Full PVA server binary: `.db` loading, hot-reload, beacons, MDEL, `__pvlist`/discovery modes; record logic comes from the spvirit-server crate |
| `spdodeca` | 1179 | Self-contained single-PV server streaming a rotating dodecahedron as NTNDArray (does *not* use spvirit-server) |
| `sptable` | ~1200 | ratatui TUI spreadsheet IOC. Rows are dynamically added PVs: 12 scalar types, arrays, **NTEnum**, **NTTable**. Modal `a` wizard **plus a vim-style `:` command line** (`:add/:set/:del/:mv/:ro/:rw/:anim/:stop/:source`, shorthands, `:help`). Bash-style **pattern expansion** (`RING:BPM{01..99}`, products) for bulk ops. **Animation** generators (sine/ramp/triangle/square/noise/walk/count, enum `cycle`) driven by a server-side tick (`--rate`, default 10 Hz). |

#### sptable command reference

Mirrors `help_text()` in `spvirit_table/main.rs` — keep in sync.

| Verb | Shorthand | Args | Effect |
|---|---|---|---|
| `add` | `a` | `<name> <type> [ro\|rw] <value>` | add PV(s) |
| `set` | `s` | `<name> <value>` | set value (choice name or index for enum) |
| `del` | `d` | `[name]` | delete (blank = selected row) |
| `rename` | `mv` | `<old> <new>` | rename (scalar/enum) |
| — | `ro`/`rw` | `<name>` | set advertised access |
| `anim` | — | `<name> <gen> [k=v ...]` | animate |
| `stop` | — | `[name]` | stop animation (blank = selected) |
| `source` | `so` | `<file>` | run a file of commands |
| `rate` | — | `<hz>` | set tick rate (also `--rate` at startup) |
| `help`/`quit` | `h`/`q` | | show help / quit |

Typespec aliases: `bool int8 int16 int32(int) int64(long) uint8 uint16
uint32 uint64 float(f32) double(f64) string(s)`; arrays via `int32[]`
suffix; plus `enum` and `table`.

Pattern forms (bash-brace style, expanded before every name verb):
`{1..8}`, `{8..1}` (descending), `{0..100..10}` (step), `{01..12}`
(zero-padded), `{A,B,C}` (list), and products like `S{1..4}:{A,B}`.

Generators: `sine ramp triangle square noise walk count` for scalars,
`cycle` for enums — e.g. `:anim RING:BPM{01..99} noise min=-1 max=1`.

Value forms: enum accepts a choice name or index (`OFF`, `ON`, `TRIP`, or
`1`); table accepts per-column `id:i32=1,2,3 x:f64=0.5,1.5`.

Known spserver limitations: ACL_CHANGE/MESSAGE/MULTIPLE_DATA/CANCEL_REQUEST/
ORIGIN_TAG return "not supported"; NtTable/NtNdArray DOL output links are
"no-op for now" (spvirit_server.rs:2700, 2730).

## Tests

- **spvirit-client**: ~39 inline unit tests (search target math, addr-list
  parsing, encode/decode round-trips, builder defaults, put_encode bitsets).
  No integration dir — integration coverage lives in spvirit-tools.
- **spvirit-tools `tests/`**: ~33 test functions in two harness styles:
  - `tests/protocol/` — in-process wire testing (`frame_harness.rs` spawns
    workspace binaries; `scenario_harness.rs` gives connect/handshake/get/put
    helpers). Used by `spvirit_protocol_*`, `spvirit_pvlist`, `spvirit_nt_*`,
    `spvirit_monitor_*`, `ioc_fields`, `pv_handle_api`.
  - `tests/interop/` — external implementations (p4p/pvxs, EPICS Base,
    PVAccessJava), env-gated so they skip unless e.g. `PVA_TEST_P4P=1` and the
    external server is installed. See chapter 06.

## demo/ directory (repo root, gitignored)

Archiver Appliance demo: `archiver_demo_server.py` (spvirit-py based, 18 PVs
of every type incl. NTEnum/NTTable/NTNDArray), `archiver_demo_gen.py`
(stdlib .db animator for spserver hot-reload), `archiver_demo.db`,
`Dockerfile` (builds the wheel + runs the server). `docker_compose.yml` is a
0-byte placeholder. `demo/README.md` documents both run modes and warns that
.db-reloaded arrays get a 1990-epoch timestamp the Archiver rejects.
