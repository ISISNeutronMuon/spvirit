//! `spsearch` must be able to bind the PVA search port even when another
//! process already holds it — that is the normal case, since a local spvirit
//! server is usually listening on 5076 too.

use std::net::{SocketAddr, UdpSocket};

/// Mirrors `spvirit_search::bind_udp_reuse`. Kept in sync by hand: the bin is
/// not a library, so the real function cannot be imported.
fn bind_udp_reuse(addr: SocketAddr) -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    sock.bind(&addr.into())?;
    Ok(sock.into())
}

#[test]
fn set_reuse_address_is_accepted_on_this_platform() {
    // The narrow claim the fix depends on: socket2's `set_reuse_address` maps
    // to plain SO_REUSEADDR and is a valid option on Windows as well as Unix,
    // so removing the `#[cfg(unix)]` guard cannot make the bind fail.
    let addr: SocketAddr = "0.0.0.0:0".parse().expect("addr");
    let sock = bind_udp_reuse(addr).expect("SO_REUSEADDR bind must succeed");
    assert!(sock.local_addr().expect("local addr").port() > 0);
}

/// Windows-only. `SO_REUSEADDR` on Windows permits a second bind to the exact
/// same address:port — which is the scenario `spsearch` exists for (sniffing
/// UDP 5076 while a local PVA server already holds it) and the scenario the
/// `#[cfg(unix)]` guard was silently breaking.
///
/// NOT asserted on Linux/macOS: there, a plain unicast `SO_REUSEADDR` bind to
/// an already-bound address:port is still `EADDRINUSE` — sharing needs
/// `SO_REUSEPORT`, which this helper does not set. Asserting the Windows
/// behaviour on every platform would make this test fail on CI for the wrong
/// reason.
#[cfg(windows)]
#[test]
fn two_search_sockets_can_share_the_same_port_on_windows() {
    let addr: SocketAddr = "0.0.0.0:0".parse().expect("addr");
    let first = bind_udp_reuse(addr).expect("first bind");
    let port = first.local_addr().expect("local addr").port();
    let shared: SocketAddr = format!("0.0.0.0:{port}").parse().expect("addr");
    let second = bind_udp_reuse(shared);
    assert!(
        second.is_ok(),
        "a second SO_REUSEADDR bind to port {port} must succeed on Windows: {:?}",
        second.err()
    );
}
