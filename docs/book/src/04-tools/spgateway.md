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

## Status

M1: configuration parsing and validation only. `spgateway <config.json>`
(without `-T`) currently reports that the run path is not yet wired and
exits; the runtime proxy path lands in a later milestone.
