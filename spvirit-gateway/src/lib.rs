//! spvirit-gateway — a p4p-compatible PVAccess gateway.
//!
//! A drop-in replacement for the EPICS p4p gateway (`pvagw`): it reads a
//! p4p-schema JSON configuration and proxies PVAccess traffic between an
//! upstream "client" network and a downstream "server" network. The gateway is
//! simultaneously a PVA *client* (upstream, via [`upstream::UpstreamPool`]) and
//! a PVA *server* (downstream, via a [`proxy::GatewaySource`] registered on a
//! `spvirit_server::PvaServer`). [`runtime::Runtime::from_config`] wires a whole
//! configuration — one shared upstream pool plus one server per `servers[]`
//! entry — and [`runtime::Runtime::run`] serves them until Ctrl-C.
//!
//! # M1 capabilities (passthrough gateway)
//!
//! - **Config**: full p4p schema parse + validation, plus an additive
//!   `x-spvirit` superset for spvirit-only knobs (negative-search cache,
//!   `getholdoff`, and the parsed-but-not-yet-consumed metrics/audit/hot-reload/
//!   rate-limit blocks). `spgateway -T` validates without starting. See
//!   [`config`].
//! - **Data plane**: `claim` resolves a name across the server's `clients` in
//!   order (with a negative-search cache for misses); `get` (with `getholdoff`
//!   staleness suppression), `put`, and `subscribe` (monitor dedup + fan-out
//!   with gateway-side delta-merge) all forward to the resolved upstream. See
//!   [`proxy`], [`bridge`], [`convert`], [`cache`].
//! - **Loop prevention**: [`loopguard::LoopGuard`] bans the gateway's own
//!   server sockets (each interface IP + `serverport`) and configured
//!   `ignoreaddr` hosts, and [`proxy::GatewaySource::claim`] consults it on
//!   every resolution (via `pvinfo_full`'s returned address) so the gateway
//!   never resolves back into itself. The ban is socket-granular for own
//!   servers, so a legitimate upstream sharing an IP on a different port still
//!   resolves; `ignoreaddr` hosts are DNS-resolved (failures non-fatal) and
//!   banned at every port.
//!
//! # Documented M1 divergences
//!
//! These are deliberate M1 limitations (spec §14), collected in the book's
//! [Known gaps](https://isisneutronmuon.github.io/spvirit/05-reference/known-gaps.html)
//! page:
//!
//! - **Representation**: array-of-structure values (e.g. an `NTNDArray`
//!   `dimension`) are proxied lossily as index-keyed structures; `union`/`any`
//!   fields degrade; non-finite floats (`NaN`/`Inf`) become JSON `null` on the
//!   put path; deeply nested struct IDs are best-effort.
//! - **Resolution**: only the first `addrlist` token is used; a config with
//!   `autoaddrlist:false` + an explicit unicast `addrlist` still emits a subnet
//!   broadcast alongside the unicast search (the underlying `spvirit-client`
//!   search branch cannot be changed from here).
//! - **Control plane**: `names()` reports only names already claimed by this
//!   gateway, not a true upstream `pvlist` fan-out (no upstream `SocketAddr` is
//!   cached in M1); `rpc` forwarding is not implemented (no general client RPC
//!   entry point exists yet) and returns an error.
//! - **Ops**: Ctrl-C shutdown is immediate (outstanding requests are
//!   hard-cancelled), not a graceful drain.
//! - **Access control** (DENY filtering, `readOnly` enforcement) and the
//!   metrics/audit/hot-reload/rate-limit features are parsed but **not**
//!   enforced in M1 — they land in later milestones.
pub mod access;
pub mod bridge;
pub mod cache;
pub mod config;
pub mod convert;
pub mod loopguard;
pub mod proxy;
pub mod runtime;
pub mod status;
pub mod upstream;
#[cfg(test)]
mod smoke {
    #[test]
    fn crate_builds() {
        assert_eq!(2 + 2, 4);
    }
}
