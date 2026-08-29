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
spgateway -v <config.json>
```

Requires the `client` and `server` features.

| Flag | Meaning |
|---|---|
| `-T`, `--test-config` | Parse and validate the configuration, print `OK` or the error, and exit (0 on success, 1 on error). Does not start the gateway. |
| `-v`, `--verbose` | Raise the log level from the default `INFO` to `DEBUG`. Ignored under `-T`. |

Flags may appear in any position relative to the config path; the first
non-flag argument is taken as the config file.

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

A normal start is not silent: the gateway installs a tracing subscriber at
`INFO` and logs a one-line-per-server startup banner (which port each server
listens on and which upstreams it proxies), plus one `Status PV: …` line per
status PV when a `statusprefix` is set, plus any warnings/errors. `-v` adds
per-module `DEBUG` detail.

## Status

M1 is a **passthrough** gateway with access control enforced: it resolves,
reads (`get`), writes (`put`), and monitors (`subscribe`) PVs across networks,
with per-server negative-search caching, `getholdoff`, `readOnly`/`pvlist`/ACF
enforcement (see below), and loop/self-connection prevention.

Not yet enforced in M1 (parsed but inert, or deferred to a later milestone):
the `x-spvirit` metrics / audit / hot-reload / rate-limit blocks, RPC
forwarding to upstream servers (the local `asTest` status RPC below is
answered by the gateway itself, not proxied), and a true upstream `pvlist`
fan-out for downstream `splist` (`names()` reports only PVs this gateway has
already claimed), and the per-server `acf-client` field (parsed and
referentially validated against `clients[]`, but not yet consulted at
runtime). Ctrl-C shutdown is immediate rather than a graceful drain.
These and the representation limits (array-of-structure, `union`/`any`,
non-finite floats, multi-token `addrlist`) are collected on the
[Known gaps](../05-reference/known-gaps.md) page.

## Access control

Each `servers[]` entry can restrict what it proxies with three independent
inputs, evaluated in a fixed precedence:

| Precedence | Input | Config field | Effect |
|---|---|---|---|
| 1 (highest) | Read-only mode | top-level `readOnly` | Denies every `put` and RPC. Never affects `get`/`subscribe`. |
| 2 | `pvlist` | per-server `pvlist` (path to a p4p-format ALLOW/DENY/ALIAS file) | First matching rule wins. `DENY` hides the PV from every operation, including reads. `ALLOW`/`ALIAS` bind an ASG/ASL for step 3. An `ALIAS` rule also rewrites the name the gateway proxies upstream. |
| 3 (lowest) | `access` | per-server `access` (path to a `.acf` file: `UAG`/`HAG`/`ASG` blocks) | Grants or denies `put`/RPC by matching the caller's user against `UAG` and host against `HAG`, gated by the ASG/ASL bound in step 2 (or `DEFAULT`/`0` if no `pvlist` is configured). **Never consulted for `get`/`subscribe`** — only a `pvlist DENY` can hide a read. |

If no `pvlist` is configured for a server, every PV is eligible (step 2 is
skipped entirely) and falls through to the ACF step under the implicit
`DEFAULT` ASG at ASL `0`. If a `pvlist` *is* configured, a PV that matches no
rule in it is **denied** — first-match-wins with no fallthrough default is a
fail-closed design, matching p4p/pvagw.

`.acf` support is a documented subset: `UAG`, `HAG`, and `ASG`/`RULE` blocks
parse; `CALC` guard expressions in a `RULE` are a hard parse error rather than
being silently ignored, so a config that depends on `CALC` fails `-T`
validation instead of serving with a weaker rule than the operator intended.
A duplicate `UAG`, `HAG`, or `ASG` name is also a hard parse error, so a
second definition can never silently shadow the first. `READ`/`GET`,
`WRITE`/`PUT`, and `RPC` are the recognised `RULE` operations; any other
op keyword is rejected. Referenced but undefined `UAG`/`HAG` names, and an
`ASG` absent from the file, fail closed (they grant nothing) rather than
erroring.

### Fail-closed configuration loading

`validate()` (and therefore `spgateway -T`) loads and parses every
configured `pvlist` and `access` file up front. A missing file, a file that
cannot be read, or one that fails to parse is a validation error — the
gateway refuses to start rather than falling back to "no restriction". There
is no way to configure a `pvlist`/`access` path that is silently ignored if
broken.

`clients[].provider` is also validated: spvirit only speaks PVAccess
upstream, so any client whose `provider` is not `"pva"` (the default) fails
validation rather than being silently accepted and then never resolving
anything.

```console
$ spgateway -T gateway.json
invalid config: pvlist '/etc/pvagw/pvacl.conf': No such file or directory (os error 2)
```

## Loop / self-connection guard

A bidirectional gateway (or two gateway instances on the same host) must
never resolve a PV search back into one of its own downstream servers. Each
server's `LoopGuard` bans:

- **Own-server sockets** — every `servers[].interface` IP (or, for a server
  with no `interface`, every local interface address the process can
  enumerate — the `0.0.0.0` backstop) paired with that server's
  `serverport`. This is socket-specific: a real upstream IOC sharing the
  gateway's IP but a different port still resolves normally.
- **`ignoreaddr` hosts** — an operator-supplied list of hostnames/IPs,
  forward-resolved to IPs and banned on *every* port.
- **The gateway's own server GUIDs** — generated up front at startup for
  every `servers[]` entry, before any client connects. A search response
  carrying one of these GUIDs is treated as a self-reference and rejected
  regardless of which address it claims to come from. This closes the gap a
  socket-only ban leaves against the default `0.0.0.0` bind, where the
  gateway's own listening address may not be enumerable (or may be reachable
  under an address the socket ban doesn't cover) before the guard is built.

## Status PVs

Setting a server's `statusprefix` (e.g. `"gw:status:"`) serves 15
introspection PVs under that prefix, gated through the same `AccessControl`
as the data plane:

| Group | PVs | Notes |
|---|---|---|
| Live | `clients`, `cache`, `refs`, `threads`, `stats`, `poke` | `clients` and `cache` read real counters (configured upstream client count; active upstream monitor count). `refs`, `threads`, and `stats` currently read `0.0` — no M1 data source exists yet for per-binding refcounts, thread-pool introspection, or aggregate request stats. `poke` is the one writable status PV: a `put` bumps an internal generation counter, and `poke`'s own value reports it — useful for confirming the status source is alive. |
| Static | `ds:bypv:rx`, `ds:bypv:tx`, `ds:byhost:rx`, `ds:byhost:tx`, `us:bypv:rx`, `us:bypv:tx`, `us:byhost:rx`, `us:byhost:tx` | Bandwidth counters, always `0.0` in M1 — no per-PV/per-host byte accounting exists yet. |
| RPC | `asTest` | Diagnostic: evaluates `get`/`put`/rpc access for a `{pv, user, host}` argument struct against this server's `AccessControl` and returns the three allow/deny verdicts plus a summary string, without touching any upstream. |

## Trust boundary

The connecting client supplies its own `user` and `host` identity in the
PVAccess `ca` connection-validation credentials, and the gateway decodes
them as-is — nothing about that exchange authenticates the claim.

For `host`, the gateway does **not** trust the client's word: `pvlist
FROM`/ACF `HAG` matching is always evaluated against the TCP socket's actual
peer-IP address, never against the client-asserted `host` string in the `ca`
credentials. That asserted `host` is decoded and available for diagnostics
(e.g. `asTest`), but it is advisory only and never feeds an access decision
— trusting it would let a client claim a trusted hostname it isn't actually
connecting from and bypass host-based rules.

For `user`, the gateway matches p4p/pvagw's long-standing posture: the
`ca`-asserted user is trusted as-is for `UAG` matching, with no independent
authentication. `UAG` is therefore authorization, not authentication —
operators who need real user authentication should treat this like any
other unauthenticated network boundary, since M1 has no mechanism (TLS
client certs, Kerberos, etc.) to bind the declared identity to the
transport.
