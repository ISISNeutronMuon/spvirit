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

## Uses

A PV name in the search table that never turns green means some client is
looking for a record that does not exist; the detail panel names the
source address, which is how you find a client nobody remembers deploying.
To confirm a server is answering, run `spget` from another terminal and
watch the row turn green when the server's search response arrives.

## Gotchas

**It only sees broadcast and multicast traffic.** A client using
`--server` or `EPICS_PVA_NAME_SERVERS` connects straight to TCP and never
searches, so nothing shows up. Absence in `spsearch` does not mean absence
on the network. This applies to your own commands too: to see your own
traffic, let the client search instead of passing `--server`.

**On a multi-interface host the listener can sit on the wrong NIC.** If
searches never appear, pin it with `--bind-addr`.
