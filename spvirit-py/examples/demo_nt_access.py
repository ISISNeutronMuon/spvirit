"""Demo: lower-level Normative Type (NT) access via Store.get_nt() / put_nt().

Starts a server with several record types and then reads back the
full NT metadata — alarm, display limits, control limits, units, etc.
— and demonstrates constructing NT objects from Python and writing
them back with put_nt().

Usage:
    maturin develop --release
    python examples/demo_nt_access.py
"""

import math
import time
import spvirit

# ─── Build a server with assorted record types ───────────────────────────────

builder = spvirit.ServerBuilder()

# Analog records
builder.ai("NT:TEMPERATURE", 22.5)
builder.ao("NT:SETPOINT", 25.0)

# Boolean
builder.bi("NT:INTERLOCK", False)

# String
builder.string_in("NT:STATUS", "RUNNING")

# Waveform (NtScalarArray)
builder.waveform("NT:WAVEFORM", [1.0, 2.0, 3.0, 4.0, 5.0])

# Enable alarm computation so alarm fields are populated
builder.compute_alarms(True)
builder.port(5085)
builder.udp_port(5086)

server = builder.build()
store = server.start_background()

# Give the server a moment to start
time.sleep(0.5)

print("=" * 60)
print("  Normative Type (NT) access demo")
print("=" * 60)

# ─── NtScalar: full metadata for an analog input ─────────────────────────────

print("\n── NtScalar (NT:TEMPERATURE) ──")
# ANCHOR: getnt
# get_nt returns the whole payload — get_value returns only the number.
# It works on IOC-style records too, so you can start with handles and
# reach through to the raw layer for the one PV that needs it.
nt = store.get_nt("NT:TEMPERATURE")
if nt is not None:
    print(f"  value  {nt.value} {nt.units}  severity={nt.alarm_severity}")
    print(f"  display {nt.display_low}..{nt.display_high} prec={nt.display_precision}")
    print(f"  control {nt.control_low}..{nt.control_high}")
# A name nothing serves gives None rather than raising.
assert store.get_nt("DOES:NOT:EXIST") is None
# ANCHOR_END: getnt
if nt is not None:
    print(f"  type          : {type(nt).__name__}")
    print(f"  value         : {nt.value}")
    print(f"  units         : {nt.units!r}")
    print(f"  alarm_severity: {nt.alarm_severity}")
    print(f"  alarm_status  : {nt.alarm_status}")
    print(f"  alarm_message : {nt.alarm_message!r}")
    print(f"  display_low   : {nt.display_low}")
    print(f"  display_high  : {nt.display_high}")
    print(f"  display_desc  : {nt.display_description!r}")
    print(f"  display_prec  : {nt.display_precision}")
    print(f"  control_low   : {nt.control_low}")
    print(f"  control_high  : {nt.control_high}")
    print(f"  control_step  : {nt.control_min_step}")

# ─── NtScalar: writable analog output ────────────────────────────────────────

print("\n── NtScalar (NT:SETPOINT) ──")
nt = store.get_nt("NT:SETPOINT")
if nt is not None:
    print(f"  value         : {nt.value}")
    print(f"  alarm_severity: {nt.alarm_severity}")

# Update the setpoint and read it back
store.set_value("NT:SETPOINT", 30.0)
nt = store.get_nt("NT:SETPOINT")
if nt is not None:
    print(f"  value (after)  : {nt.value}")

# ─── NtScalar: boolean record ────────────────────────────────────────────────

print("\n── NtScalar (NT:INTERLOCK) ──")
nt = store.get_nt("NT:INTERLOCK")
if nt is not None:
    print(f"  value         : {nt.value}")
    print(f"  type(value)   : {type(nt.value).__name__}")

# ─── NtScalar: string record ─────────────────────────────────────────────────

print("\n── NtScalar (NT:STATUS) ──")
nt = store.get_nt("NT:STATUS")
if nt is not None:
    print(f"  value         : {nt.value!r}")
    print(f"  type(value)   : {type(nt.value).__name__}")

# ─── NtScalarArray: waveform record ──────────────────────────────────────────

print("\n── NtScalarArray (NT:WAVEFORM) ──")
nt = store.get_nt("NT:WAVEFORM")
if nt is not None:
    print(f"  type          : {type(nt).__name__}")
    print(f"  value         : {nt.value}")
    print(f"  alarm         : {nt.alarm}")
    print(f"  time_stamp    : {nt.time_stamp}")
    print(f"  display       : {nt.display}")
    print(f"  control       : {nt.control}")

