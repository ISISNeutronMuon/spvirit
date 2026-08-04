# `spget_compare`

<!-- verify:begin -->
> ✅ **Verified** · no code on this page · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

Replay a captured PVAccess GET exchange and check spvirit's encoders
against it byte for byte. A protocol-debugging tool, not something you
reach for day to day.

```
spget_compare [OPTIONS]
```

Requires the `client` feature. It talks to no network — it reads a file.

## Flags

| Flag | Meaning |
|---|---|
| `--dump-file PATH` | hex dump, human-readable |
| `--dump-raw PATH` | binary dump, `u32` little-endian length prefix per frame |

One of the two is required:

```console
$ spget_compare
Provide --dump-raw or --dump-file
```

## The hex dump format

A `--dump-file` is what you get from copying frames out of a packet
capture. Direction markers separate frames; blank lines end them; an
optional four-character offset column is stripped:

```text
C->S
0000  ca 02 00 00 08 00 00 00  01 00 00 00 ...
0010  00 00 00 00

S->C
0000  ca 02 40 01 ...
```

Anything that is not a two-character hex byte is ignored, so annotated
captures usually paste in unchanged
(`spvirit-tools/src/bin/spvirit_get_compare.rs:265`).

`--dump-raw` has no direction markers. The tool infers direction from the
server bit in each frame's command flags
(`spvirit-tools/src/bin/spvirit_get_compare.rs:256`).

## What it checks

It picks the first connection-validation, create-channel, GET init, and
GET data frame out of the capture, re-encodes each with spvirit's own
encoder, and compares. One line per frame, in one of two shapes
(`spvirit-tools/src/bin/spvirit_get_compare.rs:160`):

```text
<LABEL>: OK (len=<n>)
<LABEL>: MISMATCH at offset <i> (actual len=<n>, expected len=<m>)
  actual:   <hh>  expected: <hh>
```

The offset of the first differing byte is usually enough to identify the
field. This is how a wire-compatibility bug against EPICS Base gets
localised: capture the exchange from a working `pvget`, replay it here,
and read off where spvirit diverges.
