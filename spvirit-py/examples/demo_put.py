"""Write a value to a PV from Python.

Needs a server holding a writable SIM:SETPOINT — `demo_scalars.py` will do.
"""

import spvirit

# ANCHOR: put
client = spvirit.Client()

client.put("SIM:SETPOINT", 30.0)
print("after put:", client.get("SIM:SETPOINT").value["value"])
# ANCHOR_END: put
