# `sptable`

<!-- verify:begin -->
> ✅ **Verified** · no code on this page · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

An interactive spreadsheet that *is* an IOC. Each row is a PV; adding a
row serves it immediately. It is the fastest way to put a realistic set of
PVs on the network without writing a `.db` file or a line of code.

```
sptable [OPTIONS]
```

Requires the `server` and `tui` features.

| Flag | Default | Meaning |
|---|---|---|
| `--port PORT` | 5075 | TCP port |
| `--udp-port PORT` | 5076 | UDP search port |
| `--rate HZ` | 10 | animation tick rate |

## Two ways to drive it

Single keys operate on the selected row; `:` opens a command line for
everything else.

| Key | Action |
|---|---|
| `a` | add a PV through a guided prompt |
| `e` or `Enter` | edit the selected row's value |
| `d` | delete the selected row |
| `j` / `k` or `↑` / `↓` | move the selection |
| `:` | open the command line |
| `?` | scrollable help modal |
| `q` or `Esc` | quit |

While you type a `:` command the footer shows a live hint for that verb —
and for `:anim`, once the generator name is present, it switches to that
generator's own parameters and defaults.

## Commands

```text
add|a  <name> <type> [ro|rw] <value>   add PV(s)
set|s  <name> <value>                  set value (choice name or index for enum)
del|d  [name]                          delete (blank = selected row)
rename|mv <old> <new>                  rename (scalar/enum)
ro|rw  <name>                          set advertised access
anim   <name> <gen> [k=v ...]          animate
stop   [name]                          stop animation (blank = selected)
source|so <file>                       run a script of commands
write|w  <file>                        dump the session as a loadable script
rate   <hz>                            retune the animation tick (live)
help|h    quit|q
```

## Types

```text
bool int8 int16 int32(int) int64(long) uint8 uint16 uint32 uint64
float(f32) double(f64) string(s)  ;  arrays: int32[]  ;  enum  ;  table
```

Value forms for the compound types:

```text
enum   ->  OFF,ON,TRIP 1
table  ->  id:i32=1,2,3 x:f64=0.5,1.5
```

## Patterns

Any command that takes a name takes a **bash-style brace pattern**, and
acts on every expansion:

```text
{1..8}   {8..1}   {0..100..10}   {01..12}   {A,B,C}
```

Multiple braces form a cartesian product — `S{1..4}:{A,B}` is eight PVs.
Zero-padding is inferred from the width of the bounds, so `{01..12}`
produces `01`, `02`, … A pattern that would create more than 1000 PVs is
rejected rather than expanded
(`spvirit-tools/src/bin/spvirit_table/pattern.rs:4`) — a typo like
`{1..1000000}` fails instead of hanging the machine.

```text
:add RING:BPM{01..99}:X f64 rw 0
```

## Animation

`:anim <name> <gen> [k=v ...]` drives a row from a generator sampled at
the global tick rate.

| Generator | Parameters and defaults |
|---|---|
| `sine` | `amp=1 offset=0 period=10 phase=0` |
| `ramp` | `min=0 max=1 period=10` |
| `triangle` | `min=0 max=1 period=10` |
| `square` | `lo=0 hi=1 period=10 duty=0.5` |
| `noise` | `min=0 max=1` |
| `walk` | `start=0 step=1 min=0 max=1` |
| `count` | `start=0 step=1` |
| `cycle` | `period=1` (enum only) |

```text
:anim RING:BPM{01..99} noise min=-1 max=1
:rate 50
```

`:rate` retunes every animation live — it is the tick rate, not a
per-generator frequency. Use `period` for the shape of one waveform.

**Inapplicable parameters are an error, not a no-op.** `:anim X sine min=-2`
is rejected, because sine's range comes from `amp`/`offset` and a silently
ignored `min` would look like it worked
(`spvirit-tools/src/bin/spvirit_table/anim.rs:138`).

## Sessions

`:write <file>` dumps the whole session — rate, every row with its current
value and access, and every animation with its resolved parameters — as a
script of the same commands. `:source <file>` replays it. The dump
round-trips: what you write is what you can load.

```text
# sptable session dump
rate 10
add RING:BPM01:X double rw 0.42
anim RING:BPM01:X noise min=-1 max=1
```

Animations are reconstructed from *resolved* parameters, so a session
written after `:rate 50` and edited by hand still loads exactly.

## Gotchas

**`:add` skips names that already exist** rather than overwriting, and
reports how many it skipped. Bulk-adding an overlapping pattern is safe.

**A table row cannot be `:set`.** The error is explicit — recreate it with
`:add`. Tables are a payload type, not a record with a value field; see
[Tables and images](../03-progressive/tables-and-images.md).

**Scalar values are coerced, not rejected.** Setting `3.7` on an `int32`
row rounds and clamps to the type's range
(`spvirit-tools/src/bin/spvirit_table/parse.rs:78`). The row's type wins.
