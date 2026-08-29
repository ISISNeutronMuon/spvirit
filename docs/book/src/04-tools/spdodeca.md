# `spdodeca`

<!-- verify:begin -->
> ✅ **Verified** · no code on this page · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

Serves a rotating dodecahedron wireframe as an `NtNdArray` image PV. A
demo, and a genuinely useful one: it is a moving image on the network with
no camera, no driver, and no configuration.

```
spdodeca [OPTIONS]
```

Requires the `server` feature.

| Flag | Default | Meaning |
|---|---|---|
| `--pv NAME` | `DODECA:IMAGE` | PV name to serve |
| `--width N` | 256 | image width in pixels |
| `--height N` | 256 | image height in pixels |
| `--rate HZ` | 10 | frame update rate |
| `--tcp-port PORT` | 5075 | TCP server port |
| `--udp-port PORT` | 5076 | UDP search port |
| `--listen-addr ADDR` | `0.0.0.0` | listen address |
| `--conn-timeout SECS` | 60 | idle connection timeout |
| `--debug` | off | verbose logging |

## Running it

```bash
spdodeca --width 512 --height 512 --rate 25
```

Then point any NTNDArray-capable viewer at `DODECA:IMAGE`. `spget` will
confirm it is there, though it prints the pixels rather than the picture:

```console
$ spget DODECA:IMAGE
DODECA:IMAGE 2026-08-04 10:46:46.086 {ubyteValue=[0, 0, 0, 0, 0, ...]}
```

`ubyteValue` names the populated arm of NTNDArray's union-typed `value`
field — the frame is 8-bit greyscale. `spinfo DODECA:IMAGE` prints the
whole union and the `dimension` list alongside it.

## Controlling the rotation

Alongside the image, the server serves three writable `double` scalar PVs
that set the spin rate about each axis, in rad/s:

| PV | Default | Axis |
|---|---|---|
| `DODECA:SPEED_X` | 0.3 | pitch |
| `DODECA:SPEED_Y` | 0.5 | yaw |
| `DODECA:SPEED_Z` | 0.15 | roll |

They share the image PV's prefix — the part before the first `:` — so
`--pv RIG:CAM` serves `RIG:SPEED_X` and so on, not `DODECA:SPEED_X`. Put
to one and the rotation changes live; monitor one and you see writes from
other clients echoed back.

```console
$ spput DODECA:SPEED_Y 2.0     # spin faster about the vertical axis
DODECA:SPEED_Y OK
$ spput DODECA:SPEED_X 0        # stop the pitch entirely
DODECA:SPEED_X OK
```

Setting all three to `0` freezes the wireframe — a still test image
rather than a moving one. The image PV itself is read-only; a PUT to it is
rejected.

## What it is for

**Testing an image client.** Area-detector viewers are hard to develop
against without a detector. This gives you a deterministic, always-running
one.

**Load-testing the wire.** A 512×512 frame at 25 Hz is about 6 MB/s of
PVAccess traffic through a single monitor — enough to expose buffering
problems in a client.

**Checking the NTNDArray encoding.** It exercises the union-typed `value`
field and the dimension list, which is the part of the type most likely to
be implemented differently at the other end. See
[Tables and images](../03-progressive/tables-and-images.md).
