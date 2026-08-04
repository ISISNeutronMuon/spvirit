"""Enum records: mbbi (read-only) and mbbo (writable).

Run:
    python spvirit-py/examples/demo_enums.py

Then:
    spget SIM:STATE
    spget SIM:MODE

Note: writing to an mbbo over the wire is a known gap - see the Enums
chapter. Drive enum records server-side with `pv.set(index)`.
"""

import time

import spvirit

# ANCHOR: enums
STATES = ["Idle", "Running", "Fault"]
MODES = ["Standby", "Acquire", "Calibrate"]

# mbbi is read-only over the wire; mbbo accepts client writes.
state = spvirit.mbbi("SIM:STATE", STATES, 0, desc="Machine state")
mode = spvirit.mbbo("SIM:MODE", MODES, 0, desc="Requested mode")

server = spvirit.Server(pvs=[state, mode])
server.start()

# The value is the choice *index*, not the label.
for i in range(len(STATES)):
    state.set(i)
    print(f"SIM:STATE = {i} ({STATES[i]})")
    time.sleep(1)
# ANCHOR_END: enums

time.sleep(30)
