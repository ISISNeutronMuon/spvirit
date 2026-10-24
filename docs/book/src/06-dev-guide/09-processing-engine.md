# The record-processing engine

`spvirit-ioc` loads a `.db` file and processes its records the way an EPICS
IOC does: input links pull, forward links push, alarms accumulate over a
pass and commit at its end.

## Loading a database

```rust
# fn main() -> Result<(), String> {
use std::sync::Arc;
use spvirit_ioc::IocSource;
use spvirit_server::PvaServer;

let ioc = Arc::new(IocSource::from_db_file("plant.db")?);
for line in ioc.graph().report() {
    eprintln!("{line}");
}
let server = PvaServer::builder()
    .ai("PV:DIRECT", 0.0)
    .ioc(ioc.clone())
    .build();
# let _ = server;
# Ok(())
# }
```

`spvirit-ioc` depends on `spvirit-server`, never the reverse, so the engine
cannot reach into the builder. The seam is the other way round:
`spvirit-server` declares the `StoreSource` trait and the
`PvaServerBuilder::ioc` method, and the engine implements the trait. That
also lets the builder check at construction time that the engine's records
and the builtin store's do not overlap — see
[Two stores, one server](#two-stores-one-server) below.

Registering through `.ioc()` rather than `.source()` is what wires the
engine into the server lifecycle: it gets the monitor registry (so host-side
`set_value` reaches clients), its scan threads are started, its scanner
receives named events, `<record>.<FIELD>` resolves for engine records, and the
builder checks that its records do not overlap the builtin store's. A
`.source()` registration gets none of these, so it is wrong for an engine but
right for anything that is not a record store.

## Driving the engine from host code

Records can be built without a `.db` file. The two paths converge before any
field is interpreted — a `RecordSpec` lowers to the same `DbRecord` the parser
produces — so they cannot drift:

```rust
use spvirit_ioc::{IocSource, RecordSpec};
use spvirit_ioc::alarm::Severity;

let ioc = IocSource::from_records(vec![
    RecordSpec::ao("RIG:SP").out("RIG:RBV.VAL").flnk("RIG:RBV"),
    RecordSpec::ai("RIG:RBV").inp("RIG:SP PP").egu("C")
        .hihi(100.0).hhsv(Severity::Major),
])?;
```

Writing from the host processes the chain and notifies monitor clients:

```rust
ioc.set_value("RIG:SP", DecodedValue::Float64(95.0)).await?;
```

`set_value` rather than `Source::put` matters. `put` returns its events for the
*handler* to publish; a write that starts in host code never reaches the
handler, so `put` alone would process correctly and notify nobody.

Two things the engine will not do:

- **Records cannot be added after the engine is built.** `RecordId` is
  `{set, slot}`, assigned by partitioning the link graph into lock sets; a
  late record can join two sets and invalidate every outstanding id. Base has
  the same restriction — `dbLoadRecords` after `iocInit` is unsupported.
- **There is no `on_put`.** A callback would run arbitrary code inside
  `process()` while a lock set is held. Behaviour belongs in the record graph;
  if you need a callback, use a tier 2 PV, which has no lock sets.

## Two stores, one server

A `PvaServer` can carry two independent record stores at once: the builtin
store (`.ai()`, `.ao()`, `.db_file()` and friends) and, optionally, an
engine registered through `.ioc()`. Three rules govern how they and any
other sources interact:

- **The two stores must be disjoint.** `build()` panics, naming every
  colliding record, if the builtin store and the engine store both claim
  the same name. A `.scan`, `.link`, or `on_put` callback that names an
  engine record is the same class of mistake — those callbacks drive the
  builtin store's direct-write semantics and would never fire against an
  engine record — so `build()` panics on those too.
- **Ordinary sources may legally shadow either store.** A source registered
  with `.source()` ahead of a store in resolution order can override one of
  its PVs on purpose; the registry logs a warning the first time a client
  searches for the shadowed name, but does not fail. This is the difference
  between the two tiers: stores are static, enumerable, and must be
  disjoint from each other; sources are dynamic, ordered, and may legally
  shadow.
- **Field PVs follow the winning store.** `<record>.<FIELD>` resolves
  through whichever store claims `<record>`, so a client cannot tell which
  store served a PV except by small differences in field coverage (each
  store has its own field table).

Registration order is builtin at 0, the engine (if any) at 5, and the
`record-fields` tier-2 (`SimplePvStore`) field source at 10.

## What it processes

Sub-project A covers `ai`, `ao`, `bi`, `bo`, `longin` and `longout`. Loading
a `.db` containing any other record type is an error naming the record —
silently ignoring records is how a database appears to work while half of it
does nothing.

## Lock sets

Records joined by db links — `INP`, `OUT`, `DOL`, `FLNK`, `SDIS` — share a
lock set, and a lock set is processed by one thread at a time. This is what
makes a chain of links atomic: a client cannot observe the chain half
updated. `ioc.graph().lock_sets` shows the partitioning.

## Load-time diagnostics

`graph().report()` returns one line per finding, or nothing at all:

- **unreachable** — a record that is `SCAN = Passive`, `PINI = NO`, and is
  not the target of any PP or FLNK link. It will never process.
- **cycles** — a loop in the link graph. Not an error: `PACT` breaks it at
  runtime. Reported because a cycle is usually a mistake.
- **fan-out** — a record that more than ten other records' link fields name,
  i.e. an inbound degree: `graph().high_fan_out` counts records *linked to
  by* a record, not links a record itself holds. One pass over such a record
  fans out across the database.
- **unresolved** — a link naming a record this database does not contain.
  It reads as a constant zero.

## Not yet implemented

Channel-access links (`CA`, `CP`, `CPP`) are sub-project C, and every other
record type is sub-project D.

## Scanning

Scanning (sub-project B) is driven once the engine is registered with
`.ioc()`: when the server starts, `PINI` records process (in `PHAS` order)
and then one OS thread per distinct `SCAN` period starts; records sharing a
period share its thread. At most 64 distinct periods run at once
(`MAX_PERIODIC_THREADS`); a `.db` with more leaves the extra records off every
scan list with a warning. `SCAN = Event` records process on
`post_event`, through the engine's scanner registered as an event sink.
Server shutdown stops the scan threads, waking any that are sleeping out a
long period. Over the wire only `SCAN`, `EVNT` and `PHAS` are writable on
this tier; a `SCAN` write that would need a 65th period is refused. An engine
used without a server is not scanned; call `process_pini()` yourself.
