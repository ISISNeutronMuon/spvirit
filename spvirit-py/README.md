# spvirit — EPICS PVAccess for Python, powered by Rust

`spvirit` is a pure-Rust implementation of the EPICS **PVAccess** protocol with
first-class Python bindings. It lets you build soft IOCs, simulators, gateways,
and clients entirely from Python — no EPICS base installation required — while
the protocol machinery, server, and network stack run in Rust on a Tokio
runtime.

```python
import spvirit

temp = spvirit.ai("SIM:TEMP", 21.5, units="C", prec=2)
setpoint = spvirit.ao("SIM:TEMP:SP", 20.0, units="C", drive_limits=(0.0, 100.0))

@setpoint.on_put
def validate(pv, value):
    print(f"setpoint changed to {value}")

server = spvirit.Server(pvs=[temp, setpoint])
server.start()

temp.set(21.7)          # published to all monitoring clients
print(setpoint.get())   # read the live value
```

This document is the complete guide to the Python API. For the Rust crates,
protocol internals, and the CLI tools (`spget`, `spput`, `spmonitor`,
`spserver`), see the [repository README](https://github.com/ISISNeutronMuon/spvirit).

---

## Table of contents

- [Installation](#installation)
- [Core concepts](#core-concepts)
- [IOC-style record PVs vs raw NT PVs](#ioc-style-record-pvs-vs-raw-nt-pvs)
- [Building servers with typed PV handles](#building-servers-with-typed-pv-handles)
  - [Creating PVs](#creating-pvs)
  - [Common options](#common-options)
  - [Reading and writing: set / get / set_async / get_async](#reading-and-writing-set--get--set_async--get_async)
  - [Reacting to client writes: on_put](#reacting-to-client-writes-on_put)
  - [Periodic updates: scan](#periodic-updates-scan)
  - [Computed PVs: calc](#computed-pvs-calc)
  - [Alarms: set_alarm](#alarms-set_alarm)
  - [The Server class](#the-server-class)
  - [Generating many PVs programmatically](#generating-many-pvs-programmatically)
- [Loading EPICS .db files](#loading-epics-db-files)
- [The classic builder API](#the-classic-builder-api)
- [Runtime store access](#runtime-store-access)
- [Dynamic sources](#dynamic-sources)
- [Lifecycle hooks and events](#lifecycle-hooks-and-events)
- [Normative Type classes](#normative-type-classes)
- [Client API](#client-api)
- [Low-level API: spvirit.lowlevel](#low-level-api-spviritlowlevel)
- [Wire codec: spvirit.codec](#wire-codec-spviritcodec)
- [Threading and async model](#threading-and-async-model)
- [Errors and exceptions](#errors-and-exceptions)
- [Examples](#examples)
- [Building from source](#building-from-source)

---

## Installation

```bash
pip install spvirit
```

Wheels are published for Linux (x86_64, aarch64), Windows (x86_64), and macOS
(x86_64, arm64) for Python 3.9+ (one abi3 wheel per platform). No EPICS base,
no compiler needed.

This also installs the `spvirit-tools` package, which puts the twelve
command-line tools — `spget`, `spput`, `spmonitor`, `splist`, `spinfo`,
`spexplore`, `spsearch`, `spsine`, `spserver`, `sptable`, `spdodeca`,
`spget_compare` — on your `PATH` inside the same environment. They are native
binaries, not Python wrappers, and each takes `--help`.

To build from source instead, see [Building from source](#building-from-source).

## Core concepts

- **PV handles** (`spvirit.Pv`) are typed references to process variables. You
  create them with factory functions (`spvirit.ai(...)`, `spvirit.ao(...)`, …),
  hand them to a `Server`, and then `set()`/`get()` them freely from Python
  while clients read, write, and monitor them over the network.
- **The server** owns a record store that speaks the PVAccess protocol:
  search/beacon UDP, TCP circuits, monitors with `MDEL`/`ADEL` deadbands,
  alarm computation, and QSRV-style field access (`PV.RTYP`, `PV.DESC`,
  `PV.EGU`, …) all work out of the box, including with the EPICS Archiver
  Appliance and standard tools (`pvget`, `pvput`, `pvmonitor`, Phoebus).
- **Sources** let you claim PV names dynamically from Python objects instead
  of a static record store — for gateways, bridges, or fully virtual PVs.
- **The client** side offers a high-level `Client` (get/put/monitor/info) and
  a low-level `Channel` (persistent connection, raw frame access) plus
  discovery and codec utilities.
- Everything blocking releases the GIL and runs on a shared Tokio runtime;
  async variants (`set_async`, `get_async`, `connect_async`, ...) integrate with `asyncio`. See
  [Threading and async model](#threading-and-async-model).

---

## IOC-style record PVs vs raw NT PVs

Everything on the wire is a Normative Type (usually NTScalar), but the API
works at two distinct levels — know which one you are on:

**IOC-style records** are what the typed handles, the classic builder, and
`.db` loading create. The server behaves like an EPICS record: `units`,
`prec`, `desc`, and limits become record fields visible QSRV-style
(`PV.EGU`, `PV.DESC`, …); MDEL/ADEL deadbands throttle monitors; alarm
severity is computed from the alarm limits (or set with `set_alarm`); every
post is timestamped for you. You work with plain Python values —
`pv.set(21.5)`, `store.set_value(...)`.

**Raw NT PVs** are full Normative Type payloads you construct and move
yourself: [`NtScalar` and friends](#normative-type-classes) written with
`store.put_nt(...)`, published with `notifier.notify(...)`, or served from a
[dynamic source's](#dynamic-sources) `get`/`put`. You control — and must
supply — the alarm, timestamp, display, and control metadata on each
payload; no deadbands or alarm computation are applied for you (an explicit
timestamp is honored; a zeroed one is stamped by the server).

|  | IOC-style records | Raw NT payloads |
|---|---|---|
| Created via | `spvirit.ai(...)` …, `ServerBuilder`, `db_file=` | `NtScalar(...)` …, `PvInfo` + source |
| Read/write | plain values (`set`/`get`, `set_value`) | whole payloads (`put_nt`/`get_nt`, `notify`) |
| Alarms, timestamps, deadbands | managed by the server | yours, per update |
| Best for | soft IOCs, simulators, record-like PVs | gateways, tables, images, per-update metadata |

The levels mix freely: `store.get_nt(name)` returns the full NT payload of
an IOC-style record, and `store.put_nt` writes one — useful when a mostly
record-like PV occasionally needs explicit metadata.

---

## Building servers with typed PV handles

This is the recommended API. Each factory returns a `Pv` handle; the handle is
*pending* until a `Server` is built from it, and *live* afterwards.

### Creating PVs

Scalar records (each is served as an NTScalar with the given value type):

| Factory | Python type | Wire NT type | Writable by clients | EPICS analogue |
|---|---|---|---|---|
| `spvirit.ai(name, initial, **opts)` | `float` | NTScalar `double` | no | analog input |
| `spvirit.ao(name, initial, **opts)` | `float` | NTScalar `double` | yes | analog output |
| `spvirit.bi(name, initial, **opts)` | `bool` | NTScalar `boolean` | no | binary input |
| `spvirit.bo(name, initial, **opts)` | `bool` | NTScalar `boolean` | yes | binary output |
| `spvirit.longin(name, initial, **opts)` | `int` | NTScalar `int` (32-bit) | no | long input |
| `spvirit.longout(name, initial, **opts)` | `int` | NTScalar `int` (32-bit) | yes | long output |
| `spvirit.string_in(name, initial, **opts)` | `str` | NTScalar `string` | no | string input |
| `spvirit.string_out(name, initial, **opts)` | `str` | NTScalar `string` | yes | string output |
| `spvirit.scalar(name, initial, *, type, writable=False, **opts)` | per type | NTScalar `<type>` | `writable=` flag | — |

"Read-only over the wire" means network clients cannot PUT the value; your
Python code can always `set()` it. For the remaining NTScalar wire types
(`long`, unsigned, `float`) use `spvirit.scalar(...)`, which covers all
twelve.

#### NT scalar type coverage

PVAccess defines twelve NTScalar value types: `boolean`, `byte`, `short`,
`int`, `long`, their unsigned variants (`ubyte`, `ushort`, `uint`, `ulong`),
`float`, `double`, and `string`. All twelve are creatable:

- **Scalars** — `spvirit.scalar(name, initial, *, type, writable=False, **opts)`
  selects the wire type explicitly by name (or alias, e.g. `"i32"`/`"u16"`/
  `"f64"`); `writable=True` serves the output flavor, `False` (default) the
  input flavor.
- **Arrays** — `waveform`/`aai`/`aao(name, data, *, type=None)` take the same
  `type=` for the element type; see below for the inference behavior when
  `type=` is omitted.

```python
counter = spvirit.scalar("SIM:COUNT", 0, type="long", writable=True)
gain    = spvirit.scalar("SIM:GAIN", 1, type="ushort")
level   = spvirit.scalar("SIM:LEVEL", 3.3, type="float", units="V")
```

The type string follows the `PvInfo` convention used throughout the API:
`"boolean"|"bool"`, `"byte"|"int8"|"i8"`, `"short"`, `"int"`, `"long"`,
`"ubyte"`, `"ushort"`, `"uint"`, `"ulong"`, `"float"|"f32"`,
`"double"|"f64"`, `"string"|"str"`. An unrecognized type string raises
`ValueError`.

**Coercion is strict.** Converting a Python initial value (or a later `set()`
through a typed handle) to the target wire type follows these rules:

- Out-of-range values (e.g. `300` into `"byte"`, `-1` into `"ushort"`) raise
  `OverflowError`.
- Wrong-kind values (`str` into an int/float type, a fractional `float` like
  `1.5` into an int type, a `bool` into a non-boolean type) raise `TypeError`.
- Widening is always allowed: `int` → `float`/`double`, and an *integral*
  float (`2.0`) → any int type.
- Narrowing `double` → `float` is allowed and may lose precision (lossy
  f64→f32 rounding), same as an EPICS `float` field.

`server.pv(name)` — which can attach to *any* served record, including
`.db`-loaded and classic-builder ones — no longer raises `KeyError` for
`long`/unsigned records. It maps wire types onto handles as follows:

| Wire NT scalar type | Handle kind | Notes |
|---|---|---|
| `double`, `float` | `float` | `float` (f32) is widened to Python `float` |
| `boolean` | `bool` | |
| `byte`, `short`, `int` | `int` | narrower ints are widened to 32-bit |
| `string` | `str` | |
| NTEnum | `int` | the value is the choice index |
| NTScalarArray (any element type) | array | see element notes below |
| `long`, `ubyte`, `ushort`, `uint`, `ulong` | dynamically typed handle | `repr(pv)` shows the wire type, e.g. `<spvirit.Pv 'SIM:COUNT' (long)>` |

A dynamically typed handle's `set()`/`get()` still uses plain Python `int`
values, coerced with the same strict rules as above (an out-of-range write
raises `OverflowError`).

**Widened `int` handles are not range-checked.** A `byte`/`short`/`int`
record maps onto the plain Python `int` handle kind above, which is *not*
the same code path as `spvirit.scalar()`'s typed handles or
`store.set_value` — writes through it use the core's C-style narrowing cast
instead of strict coercion, so an out-of-range write silently wraps (e.g.
`300` written to a `byte` record stores `44`) rather than raising
`OverflowError`. Use `spvirit.scalar(name, initial, type="byte", ...)` or
`store.set_value` when you need strict range enforcement on these types.

Array element types: `waveform`/`aai`/`aao` infer the element type from the
data when `type=` is omitted — `bytes` becomes an unsigned-byte (`ubyte[]`)
array and reads back as `bytes`; lists become `boolean[]`/`long[]`/
`double[]`/`string[]` from the first element (int lists are stored as 64-bit
elements) and read back as lists. Mixed-type lists raise `TypeError`. Passing
`type=` overrides inference and coerces every element strictly (same
OverflowError/TypeError rules as scalars).

Enum records (served as NTEnum, value is the choice index):

```python
mode = spvirit.mbbi("SIM:MODE", ["Off", "Standby", "Running"], 0)
cmd  = spvirit.mbbo("SIM:CMD",  ["Stop", "Start", "Reset"],   0, desc="Command")
```

`mbbi` is read-only over the wire, `mbbo` is writable. Writes with an
out-of-range index are rejected. Only the `desc` option applies to enums.

Array records (served as NTScalarArray):

```python
wf    = spvirit.waveform("SIM:WF", [0.0] * 1024)                # writable
raw   = spvirit.aai("SIM:RAW", bytes(512))                       # read-only, U8 array
table = spvirit.aao("SIM:TBL", [1, 2, 3])                        # writable
counts = spvirit.waveform("SIM:CNT", [0] * 1024, type="ushort")  # explicit element type
```

`data` may be a list of `bool`/`int`/`float`/`str` (element type inferred from
the first element; an empty list defaults to `double[]` *only when `type=` is
omitted*) or `bytes` (stored as an unsigned-byte array, returned to Python as
`bytes`). Pass `type=` to pick any of the twelve wire element types
explicitly, overriding inference.

Type-inferred creation — `spvirit.pv(name, initial, **opts)` picks the record
type from the initial value:

| Initial value | Result |
|---|---|
| `bool` | `bo` (checked before `int` — `True` is an `int` in Python) |
| `int` | `longout` |
| `list` or `bytes` | `waveform` |
| `float` | `ao` |
| `str` | `string_out` |
| anything else | `TypeError` |

Inference always produces the *writable* flavor; use the explicit factories
when you need read-only records. Pass `type=` to `spvirit.pv(...)` to override
inference and pick the wire type explicitly: `double`/`boolean`/`int`/
`string` map onto the same native handle kinds as above, any other type
(`long`, unsigned, `float`) produces a dynamically typed handle, and a
`list`/`bytes` initial value with `type=` produces a typed waveform.

### Common options

All scalar factories (and `spvirit.pv`) accept these keyword-only options:

```python
pressure = spvirit.ai(
    "SIM:PRESSURE", 101.3,
    units="kPa",                              # engineering units (EGU)
    prec=1,                                   # display precision (PREC)
    desc="Chamber pressure",                  # description (DESC)
    adel=0.5,                                 # archive deadband (ADEL)
    mdel=0.1,                                 # monitor deadband (MDEL)
    drive_limits=(0.0, 200.0),                # control limits (DRVL, DRVH)
    alarm_limits=(10.0, 20.0, 150.0, 180.0),  # (LOLO, LOW, HIGH, HIHI)
)
```

- `mdel` suppresses monitor updates smaller than the deadband; `adel` does the
  same for the archive-tuned monitor channel (what the Archiver Appliance
  subscribes to).
- `alarm_limits` combined with the server's alarm computation drives
  MINOR/MAJOR alarm severity automatically as the value crosses the limits.
- Field values are visible to clients QSRV-style: `pvget SIM:PRESSURE.EGU`,
  `SIM:PRESSURE.DESC`, `SIM:PRESSURE.RTYP`, etc.

### Reading and writing: set / get / set_async / get_async

```python
temp.set(21.7)        # blocking; full posting pipeline (monitors, deadbands, alarms)
value = temp.get()    # blocking; returns float/bool/int/str/list per PV type

# asyncio variants
await temp.set_async(21.8)
value = await temp.get_async()
```

- All four release the GIL; blocking calls are safe from any Python thread.
- Writing the wrong Python type raises `TypeError`.
- On an array handle, `set()` coerces the given list/bytes to the record's
  current element type — a plain Python `int` list written to a `ushort[]`
  record stays `ushort[]` on the wire, and an out-of-range element raises
  `OverflowError`. `set()` never changes an array's element type.
- Calling `set`/`get` on a handle that has not been given to a `Server` yet
  raises `RuntimeError` ("unbound").
- `pv.name` is the PV name; `repr(pv)` shows the name and value kind,
  e.g. `<spvirit.Pv 'SIM:TEMP' (float)>`.
- `set()` writes authoritatively: it is **not** subject to the PV's `on_put`
  validator, which guards client (wire) writes only — the same split as
  p4p's `post()` vs its put handler.

### Reacting to client writes: on_put

`on_put` attaches a validator/handler that runs **before** a client PUT is
applied. Use it as a decorator or a plain method call:

```python
sp = spvirit.ao("SIM:SP", 20.0)

@sp.on_put
def check(pv, value):
    if value < 0:
        return False          # reject: client's put fails on the wire
    print(f"{pv.name} <- {value}")
```

- The callback receives `(pv, value)` — the handle itself and the incoming
  value, already converted to the handle's Python type.
- Returning `False` **or raising any exception** rejects the PUT: the value is
  not applied and the writing client receives an error. Any other return
  value (including `None`) accepts it.
- Callbacks may freely call `set()`/`get()` on *other* PVs (re-entrancy is
  safe), e.g. to update a readback when a setpoint changes.
- Not supported on array PVs (`TypeError`).
- `on_put` returns the callback unchanged, so the decorated function remains
  usable.

### Periodic updates: scan

`scan` registers a function called at a fixed period; its return value is
posted to the PV. Two forms:

```python
noise = spvirit.ai("SIM:NOISE", 0.0)

@noise.scan(period=0.1)          # decorator form
def tick(pv):
    return random.gauss(0.0, 1.0)

heartbeat = spvirit.longin("SIM:HB", 0)
count = itertools.count()
heartbeat.scan(1.0, lambda pv: next(count))   # direct form: scan(period, fn)
```

- The callback receives the handle and must return the new value.
- If it returns `None`, returns a non-convertible value, or raises, the scan
  re-posts the **last value this scan produced** (before the first successful
  tick, the type default: `0.0`, `False`, `0`, or `""`). It does not read the
  PV's live value.
- Scans must be attached **before** constructing the `Server`; attaching one
  to an already-bound handle is a no-op (a warning is logged).
- Not supported on array PVs (`TypeError`).

### Computed PVs: calc

`spvirit.calc` creates a read-only float PV recomputed whenever any of its
input PVs changes:

```python
a = spvirit.ao("SIM:A", 1.0)
b = spvirit.ao("SIM:B", 2.0)
total = spvirit.calc("SIM:SUM", [a, b], lambda vals: vals[0] + vals[1])
```

- Inputs must all be float PVs (`ai`/`ao`); anything else raises `TypeError`.
- The callback receives the current input values as `list[float]` and returns
  a `float`. Exceptions or non-float returns are logged and treated as `0.0`.
- Like scans, calc PVs must be created before the `Server` is built.

### Alarms: set_alarm

Set a record's alarm state explicitly, independent of its value:

```python
temp.set_alarm(2, 3, "sensor unplugged")   # severity=MAJOR, status=STATE
temp.set_alarm(0, 0)                       # clear (message defaults to "")
```

Severity follows the EPICS convention: 0 = NO_ALARM, 1 = MINOR, 2 = MAJOR,
3 = INVALID. The change is published to monitoring clients immediately.
Available on scalar and array handles.

### The Server class

```python
server = spvirit.Server(
    pvs=[temp, setpoint, wf],   # typed handles (and/or use db_file/db_string)
    db_file="records.db",       # optional: load an EPICS .db file
    db_string="...",            # optional: inline .db content
    sources=[("gw", 10, MySource())],  # optional: dynamic sources (label, order, obj)
    port=5075,                  # TCP port (default 5075)
    udp_port=5076,              # UDP search/beacon port (default 5076)
    listen_ip="0.0.0.0",        # bind address
    advertise_ip="192.168.1.10",  # address advertised in beacons/search replies
    compute_alarms=True,        # derive severity from alarm_limits
    beacon_period=15,           # beacon interval, whole seconds
)
```

All arguments are keyword-only and optional. Then either:

- `server.start()` — serve on a background thread and return immediately (the
  usual choice), or
- `server.run()` — serve on the *current* thread, blocking forever, or
- `server.start_background()` — like `start()` but returns a
  [`Store`](#runtime-store-access) handle.

Other methods:

- `server.pv(name) -> Pv` — mint a typed handle to **any** served record,
  including ones loaded from a `.db` file or added via the classic builder.
  The handle's type is inferred from the record (floats → float, enums → int
  index, arrays → array). Unknown names raise `KeyError`, and so do records
  with no handle representation (NTTable, NTNDArray, generic structures —
  use the [`Store`](#runtime-store-access) for those).
- `Server.builder()` — static shorthand for `spvirit.ServerBuilder()`.
- `server.store() -> Store` — runtime get/set access (see below).
- `server.notifier() -> Notifier` — publish monitor updates for
  source-claimed PVs.
- `server.add_source(label, order, source)` — register a dynamic source after
  construction.

Handles work identically before and after `start()`: `set()` on a pending
handle raises `RuntimeError`; once the server is constructed the handle is
bound and live.

### Generating many PVs programmatically

Handles are plain Python objects — build them in loops and comprehensions:

```python
import spvirit

channels = {
    f"BL:DET:CH{i:03d}": spvirit.ai(f"BL:DET:CH{i:03d}", 0.0, units="counts")
    for i in range(100)
}

setpoints = []
for i in range(16):
    sp = spvirit.ao(f"BL:MOT:M{i}:SP", 0.0, drive_limits=(-180.0, 180.0))

    @sp.on_put
    def moved(pv, value, i=i):          # bind i per-PV
        print(f"motor {i} -> {value}")

    setpoints.append(sp)

server = spvirit.Server(pvs=[*channels.values(), *setpoints])
server.start()

channels["BL:DET:CH042"].set(1234.0)    # keep the dict for later access
```

---

## Loading EPICS .db files

Existing StreamDevice/EPICS-style `.db` files load directly:

```python
server = spvirit.Server(db_file="ioc.db")
# or
server = spvirit.Server(db_string="""
record(ao, "DEMO:VOLTAGE") {
    field(DESC, "Supply voltage")
    field(EGU,  "V")
    field(VAL,  "5.0")
}
""")

v = server.pv("DEMO:VOLTAGE")   # typed handle onto a .db-loaded record
v.set(5.2)
```

Supported record types include `ai`, `ao`, `bi`, `bo`, `stringin`,
`stringout`, `mbbi`, `mbbo`, `waveform`, `aai`, `aao`, and common fields
(`VAL`, `DESC`, `EGU`, `PREC`, `HIHI`/`HIGH`/`LOW`/`LOLO`, `DRVH`/`DRVL`,
`ADEL`, `MDEL`, …).

---

## The classic builder API

The fluent `ServerBuilder` predates the typed handles and remains fully
supported — it also exposes a few record shapes the handle API does not yet
(tables, ND arrays, sub-arrays, generic structures):

```python
import spvirit

server = (
    spvirit.ServerBuilder()
    .ai("DEMO:TEMP", 21.5)
    .ao("DEMO:SP", 20.0)
    .mbbo("DEMO:MODE", ["Off", "On", "Auto"], 0)
    .waveform("DEMO:WF", [0.0] * 100)
    .waveform("DEMO:CNT", [0] * 100, type="ushort")
    .sub_array("DEMO:WF10", [0.0] * 100, indx=5, nelm=10)
    .nt_table("DEMO:TBL", {"name": ["a", "b"], "value": [1.0, 2.0]},
              types={"value": "float"})
    .nt_ndarray("DEMO:IMG", bytes(64 * 48), dims=[(64, 0), (48, 0)])
    .generic("DEMO:CFG", "my:struct:1.0", {"gain": 2.5, "taps": [1, 2, 3]},
             types={"taps": "short[]"})
    .db_file("extra.db")
    .on_put("DEMO:SP", lambda name, value: print(name, "<-", value))
    .scan("DEMO:TEMP", 1.0, lambda name: read_sensor())
    .port(5075)
    .udp_port(5076)
    .listen_ip("0.0.0.0")
    .advertise_ip("192.168.1.10")
    .compute_alarms(True)
    .beacon_period(15)
    .add_source("gateway", 10, MySource())
    .build()
)
server.start()
```

The full method set: record definitions `ai`, `ao`, `bi`, `bo`, `string_in`,
`string_out`, `mbbi`, `mbbo`, `waveform`, `aai`, `aao`, `sub_array`,
`nt_table`, `nt_ndarray`, `generic` (no `longin`/`longout` — 32-bit integer
records exist only in the handle API); loading `db_file`, `db_string`;
callbacks `on_put`, `scan`; configuration `port`, `udp_port`, `listen_ip`,
`advertise_ip`, `compute_alarms`, `beacon_period` (whole seconds),
`add_source`; and `build()`.

Differences from the handle API:

- `on_put(name, callback)` is string-keyed and receives `(pv_name, value)`;
  exceptions are logged, **not** used to reject the put.
- `scan(name, period, callback)` receives the PV name and returns the new
  value; errors post `0.0`.
- `build()` returns the same `Server` class described above, so `server.pv(name)`
  works to obtain typed handles onto builder-defined records afterwards.
- A builder is single-use: any method after `build()` raises `RuntimeError`.
- The builder's record methods take no *metadata* kwargs (`units=`, `prec=`,
  …); set fields via a `.db` file or use the handle factories. Value types
  are selectable, though: `waveform`/`aai`/`aao`/`sub_array`/`nt_ndarray`
  take `type=` for the element type, and `nt_table`/`generic` take `types=` —
  a dict mapping column/field name to a type string (e.g.
  `{"taps": "short[]"}` for an array-typed generic field). Omitting
  `type=`/`types=` falls back to the same inference as the handle
  factories.

---

## Runtime store access

`Store` (from `server.store()` or `start_background()`) provides direct,
name-keyed access to the record store — useful for generic tooling where
typed handles are inconvenient:

```python
store = server.store()

store.pv_names()                      # -> list[str], all served PVs
store.get_value("DEMO:TEMP")          # -> scalar value or None
store.set_value("DEMO:TEMP", 22.0)    # -> True if the PV exists
store.set_array_value("DEMO:WF", [1.0, 2.0, 3.0])
store.get_nt("DEMO:TEMP")             # -> full NT payload (NtScalar, ...)
store.put_nt("DEMO:TEMP", nt)         # write a full NT payload
```

`get_nt`/`put_nt` round-trip complete Normative Type payloads including alarm,
timestamp, display, and control substructures — see
[Normative Type classes](#normative-type-classes).

`set_value` and `set_array_value` coerce the incoming value strictly to the
record's *existing* wire type (the same OverflowError/TypeError rules as
elsewhere) and never retype a record — there is no way to change a scalar or
scalar-array record's wire type after creation through the store. `put_nt`
follows the same never-retype rule for `NtScalar`/`NtScalarArray` payloads;
for `NtTable`/`NtNdArray` payloads, `put_nt` replaces the record's stored
payload wholesale — columns, element types, and all — since the whole
structure comes from the payload you supply. Writing to a name that isn't
served returns `False` (or, for `get_value`, `None`) rather than raising.

---

## Dynamic sources

Sources claim PV names at runtime instead of serving a fixed record store —
the building block for gateways, protocol bridges, and virtual PV namespaces.
A source is **any Python object** implementing this duck-typed protocol
(each method may be `def` or `async def`):

```python
import time
import spvirit

class SensorSource:
    def __init__(self):
        self.notifier = None

    def claim(self, name):
        """Return PvInfo (or a dict) to claim `name`, None to decline."""
        if name.startswith("SENSOR:"):
            return spvirit.PvInfo.nt_scalar("double", writable=True)
        return None

    def get(self, name):
        """Return the current value as an NT payload."""
        return spvirit.NtScalar(read_hardware(name), units="C")

    def put(self, name, value):
        """Apply a client write. Raise to reject.
        Optionally return NT payload(s) to publish as monitor updates."""
        write_hardware(name, value)
        return spvirit.NtScalar(value)

    def names(self):                       # optional: PV listing support
        return ["SENSOR:T1", "SENSOR:T2"]

    def rpc(self, name, args):             # optional: NTURI RPC support
        return spvirit.NtScalar(float(args["x"]) * 2)

    def on_start(self, notifier):          # optional: called at server start, not at construction
        self.notifier = notifier           # keep it to push updates later

    def fields(self, name):                # optional: `.FIELD` access support
        if not name.startswith("SENSOR:"):
            return None
        return {"DESC": "a sensor reading", "EGU": "C"}

server = spvirit.Server(sources=[("sensors", 10, SensorSource())])
server.start()
```

Key points:

- `claim(name)` is called on client search; return `spvirit.PvInfo`, a dict
  `{"struct_id": ..., "fields": {...}, "writable": bool}`, or `None`.
  Convenience constructors: `PvInfo.nt_scalar("double", writable=True)`,
  `PvInfo.nt_scalar_array("double")` (pass the *element* type), or the full
  `PvInfo(struct_id, fields, writable=False)` where `fields` maps field names
  to type strings (`"double"`, `"int"`, `"string"`, `"boolean"`, `"double[]"`,
  `"any"`, …). A `PvInfo` exposes read-only `.writable` and `.struct_id`
  properties.
- `put` return values become monitor updates: return one NT payload, a dict
  `{pv_name: payload}`, or a list of `(pv_name, payload)` tuples to fan
  updates out to related PVs; return `None` for no propagation. Raising an
  exception rejects the client's PUT.
- **Push updates** at any time via the `Notifier` (from `on_start` or
  `server.notifier()`): `notifier.notify(pv_name, nt_payload)`. It is safe to
  call from any thread, including from inside another source callback.
- The `order` number decides precedence when several sources could claim a
  name — lower is tried first; the built-in record store is order 0.
- `async def` source methods run on a dedicated background asyncio event loop
  (thread name `spvirit-asyncio`) — do not call `asyncio.run` yourself inside
  source methods.

### `fields(self, name) -> dict | None` (optional)

Return a mapping of EPICS field name to value for the record `name`, or
`None` if this source does not own it. Defining it makes `<name>.<FIELD>`
resolvable for this source's records, matching what the builtin store and
the processing engine already serve. Fields you omit read as their
`dbCommon` defaults, so a source usually only lists `DESC`, `EGU`, `RTYP`
and whatever else it genuinely knows.

```python
def fields(self, name):
    if name != "PY:A":
        return None
    return {"DESC": "a python record", "EGU": "mm"}
```

Field PVs are read-only.

The `.FIELD` tier is derived automatically on every registration path �
`ServerBuilder.add_source`, `Server(sources=[...])`, and post-`build()`
`Server.add_source` alike. It is registered as a separate source labelled
`{label}-fields` at `order + 10`, so a source of your own registered at that
exact order shares the slot; ties resolve by registration order.

---

## Lifecycle hooks and events

### `@builder.on_start`

Registered on the **builder**, before `build()` — there is no `server.on_start`.
A hook runs once, at server start: after the store is built, before scan
tasks spawn, and before the server accepts connections. No client can
observe a pre-initialisation value.

```python
b = spvirit.ServerBuilder().ao("SETPOINT", 0.0).port(0).udp_port(0)

@b.on_start
def initialise(store):
    store.set_value("SETPOINT", 22.5)

server = b.build()
server.start_background()
```

Hooks may be `def` or `async def` (an `async def` is awaited on the
`spvirit-asyncio` loop) and run in registration order. A [dynamic
source's](#dynamic-sources) `on_start(notifier)` shares that same ordered
list — builder hooks and source hooks interleave in true registration
order, whichever `add_source`/`@builder.on_start` call came first. A
source's `order` value governs which source claims a PV name; it has no
effect on this startup sequence.

A hook that raises (or panics) **aborts startup** — the server does not
serve, and the error names the hook. This is deliberate: startup is a
barrier under your control, so failing loudly and early is safe. A handler
firing at 3am from a live event is a different story — see below.

> **Migration note.** Before this feature, a Python source's `on_start` fired
> during `build()`. It now fires at server start (`start()` /
> `start_background()` / `run()`), alongside `@builder.on_start` hooks. Code
> that relied on a source's `on_start` running at `build()` time will now see
> it run later.

### `@builder.on_event` and `post_event`

A named trigger that fans out across the whole server. Handlers are also
registered on the builder, not the running server.

```python
b = spvirit.ServerBuilder().ao("SHUTTER:COUNT", 0.0).port(0).udp_port(0)

@b.on_event("SHUTTER")
async def on_shutter(store, event):
    store.set_value("SHUTTER:COUNT", store.get_value("SHUTTER:COUNT") + 1)

server = b.build()
server.start_background()
server.post_event("SHUTTER")
```

Handlers receive `(store, event)`; the second argument lets one function
serve several events. They are **deferred**: `post_event` queues them and
returns. The only guarantee is that when `post_event` returns, any IOC
records on that event have processed and the handlers are queued — never
read "queued" as "run". Handlers run one at a time, in registration order,
**across all events** on a single dispatcher — a slow handler for one event
delays every other event's handlers behind it.

The record half of that guarantee is real, not aspirational: `post_event`
blocks the calling Python thread until every registered record consumer has
finished, releasing the GIL for the duration, so a consumer that itself
needs the GIL cannot deadlock against the poster. (Nothing registers such a
consumer yet; the seam exists for the scan-list work.)

A handler that raises is logged and skipped; the next handler still runs
(unlike a raising `on_start`, which aborts startup — see above). The queue
is bounded (1024 entries); under sustained overload, invocations are
dropped with a counter rather than growing without limit. Posting an event
with no registered handlers is a no-op, not an error.

`server.drain_events()` blocks until the queue is empty. It exists so tests
can wait for handlers deterministically; production code should not need
it, since it does not know when a handler will next need to fire. Calling
it with handlers queued but the server never started fails immediately and
says so, rather than waiting out the 10 s drain timeout.

`post_event` takes a name only. Data travels through PVs, where it is
observable and visible to both stores.

---

## Normative Type classes

Full-fidelity payloads for `Store.get_nt`/`put_nt`, sources, and the notifier.

```python
nt = spvirit.NtScalar(
    42.5,
    units="mm",
    display_low=0.0, display_high=100.0,
    display_description="Position", display_precision=3,
    control_low=0.0, control_high=90.0, control_min_step=0.1,
    alarm_severity=0, alarm_status=0, alarm_message="",
)
nt.value            # 42.5   (all properties are read-only)
nt.units            # "mm"
nt.alarm_severity   # 0
nt.value_type       # "double"

count = spvirit.NtScalar(1000, type="long")
count.value_type    # "long"
```

- `NtScalar(value, units="", display_low=0.0, display_high=0.0,
  display_description="", display_precision=0, control_low=0.0,
  control_high=0.0, control_min_step=0.0, alarm_severity=0, alarm_status=0,
  alarm_message="", type=None)` — scalar with display/control/alarm metadata.
  `type=` picks the wire value type explicitly (same type-string convention
  and strict coercion as `spvirit.scalar(...)`); omitted, the type is
  inferred from `value` the same way `spvirit.pv(...)` infers it. `.value_type`
  returns the canonical wire type name, e.g. `"double"`, `"ushort"`.
- `NtScalarArray(value, type=None)` — array payload (list or bytes); `type=`
  picks the element type explicitly. Exposes `.value`, `.value_type`,
  `.alarm`, `.time_stamp`, `.display`, `.control`.
- `NtTable(columns, *, labels=None, types=None, descriptor=None)` — build a
  table from a `{column_name: list}` dict; `labels` defaults to the column
  names, `types` optionally maps column name to a value-type string.
  `.labels`, `.columns() -> dict[str, list]`, `.column_types() -> dict[str, str]`,
  `.descriptor`, `.alarm`, `.time_stamp` (the latter two `None` unless the
  table came from a read).
- `NtNdArray(value, dims, *, type=None)` — build an N-D array from flat data
  and a list of per-dimension sizes (offset 0, binning 1, not reversed);
  `type=` picks the element type explicitly. `.value`, `.value_type`,
  `.dimensions()` (list of dicts with `size`/`offset`/`full_size`/`binning`/
  `reverse`), `.unique_id`, `.compressed_size`, `.uncompressed_size`,
  `.data_time_stamp`.
- Substructure classes, each with read-only properties matching their
  constructor arguments:
  - `Alarm(severity=0, status=0, message="")`
  - `TimeStamp(seconds_past_epoch=0, nanoseconds=0, user_tag=0)`
  - `Display(limit_low=0.0, limit_high=0.0, description="", units="", precision=0)`
  - `Control(limit_low=0.0, limit_high=0.0, min_step=0.0)`
- Enum and generic-structure payloads are represented as plain dicts:
  enums as `{"index": int, "choices": [...], "selected": str}`, generic
  structures as `{"struct_id": ..., <field>: <value>, ...}`.

---

## Client API

High-level client for talking to any PVAccess server (spvirit or otherwise):

```python
import spvirit

client = spvirit.Client()                       # defaults: broadcast search

result = client.get("SIM:TEMP")                 # GetResult
result.value                                    # decoded value (dict for NT structures)
result.pv_name                                  # "SIM:TEMP"
result.raw_pva, result.raw_pvd                  # raw wire bytes, if you need them

client.get("SIM:TEMP", fields=["value", "alarm.severity"])   # partial pvRequest
client.get("SIM:TEMP", fields="value")          # single field: plain str is fine too

client.put("SIM:SP", 21.0)                      # blocking put (fields default: ["value"])
client.put("SIM:MODE", 2, fields=["value.index"])

def on_update(update):                          # a MonitorUpdate, not a bare value
    print(update.value)
    if done:
        return False                            # returning False stops the monitor
client.monitor("SIM:TEMP", on_update)           # blocks until stopped;
                                                # an exception raised in the
                                                # callback stops it and propagates

client.info("SIM:TEMP")                         # {"struct_id": ..., "fields": [...]}
client.pvlist("192.168.1.10:5075")              # PV names from a specific server
```

Every monitor callback — `Client.monitor`, `Client.subscribe` and
`Channel.monitor` — receives a `spvirit.lowlevel.MonitorUpdate`:

| Attribute | Meaning |
|---|---|
| `.value` | the decoded value, the dict these callbacks used to receive directly |
| `.changed` | dotted paths of the fields this update carries, e.g. `["value", "timeStamp.secondsPastEpoch"]` |
| `.overrun` | dotted paths of fields the server dropped at least one earlier update for |
| `.has_overrun` | `True` when `.overrun` is non-empty |

`"<whole structure>"` appears in `.changed` / `.overrun` when bit 0 is set —
the server is reporting the entire structure rather than named fields.

```python
def on_update(update):
    print(update.value["value"])
    if update.has_overrun:
        print("dropped updates for:", update.overrun)
```

> **Breaking change (0.1.20).** Monitor callbacks used to receive the decoded
> value itself. Add `.value` to restore the previous behaviour:
> `lambda v: print(v)` becomes `lambda u: print(u.value)`.

Non-blocking monitors — `subscribe` returns immediately with a
`Subscription` handle while updates are delivered on a background thread:

```python
sub = client.subscribe("SIM:TEMP", lambda u: print(u.value))
# ... program continues; run as many concurrent subscriptions as you like ...
sub.pv_name       # "SIM:TEMP"
sub.is_active     # True while updates are flowing
sub.close()       # stop promptly (idempotent, works even on a quiet PV)
sub.error         # None, or the message if the subscription ended on an error

with client.subscribe("SIM:PRESSURE", handle) as sub:   # context manager
    ...
```

- The callback receives each update sequentially (per subscription) on a
  runtime worker thread; keep it short, and hand heavy work to a queue.
  Returning `False` unsubscribes, exactly like `monitor`.
- Inside the callback you may call other spvirit operations (`pv.set()`,
  `client.get()`, …) — re-entrancy is safe.
- Failures don't raise (there is no caller to raise into): on a network error
  or an exception in the callback, the subscription ends, `is_active` becomes
  `False`, and `error` holds the message.
- Dropping the last reference to a `Subscription` closes it — keep the handle
  alive for as long as you want updates.

Configure with the builder when defaults don't fit:

```python
client = (
    spvirit.Client.builder()
    .server_addr("127.0.0.1:5075")   # skip UDP search, connect directly
    .search_addr("192.168.1.255")    # or: unicast/broadcast search target
    .name_server("10.0.0.5:5075")    # or: TCP name server
    .udp_port(5076)                  # search port        (default 5076)
    .port(5075)                      # TCP port           (default 5075)
    .timeout(2.0)                    # operation timeout  (default 5.0 s)
    .no_broadcast(True)              # disable broadcast search
    .bind_addr("0.0.0.0")
    .authnz_user("ops")              # AUTHZ identity
    .authnz_host("console1")
    .debug(True)                     # wire-level logging
    .build()
)
```

Server discovery (UDP beacons):

```python
for srv in spvirit.discover_servers(timeout=2.0):
    print(srv.guid.hex(), srv.tcp_addr)
```

(`spvirit.lowlevel` has its own `discover_servers` for diagnostic use — note
it returns `{"guid": hexstr, "addr": ...}` dicts rather than
`DiscoveredServer` objects, defaults to `timeout=1.0`, and accepts a
`targets=` list.)

All client operations block with the GIL released and raise the
[`SpviritError` exception tree](#errors-and-exceptions) on failure. Every
`fields=` parameter across the client API accepts either a list of dotted
paths or a single string.

---

## Low-level API: spvirit.lowlevel

For repeated operations on one PV, protocol inspection, or precise control,
`spvirit.lowlevel.Channel` keeps a persistent TCP connection:

```python
from spvirit.lowlevel import Channel

with Channel.connect("SIM:TEMP", "127.0.0.1:5075", timeout=5.0) as ch:
    ch.pv_name, ch.is_open, ch.server_addr, ch.sid
    desc = ch.introspect()          # StructureDesc (see codec section)
    r1 = ch.get()                   # reuses the connection — fast repeated gets
    r2 = ch.get(fields=["value"])
    ch.put(22.0)                    # fields: None -> ["value"], or str, or list
    ch.monitor(lambda u: print(u.value))
                                    # blocks; the callback gets a MonitorUpdate
                                    # and returns False to stop, a raised
                                    # exception stops it and propagates
    ch.close()                      # or rely on the context manager;
                                    # later operations raise ProtocolError
# async variants: connect_async, get_async, put_async, introspect_async,
# read_packet_async (monitor, close, and read_until are sync-only)
```

Raw frame access for protocol work:

```python
pkt = ch.read_packet(timeout=1.0)          # next raw PVA frame -> Packet
pkt.magic, pkt.version, pkt.command        # raw header fields (ints)
pkt.command_name, pkt.payload_length       # e.g. "MONITOR", body length
pkt.flags                                  # raw flag byte; decoded views:
pkt.is_application, pkt.is_control, pkt.is_client, pkt.is_server, pkt.is_msb
pkt.is_segmented                           # segmentation flag bits (int, 0 = none)
pkt.bytes, pkt.payload, len(pkt)           # full frame / payload bytes / frame len
pkt.details()                              # command-specific decoded dict
pkt.decode()                               # same as codec.decode_packet(pkt.bytes)
pkt = ch.read_until(lambda p: p.command_name == "MONITOR", timeout=5.0,
                    max_frames=100)        # RuntimeError if max_frames exhausted
```

> **Breaking change (0.1.20).** `read_packet` / `read_packet_async` /
> `read_until` now reassemble segmented messages before returning. A segmented
> message arrives as one `Packet` whose `is_segmented` is `0` and whose
> `payload_length` covers the concatenated payload — the individual segments
> are never surfaced. Code that stitched segments together by hand must stop
> doing so; it would now concatenate whole messages. Control frames (echo,
> flow control) still pass through one frame at a time and are unaffected.

Operations on one channel serialize internally, so concurrent `get_async`
calls on the same channel are safe but sequential on the wire. Monitors send
protocol echo keep-alives automatically.

Discovery utilities (`from spvirit.lowlevel import ...`), all with `*_async`
variants where noted:

- `search_pv(pv_name, udp_port=5076, timeout=3.0, targets=None, debug=False) -> "ip:port"`
  — UDP broadcast search (async: `search_pv_async`).
- `search_pv_tcp(pv_name, name_server, timeout=3.0, debug=False) -> "ip:port"`
  — search via a TCP name server.
- `discover_servers(udp_port=5076, timeout=1.0, targets=None, debug=False) -> [{"guid": hexstr, "addr": "ip:port"}]`
  (async: `discover_servers_async`).
- `pvlist(server_addr, timeout=5.0) -> (names, source)` where `source` says
  how the listing was obtained (`"pvlist"`, `"getfield"`, `"server_rpc"`,
  `"server_get"`) (async: `pvlist_async`).
- `parse_addr_list(s)`, `auto_broadcast_targets()`,
  `default_search_targets(search_addr=None, bind_addr=None)` — address-list
  helpers honoring `EPICS_PVA_ADDR_LIST` / `EPICS_PVA_AUTO_ADDR_LIST`.

`targets=` accepts a list of `(target_ip, bind_ip)` tuples or of
`{"target": ..., "bind": ...}` dicts, so the output of
`default_search_targets()` / `auto_broadcast_targets()` can be passed
straight through.

---

## Wire codec: spvirit.codec

Standalone encoders/decoders for PVAccess wire data — useful with
`Channel.read_packet`, captured traffic, or tests:

```python
from spvirit import codec

desc = codec.decode_introspection(intro_bytes)   # -> StructureDesc
print(desc.dump())                               # readable multi-line layout
desc.struct_id                                   # e.g. "epics:nt/NTScalar:1.0"
[f.name for f in desc.fields]                    # FieldDesc: .name, .field_type,
"value" in desc, desc.field("value")             #   .type_code, .is_array, .struct_desc

value = codec.decode_value(data_bytes, desc)     # decode pvData -> Python
codec.format_value(value)                        # compact one-line rendering
codec.extract_nt_value(value)                    # pull `value` out of an NT dict

codec.encode_pv_request(["value", "alarm"])      # pvRequest bytes (None/empty = all)
codec.encode_put_payload(desc, {"value": 42.0})  # bitset + data for a PUT

codec.decode_packet(frame_bytes)                 # full frame -> dict with magic,
                                                 # command_name, flags, details, ...
```

The wire-level encoders/decoders (`decode_introspection`, `decode_value`,
`encode_pv_request`, `encode_put_payload`) accept `is_be=True` for big-endian
streams (default little); `format_value`, `extract_nt_value`, and
`decode_packet` take no endianness flag. `StructureDesc` also supports
`len(desc)` for its field count.

---

## Threading and async model

- The extension runs one shared multi-threaded Tokio runtime per process,
  created lazily. Blocking methods release the GIL, so other Python threads
  keep running during network operations.
- **Sync and async mix freely.** `set()`/`get()`/`Client.get()` etc. can be
  called from any Python thread. The async variants (`Pv.set_async`/`get_async`,
  `Channel.connect_async`/`get_async`/`put_async`, …) return awaitables for
  use inside `asyncio` code:

  ```python
  async def main():
      await sp.set_async(21.0)
      print(await temp.get_async())
  asyncio.run(main())
  ```

- **Callbacks are re-entrant.** `on_put`, scan, and calc callbacks run on
  runtime worker threads while holding the GIL only as needed; inside them
  you may call blocking handle methods on other PVs (e.g. update a readback
  from a setpoint's `on_put`).
- `Server.start()` serves from a background thread; `Server.run()` occupies
  the calling thread. Python `async def` source methods execute on a
  dedicated asyncio loop thread managed by the extension.
- Long-running Python callbacks stall the worker they run on — keep `on_put`
  and scan bodies short, and push slow work to your own threads, publishing
  results with `pv.set(...)` or `notifier.notify(...)`.

## Errors and exceptions

Client, channel, discovery, and codec operations raise this hierarchy
(rooted at `spvirit.SpviritError`):

| Exception | Meaning |
|---|---|
| `spvirit.TimeoutError` | operation or search timed out (also subclasses builtin `TimeoutError`) |
| `spvirit.SearchError` | PV could not be located |
| `spvirit.ProtocolError` | unexpected/invalid protocol state |
| `spvirit.DecodeError` | malformed wire data |
| `spvirit.IoError` | socket/OS-level failure (also subclasses builtin `OSError`) |
| `spvirit.PutRejectedError` | a put was rejected by an `on_put` validator |

`spvirit.TimeoutError` and `spvirit.IoError` deliberately dual-inherit, so
idiomatic `except TimeoutError:` and `except OSError:` catch them alongside
`except spvirit.SpviritError:`.

PV-handle operations use builtin exceptions where a builtin is the idiomatic
fit:

| Exception | Raised when |
|---|---|
| `RuntimeError` | handle not yet bound to a server; `ServerBuilder` method after `build()` |
| `KeyError` | `server.pv(name)` for an unknown or unsupported record |
| `TypeError` | wrong value type; wrong-kind value for a `type=`-selected wire type (e.g. `str` into an int type, a fractional `float` into an int type, `bool` into a non-boolean type); `on_put`/`scan` on an array PV; bad `calc` inputs; metadata options on an array `pv()` |
| `ValueError` | invalid address strings; non-finite floats in client puts; unknown `type=`/`types=` value-type string |
| `OverflowError` | a `type=`-coerced value is out of range for the target wire type |

## Examples

Runnable scripts live in
[`spvirit-py/examples/`](https://github.com/ISISNeutronMuon/spvirit/tree/main/spvirit-py/examples).
Start with the mailbox pair, then pick by topic:

- `demo_mailbox.py` / `demo_mailbox_client.py` — the smallest useful IOC (one
  writable PV) and a client that gets, puts, and subscribes to it.
- `demo_pvfind.py` — locate a PV (UDP search or a given address) and print its
  full structure introspection and current value.
- `demo_wire_inspector.py` — decode the actual PVAccess frames behind a get:
  introspection dump, captured raw bytes through `codec.decode_packet`, and
  live frames off the TCP stream.
- `demo_gateway.py` — a mini-gateway: a dynamic source that proxies another
  server under a `GW:` prefix with a put allow-list.
- `demo_expr_namespace.py` — an infinite computed namespace: any
  `EXPR:<expression>` PV is evaluated on demand.
- `demo_10k_farm.py` — 10,000 PVs plus calc aggregation built in a
  comprehension, with build/serve/write timings.

The directory also holds demos for every other part of the API (channels,
codec, discovery, NT access, and one per dynamic-source pattern).

## Building from source

Requires Rust (stable) and Python ≥ 3.9.

```bash
git clone https://github.com/ISISNeutronMuon/spvirit
cd spvirit/spvirit-py
python -m venv .venv && . .venv/bin/activate    # .venv\Scripts\activate on Windows
pip install maturin
maturin develop --release      # build + install into the venv
python tests/test_pv_handles.py
```

Releases are cut by tagging `spvirit-py-vX.Y.Z`; CI builds abi3 wheels for all
supported platforms plus an sdist and publishes to PyPI via trusted
publishing.

## License

BSD-3-Clause. Developed at the ISIS Neutron and Muon Source.
