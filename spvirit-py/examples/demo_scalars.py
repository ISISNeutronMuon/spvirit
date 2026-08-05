"""A fully described scalar PV: units, precision, limits, deadband.

Also demonstrates picking the NTScalar wire type explicitly via
``spvirit.scalar(name, initial, type=...)``, for the eight types
(byte/short/ubyte/ushort/uint/ulong, plus explicit float/bool/long/string)
the inferring constructors (``ai``/``ao``/``bi``/``bo``/``longin``/
``longout``/``string_in``/``string_out``) don't reach.

Try it:
    python spvirit-py/examples/demo_scalars.py

Then from another terminal:
    spget SIM:TEMPERATURE
    spget SIM:TEMPERATURE.MDEL     # 0.5
    spput SIM:SETPOINT 500         # accepted — drive limits are advisory
    spinfo SIM:GAIN                # wire type: ushort
    spinfo SIM:STATUS              # wire type: byte
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

# ANCHOR: types
# `type=` picks the wire type by name (or alias, e.g. "u16"); `writable=True`
# serves the output flavor, `False` (default) the input flavor.
gain = spvirit.scalar("SIM:GAIN", 1, type="ushort", writable=True)
status = spvirit.scalar("SIM:STATUS", 0, type="byte")
# ANCHOR_END: types

server = spvirit.Server(pvs=[temperature, setpoint, gain, status])
server.run()
# ANCHOR_END: meta
