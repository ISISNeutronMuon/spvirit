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

let ioc = IocSource::from_db_file("plant.db")?;
for line in ioc.graph().report() {
    eprintln!("{line}");
}
let server = PvaServer::builder()
    .source("ioc", 0, Arc::new(ioc))
    .build();
# let _ = server;
# Ok(())
# }
```

`spvirit-ioc` depends on `spvirit-server`, so it cannot add a method to
`PvaServerBuilder` (`PvaServer::builder()`'s return type). Register it
through `source()` like any other source; the `order` argument decides
which source answers a name first when several could.

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
