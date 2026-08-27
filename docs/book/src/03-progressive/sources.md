# Custom data sources

<!-- verify:begin -->
> ✅ **Verified** · [`multi_source.rs`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-server/examples/multi_source.rs) · [`wildcard_source.rs`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-server/examples/wildcard_source.rs) · [`json_source.rs`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-server/examples/json_source.rs) · [`rpc_source.rs`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-server/examples/rpc_source.rs) · [`demo_source_multi.py`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-py/examples/demo_source_multi.py) · [`demo_source_wildcard.py`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-py/examples/demo_source_wildcard.py) · [`demo_source_sensor.py`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-py/examples/demo_source_sensor.py) · [`demo_source_rpc.py`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-py/examples/demo_source_rpc.py) · check [`docs_verify`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-tools/tests/docs_verify.rs) · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

## What you'll build

A PV provider that is not a record store at all — PVs backed by a file, by
another process, by a naming convention, or computed on demand.

Everything so far has used the built-in record store: you declare PVs up
front and the server holds their values. A **source** replaces that with
your own code. The server asks "do you own this name?" and, if you say yes,
routes every GET, PUT and subscribe to you.

## The `Source` trait

```rust
fn claim(&self, name: &str)   -> Option<PvInfo>;   // do I own this name?
fn get(&self, name: &str)     -> Option<NtPayload>;
fn put(&self, name, value)    -> Result<Vec<(String, NtPayload)>, String>;
fn subscribe(&self, name)     -> Option<Receiver<NtPayload>>;
fn pushes_own_updates(&self)  -> bool;                        // default false
fn rpc(&self, name, args)     -> Result<NtPayload, String>;   // has a default
fn names(&self)               -> Vec<String>;
```

(Shown with the `Pin<Box<dyn Future<...>>>` wrappers elided — the trait is
object-safe rather than `async fn`, so every method returns a boxed future.
See `spvirit-server/src/pvstore.rs:55`.)

`claim` is the interesting one. It runs on every channel search, and
returning `Some(PvInfo)` commits you to serving that name.

In Python there is no trait to implement — a source is any object with the
matching methods, checked by duck typing:

```python
class MySource:
    def claim(self, name): ...        # -> PvInfo | None
    def get(self, name): ...          # -> NtScalar/NtScalarArray/... | None
    def put(self, name, value): ...   # -> payload, or raise to reject
    def rpc(self, name, args): ...    # optional
    def names(self): ...              # -> list[str]
    def on_start(self, notifier): ... # optional: stash the notifier
```

Two differences from Rust worth knowing up front. `on_start` has no Rust
counterpart — it is how a Python source gets the `Notifier` it needs to push
monitor updates. And `subscribe` is *not* part of the Python protocol: define
it and it is ignored (`spvirit-py/src/source.rs:716`). Monitors are driven by
`notifier.notify(name, payload)` instead.

