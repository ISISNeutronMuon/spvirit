"""Minimal client — get, put, and monitor the mailbox served by demo_mailbox.py.

    python demo_mailbox_client.py            # find the server via UDP search
    python demo_mailbox_client.py 10.0.0.5:5075   # or connect directly
"""
import sys
import time

import spvirit

PV = "DEMO:MAILBOX"


def main() -> None:
    if len(sys.argv) > 1:
        client = spvirit.Client.builder().server_addr(sys.argv[1]).build()
    else:
        client = spvirit.Client()

    result = client.get(PV)
    print(f"get:  {PV} = {result.value['value']}")

    client.put(PV, 42.0)
    print(f"put:  {PV} <- 42.0")
    print(f"get:  {PV} = {client.get(PV).value['value']}")

    print("monitoring for 5 seconds (put to the PV from another terminal) ...")
    updates = []
    sub = client.subscribe(PV, lambda v: (updates.append(v), print(f"  update: {v['value']}")))
    time.sleep(5.0)
    sub.close()
    print(f"received {len(updates)} update(s)")


if __name__ == "__main__":
    main()
