"""Setting alarm severity explicitly.

Run:
    python spvirit-py/examples/demo_alarms.py

Then:
    spget SIM:LINK
    spget SIM:PRESSURE
    spinfo SIM:PRESSURE      # limits published, severity still 0
"""

import time

import spvirit

# ANCHOR: limits
pressure = spvirit.ai(
    "SIM:PRESSURE",
    50.0,
    units="bar",
    desc="Vessel pressure",
    # lolo, low, high, hihi — published to clients, not evaluated.
    alarm_limits=(5.0, 10.0, 90.0, 110.0),
)

link = spvirit.ai("SIM:LINK", 0.0, desc="Device link health")

server = spvirit.Server(pvs=[pressure, link], compute_alarms=True)
server.start()
# ANCHOR_END: limits

# ANCHOR: manual
# severity: 0=NONE 1=MINOR 2=MAJOR 3=INVALID
# status is an EPICS status code; the message is free text.
link.set_alarm(3, 17, "device unreachable")

# Deciding severity yourself, from the value. This is the route that works
# for handle-built PVs, since the limits above are never compared.
for reading in (50.0, 95.0, 120.0):
    pressure.set(reading)
    if reading >= 110.0:
        pressure.set_alarm(2, 4, "HIHI")
    elif reading >= 90.0:
        pressure.set_alarm(1, 4, "HIGH")
    else:
        pressure.set_alarm(0, 0, "")
    print(f"SIM:PRESSURE = {reading}")
    time.sleep(1)
# ANCHOR_END: manual

time.sleep(30)