# Update the waveform and read back
store.set_array_value("NT:WAVEFORM", [math.sin(x * 0.1) for x in range(10)])
nt = store.get_nt("NT:WAVEFORM")
if nt is not None:
    print(f"  value (after) : {nt.value}")

# ─── Comparing get_value vs get_nt ───────────────────────────────────────────

print("\n── get_value() vs get_nt() ──")
simple = store.get_value("NT:TEMPERATURE")
full = store.get_nt("NT:TEMPERATURE")
print(f"  get_value() → {simple}  (type: {type(simple).__name__})")
print(f"  get_nt()    → {full}    (type: {type(full).__name__})")
print(f"  get_nt() exposes alarm, display, control metadata")
print(f"  that get_value() does not")

# ─── Non-existent PV returns None ─────────────────────────────────────────────

print("\n── Missing PV ──")
result = store.get_nt("DOES:NOT:EXIST")
print(f"  get_nt('DOES:NOT:EXIST') → {result}")

# ─── Constructing NT objects from Python and writing with put_nt() ────────────

print("\n── Creating NtScalar from Python ──")
# ANCHOR: builder
# Python has no builder chain — every field is a keyword argument on the
# constructor, and anything you leave out keeps its default.
nt_scalar = spvirit.NtScalar(
    value=42.0,
    units="degC",
    display_low=0.0,
    display_high=100.0,
    display_description="Temperature setpoint",
    display_precision=2,
    control_low=5.0,
    control_high=95.0,
)

# Nothing computes alarms for a payload you built yourself: an NtScalar is
# NO_ALARM unless you pass alarm_severity/alarm_status/alarm_message.
warm = spvirit.NtScalar(
    value=96.0,
    units="degC",
    alarm_severity=1,  # 0=NONE 1=MINOR 2=MAJOR 3=INVALID
    alarm_status=4,
    alarm_message="HIGH",
)

# Write the whole payload, metadata included. put_nt coerces to the record's
# existing wire type and never retypes it.
store.put_nt("NT:SETPOINT", nt_scalar)
# ANCHOR_END: builder
print(f"  Created: {nt_scalar}")
print(f"  warm    : {warm} severity={warm.alarm_severity}")
print(f"  value           : {nt_scalar.value}")
print(f"  units           : {nt_scalar.units!r}")
print(f"  display_low     : {nt_scalar.display_low}")
print(f"  display_high    : {nt_scalar.display_high}")
print(f"  display_desc    : {nt_scalar.display_description!r}")
print(f"  display_prec    : {nt_scalar.display_precision}")
print(f"  control_low     : {nt_scalar.control_low}")
print(f"  control_high    : {nt_scalar.control_high}")

# Read back and verify the write above
nt = store.get_nt("NT:SETPOINT")
print(f"  Readback value  : {nt.value}")
print(f"  Readback units  : {nt.units!r}")

print("\n── Creating NtScalarArray from Python ──")
nt_array = spvirit.NtScalarArray([10.0, 20.0, 30.0, 40.0, 50.0])
print(f"  Created: {nt_array}")
print(f"  value   : {nt_array.value}")
print(f"  alarm   : {nt_array.alarm}")
print(f"  display : {nt_array.display}")

# Write it to the server
ok = store.put_nt("NT:WAVEFORM", nt_array)
print(f"  put_nt('NT:WAVEFORM', ...) → {ok}")

# Read back
nt = store.get_nt("NT:WAVEFORM")
print(f"  Readback value  : {nt.value}")

print("\n── Constructing helper objects ──")
alarm = spvirit.Alarm(severity=2, status=1, message="HIHI")
print(f"  {alarm}")

ts = spvirit.TimeStamp(seconds_past_epoch=1700000000, nanoseconds=123456789)
print(f"  {ts}")

disp = spvirit.Display(limit_low=-10.0, limit_high=50.0, units="degC", precision=3)
print(f"  {disp}")

ctrl = spvirit.Control(limit_low=0.0, limit_high=100.0, min_step=0.1)
print(f"  {ctrl}")

print("\n" + "=" * 60)
print("  Done.")
print("=" * 60)
