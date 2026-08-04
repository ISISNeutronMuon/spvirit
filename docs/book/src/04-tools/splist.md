# `splist`

<!-- verify:begin -->
> ✅ **Verified** · no code on this page · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

Discover PVA servers, and list the PVs on one. The equivalent of EPICS
Base `pvlist`.

```
splist [OPTIONS] [TARGET]
```

Requires the `client` feature. `TARGET` may be `ip:port`, a bare `ip`, or
a GUID beginning with `0x`.

## Two modes

**No argument — find servers:**

```console
$ splist
GUID 0xC4960000E061B581C193C818 version 2: tcp@[ 10.64.23.134:5075 ]
```

**With a target — list its PVs:**

```console
$ splist 127.0.0.1:5075
VAC:ERROR
VAC:LINK
VAC:PRESSURE
VAC:RGA
VAC:SETPOINT
__pvlist
```

The GUID from the first form works as the target of the second, which is
useful when a server advertises an address you cannot route to directly.

## `__pvlist`

That last entry is not one of your PVs. It is the server's introspection
channel — the mechanism the second form uses. Every spvirit server exposes
it unless started with `--pvlist-mode off`, and it appears in every
listing.

## Gotchas

**Listing is opt-in on the server side, discovery is not.** With
`--pvlist-mode discover` or `off`, the server still answers a broadcast
search — `splist` with no argument finds it — but refuses to enumerate:

```console
$ splist 127.0.0.1:5075
Error: Protocol("failed to list PVs from 127.0.0.1:5075: ... __pvlist:
create_channel error: code=1 message=PV not found; GET_FIELD: disabled;
RPC(server): ... RPC list endpoint disabled (set --pvlist-mode=list) ...")
```

The error is long because `splist` tries four routes in turn — the
`__pvlist` channel, `GET_FIELD`, an RPC endpoint, and a `server` GET — and
reports every one that failed. "RPC list endpoint disabled" is the line
that tells you it is a policy decision, not a broken server. Normal reads
still work throughout. `--pvlist-max` and `--pvlist-allow-pattern` expose
only part of a database. See [`spserver`](spserver.md).

**Discovery is a UDP broadcast.** On a host with several interfaces the
search may leave by the wrong one. `--search-addr` or
`EPICS_PVA_ADDR_LIST` pins it; `--server` skips discovery entirely.
