# Custom data sources

<!-- verify:begin -->
> ✅ **Verified** · [`multi_source.rs`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-server/examples/multi_source.rs) · [`wildcard_source.rs`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-server/examples/wildcard_source.rs) · [`json_source.rs`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-server/examples/json_source.rs) · [`rpc_source.rs`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-server/examples/rpc_source.rs) · check [`docs_verify`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-tools/tests/docs_verify.rs) · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
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
fn rpc(&self, name, args)     -> Result<NtPayload, String>;   // has a default
fn names(&self)               -> Vec<String>;
```

(Shown with the `Pin<Box<dyn Future<...>>>` wrappers elided — the trait is
object-safe rather than `async fn`, so every method returns a boxed future.
See `spvirit-server/src/pvstore.rs:55`.)

`claim` is the interesting one. It runs on every channel search, and
returning `Some(PvInfo)` commits you to serving that name.

## Priority and the registry

Sources are registered with an integer **order**. Lower is checked first,
and the built-in record store sits at **0**:

```rust
{{#include ../../../../spvirit-server/examples/multi_source.rs:register}}
```

So `-10` shadows the built-in store, and `10` is a fallback for names it
does not know. This is the whole resolution model: first claim wins.

## Claiming by naming convention

A source can serve PVs that were never declared. This one owns everything
starting with `XYZ:` and creates each PV on first touch:

```rust
{{#include ../../../../spvirit-server/examples/wildcard_source.rs:claim}}
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

## RPC

`rpc` is the one trait method with a default implementation — it returns
`Err("RPC not supported")`, so a source only opts in by overriding it:

```rust
{{#include ../../../../spvirit-server/examples/rpc_source.rs:rpc}}
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
```

## Next

[A complete IOC](complete-ioc.md).
