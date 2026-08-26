# `spgateway`

<!-- verify:begin -->
> ✅ **Verified** · no code on this page · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

A p4p-compatible PVAccess gateway. It reads a p4p-schema JSON configuration
(with an additive `x-spvirit` superset for spvirit-only features — metrics,
audit, hot reload, negative-search caching, rate limiting) and proxies
PVAccess traffic between an upstream "client" network and a downstream
"server" network.

```
spgateway <config.json>
spgateway -T <config.json>
spgateway --test-config <config.json>
```

Requires the `client` and `server` features.

| Flag | Meaning |
|---|---|
| `-T`, `--test-config` | Parse and validate the configuration, print `OK` or the error, and exit (0 on success, 1 on error). Does not start the gateway. |

## Validating a configuration

```console
$ spgateway -T gateway.json
OK
```

```console
$ spgateway -T broken.json
invalid config: server 's' references unknown client 'nope'
```

## Running

```console
$ spgateway gateway.json
```

`spgateway <config.json>` starts the gateway: it builds one shared upstream
client pool and one PVA server per `servers[]` entry, then serves until it
receives Ctrl-C. Each server resolves names across its configured `clients`
(in order), reads with `getholdoff` staleness suppression, writes, and
fans out monitors — proxying between the upstream "client" network and the
downstream "server" network. Requires the `client` and `server` features.

## Status

M1 is a **passthrough** gateway: it resolves, reads (`get`), writes (`put`),
and monitors (`subscribe`) PVs across networks, with per-server negative-search
caching, `getholdoff`, and loop/self-connection prevention.

Not yet enforced in M1 (parsed but inert, or deferred to a later milestone):
access control (DENY filtering, `readOnly`), the `x-spvirit` metrics / audit /
hot-reload / rate-limit blocks, RPC forwarding, and a true upstream `pvlist`
fan-out for downstream `splist`. Ctrl-C shutdown is immediate rather than a
graceful drain. These and the representation limits (array-of-structure,
`union`/`any`, non-finite floats, multi-token `addrlist`) are collected on the
[Known gaps](../05-reference/known-gaps.md) page.
