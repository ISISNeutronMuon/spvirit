"""Alarm severity: computed from limits, and set explicitly.

Run:
    python spvirit-py/examples/demo_alarms.py

Then:
    spget SIM:LINK
    spget SIM:PRESSURE
    spinfo SIM:PRESSURE      # the published valueAlarm limits
"""

import time

import spvirit

# ANCHOR: limits
pressure = spvirit.ai(
    "SIM:PRESSURE",
    50.0,
    units="bar",
    desc="Vessel pressure",
    # lolo, low, high, hihi — published, and evaluated on every write
    # because the server is started with compute_alarms=True.
    alarm_limits=(5.0, 10.0, 90.0, 110.0),
)

link = spvirit.ai("SIM:LINK", 0.0, desc="Device link health")

server = spvirit.Server(pvs=[pressure, link], compute_alarms=True)
server.start()
# ANCHOR_END: limits

# ANCHOR: manual
# Alarms you decide yourself, independent of the value.
# severity: 0=NONE 1=MINOR 2=MAJOR 3=INVALID
# status is an EPICS status code; the message is free text.
link.set_alarm(3, 17, "device unreachable")

# Each write is compared with the limits above: 50 is in range, 95 raises
# MINOR (HIGH), 120 raises MAJOR (HIHI). No set_alarm needed.
for reading in (50.0, 95.0, 120.0):
    pressure.set(reading)
    print(f"SIM:PRESSURE = {reading}")
    time.sleep(1)
# ANCHOR_END: manual

time.sleep(30)
