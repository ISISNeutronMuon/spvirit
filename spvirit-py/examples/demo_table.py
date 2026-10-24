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

builder = spvirit.Server.builder()

# ANCHOR: table
# An NTTable is a dict of equal-length columns.
# Labels default to the column names.
builder.nt_table("SIM:TBL", {"x": [0.0] * 8, "y": [0.0] * 8})
# ANCHOR_END: table

# ANCHOR: ndarray
# An NTNDArray is flat data plus dimensions. On the builder each dimension
# is a (size, offset) pair: the served extent and where it starts in the
# underlying frame.
builder.nt_ndarray("SIM:IMG", [0] * 16, [(4, 0), (4, 0)], type="ubyte")
# ANCHOR_END: ndarray

server = builder.build()
server.start()
store = server.store()

# ANCHOR: drive
# A client can write the table over the wire; a wire PUT to the image applies
# its metadata but not the image value. Here the server drives both with
# put_nt. The payload constructors take dimensions as a flat list of sizes,
# where the builder takes (size, offset) pairs.
for tick in range(20):
    xs = [float(i) for i in range(8)]
    ys = [math.sin(v * 0.7 + tick * 0.15) for v in xs]
    store.put_nt("SIM:TBL", spvirit.NtTable({"x": xs, "y": ys}, labels=["X", "Y"]))

    frame = [(i * 16 + tick * 8) % 256 for i in range(16)]
    store.put_nt("SIM:IMG", spvirit.NtNdArray(frame, [4, 4], type="ubyte"))
    time.sleep(0.5)
# ANCHOR_END: drive
