"""Serving an EPICS .db file — the Python mirror of
spvirit-server/examples/db_file.rs.

The same file `spserver --db` takes, and the same one the Rust example
loads, serves unchanged from Python.

    python spvirit-py/examples/demo_db_file.py

Then:
    splist
    spget DEMO:TEMP
    spput DEMO:SETPOINT 46      # >= HIHI -> MAJOR
    spget DEMO:SETPOINT
"""

import time

import spvirit

# ANCHOR: load
server = spvirit.Server(
    db_file="spvirit-server/examples/example.db",
    # .db LOW/HIGH/LOLO/HIHI are only evaluated when this is on.
    compute_alarms=True,
)
# `db_string="record(ai, \"X\") { field(VAL, \"1\") }"` takes the same
# syntax inline, which is what you want in a test.
# ANCHOR_END: load

# ANCHOR: handle
# A .db-loaded record has no handle. `Server.pv()` mints one, typed from
# the record's wire type, so you can drive it like any other:
temp = server.pv("DEMO:TEMP")
temp.set(23.4)
print(f"DEMO:TEMP = {temp.get()}")

# The handle is *already bound* to the served record, so `on_put`, `scan`
# and `calc` are silently ignored on it — those must be attached to an
# unbound handle before the server is built. A .db record therefore cannot
# have a write validator; declare it with `spvirit.ao(...)` if it needs one.
# ANCHOR_END: handle

server.start()

store = server.store()
print("demo_db_file running - try `splist`")
for name in store.pv_names():
    print(f"  {name} = {store.get_value(name)}")

try:
    while True:
        time.sleep(3600)
except KeyboardInterrupt:
    print("\nbye.")
