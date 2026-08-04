"""Periodic updates with scan — the Python mirror of
spvirit-server/examples/scan_callback.rs.

    python demo_scan.py
    pvmonitor SIM:TEMPERATURE
"""
import math
import time

import spvirit

# ANCHOR: sim
temp = spvirit.ai("SIM:TEMPERATURE", 22.5, units="degC", prec=2)


@temp.scan(period=0.1)
def _simulate(pv):
    return 22.5 + math.sin(time.monotonic())


server = spvirit.Server(pvs=[temp])
# ANCHOR_END: sim

server.run()
