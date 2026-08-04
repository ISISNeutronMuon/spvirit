# `spsearch`

<!-- verify:begin -->
> ✅ **Verified** · no code on this page · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

A passive network monitor. It listens on the PVA UDP search multicast
group and shows every PV name anyone on the network is asking for, and
which servers answered.

```
spsearch [OPTIONS]
```

Requires the `client` and `tui` features.

## Flags

`spsearch` does **not** take the shared client options — it never opens a
TCP channel, so most of them are meaningless. It has three:

| Flag | Default | Meaning |
|---|---|---|
| `-p`, `--udp-port PORT` | 5076 | UDP search port to listen on |
| `-b`, `--bind-addr IP` | — | local IP for the listener |
| `-d`, `--debug` | off | verbose logging |

## Keys

| Key | Action |
|---|---|
| `q` | quit |
| `h` | toggle help |
| `Tab` | cycle focus between the table and the detail panel |
| `↑` / `↓`, PgUp / PgDn | navigate |
| `/` | filter PV names (Enter applies, Esc cancels) |
| `s` | cycle sort mode |
| `p` | pause / resume updates |
| `c` | clear stale entries (older than 5 minutes) |

Green rows are PVs that at least one server has answered for. The detail
panel shows who searched and who responded.

## What it is for

**Finding the client nobody remembers deploying.** A PV name appearing in
the search table with no green means something is looking for a record
that does not exist. The detail panel names the source address.

**Confirming a server is answering.** Watch the row turn green while
running `spget` from another terminal — that is the search-response leg of
the protocol, live.

**Diagnosing a multi-interface host.** If searches never appear, the
listener is on the wrong NIC; pin it with `--bind-addr`.

## Gotchas

**It only sees broadcast and multicast traffic.** A client using
`--server` or `EPICS_PVA_NAME_SERVERS` connects straight to TCP and never
searches, so nothing shows up. Absence in `spsearch` does not mean absence
on the network.

**It shows requests, not the ones you make yourself with `--server`.** The
same caveat, from the other side: to see your own traffic, let it search.
