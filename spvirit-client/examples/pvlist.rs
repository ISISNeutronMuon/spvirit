use std::net::SocketAddr;
use std::time::Duration;

use spvirit_client::PvaClient;
use spvirit_client::search::{build_search_targets, discover_servers, search_pv};
use spvirit_codec::spvd_decode::format_structure_tree;

/// Discovery and introspection example — the library form of `splist` and
/// `spinfo`.
///
/// Usage:
///
/// ```text
/// cargo run --example pvlist                    # discover servers
/// cargo run --example pvlist -- 127.0.0.1:5075  # list that server's PVs
/// cargo run --example pvlist -- MY:PV           # search for it, then introspect
/// ```
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arg = std::env::args().nth(1);
    let timeout = Duration::from_secs(2);
    // The standard PVA search port; `spinfo`/`splist` expose it as --udp-port.
    let udp_port = 5076;
    let targets = build_search_targets(None, None);

    match arg {
        // No argument: ask the network who is out there.
        None => {
            // ANCHOR: discover
            let servers = discover_servers(udp_port, timeout, &targets, false).await?;
            for server in &servers {
                let guid: String = server.guid.iter().map(|b| format!("{b:02X}")).collect();
                println!("GUID 0x{guid}  tcp {}", server.tcp_addr);
            }
            // ANCHOR_END: discover
            if servers.is_empty() {
                println!("(no servers responded)");
            }
        }

        // An address: enumerate that server's PVs.
        Some(a) if a.parse::<SocketAddr>().is_ok() => {
            let server_addr: SocketAddr = a.parse()?;
            // ANCHOR: list
            let client = PvaClient::builder().timeout(timeout).build();
            let (names, source) = client.pvlist_with_fallback(server_addr).await?;
            println!("{} PVs via {source:?}", names.len());
            for name in &names {
                println!("  {name}");
            }
            // ANCHOR_END: list
        }

        // Anything else: treat it as a PV name — locate it, then describe it.
        Some(pv) => {
            // ANCHOR: search
            let (server_addr, _guid) = search_pv(&pv, udp_port, timeout, &targets, false).await?;
            println!("{pv} is served by {server_addr}");
            // ANCHOR_END: search

            // ANCHOR: info
            let client = PvaClient::builder().timeout(timeout).build();
            let desc = client.pvinfo(&pv).await?;
            println!("{}", format_structure_tree(&desc));
            // ANCHOR_END: info
        }
    }

    Ok(())
}
