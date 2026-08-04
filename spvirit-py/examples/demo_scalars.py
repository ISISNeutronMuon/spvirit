"""A fully described scalar PV: units, precision, limits, deadband.

Try it:
    python spvirit-py/examples/demo_scalars.py

Then from another terminal:
    spget SIM:TEMPERATURE
    spget SIM:TEMPERATURE.MDEL     # 0.5
    spput SIM:SETPOINT 500         # accepted — drive limits are advisory
"""

import spvirit

# ANCHOR: meta
temperature = spvirit.ai(
    "SIM:TEMPERATURE",
    22.5,
    units="degC",
    prec=2,
    desc="Sample block temperature",
    # lolo, low, high, hihi
    alarm_limits=(0.0, 15.0, 30.0, 40.0),
    # Monitors stay quiet for changes smaller than this.
    mdel=0.5,
)

setpoint = spvirit.ao(
    "SIM:SETPOINT",
    25.0,
    units="degC",
    prec=1,
    desc="Demanded temperature",
    drive_limits=(0.0, 100.0),
)

server = spvirit.Server(pvs=[temperature, setpoint])
server.run()
# ANCHOR_END: meta
