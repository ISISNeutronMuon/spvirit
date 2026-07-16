"""Minimal mailbox server — the smallest useful spvirit IOC.

One writable PV. Clients can get it, put to it, and monitor it; every
accepted put is fanned out to all subscribers. Run it, then talk to it
from another terminal:

    python demo_mailbox.py [tcp_port] [udp_port]     # defaults: 5075 5076
    # elsewhere:
    python demo_mailbox_client.py
    # or with EPICS tools:
    pvget DEMO:MAILBOX
    pvput DEMO:MAILBOX 42.0
    pvmonitor DEMO:MAILBOX
"""
import sys

import spvirit

mailbox = spvirit.ao("DEMO:MAILBOX", 0.0, desc="A simple mailbox PV")


@mailbox.on_put
def on_put(pv, value):
    print(f"client wrote {value} to {pv.name}")


def main() -> None:
    tcp = int(sys.argv[1]) if len(sys.argv) > 1 else 5075
    udp = int(sys.argv[2]) if len(sys.argv) > 2 else 5076
    server = spvirit.Server(pvs=[mailbox], port=tcp, udp_port=udp)
    print(f"Serving DEMO:MAILBOX (TCP {tcp} / UDP {udp}).")
    print("Ctrl-C to stop.")
    server.run()


if __name__ == "__main__":
    main()
