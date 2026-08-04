"""Serving an array PV — the Python mirror of
spvirit-server/examples/waveform.rs.

    python demo_waveform.py
    pvmonitor SIM:SPECTRUM
"""
import math
import time

import spvirit

# ANCHOR: serve
spectrum = spvirit.waveform("SIM:SPECTRUM", [0.0] * 1024)

server = spvirit.Server(pvs=[spectrum])
server.start()

tick = 0
while True:
    phase = tick * 0.03
    spectrum.set([
        math.sin(phase + i * 0.02) + 0.25 * math.cos(phase * 0.5 + i * 0.05)
        for i in range(1024)
    ])
    tick += 1
    time.sleep(0.1)
# ANCHOR_END: serve
