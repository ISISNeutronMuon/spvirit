"""A complete soft IOC — the Python mirror of
spvirit-server/examples/complete_ioc.rs.

Everything from Part III in one server: metadata, deadbands, validation,
periodic scanning, a computed PV, an array, and an explicit alarm.

Run:
    python spvirit-py/examples/demo_complete_ioc.py

Then:
    splist
    spget VAC:PRESSURE
    spput VAC:SETPOINT 5e-7      # accepted
    spput VAC:SETPOINT 1.0       # rejected - outside range
    spmonitor VAC:PRESSURE
"""

import math
import time

import spvirit

# ANCHOR: build
# --- Readback: scanned, with units and a monitor deadband ---------------
pressure = spvirit.ai(
    "VAC:PRESSURE",
    1.0e-6,
    units="mbar",
    prec=3,
    desc="Chamber pressure",
    mdel=1.0e-8,  # suppress sub-nanobar jitter
)

_tick = 0


@pressure.scan(period=0.5)
def _pump_down(pv):
    global _tick
    n = float(_tick)
    _tick += 1
    # A decaying pump-down curve with a little noise.
    return 1.0e-6 * math.exp(-n / 40.0) + 1.0e-9 * math.sin(n * 1.7)


# --- Setpoint: validated on write ---------------------------------------
setpoint = spvirit.ao(
    "VAC:SETPOINT", 1.0e-6, units="mbar", prec=3, desc="Target pressure"
)


@setpoint.on_put
def _check_range(pv, value):
    # Drive limits are advisory, so enforce the range here. Raising rejects
    # the PUT and sends the exception text back to the client; returning
    # False also rejects, but with a fixed "rejected by on_put" message.
    if not 1.0e-9 <= value <= 1.0e-3:
        raise ValueError(f"{pv.name}: {value} outside 1e-9..1e-3")
    print(f"{pv.name} -> {value:e}")


# --- Derived: recomputed whenever an input moves -------------------------
# `calc` takes handles, not names, and no metadata keywords — units and
# desc are not settable on a computed PV from Python.
error = spvirit.calc("VAC:ERROR", [pressure, setpoint], lambda vals: vals[0] - vals[1])

# --- Array: a spectrum a client can read but not write -------------------
spectrum = spvirit.aai("VAC:RGA", [0.0] * 64)

# --- Status: severity we set ourselves -----------------------------------
status = spvirit.ai("VAC:LINK", 0.0, desc="Gauge controller link")

# One flat list, whatever the handle types. Callbacks must already be
# attached at this point: the handles are bound here, and `on_put`/`scan`/
# `calc` are ignored on a bound handle.
server = spvirit.Server(pvs=[pressure, setpoint, error, spectrum, status])
server.start()
# ANCHOR_END: build

# ANCHOR: drive
# Everything above is declarative. Anything else you want the IOC to do is
# an ordinary loop driving the handles. `scan` is not available on an array
# PV, so VAC:RGA is updated from here.

# The gauge controller is reachable, so clear the alarm explicitly.
status.set_alarm(0, 0, "")

frame = 0
while True:
    spectrum.set([abs(math.sin(i * 0.2 + frame * 0.1)) for i in range(64)])
    frame += 1
    time.sleep(0.2)
# ANCHOR_END: drive
