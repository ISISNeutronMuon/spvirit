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
- [Building servers with typed PV handles](#building-servers-with-typed-pv-handles)
  - [Creating PVs](#creating-pvs)
  - [Common options](#common-options)
  - [Reading and writing: set / get / aset / aget](#reading-and-writing-set--get--aset--aget)
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
- [Normative Type classes](#normative-type-classes)
- [Client API](#client-api)
- [Low-level API: spvirit.lowlevel](#low-level-api-spviritlowlevel)
- [Wire codec: spvirit.codec](#wire-codec-spviritcodec)
- [Threading and async model](#threading-and-async-model)
- [Errors and exceptions](#errors-and-exceptions)
- [Building from source](#building-from-source)

---

## Installation

```bash
pip install spvirit
```

Wheels are published for Linux (x86_64, aarch64), Windows (x86_64), and macOS
(x86_64, arm64) for Python 3.9+ (one abi3 wheel per platform). No EPICS base,
no compiler needed.

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
  async variants (`aget`, `aset`, `*_async`) integrate with `asyncio`. See
  [Threading and async model](#threading-and-async-model).

---

## Building servers with typed PV handles

This is the recommended API. Each factory returns a `Pv` handle; the handle is
*pending* until a `Server` is built from it, and *live* afterwards.

### Creating PVs

Scalar records:

| Factory | Value type | Writable by clients | EPICS analogue |
|---|---|---|---|
| `spvirit.ai(name, initial, **opts)` | `float` | no | analog input |
| `spvirit.ao(name, initial, **opts)` | `float` | yes | analog output |
| `spvirit.bi(name, initial, **opts)` | `bool` | no | binary input |
| `spvirit.bo(name, initial, **opts)` | `bool` | yes | binary output |
| `spvirit.longin(name, initial, **opts)` | `int` (32-bit) | no | long input |
| `spvirit.longout(name, initial, **opts)` | `int` (32-bit) | yes | long output |
| `spvirit.string_in(name, initial, **opts)` | `str` | no | string input |
| `spvirit.string_out(name, initial, **opts)` | `str` | yes | string output |

"Read-only over the wire" means network clients cannot PUT the value; your
Python code can always `set()` it.

Enum records (served as NTEnum, value is the choice index):

```python
mode = spvirit.mbbi("SIM:MODE", ["Off", "Standby", "Running"], 0)
cmd  = spvirit.mbbo("SIM:CMD",  ["Stop", "Start", "Reset"],   0, desc="Command")
```

`mbbi` is read-only over the wire, `mbbo` is writable. Writes with an
out-of-range index are rejected. Only the `desc` option applies to enums.

Array records (served as NTScalarArray):

```python
wf    = spvirit.waveform("SIM:WF", [0.0] * 1024)     # writable
raw   = spvirit.aai("SIM:RAW", bytes(512))            # read-only, U8 array
table = spvirit.aao("SIM:TBL", [1, 2, 3])             # writable
```

`data` may be a list of `bool`/`int`/`float`/`str` (element type inferred from
the first element) or `bytes` (stored as an unsigned-byte array, returned to
Python as `bytes`).

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
when you need read-only records.

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

### Reading and writing: set / get / aset / aget

```python
temp.set(21.7)        # blocking; full posting pipeline (monitors, deadbands, alarms)
value = temp.get()    # blocking; returns float/bool/int/str/list per PV type

# asyncio variants
await temp.aset(21.8)
value = await temp.aget()
```

- All four release the GIL; blocking calls are safe from any Python thread.
- Writing the wrong Python type raises `TypeError`.
- Calling `set`/`get` on a handle that has not been given to a `Server` yet
  raises `RuntimeError` ("unbound").
- `pv.name()` returns the PV name; `repr(pv)` shows the name and value kind,
  e.g. `<spvirit.Pv 'SIM:TEMP' (float)>`.

### Reacting to client writes: on_put

`on_put` attaches a validator/handler that runs **before** a client PUT is
applied. Use it as a decorator or a plain method call:

```python
sp = spvirit.ao("SIM:SP", 20.0)

@sp.on_put
def check(pv, value):
    if value < 0:
        return False          # reject: client's put fails on the wire
    print(f"{pv.name()} <- {value}")
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
    compute_alarms=True,        # derive severity from alarm_limits
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
  index, arrays → array). Unknown names raise `KeyError`.
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
    .sub_array("DEMO:WF10", [0.0] * 100, indx=5, nelm=10)
    .nt_table("DEMO:TBL", {"name": ["a", "b"], "value": [1.0, 2.0]})
    .nt_ndarray("DEMO:IMG", bytes(64 * 48), dims=[(64, 0), (48, 0)])
    .generic("DEMO:CFG", "my:struct:1.0", {"gain": 2.5, "taps": [1, 2, 3]})
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

Differences from the handle API:

- `on_put(name, callback)` is string-keyed and receives `(pv_name, value)`;
  exceptions are logged, **not** used to reject the put.
- `scan(name, period, callback)` receives the PV name and returns the new
  value; errors post `0.0`.
- `build()` returns the same `Server` class described above, so `server.pv(name)`
  works to obtain typed handles onto builder-defined records afterwards.
- A builder is single-use: methods after `build()` raise `RuntimeError`.

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

    def on_start(self, notifier):          # optional: called at registration
        self.notifier = notifier           # keep it to push updates later

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
  `"any"`, …).
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
```

- `NtScalar(value, units="", display_low=0.0, display_high=0.0,
  display_description="", display_precision=0, control_low=0.0,
  control_high=0.0, control_min_step=0.0, alarm_severity=0, alarm_status=0,
  alarm_message="")` — scalar with display/control/alarm metadata.
- `NtScalarArray(value)` — array payload (list or bytes); exposes `.value`,
  `.alarm`, `.time_stamp`, `.display`, `.control`.
- `NtTable` — returned by reads (no Python constructor); `.labels`,
  `.columns() -> dict[str, list]`, `.descriptor`, `.alarm`, `.time_stamp`.
- `NtNdArray` — returned by reads; `.value`, `.dimensions()` (list of dicts
  with `size`/`offset`/`full_size`/`binning`/`reverse`), `.unique_id`,
  `.compressed_size`, `.uncompressed_size`, `.data_time_stamp`.
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

client.put("SIM:SP", 21.0)                      # blocking put (fields default: ["value"])
client.put("SIM:MODE", 2, fields=["value.index"])

def on_update(value):
    print(value)
    if done:
        return False                            # returning False stops the monitor
client.monitor("SIM:TEMP", on_update)           # blocks until stopped

client.info("SIM:TEMP")                         # {"struct_id": ..., "fields": [...]}
client.pvlist("192.168.1.10:5075")              # PV names from a specific server
```

Non-blocking monitors — `monitor_non_blocking` returns immediately with a
`Subscription` handle while updates are delivered on a background thread:

```python
sub = client.monitor_non_blocking("SIM:TEMP", lambda v: print(v))
# ... program continues; run as many concurrent subscriptions as you like ...
sub.pv_name       # "SIM:TEMP"
sub.is_active     # True while updates are flowing
sub.close()       # stop promptly (idempotent, works even on a quiet PV)
sub.error         # None, or the message if the subscription ended on an error

with client.monitor_non_blocking("SIM:PRESSURE", handle) as sub:   # context manager
    ...
```

- The callback receives each update sequentially (per subscription) on a
  runtime worker thread; keep it short, and hand heavy work to a queue.
  Returning `False` or raising unsubscribes, exactly like `monitor`.
- Inside the callback you may call other spvirit operations (`pv.set()`,
  `client.get()`, …) — re-entrancy is safe.
- Network failures don't raise (there is no caller to raise into): the
  subscription ends, `is_active` becomes `False`, and `error` holds the
  message.
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

All client operations block with the GIL released and raise the
[`SpviritError` exception tree](#errors-and-exceptions) on failure.

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
    ch.monitor(lambda v: print(v))  # blocks; callback returns False to stop
# closed on exit; every method also exists as *_async (connect_async,
# get_async, put_async, introspect_async, read_packet_async)
```

Raw frame access for protocol work:

```python
pkt = ch.read_packet(timeout=1.0)          # next raw PVA frame -> Packet
pkt.command_name, pkt.payload_length       # header fields
pkt.is_application, pkt.is_control, pkt.is_server, pkt.is_msb
pkt.bytes, pkt.payload                     # full frame / payload bytes
pkt.details()                              # command-specific decoded dict
pkt = ch.read_until(lambda p: p.command_name == "MONITOR", timeout=5.0,
                    max_frames=100)
```

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

All functions accept `is_be=True` for big-endian streams (default little).

---

## Threading and async model

- The extension runs one shared multi-threaded Tokio runtime per process,
  created lazily. Blocking methods release the GIL, so other Python threads
  keep running during network operations.
- **Sync and async mix freely.** `set()`/`get()`/`Client.get()` etc. can be
  called from any Python thread. The async variants (`aget`, `aset`,
  `connect_async`, `get_async`, …) return awaitables for use inside
  `asyncio` code:

  ```python
  async def main():
      await sp.aset(21.0)
      print(await temp.aget())
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
| `spvirit.TimeoutError` | operation or search timed out |
| `spvirit.SearchError` | PV could not be located |
| `spvirit.ProtocolError` | unexpected/invalid protocol state |
| `spvirit.DecodeError` | malformed wire data |
| `spvirit.IoError` | socket/OS-level failure |

PV-handle operations use builtin exceptions instead:

| Exception | Raised when |
|---|---|
| `RuntimeError` | handle not yet bound to a server; or a put was rejected |
| `KeyError` | `server.pv(name)` for an unknown/unsupported record |
| `TypeError` | wrong value type; `on_put`/`scan` on an array PV; bad `calc` inputs |
| `ValueError` | invalid address strings; non-finite floats in client puts |

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