`on_start` fires at server start (`start()`/`start_background()`/`run()`),
not at `build()` — and it shares one ordered list with `@builder.on_start`
hooks registered on `ServerBuilder`: whichever was registered first (a
source via `add_source`, or a hook via `@builder.on_start`) runs first. A
source's `order` only decides which source claims a PV name; it plays no
part in this startup sequence. See [Lifecycle hooks and
events](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-py/README.md#lifecycle-hooks-and-events)
in the README for the full picture, including server-wide named events
(`post_event`/`on_event`).

> **Migration note.** This is a behaviour change: a Python source's
> `on_start` used to fire during `build()`. Code that relied on that timing
> (e.g. publishing an initial value before `build()` returns) now needs to
> wait until the server has started instead.

## Priority and the registry

Sources are registered with an integer **order**. Lower is checked first,
and the built-in record store sits at **0**:

```rust
{{#include ../../../../spvirit-server/examples/multi_source.rs:register}}
```

```python
{{#include ../../../../spvirit-py/examples/demo_source_multi.py:register}}
```

So `-10` shadows the built-in store, and `10` is a fallback for names it
does not know. This is the whole resolution model: first claim wins.

## Claiming by naming convention

A source can serve PVs that were never declared. This one owns everything
starting with `XYZ:` and creates each PV on first touch:

```rust
{{#include ../../../../spvirit-server/examples/wildcard_source.rs:claim}}
```

The Python equivalent, claiming `SCRATCH:` instead — same three methods,
plus the notifier that publishes each PUT to subscribers:

```python
{{#include ../../../../spvirit-py/examples/demo_source_wildcard.py:claim}}
```

```console
$ spget XYZ:NEW
XYZ:NEW   0          # sprang into existence on the search

$ spput XYZ:NEW 42 && spget XYZ:NEW
XYZ:NEW  42

$ spget ABC:NEW
Error: Timeout("search response")    # nothing claims ABC:
```

Note the failure mode for an unclaimed name: a **search timeout**, not a
"not found" error. Nothing answers the UDP search, so the client waits.

## Backing PVs with a file

```rust
{{#include ../../../../spvirit-server/examples/json_source.rs:impl}}
```

Values survive a restart because `put` writes the JSON file synchronously
and the constructor reads it back:

```console
$ spput JSON:SETPOINT_A 123.4
JSON:SETPOINT_A OK
# ... stop the server, start it again ...
$ spget JSON:SETPOINT_A
JSON:SETPOINT_A 123.4
```

## Pushing monitor updates

A source with a value that changes on its own — a polled instrument, a
subscription to some other system — needs to *push*. In Rust that is
`subscribe`, returning a channel the server drains. In Python `subscribe` is
ignored; you keep the `Notifier` from `on_start` and call it from whatever
thread produces the data:

```python
{{#include ../../../../spvirit-py/examples/demo_source_sensor.py:notify}}
```

**Two delivery paths, and why `pushes_own_updates` exists.** There are two
ways a source's changes reach a monitoring client, and a source must use
exactly one:

- **Return a channel from `subscribe`** and leave `pushes_own_updates` at its
  default `false`. When a client starts a monitor, the server calls your
  `subscribe`, then runs one pump task per PV that drains the channel into the
  monitor registry — so every value you send reaches the client. This is the
  path for gateway proxies, group PVs, and any source whose values originate
  elsewhere. It is what makes a monitor through the gateway deliver *ongoing*
  updates, not just the first value.
- **Push into the monitor registry yourself** — the built-in record store and
  the IOC engine already call `notify_monitors` on every write. Such a source
  returns `pushes_own_updates() -> true`, which tells the server *not* to also
  pump `subscribe`; pumping a source that already self-notifies would deliver
  every update twice.

Most custom sources take the first path and never touch `pushes_own_updates`.
Override it to `true` only if your source itself drives the monitor registry.
In Python the choice does not arise: `subscribe` is ignored and the `Notifier`
is the self-notifying path, so Python sources behave as if `pushes_own_updates`
were `true`.

## RPC

`rpc` is the one trait method with a default implementation — it returns
`Err("RPC not supported")`, so a source only opts in by overriding it:

```rust
{{#include ../../../../spvirit-server/examples/rpc_source.rs:rpc}}
```

In Python it is likewise optional — a source without an `rpc` method simply
has no RPC channel:

```python
{{#include ../../../../spvirit-py/examples/demo_source_rpc.py:rpc}}
```

**spvirit ships no general-purpose RPC client.** Neither `spvirit-client`
nor any of the CLI tools can call an arbitrary RPC channel — the only RPC
in the client is an internal path used by `pvlist`. To exercise an RPC
source, use p4p or pvxs:

```python
from p4p.client.thread import Context
ctx = Context('pva')
print(ctx.rpc('RPC:add', {'a': 3.0, 'b': 4.0}))   # 7.0
```

## Other shapes in the repo

| Example | Pattern |
|---|---|
| `passthrough_source.rs` | decorator — wraps another source to add logging, access control, rate limiting |
| `aggregate_source.rs` | derived PVs computed from the built-in store's values |
| `custom_pvstore.rs` | replacing the store wholesale rather than layering on it |
| `mailbox.rs` | minimal writable scratch PVs |

The Python family is `demo_source_*.py` — `sensor`, `async`, `multi`,
`passthrough`, `aggregate`, `rpc`, `wildcard`. `demo_source_async.py` is the
one with no Rust counterpart here: it shows a source whose `get` is an
`async def`, which the adapter awaits on the server's runtime.

## What to notice

**`claim` is on the hot path.** It is called for every channel search from
every client on the network. Keep it cheap — no I/O, no locks held across
awaits. Do the expensive work in `get`.

**`names()` drives `splist`.** A source that returns an empty `names()`
still serves its PVs; they just do not show up in listings. The wildcard
source cannot enumerate what does not exist yet, which is the honest
answer for a dynamic namespace.

**Sources bypass the record layer entirely.** No MDEL, no alarm
computation, no scan, no `.FIELD` access — those are properties of
`RecordInstance`, and a source does not have one. If you want deadbands,
implement them in `subscribe`.

**Returning `Some` from `claim` is a commitment.** There is no way to
un-claim afterwards; a subsequent `get` returning `None` surfaces to the
client as an error rather than falling through to the next source.

## Run it

```bash
cargo run -p spvirit-server --example multi_source
cargo run -p spvirit-server --example wildcard_source
cargo run -p spvirit-server --example json_source
cargo run -p spvirit-server --example rpc_source

python spvirit-py/examples/demo_source_multi.py
python spvirit-py/examples/demo_source_wildcard.py
python spvirit-py/examples/demo_source_sensor.py
python spvirit-py/examples/demo_source_rpc.py
```

Each is a server; drive it from a second terminal. `multi_source` registers
several sources on one server, and `splist` shows them merged into one flat
namespace — nothing in the listing says which source owns which PV:

```console
$ splist 127.0.0.1:5075
COMPUTED:TIME
CONST:E
CONST:PI
SIM:COUNTER
__pvlist

$ spget CONST:PI
CONST:PI 2026-08-06 09:19:57.058 3.141593

$ spget COMPUTED:TIME
COMPUTED:TIME 2026-08-06 09:19:57.139 1786007997.139291

$ spget SIM:COUNTER
SIM:COUNTER 2026-08-06 09:19:56.859   3
```

`wildcard_source` claims a whole prefix, so the PV does not exist until you
write to it:

```console
$ spput XYZ:MyValue 42.0
XYZ:MyValue OK

$ spget XYZ:MyValue
XYZ:MyValue  42

$ spput XYZ:sensor/temp 21.5
XYZ:sensor/temp OK

$ splist 127.0.0.1:5075
STATIC:HEARTBEAT
XYZ:MyValue
XYZ:sensor/temp
__pvlist
```

Two things to notice. `spget XYZ:MyValue` prints **no timestamp** — the
source returns a bare value and nothing stamped it, unlike a record. And
`splist` only reports the names created so far; a wildcard source cannot
enumerate an infinite namespace.

`json_source` writes through to disk, so the value survives a restart:

```console
$ spput JSON:SETPOINT_A 123.4
JSON:SETPOINT_A OK

$ spget JSON:SETPOINT_A
JSON:SETPOINT_A 123.4

# stop the server with Ctrl-C, start it again

$ spget JSON:SETPOINT_A
JSON:SETPOINT_A 123.4
```

The server prints its side of that on startup:

```console
[json_source] loaded 4 PVs from pvstore.json
JSON file-backed source server running on port 5075
  Persistent PVs: JSON:SETPOINT_A, JSON:SETPOINT_B, JSON:LIMIT_HI, JSON:LIMIT_LO
  In-memory PV:   SIM:HEARTBEAT
  Storage file:   pvstore.json
```

It creates `pvstore.json` in your working directory — delete it if you want
to start from the defaults again.

`rpc_source` has no expected output here, because as noted above spvirit
ships no RPC client; use p4p or `pvcall` from pvxs against it.

## Next

[A complete IOC](complete-ioc.md).
