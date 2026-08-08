"""Watch a PV for changes.

Needs a server publishing SIM:TEMPERATURE — `demo_scan.py` will do.
"""

import spvirit

# ANCHOR: monitor
client = spvirit.Client()

seen = 0


def on_update(update):
    # `update` is a MonitorUpdate: .value plus the .changed / .overrun paths.
    global seen
    seen += 1
    print(f"{seen}: {update.value['value']:.3f}")
    return seen < 5  # returning False ends the monitor


client.monitor("SIM:TEMPERATURE", on_update)
# ANCHOR_END: monitor
