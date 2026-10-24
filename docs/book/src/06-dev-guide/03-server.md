# spvirit-server — Server Architecture

The server crate provides `.db` parsing, the `Source` provider abstraction,
the PVAccess protocol runtime, and the ergonomic typed-handle (`Pv<T>`) layer.
It is the largest and most active crate.

## Module map

All paths under `spvirit-server/src/`.

| File | ~Lines | Purpose |
|---|---|---|
| `pva_server.rs` | 2943 | `PvaServer` + `PvaServerBuilder` (classic API), `ServeBuilder`/`RunningServer` (handle API), shared record-construction helpers `make_scalar_record`/`make_output_record`/`make_array_record` |
| `handler.rs` | 4410 | **The core.** TCP connection processor (`handle_connection`), UDP search responder (`run_udp_search`), TCP accept loop, `ServerState`, wildcard matching, GUID generation, `MAX_PIPELINE_WINDOW` |
| `simple_store.rs` | 1738 | `SimplePvStore`: in-memory `Source` backed by `RecordInstance`s — value/NT writes, subscribers, MDEL gate, link evaluation, PUT application, `descriptor_for_payload` |
| `pv.rs` | 1646 | Typed handle layer: `Pv<T>`, `PvArray`, `AnyPv`, `PvScalar` trait, pending/bound state machine, builder methods, `attach` |
| `group.rs` | 1436 | QSRV-style group PVs: `info(Q:group)` JSON parsing → `GroupPvDef`, and `GroupSource` composing members into `NtPayload::Generic` |
| `types.rs` | 1709 | Record model: `RecordType`, `ScanMode`, `LinkExpr`, `DbCommonState`, `RecordData`, `RecordInstance` + value-mutation methods |
| `apply.rs` | 1527 | Pure functions applying a decoded PUT to NT payloads (`apply_value_update`, `apply_table_put`, `apply_ndarray_put`, …) |
| `db.rs` | 1183 | EPICS `.db` file parser (regex, line-oriented) |
| `events.rs` | 994 | `Events`: server-wide `on_start` hooks, named `on_event` handlers, the `EventSink` trait, and the single-task dispatcher behind `post_event` |
| `monitor.rs` | 2893 | `MonitorRegistry`: per-PV subscriber lists, the per-connection `ConnWriter` map, `build_monitor_frame` (pure, self-contained frames), pipeline credit accounting, subscribe-only pump management |
| `record_fields.rs` | 731 | QSRV-style field access: serves `<pv>.<FIELD>` and `<pv>.<FIELD>$` as read-only channels; dbCommon defaults table |
| `pvstore.rs` | 1414 | The **`Source` trait** + `StoreSource` + `PvInfo` + `SourceRegistry` |
| `conn_writer.rs` | 512 | `ConnWriter`: the per-connection two-lane flat-combining writer (coalescing monitor lane + FIFO lossless control lane) that replaced the old mpsc-plus-writer-task tail |
| `field_provider.rs` | 372 | `RecordFieldProvider`: the value-level field-access seam shared by `SimplePvStore` and `spvirit-ioc` (`field_descriptor` resolves type without a read) |
| `convert.rs` | 437 | `DecodedValue` → `ScalarValue`/`ScalarArrayValue` conversions |
| `server.rs` | 418 | Orchestration: `run_pva_server_with_registry` binds TCP/UDP/beacon and joins the tasks |
| `search_resolve.rs` | 729 | `SearchResolver`: background resolution of names `Source::try_claim` could not answer, with a hard concurrency cap |
| `diag.rs` | 399 | Live downstream-client registry and per-PV/per-host byte counters (gateway diagnostic PVs) |
| `request_ctx.rs` | 175 | Task-local per-connection request context (peer address, plus the `ca` user/host decoded from CONNECTION_VALIDATION), visible to `Source` impls |
| `decode.rs` | 110 | PUT-body decoding with fallback strategies (segment reassembly is the codec's `SegmentReassembler`, driven from `handler.rs`) |
| `beacon.rs` | 67 | Periodic UDP beacon sender |
| `state.rs` | 375 | Per-connection state: `ConnState`, `MonitorSub`, `MonitorState`, and `ChannelTables` (shared with `MonitorRegistry` so upstream-death teardown can forget a destroyed channel) |

## The provider model: `Source` and `SourceRegistry`

`Source` (pvstore.rs:111) is the object-safe provider abstraction (modeled on
pvxs's provider registry). Methods return boxed futures: `claim` (returns
`PvInfo{descriptor, writable}` or `None`), `get`, `put` (returns the
`(name, payload)` pairs the handler should publish. `IocSource` returns every
record its pass changed; `SimplePvStore` returns only the target and notifies
link outputs itself), `subscribe` (returns `mpsc::Receiver<NtPayload>`),
`rpc` (default `Err`), `names`, plus `try_claim` (the non-blocking
search-path answer, default `Unknown`) and `pushes_own_updates` (default
`false`). `StoreSource` (pvstore.rs:237) extends `Source` for fixed record
stores (`record_names`, `set_monitor_registry`, `start_scanning`, …).

`SourceRegistry` (pvstore.rs:323) is a priority-ordered list — the **first
source to claim a name wins**. Note that `get`/`put`/`subscribe` each call
`claim` again before dispatching, so `claim` must be cheap and idempotent.

Registration in `PvaServer::serve_after_start_hooks` (pva_server.rs:1121–1222),
which `run()` calls after the start hooks. `builtin` and `ioc` are registered
with `add_store`:

| Order | Label | Source | Claims |
|---|---|---|---|
| 0 | `builtin` | `SimplePvStore` | exact record names |
| 5 | `ioc` | the `.ioc()` engine (`StoreSource`), if any | its record names and their `<name>.<FIELD>` refs |
| 10 | `record-fields` | `RecordFieldSource` | `<name>.<FIELD>` refs |
| user | … | `.source()` extras | whatever they claim |

## SimplePvStore

`simple_store.rs:64`. Holds `RwLock<HashMap<String, PvEntry>>` where `PvEntry`
= record + in-process subscriber senders + `last_posted` (the MDEL reference
value). Key paths:

- **Public writers** `set_value` / `set_array_value` / `put_nt` bypass
  on_put/validators; each calls an `_inner` writer then `evaluate_links`.
- **`Source::put`** (simple_store.rs:479) — the wire PUT path: run the PUT
  validator (cloned out of the lock first, so user callbacks can't hold the
  lock across `.await`), apply via `RecordInstance::apply_put`
  (apply.rs:569). It refuses a value the record's type cannot represent (and
  an out-of-range enum index, or any Generic PUT) with `Err` naming the
  reason (record untouched, not restamped; the client gets an error status
  and `on_put` does not run). Otherwise it restamps and reports
  `value_changed` / `client_stamped`, then MDEL-gate the post (or force it when the PUT was
  client-stamped and the value did not change), spawn the `on_put` callback
  as a detached task, evaluate links.
