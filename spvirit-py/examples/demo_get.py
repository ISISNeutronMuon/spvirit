"""Read one PV value from Python.

Needs a server holding SIM:TEMPERATURE — `demo_first_pv.py` will do.
"""

import spvirit

# ANCHOR: get
client = spvirit.Client()
result = client.get("SIM:TEMPERATURE")

# result.value is the whole NTScalar as a dict — value plus alarm,
# timeStamp, display, control and valueAlarm.
print(result.pv_name, "=", result.value["value"])
print("severity:", result.value["alarm"]["severity"])
# ANCHOR_END: get
