"""Reacting to client writes with on_put — the Python mirror of
spvirit-server/examples/on_put.rs.

    python demo_on_put.py
    pvput SIM:SETPOINT 30
    pvput SIM:SETPOINT 500     # rejected
"""
import spvirit

# ANCHOR: callback
setpoint = spvirit.ao("SIM:SETPOINT", 25.0, drive_limits=(0.0, 100.0))


@setpoint.on_put
def _on_setpoint(pv, value):
    print(f"{pv.name} was set to {value}")
    if value > 100.0:
        return False  # reject the PUT on the wire


server = spvirit.Server(pvs=[setpoint])
# ANCHOR_END: callback

server.run()
