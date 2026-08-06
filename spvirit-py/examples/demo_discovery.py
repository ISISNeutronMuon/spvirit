"""Discovery primitives demo — UDP search, server discovery, pvlist.

Exercises the discovery helpers in ``spvirit.lowlevel``:
  - ``parse_addr_list(str)``
  - ``auto_broadcast_targets()``
  - ``default_search_targets(search_addr=None, bind_addr=None)``
  - ``search_pv(pv_name, udp_port=..., targets=..., timeout=...)``
  - ``search_pv_tcp(pv_name, name_server, timeout=...)``
  - ``discover_servers(udp_port=..., timeout=..., targets=...)``
  - ``pvlist(server_addr, timeout=...)``

Usage::

    python3 demo_discovery.py [PV_NAME]
"""
from __future__ import annotations

import sys

from spvirit import lowlevel as ll


def main() -> None:
    # ── Helpers that work offline ────────────────────────────────────────
    print("parse_addr_list('1.2.3.4 5.6.7.8, 10.0.0.1') ->")
    print(f"  {ll.parse_addr_list('1.2.3.4 5.6.7.8, 10.0.0.1')}")

    auto = ll.auto_broadcast_targets()
    print(f"\nauto_broadcast_targets: {len(auto)} entries")
    for t in auto[:4]:
        print(f"  target={t['target']:<20}  bind={t['bind']}")
    if len(auto) > 4:
        print(f"  ... ({len(auto) - 4} more)")

    default = ll.default_search_targets()
    print(f"\ndefault_search_targets: {len(default)} entries "
          f"(honours EPICS_PVA_ADDR_LIST / EPICS_PVA_AUTO_ADDR_LIST)")

    # ── Discover servers on the LAN ─────────────────────────────────────
    # ANCHOR: discover
    print("\ndiscover_servers(timeout=1.5) ...")
    try:
        servers = ll.discover_servers(timeout=1.5)
    except Exception as e:  # noqa: BLE001
        print(f"  discovery failed: {e}")
        servers = []
    for s in servers:
        print(f"  guid={s['guid']}  addr={s['addr']}")
    if not servers:
        print("  (no servers responded — run a spserver locally to see results)")
    # ANCHOR_END: discover

    # ── pvlist against the first discovered server ───────────────────────
    # ANCHOR: list
    if servers:
        addr = servers[0]["addr"]
        print(f"\npvlist({addr!r}) ...")
        try:
            names, source = ll.pvlist(addr, timeout=3.0)
            print(f"  via {source}: {len(names)} names")
            for n in names[:10]:
                print(f"    {n}")
        except Exception as e:  # noqa: BLE001
            print(f"  pvlist failed: {e}")
    # ANCHOR_END: list

    # ── Targeted UDP search for a specific PV ───────────────────────────
    # ANCHOR: search
    if len(sys.argv) > 1:
        pv = sys.argv[1]
        print(f"\nsearch_pv({pv!r}, timeout=2.0) ...")
        try:
            addr = ll.search_pv(pv, timeout=2.0)
            print(f"  -> {addr}")
        except Exception as e:  # noqa: BLE001
            print(f"  not found: {e}")
    # ANCHOR_END: search


if __name__ == "__main__":
    main()
