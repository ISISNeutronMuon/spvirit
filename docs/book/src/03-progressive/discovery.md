# Discovery and introspection

<!-- verify:begin -->
> ✅ **Verified** · [`pvlist.rs`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-client/examples/pvlist.rs) · [`demo_discovery.py`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-py/examples/demo_discovery.py) · [`demo_pvfind.py`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-py/examples/demo_pvfind.py) · check [`docs_verify`](https://github.com/ISISNeutronMuon/spvirit/blob/main/spvirit-tools/tests/docs_verify.rs) · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

## What you'll build

Four questions, in the order you actually hit them: what servers are on this
network, which one has my PV, what else does it serve, and what shape is that
PV? These are the library forms of [`splist`](../04-tools/splist.md) and
[`spinfo`](../04-tools/spinfo.md) — the same four calls those tools make.

Nothing here reads a value. Every operation on this page is metadata.

## Rust

**Find servers.** A UDP search with no PV name attached: every PVA server that
hears it answers with its GUID and TCP address.

```rust
{{#include ../../../../spvirit-client/examples/pvlist.rs:discover}}
```

`build_search_targets(None, None)` produces the target list the way EPICS
Base does — `EPICS_PVA_ADDR_LIST` merged with auto-discovered broadcast
addresses, unless `EPICS_PVA_AUTO_ADDR_LIST` disables the latter. Pass
`Some(ip)` as the first argument to pin a single target.

**Locate a PV.** The same search, narrowed to one name:

```rust
{{#include ../../../../spvirit-client/examples/pvlist.rs:search}}
```

**List a server's PVs.** This one takes a `SocketAddr`, not a name — which is
why the two steps above come first:

```rust
{{#include ../../../../spvirit-client/examples/pvlist.rs:list}}
```

**Describe a PV.** This step builds its own `PvaClient` and resolves the PV
name directly, so it does not need the address `search_pv` found above.
`pvinfo` returns a `StructureDesc`; `format_structure_tree` renders it the way
`spinfo` does:

```rust
{{#include ../../../../spvirit-client/examples/pvlist.rs:info}}
```

## Python

The same four beats. Discovery and listing live in `spvirit.lowlevel`:

```python
{{#include ../../../../spvirit-py/examples/demo_discovery.py:discover}}
```

```python
{{#include ../../../../spvirit-py/examples/demo_discovery.py:search}}
```

This continues straight from discovery, reusing the `servers` list that
`discover_servers` returned:

```python
{{#include ../../../../spvirit-py/examples/demo_discovery.py:list}}
```

Introspection goes through a channel. `demo_pvfind.py` walks the returned
description with a small recursive `walk()` helper — defined just above this
block in the file — that yields one row per field, descending into
`field.struct_desc` wherever a field is itself a structure; formatting each
value's type goes through `codec`, `spvirit.codec`, imported at the top of the
same file:

```python
{{#include ../../../../spvirit-py/examples/demo_pvfind.py:info}}
```

`Channel.introspect()` is the low-level route, and the one to use when you
want the channel open anyway for a subsequent `get()`. For a one-shot
description there is also `Client.info(pv_name)`, and `Client.pvlist(addr)`
mirrors `lowlevel.pvlist`.

## From the command line

```console
$ splist
$ splist 127.0.0.1:5075
$ spinfo VAC:PRESSURE
```

## What to notice

**Listing needs an address, not a name.** `pvlist` takes a `SocketAddr`
because listing is a question about a *server*, while a PV name is a question
about the *network*. If all you have is a PV name, `search_pv` (Rust) or
`lowlevel.search_pv` (Python) converts one into the other. Rust also offers
`resolve_pv_server`, which applies the full `PvGetOptions` — name servers,
explicit `--server`, the lot — rather than a bare broadcast.

**The second return value names the route that worked.** `pvlist_with_fallback`
tries four strategies in turn and tells you which answered:
`PvListSource::PvList`, `GetField`, `ServerRpc`, or `ServerGet`. Python returns
it as the second element of a `(names, source)` tuple — that is the `source`
the Python snippet above prints. It matters because the
routes differ in completeness — a server answering by `ServerGet` may be
giving you a truncated view. The [`splist`](../04-tools/splist.md) page has
the detail.

**Introspection transfers no data.** `pvinfo` uses `CMD_GET_FIELD` (`0x11`),
so the server replies with a type description and no value. That makes it
safe on PVs a GET would choke on — a 4-megapixel image, or a PV whose read
triggers expensive device I/O.

**`__pvlist` is in every listing.** It is the server's own introspection
channel, not one of your PVs. Filter it out if you are building a UI.

**Discovery is a UDP broadcast.** On a multi-homed host the search can leave
by the wrong interface and find nothing. `build_search_targets(Some(ip), None)`
or `EPICS_PVA_ADDR_LIST` pins it.
