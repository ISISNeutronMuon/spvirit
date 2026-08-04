"""The smallest useful spvirit server: three records, no simulation.

Run this, then read a value from another terminal:

    spget SIM:TEMPERATURE
"""

import spvirit

# ANCHOR: serve
temp = spvirit.ai("SIM:TEMPERATURE", 22.5)      # input  — read-only to clients
setpoint = spvirit.ao("SIM:SETPOINT", 25.0)     # output — clients may write
enable = spvirit.bo("SIM:ENABLE", False)        # output — a writable bool

server = spvirit.Server(pvs=[temp, setpoint, enable])
server.run()
# ANCHOR_END: serve