- **Links/calc**: `evaluate_links` (simple_store.rs:391) is a BFS over
  `LinkDef`s whose inputs include the changed PV, with a `visited` set for
  cycle detection; uses `set_value_inner` to avoid re-triggering.
- **Descriptors**: `descriptor_for_payload` / `descriptor_for_record`
  (simple_store.rs:680/690) delegate every payload shape, NTScalar included,
  to `spvirit_codec::spvd_encode::nt_payload_desc`. A local copy used to
  drift from the codec and broke string PUTs.

## Protocol runtime

`run_pva_server_with_registry` (server.rs:183) binds TCP **first** (eager
`EADDRINUSE` so a failed start doesn't ghost-beacon), then spawns UDP-search,
TCP-accept and beacon tasks and joins them.

- **UDP search** (handler.rs:1157): binds 5076 with SO_REUSEADDR/SO_REUSEPORT
  (so a co-located p4p can share the port); answers Search packets from the
  non-blocking `SourceRegistry::try_claim`. Names no source can decide from
  memory are handed to `SearchResolver` (search_resolve.rs) and answered on
  the client's retry, so a slow proxying `claim` never stalls the search
  task. Response IP: advertise_ip → non-unspecified listen_ip →
  `infer_udp_response_ip` (connect-a-socket trick) → zeros.
- **TCP connection** (`handle_connection`, handler.rs:1501): a per-connection
  reader whose write half is wrapped in a [`ConnWriter`](#monitor-delivery-the-connwriter)
  (handler.rs:1515) — the flat-combining two-lane writer that replaced the old
  dedicated writer task draining an `mpsc::channel<Vec<u8>>(128)`. Handshake:
  SET_BYTE_ORDER → CONNECTION_VALIDATION → client's validation →
  CONNECTION_VALIDATED. Then the command dispatch (CreateChannel; Op 10 GET /
  11 PUT / 12 PUT_GET / 13 MONITOR / 20 RPC; DestroyChannel; DestroyRequest;
  GetField; Echo; AuthNZ silently accepted; CANCEL_REQUEST / ACL_CHANGE /
  MESSAGE / MULTIPLE_DATA / ORIGIN_TAG and Op 14/16 answered with an error).
  Idle timeout enforced per-read.
- **Beacons** (beacon.rs): tick every `beacon_period` (default 15 s, 0
  disables), reading an `AtomicU16` change counter.

### Data flow: external PUT → monitor update

1. Handler decodes the PUT and calls `state.sources.put(name, value)`
   (handler.rs:1953, :2055).
2. `SimplePvStore::put`: validator → apply under write lock → MDEL gate →
   in-process subscriber sends → returns the target's `(name, payload)` (or
   nothing if the post was gated). `on_put` is spawned, and links are
   evaluated if the value changed. Link outputs are notified from inside the
   store via `set_value_inner`; they are not added to the returned list.
3. Handler's `notify_changed_records` (handler.rs:753) bumps the beacon
   counter and calls `registry.notify_monitors` per change.
4. `MonitorRegistry::notify_monitors` (monitor.rs:594) builds one frame per
   subscriber via the pure `build_monitor_frame` (monitor.rs:477) — first
   frame full (possibly field-filtered); a **filtered** subscriber's later
   frames are self-contained *full filtered* frames (not sparse deltas, so a
   coalesced drop cannot corrupt the client's value), no-op-suppressed when the
   filtered view is unchanged; an **unfiltered** subscriber gets full-or-
   suppressed (suppressed when unchanged). Pipeline credit (`nfree`, capped at
   `MAX_PIPELINE_WINDOW`) gates delivery. Each frame is then handed to the
   subscriber's `ConnWriter` on the correct lane (`route_monitor_frame`,
   monitor.rs:679).
5. The `ConnWriter` delivers it: a **non-pipelined** monitor goes on the
   coalescing monitor lane (latest-per-ioid, lossy under load); a **pipelined**
   monitor goes on the FIFO lossless control lane so no charged credit is ever
   coalesced away. Whichever task wins the flusher election drains the lanes
   (control first) to the socket — in the common case inline on the producing
   task, with no cross-thread wakeup.

**Internal writes** (scan, `Pv::set`, links) enter at step 2 via
`store.set_value` and notify monitors *from inside the store* (the store holds
the registry via `set_registry`). So there are two notification origins:
protocol PUTs notify from the handler, internal writes from the store. Note
the beacon change counter is only bumped on the protocol-PUT path. With an
`.ioc()` engine there is another origin: scan-thread passes publish through
the engine's notify-drain task (`IocSource::start_scanning`) into the same
`notify_monitors`.

**Subscribe-only sources** (gateway proxies, group PVs, async backends) have a
third origin. Their values change upstream, never through a local PUT or store
write, so neither path above fires. At monitor init the handler checks
`state.sources.pushes_own_updates(name)` (handler.rs); if the source does *not*
self-notify, it calls `subscribe(name)` and hands the channel to
`MonitorRegistry::ensure_pump` (monitor.rs), which spawns one task per PV that
drains the channel into `notify_monitors`. The pump is shared across every
subscriber of that PV and retired (`retire_pump_if_idle`) when the last one
goes away; it holds a `Weak` reference to the registry to avoid a cycle.
Self-notifying sources (`SimplePvStore`, `IocSource`, `PySourceAdapter`) return
`true` and are skipped, so their updates are never delivered twice. Without this
pump a monitor through the gateway received only its initial snapshot and then
nothing.

### Concurrency summary

Tokio tasks: UDP-search loop, TCP-accept loop, beacon loop, one per
connection, one per `.scan()`, detached `on_put` tasks, one per
group-subscription fan-in, one monitor pump per subscribe-only PV under
monitor, and, with an `.ioc()` engine, its notify-drain task. The engine's
scan lists run on OS threads, one per distinct `SCAN` period (at most 64),
stopped and joined at server shutdown. Note there is **no** dedicated per-connection writer task any more:
the `ConnWriter` flat-combines, so socket writes happen inline on whichever
task deposited the frame and won the flusher election. Locks: store `pvs`
(`RwLock`), monitor registry (`Mutex`), source registry (`RwLock`); each
`ConnWriter` guards its coalescing state with a **std `Mutex`** (never held
across `.await`) and its socket write half with an async `Mutex`; each `Pv`
handle's shared state is a **std `Mutex`** held only briefly, never across
`.await`. Channels: per-subscriber NtPayload (64) — the store's
`Source::subscribe` stream a monitor pump drains. The old per-connection
outbound `mpsc(128)` is gone, subsumed by the `ConnWriter`.

## Monitor delivery: the `ConnWriter`

Every connection's write half is owned by a `ConnWriter` (conn_writer.rs),
stored in `MonitorRegistry::conns` keyed by connection id. It replaced the old
per-connection `mpsc(128)` plus a dedicated writer task with **flat combining**
(Hendler, Incze, Shavit & Tzafrir, SPAA 2010): producers deposit complete PVA
frames and one of them is elected "flusher" to drain them, so the common case
writes inline on the producing task with no cross-thread wakeup.

Two lanes give the combiner priority scheduling, decided at drain time when it
holds the whole pending batch:

- **`control`** — a `VecDeque<Vec<u8>>`: FIFO, never coalesced, never dropped.
  Carries the handshake, GET/PUT/PUT_GET/RPC and monitor-**init** responses,
  errors, and control frames. **Drained first**, so a monitor's INIT response
  always precedes that ioid's DATA frames. Also carries every **pipelined**
  monitor DATA frame (see below).
- **`monitor`** — a `HashMap<u32, Vec<u8>>` keyed by ioid: latest-frame-per-ioid
  conflation. A newer frame for an ioid replaces the older one, so intermediate
  monitor values are dropped under load — exactly like pvxs/pvagw. Carries
  **non-pipelined** monitor DATA frames only.

`send_control` and `send_monitor` deposit under a std `Mutex` and, if no flusher
is active, set the `flushing` flag and become the flusher. The drain, the
empty-check and clearing `flushing` all happen under a single lock acquisition,
so there is no lost wakeup. Cross-operation reordering is protocol-legal: PVA
frames are correlated by request id, not stream position. A `dead` flag, set on
the first socket-write error, makes all further deposits no-ops so a
dead-but-not-closed peer cannot wedge producers. A `FlushGuard` gives cancel
safety: if the flusher future is dropped mid-`write_all` (its task cancelled),
the guard hands the election to a spawned continuation so pending bytes are
never stranded and no depositor is wedged behind a stale `flushing == true`.

**Frame building is pure.** `MonitorRegistry::build_monitor_frame` (monitor.rs:477)
takes only a `MonitorSub` and an `NtPayload` and returns `Option<Vec<u8>>`:
`Some(bytes)` when there is something to send, `None` for a no-op (a duplicate
under the subscriber's field view). A `None` **does not consume pipeline
credit** and does not advance the delta baseline. Because the monitor lane may
drop intermediate frames, every subsequent filtered frame is a *self-contained
full filtered* frame, not a sparse delta — a dropped self-contained frame is
harmless, whereas a dropped delta would silently corrupt the client's value.

**Pipelined flow control is lossless.** A `MonitorSub` (state.rs) carries per-sub
`nfree`, `last_snapshot`, `filtered_desc` and `pipeline_enabled`. Pipelined
subscribers do credit-based flow control, so every charged frame must arrive;
`route_monitor_frame` (monitor.rs:679) sends their frames on the lossless
**control** lane, never the coalescing monitor lane — coalescing one away would
spend a credit the client never receives and drift its window until it stalls.
The client-supplied window is capped server-side at `MAX_PIPELINE_WINDOW = 4096`
(handler.rs:49) at three points: monitor init, ACK refill, and the registry's
`update_monitor_subscription` gate — so a stalled pipelined client cannot grow
the lossless lane without bound.

Pump retirement is **cooperative** (a `oneshot` shutdown signal, not `abort`):
aborting a pump could drop its future mid-`write_all` inside a shared
`ConnWriter` and wedge that connection's flusher, so `retire_pump_if_idle`
lets any in-flight `notify_monitors` finish first.

> The standalone [`spserver`](../04-tools/spserver.md) binary is a separate
> implementation and does **not** use `ConnWriter`. It keeps the older
> per-connection `mpsc(128)` plus a writer task and borrows only the pure
> `build_monitor_frame`. It applies the same `MAX_PIPELINE_WINDOW = 4096` cap
> (`spvirit-tools/src/bin/spvirit_server.rs:1145`).

## Record model and .db parsing

- `RecordType` (types.rs:24): 17 kinds; `from_db_name` maps `.db` strings
  (`mbbi`|`ntenum` both → `Mbbi`); `is_output()` gates writability (`mbbi`,
  an input, is read-only over the wire).
- `RecordData` (types.rs:128): one variant per record family carrying the NT
  payload + record-specific fields (INP/OUT/DOL/DRVL/DRVH/SIML/…).
  **`nt_mut()` panics** on non-scalar variants (types.rs:285, 423). Use
  `nt_scalar()` (pub, fallible, types.rs:301) or the crate-internal
  `nt_scalar_mut()` in generic code.
- `RecordInstance` (types.rs:345) adds `raw_fields: HashMap<String,String>`
  (verbatim `.db` fields — used by the record-fields source and MDEL lookup).
  `set_scalar_value` (types.rs:528) does exhaustive cross-type numeric
  coercion and timestamp stamping.
- `parse_db` (db.rs:836) is **line-oriented** (one statement per logical
  line; a quoted value may span lines). A packed one-liner
  `record(...){field(...)}` is rejected with an `unrecognised line` error.
- `.db` **cannot** load longin/longout/mbbi/mbbo/table/ndarray/generic — the
  two `TODO(follow-up)` markers in the codebase (types.rs:45, db.rs:562).
  Those record types exist in the builtin store only via the builder/handle
  APIs, and a `.db` file naming one is a load error that names the record.
  (`spvirit-ioc`'s engine does load `longin`/`longout` from `.db`; see
  chapter 09.)

## Typed handle layer (`pv.rs`)

`PvScalar` (pv.rs:88) is implemented for `f64`/`bool`/`i32`/`String` and
(added by the value-types work, commit `0819a18`) `ScalarValue`. Each impl overrides `from_decoded` to dodge the
truthy-first bug in `convert::decoded_to_scalar_value` (convert.rs:140 checks
bool before numerics, so any nonzero numeric becomes `Bool` — documented at
pv.rs:94–102).

A handle is `Arc<PvShared{name, Mutex<PvState>}>` where `PvState` is
`Pending(record + validator + scan + calc)` or `Bound(Arc<SimplePvStore>)`.
Builder methods mutate the pending record and **warn + no-op if already
bound** — this is the "attach before serving" rule surfaced in the Python API
docs. `ServeBuilder::build` (pva_server.rs:1335) drains each handle's parts
into the classic builder, builds, then flips handles to `Bound`.
`Pv::attach`/`RunningServer::pv` mint handles to existing records and refuse
payload shapes that don't match the requested type (regression test
`scalar_attach_rejects_array_backed_record`, pv.rs:1236).

### Callbacks

| Mechanism | When it runs | Can reject? | Where registered |
|---|---|---|---|
| PUT validator (`Pv::on_put`) | **before** apply | yes (`Err` rejects on the wire) | `store.set_validator` |
| `on_put` (classic builder) | **after** apply, detached `tokio::spawn` | no (fire-and-forget) | `SimplePvStore.on_put` |
| `scan` | interval task calling `store.set_value` | n/a | spawned in `PvaServer::run` |
| `calc`/`link` | `evaluate_links` after any input changes | n/a | `LinkDef` list |
| `on_start` | once, awaited in registration order, before scans/dispatcher/listener | yes — a panic aborts `run()`/`run_start_hooks()`, naming the hook | `PvaServerBuilder::on_start` / `ServeBuilder::on_start` |
| `on_event` | queued by `post_event`, run one at a time on the dispatcher task | no — `catch_unwind` logs and counts, dispatcher continues | `PvaServerBuilder::on_event` / `ServeBuilder::on_event` |
| `EventSink::on_event` | awaited inline by `post_event`, before any handler is queued | no — `catch_unwind` logs and counts, the fan-out continues | `PvaServerBuilder::event_sink` / `ServeBuilder::event_sink` / `Events::add_sink` |

## Lifecycle hooks and named events (`events.rs`)

`Events` (events.rs:71) is one `Arc` shared by the builder and the built
`PvaServer`: `sinks: RwLock<Vec<Arc<dyn EventSink>>>`, `handlers:
RwLock<HashMap<String, Vec<EventHandler>>>`, an `mpsc::channel` of capacity
`DISPATCH_QUEUE_CAPACITY` (1024), and `AtomicU64` counters for drops
(`dropped_count`) and handler panics (`failed_count`).

- **`EventSink`** (events.rs:40) is an async trait — `fn on_event(&self,
  event: &str) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>` — awaited
  inline by `Events::post`, in registration order, before any handler is
  queued. It is a boxed future rather than a plain `fn` for the same reason
  `EventHandler` is: every store mutation is `async`, and `post` is reachable
  from inside the dispatcher on a `current_thread` runtime, where
  `Handle::block_on` panics, `block_in_place` panics, and
  `futures::executor::block_on` deadlocks on the store's tokio `RwLock` — so
  a sync signature could not be honoured by any sink that touches a record.
  The returned future borrows `&self`, not `event`. This is the seam the
  `spvirit-ioc` scanner uses for EPICS `EVNT` scan lists: when an engine is
  registered with `.ioc()`, `serve_after_start_hooks` adds the engine's
  `Scanner` as a sink (pva_server.rs:1156).
- **`EventHandler`** (events.rs:50) is the deferred counterpart:
  `Arc<dyn Fn(Arc<SimplePvStore>, String) -> Pin<Box<dyn Future<Output = ()> +
  Send>> + Send + Sync>`. `Events::post` (events.rs:222) awaits the sinks —
  each wrapped in `catch_unwind(AssertUnwindSafe(..))`, so a panicking sink
  is logged, counted in `failed_count`, and neither truncates the fan-out nor
  stops handlers being queued — then increments `inflight` for the whole
  batch *before* enqueueing any of it (so a concurrent `drain()` can never
  observe `inflight == 0` mid-batch), and `try_send`s each handler — a full
  queue drops and counts rather than blocking the poster.
- **`start_dispatcher`** (events.rs:149) spawns the single task that drains
  the channel and awaits each handler wrapped in
  `futures::FutureExt::catch_unwind(AssertUnwindSafe(fut))`: a panicking
  handler is logged, counted in `failed_count`, and the loop continues —
  handlers never abort the dispatcher. Handlers therefore run strictly one
  at a time, in the order they were enqueued across *all* events, so a slow
  handler for one event delays every other event's handlers behind it.
- **`drain()`** (events.rs:297) is test-only: it polls `inflight` down to
  zero with a 10 s timeout that panics by name rather than hanging CI if the
  dispatcher-invariant (`catch_unwind` always present, `inflight` always
  decremented) is ever broken by a regression. It first checks the
  `dispatcher_started` flag and fails immediately if handler invocations are
  queued with no dispatcher to run them — `build() -> post_event() ->
  drain_events()` without a start used to burn the full 10 s and then blame
  the dispatcher.

**Startup hooks** (`StartHook`, events.rs:57) are not part of `Events` —
they are a plain `Vec` on `PvaServer` (`start_hooks`, pva_server.rs:868),
run by `run_start_hooks()` (pva_server.rs:940) to completion, in order,
*before* `serve_after_start_hooks()` builds the source registry, spawns
scan tasks, starts the event dispatcher, and binds (pva_server.rs:1051–1222:
`run()` is exactly `run_start_hooks().await?` then
`serve_after_start_hooks().await`). A panicking hook returns
`Err(format!("on_start hook #{i} panicked; aborting startup: {cause}"))` —
the `catch_unwind` payload is downcast to `String`/`&str`, so the real cause
(and any label the hook panicked with, such as a Python source's) survives
instead of reaching the user only through the default panic hook on stderr
— and `run()` never reaches `serve_after_start_hooks`: no scan task,
dispatcher, or listener starts.
`ServeBuilder::start()` runs the same hook phase to completion *before* it
returns (`Result<RunningServer, String>`), so an aborting hook fails the
call rather than handing back a handle to a server that never bound, and
`RunningServer::pv(...)` can never read a pre-hook value. `start()` then
starts the dispatcher (idempotent) before spawning, so
`RunningServer::post_event` does not race the spawn; `RunningServer` keeps
its own `Arc<Events>` (`events()`, `post_event()`) because `start()` moves
the `PvaServer` into the spawned task.

**The two APIs order the dispatcher differently.** `run()` is hooks →
sources → scan tasks → dispatcher → bind. `start()` and Python's
`start_background()` are hooks → dispatcher → spawn(sources → scan tasks →
bind), because the dispatcher must be running before the call returns or a
`post_event()` on the next line would race the spawned task. The consequence
is that on the handle API an `on_event` handler can run before
`.source()`-registered sources exist and before any scan task is spawned.
Handlers receive only `Arc<SimplePvStore>`, and nothing has bound on either
path, so this is not observable by a client — but a handler must not assume
a source is registered. What both orders share is the part that matters: no
hook runs late, and nothing binds before every hook has returned.
`run_start_hooks` also installs the `MonitorRegistry` onto the store before
the first hook runs (pva_server.rs:942), so a hook that writes the store
reaches any monitor subscribed later, and a hook that reads the registry
never sees `None`.

**Python sources fold into the same list.** `spvirit-py/src/server.rs`
registers a source's `on_start(notifier)` as one more `PvaServerBuilder::on_start`
closure, at the point `add_source`/`.add_source()` is called — so a source's
hook and a `@builder.on_start` hook interleave in true registration order,
not "sources first" or "hooks first". This changed behaviour from an
earlier revision where a Python source's `on_start` fired eagerly inside
`build()`; see the Python-facing note in [Custom data
sources](../03-progressive/sources.md).

## Alarms and deadbands

- **Alarm computation** is `NtScalar::update_alarm_from_value`
  (spvirit-types/src/lib.rs:281), invoked only when `compute_alarms` is true —
  and **`compute_alarms` defaults to `false`** (server.rs:64; also
  pva_server.rs:95).
- **Dual alarm-limit fields**: `NtScalar` has `alarm_low/high/lolo/hihi`
  (`Option<f64>` — what the alarm engine reads) *and*
  `value_alarm_*_limit` (`f64` — the NT wire metadata). Both the `.db`
  parser and `Pv::alarm_limits()` set both, through
  `NtScalar::with_alarm_limits(low, high, lolo, hihi)` (note the order differs
  from `Pv::alarm_limits(lolo, low, high, hihi)`). Neither evaluates the
  initial value; severity is computed on the next write.
- **MDEL** (monitor deadband): `should_post_update` (simple_store.rs:644)
  suppresses the *post* (not the store) when the record is a numeric scalar,
  MDEL > 0, severity unchanged, and the delta is under MDEL. **ADEL is parsed
  and exposed via field access but not wired into any posting logic**
  (pv.rs:309 comment).

## Known gaps / gotchas (beyond those above)

1. **Timestamps are load-bearing.** An `NtScalar` with `time_stamp: None` is
   stamped with `now()` at encode time, which breaks monitor deltas; a `None`
   table/NDArray timestamp encodes as epoch 0, which the EPICS Archiver
   Appliance rejects. Mutation paths stamp timestamps (`set_scalar_value`
   types.rs:528, `set_array_value`/`set_nt_payload` types.rs:593/628);
   `stamp_missing_timestamps` (types.rs:457) is called on store insert
   (simple_store.rs:83, 118) so static/`.db`-loaded records are stamped too
   (see chapter 08).
2. **PUT to `Generic` is refused.** `RecordInstance::apply_put` has no
   wire path for `Generic` (apply.rs:710) and answers a client PUT with an
   error. `NtTable`/`NtNdArray` dispatch to `apply_table_put`/
   `apply_ndarray_put` (apply.rs:675/678) and `NtEnum` unwraps
   `value.index`. A table and an enum are fully writable over the wire; an
   NDArray PUT applies its metadata, but not the union-shaped image `value`.
   `Generic` is writable only via `put_nt`.
3. **CANCEL_REQUEST is unimplemented** (returns an error message); some
   clients use it. ACL_CHANGE/MESSAGE/MULTIPLE_DATA/ORIGIN_TAG and Op 14/16
   likewise return errors.
4. `group.rs::poll_any_member` (group.rs:575) races member receivers with
   `select_all` and rotates the list after each win, so a chatty low-index
   member cannot starve the others.
5. Default `conn_timeout` is ~64000 s (~17.8 h); the doc comment rounds it to
   18 h (pva_server.rs:672).
6. The beacon change counter only increments on protocol PUTs, not internal
   scan/set writes (handler.rs:115, 755).

## Tests and examples

Tests are inline per-module; the biggest suites are in `simple_store.rs`
(MDEL, timestamps, put/subscribe, all 12 array element types, put_nt, validator
rejection), `pv.rs` (constructors, attach guards, ScalarValue preservation) and
`pva_server.rs` (builder wiring, db_string, links, serve/bind). Run:
`cargo test -p spvirit-server`.

24 runnable examples under `spvirit-server/examples/` — from
`simple_server.rs` up to `snake.rs` (a Snake game over PVAccess). The
`custom_pvstore` / `multi_source` / `wildcard_source` / `json_source` /
`aggregate_source` / `passthrough_source` / `rpc_source` set demonstrates the
`Source` trait patterns; `mailbox.rs` is the p4p SharedPV equivalent.
