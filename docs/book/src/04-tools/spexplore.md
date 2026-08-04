# `spexplore`

<!-- verify:begin -->
> ✅ **Verified** · no code on this page · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

A three-pane terminal browser for a PVA network: servers on the left, that
server's PVs in the middle, the selected PV's live value and structure on
the right.

```
spexplore [OPTIONS]
```

Requires the `client` **and** `tui` features. Takes no PV argument — you
discover everything from inside.

## Flags beyond the shared set

| Flag | Meaning |
|---|---|
| `--poll-interval SECS` | how often to refresh the selected PV |

## Workflow

1. Press `r` to discover servers.
2. Select a server in the left pane and press Enter.
3. Select a PV in the middle pane and press Enter.
4. Watch streaming value and structure updates on the right.

## Keys

| Key | Action |
|---|---|
| `q` | quit |
| `h` | toggle the help modal |
| `Tab` | cycle focus between the three panes |
| `↑` / `↓` | navigate the focused pane |
| `Enter` | activate the selection |
| `f` | type a PV filter (Enter applies) |
| `a` | add a PV by name (Enter applies) |
| `t` | toggle between the text and chart views |
| `r` | refresh discovery, list, or monitor |
| `p` | pause / resume the monitor |
| `x` or `Esc` | cancel in-flight operations |

The chart view (`t`) draws the selected scalar as a sparkline over the
last 240 samples.

## Gotchas

**`a` exists because listing can be refused.** A server started with
`--pvlist-mode discover` or `off` shows up in the left pane with an empty
PV list. Press `a`, type the name, and it monitors normally — enumeration
and access are separate permissions. See [`splist`](splist.md).

**Discovery is manual.** Nothing happens until you press `r`; the status
line says so on startup. This keeps the tool quiet on a busy network.

**Every pane operation is cancellable.** A slow or unreachable server
blocks nothing — `x` drops the in-flight request and returns the UI.
