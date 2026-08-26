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

Registering through `.ioc()` rather than `.source()` is what makes
`<record>.<FIELD>` resolve for engine records and what enables the
disjointness check. A `.source()` registration still works and is the right
call for anything that is not a record store.

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
  store served a PV except by the differences documented in the A2 spec's
  "Deviations" section.

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

Scanning is sub-project B: `SCAN = 1 second` parses and appears in the
diagnostics, but nothing drives it yet. `PINI` records process when you call
`process_pini()`. Channel-access links (`CA`, `CP`, `CPP`) are sub-project C,
and every other record type is sub-project D.
