"""Typed PV handles — the recommended way to build a soft IOC in Python.

Attach on_put/scan/calc BEFORE the PV is served (spvirit.Server(...)):
attaching any of them afterwards is a silent no-op — the core only logs a
tracing warning, it does not raise.
"""
import time

import spvirit

temp = spvirit.ai("DEMO:TEMP", 22.5, units="degC", prec=2)
setpoint = spvirit.ao("DEMO:SP", 25.0, drive_limits=(0.0, 100.0))


@setpoint.on_put
def _on_setpoint(pv, value):
    print(f"setpoint -> {value}")
    if value > 100.0:
        return False  # reject the PUT on the wire


@temp.scan(period=1.0)
def _simulate(pv):
    sp = setpoint.get()
    t = pv.get()
    return t + 0.1 * (sp - t)  # relax toward the setpoint


power = spvirit.calc("DEMO:POWER", [temp, setpoint],
                     lambda v: max(0.0, v[1] - v[0]))

server = spvirit.Server(pvs=[temp, setpoint, power])
server.start()
print("serving DEMO:TEMP / DEMO:SP / DEMO:POWER — Ctrl+C to stop")

# Every handle also has async aset/aget for use inside asyncio code, e.g.:
#     async def bump():
#         await setpoint.set_async(30.0)
#         print(await temp.get_async())
#     asyncio.run(bump())

while True:
    time.sleep(5)
    print(f"T={temp.get():.2f} SP={setpoint.get():.2f} P={power.get():.2f}")
