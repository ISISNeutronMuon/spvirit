"""NTTable and NTNDArray - payloads that are not EPICS records.

Run:
    python spvirit-py/examples/demo_table.py

Then:
    spget SIM:TBL
    sptable SIM:TBL
    spget SIM:IMG
"""

import math
import time

import spvirit

# ANCHOR: table
builder = spvirit.Server.builder()

# An NTTable is a dict of equal-length columns.
# Labels default to the column names.
builder.nt_table("SIM:TBL", {"x": [0.0] * 8, "y": [0.0] * 8})

# An NTNDArray is flat data plus (size, full_size) dimension pairs.
builder.nt_ndarray("SIM:IMG", [0] * 16, [(4, 4), (4, 4)], type="ubyte")

server = builder.build()
server.start()
store = server.store()

for tick in range(20):
    xs = [float(i) for i in range(8)]
    ys = [math.sin(v * 0.7 + tick * 0.15) for v in xs]
    store.put_nt("SIM:TBL", spvirit.NtTable({"x": xs, "y": ys}, labels=["X", "Y"]))
    time.sleep(0.5)
# ANCHOR_END: table
