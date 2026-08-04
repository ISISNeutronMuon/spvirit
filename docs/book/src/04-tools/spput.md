# `spput`

<!-- verify:begin -->
> ✅ **Verified** · no code on this page · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

Write a PV. The equivalent of EPICS Base `pvput`.

```
spput [OPTIONS] [PV] [VALUE]
```

Requires the `client` feature.

## Three ways to give a value

```bash
spput VAC:SETPOINT 5e-4          # positional
spput VAC:SETPOINT=5e-4          # PV=VALUE
spput SIM:MODE --json '{"value":{"index":2}}'
```

Use the `PV=VALUE` form for negative numbers. A positional value cannot
start with `-`, because the parser reads it as a flag:

```bash
spput COUNTER=-1                 # works
spput COUNTER -1                 # does not
```

`--json` takes a full payload structure and is how you write anything that
is not a bare scalar.

## Flags beyond the shared set

| Flag | Meaning |
|---|---|
| `--json JSON` | JSON payload to write |
| `--simple-flow` | init + write only; skip the pre/post GET, the get-put probe, and `DESTROY_REQUEST` |
| `--no-flow-fallback` | do not retry with the simple flow when the full flow fails |

By default `spput` performs the **full EPICS-Base-style PUT flow**: a GET
before the write, a get-put capability probe, the write, a GET after, then
an explicit destroy. That is what a real `pvput` does, and it is what
exercises the same code paths in a server under test.

## Output

```console
$ spput VAC:SETPOINT 5e-4
VAC:SETPOINT OK

$ spput VAC:SETPOINT 1.0
VAC:SETPOINT ERROR protocol error: PUT failed: VAC:SETPOINT: 1 outside 1e-9..1e-3
Error: Protocol("PUT failed: VAC:SETPOINT: 1 outside 1e-9..1e-3")
```

The exit status is non-zero on failure, so `spput ... && spget ...` is
safe in a script.

## Gotchas

**A rejected write reaches the server twice.** When the full flow fails,
`spput` silently falls back to the simple flow
(`spvirit-tools/src/bin/spvirit_put.rs:214`). A server-side `on_put`
callback therefore fires once for an accepted write and twice for a
rejected one. Pass `--no-flow-fallback` to suppress the retry, and write
`on_put` callbacks to be idempotent either way — see
[Reacting to writes](../03-progressive/reacting-to-writes.md).

**Read-only records refuse explicitly.** `ai`, `bi`, `stringin` and `aai`
answer with `PUT init error: Write access denied` rather than accepting
and discarding.

**Enum writes are accepted and dropped.** `spput SIM:MODE --json
'{"value":{"index":2}}'` prints `OK` and changes nothing. This is a server
bug, documented in [Enums](../03-progressive/enums.md).
