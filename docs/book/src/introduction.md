# Spvirit

Spvirit is a pure-Rust implementation of the EPICS **PVAccess** protocol —
client, server, wire codec, command-line tools, and Python bindings — with no
dependency on an EPICS base installation.

This site teaches it from the beginning. If you have never met EPICS, start at
[EPICS in 10 minutes](01-fundamentals/epics-in-10-minutes.md). If you know
EPICS and want a soft IOC running, jump to
[Your first PV](02-getting-started/first-pv.md).

## How the site is laid out

- **[Fundamentals](01-fundamentals/what-is-spvirit.md)** — what EPICS is, what
  Normative Types are, and the one distinction worth understanding before you
  write anything: [records vs raw NT](01-fundamentals/records-vs-raw-nt.md).
- **[Getting started](02-getting-started/install.md)** — install, then a PV
  served and read in under a page.
- **[Progressive examples](03-progressive/scalars.md)** — twelve chapters,
  Rust and Python side by side, ending in
  [a complete IOC](03-progressive/complete-ioc.md).
- **[Command-line tools](04-tools/index.md)** — a page per `sp*` binary, with
  real captured output.
- **[Reference](05-reference/crate-map.md)** — the crate map, the
  [record-type matrix](05-reference/record-types.md),
  [troubleshooting](05-reference/troubleshooting.md), and the
  [known gaps](05-reference/known-gaps.md).
- **[Developer guide](06-dev-guide/README.md)** — internals, with
  file-and-line citations, for people changing spvirit itself.

Every code sample on this site is included verbatim from a file in the
repository that is compiled by CI. The badge at the top of each chapter links
to that source and to the test that checks it.
