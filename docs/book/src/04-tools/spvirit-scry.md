# `spvirit-scry` and `spvirit-scry_influx_collector`

<!-- verify:begin -->
> ✅ **Verified** · no code on this page · [![docs-verify](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml/badge.svg)](https://github.com/ISISNeutronMuon/spvirit/actions/workflows/ci.yml)
>
> The badge reports the whole `docs-verify` suite, not this chapter alone.
<!-- verify:end -->

EPICS PVAccess packet capture, decoding, and real-time traffic monitoring.
These two binaries capture live PVAccess traffic off a network interface and
decode it as it goes past. They track connection state (channels,
operations, PV names) and show it in a TUI dashboard, or export it to
InfluxDB and Loki for production monitoring.

- `spvirit-scry` -- CLI/TUI packet capture with real-time traffic statistics.
- `spvirit-scry_influx_collector` -- packet capture with InfluxDB and Loki export.

They used to ship as the separate `spvirit-scry` crate and now live in
`spvirit-tools`, decoding with the workspace's own codec. The binary names
and flags are unchanged.

## Building them

They need the optional `scry` feature, which is **not** on by default and is
not part of the `pip install spvirit-tools` wheel, because packet capture
links against a system library:

- **Linux**: `libpcap-dev`, `libssl-dev` and `pkg-config` (or your
  distribution's equivalents); the InfluxDB collector's HTTP client links
  OpenSSL.
- **Windows**: the [Npcap](https://npcap.com/) runtime and the
  [Npcap SDK](https://npcap.com/#download). The build script finds the SDK
  automatically (see below), or set `NPCAP_SDK` to the extracted SDK root.

```bash
cargo build -p spvirit-tools --features scry --bins
# or
cargo install spvirit-tools --features scry
```

The build script searches for the Npcap SDK in this order:

1. the `NPCAP_SDK` environment variable (explicit override)
2. `npcap-sdk/` relative to the package root
3. `C:\npcap\`, `C:\npcap-sdk\`, `C:\Program Files\Npcap\` (including versioned subdirs)
4. `%LOCALAPPDATA%\npcap-sdk\`

Capturing needs privileges: `sudo` or `CAP_NET_RAW` on Linux,
administrator rights on Windows.

## Features

- Live packet capture with rate and request-type statistics.
- TUI view for interactive monitoring.
- PVA connection state tracking — channels, operations, PV name resolution.
- Filtering and tracking options (`--exclude-cmds`, `--tracked-only`).
- InfluxDB and Loki export via the collector binary.
- Passive source fingerprinting — infers which process is actually producing a channel's values from wire behaviour alone (`--fingerprint`).

## Requirements

- **Rust** toolchain (edition 2024).
- **Linux**: `libpcap-dev`, `libssl-dev` and `pkg-config` installed and available.
- **Windows**: [Npcap](https://npcap.com/) runtime and the [Npcap SDK](https://npcap.com/#download). The build script auto-detects the SDK, or set `NPCAP_SDK` to the extracted SDK root.
- **Linux**: Packet capture requires either `sudo` or `CAP_NET_RAW` capability (see below).

### Packet capture without sudo (Linux)

After installing, grant the binary the `CAP_NET_RAW` capability so it can capture without `sudo`:

```bash
sudo setcap cap_net_raw=eip $(which spvirit-scry)
sudo setcap cap_net_raw=eip $(which spvirit-scry_influx_collector)
```

This only needs to be done once per install (re-run after `cargo install` upgrades the binary).

## Usage

### `spvirit-scry`

```bash
spvirit-scry --device eth0 --tui
```

Options:
- `--device` / `-i`: network interface to capture from.
- `--integration-time` / `-t`: stats integration period in seconds.
- `--buffer-size` / `-b`: capture buffer size (100–50000).
- `--resolve` / `-r`: resolve IP addresses.
- `--tui`: enable TUI.
- `--debug` / `-d`: debug logging.
- `--version` / `-v`: print version and exit.
- `--exclude-cmds` / `-x`: comma-separated PVA commands to exclude (e.g. `PUT,MONITOR`).
- `--pva-ttl`: PVA channel state TTL in seconds (default 86400).
- `--pva-max-memory-mb`: ceiling on PVA tracker state in MiB, `0` to disable (default 1024).
- `--pva-max-channels`: max PVA channels to track per connection (default 40000).
- `--tracked-only`: only show packets for tracked connections.
- `--pva-max-update-rate`: max update timestamps kept per channel for rate calculation (default 10000).
- `--fingerprint`: enable passive source fingerprinting (off by default). Results appear on the TUI's Fingerprints page (key `5`).
- `--fp-window`: fingerprint window in seconds (default 900).
- `--fp-min-samples`: minimum samples before a fingerprint is emitted (default 30).
- `--fp-reservoir`: per-channel reservoir size per window (default 200).
- `--fp-round-robin-slots`: round-robin channel slots; `N > 1` extends time-to-detection N-fold (default 1).
- `--fp-max-channels`: maximum channels fingerprinted concurrently (default 5000).
- `--fp-d1`: enable D1 source-change detection (off by default, see below).
- `--fp-suppress-secs`: suppress D1 for this many seconds from startup (maintenance window).

### `spvirit-scry_influx_collector`

```bash
spvirit-scry_influx_collector --device eth0 --config-file influxdb_config.toml
```

Options:
- `--device` / `-i`: network interface to capture from.
- `--integration-time` / `-t`: stats integration period in seconds.
- `--buffer-size` / `-b`: capture buffer size (100–50000).
- `--resolve` / `-r`: resolve IP addresses.
- `--debug` / `-d`: debug logging.
- `--version` / `-v`: print version and exit.
- `--config-file` / `-c`: path to InfluxDB config file.
- `--fingerprint`: enable passive source fingerprinting (off by default).
- `--fp-window`: fingerprint window in seconds (default 900).
- `--fp-min-samples`: minimum samples before a fingerprint is emitted (default 30).
- `--fp-reservoir`: per-channel reservoir size per window (default 200).
- `--fp-round-robin-slots`: round-robin channel slots; `N > 1` extends time-to-detection N-fold (default 1).
- `--fp-max-channels`: maximum channels fingerprinted concurrently (default 5000).
- `--fp-d1`: enable D1 source-change detection (off by default, see below).
- `--fp-suppress-secs`: suppress D1 for this many seconds from startup (maintenance window).

## Source fingerprinting

`--fingerprint` derives a per-channel fingerprint from four observables of the
traffic itself, so it holds regardless of what any process, configuration file
or directory service claims:

- **Sub-second timestamp phase** — where in the second a source stamps its
  updates, computed with circular statistics so a source phase-locked near the
  second boundary is not mistaken for an unlocked one. On real ISIS traffic two
  channels sat 434 ms apart with a within-channel spread under 0.2 ms.
- **Value quantisation lattice** — the GCD of *successive differences*, which
  recovers the digitiser LSB (0.0175 °C, 0.01 pH on the reference channels).
- **Update cadence** — modal interval plus p50/p95, a joint fingerprint of the
  scan rate and the deadband policy.
- **Arrival minus stamp** — available only to a wire observer, and the best
  single signal for telling bridge-sourced from device-sourced data.

A fingerprint accumulates per `(channel, server address)` over a rolling window
(default 15 minutes) and is emitted once at least 30 samples are in. Results
appear on the TUI's **Fingerprints** page (key `5`; `v` cycles list, clusters
and the discontinuity log), as a badge column in the Connections detail table,
and from the collector as InfluxDB measurements plus Loki events.

### What is exported

`pv_fingerprint` — one point per channel per closed window. Tags: `host`,
`channel`, `server`, `struct_id` (`unknown` when the channel carries no
normative-type id), `sampling` (`full`, `reservoir` or `round_robin`). Fields:
`phase_mean_ms`, `phase_spread_us`, `phase_circular_var`, `value_lsb`,
`lsb_confidence`, `cadence_modal_s`, `cadence_p50_s`, `cadence_p95_s`,
`arrival_delta_mean_ms`, `arrival_delta_var_ms`, `sample_count`,
`observed_count`, `has_nan_limits`, `struct_id_changed`. `value_lsb` and
`arrival_delta_var_ms` are *omitted* rather than zeroed when they could not be
computed: a lattice of zero and an unknown lattice are different facts.

`pv_fingerprint_coverage` — what the observer actually looked at: channels
tracked, admitted, deferred at capacity, reservoir-sampled and rotated out,
with monotonic totals and saturation flags.

`pv_fingerprint_detect` — everything the detector declined to emit: firings
suppressed by the maintenance window, repeats, incomparable pairs, channels
skipped by the D2 pair cap, populations too small for D4, and events dropped by
the per-ingest cap.

`pv_fingerprint_event` (Loki) — one structured event per rule firing, carrying
both the prior and current fingerprint so the event is self-contained. Rules:
D1 source change, D2 server/fingerprint disagreement, D3 synthesised metadata,
D4 clock-offset outlier.

### What it does not do

These are limitations of the method, not of this implementation, and they do
not go away with more data:

- **A fingerprint is not an identity.** It establishes *sameness* and
  *difference*. It can say two channels share a source, or that a channel's
  source changed; it cannot name an unknown source. Two unrelated IOCs started
  at the same moment with the same scan rate will look alike, so the
  fingerprint is strongest when it says "these differ" and weakest when it says
  "these match". There is no cryptographic assurance: traffic can be shaped to
  mimic a fingerprint.
- **Planned failover is indistinguishable from silent re-sourcing.** A
  legitimate gateway failover produces exactly the D1 signature, and nothing on
  the wire separates the two. This is why D1 is opt-in behind `--fp-d1`, with
  `--fp-suppress-secs` as a startup maintenance window. D2, D3 and D4 run
  whenever fingerprinting is on.
- **Coverage is partial, and is reported rather than assumed.** A passive
  observer only sees the segments it is placed on; on a multi-node swarm with an
  internal gateway mesh one observation point sees a subset of channels. The
  `pv_fingerprint_coverage` measurement says what this observer saw — it cannot
  say what it was never positioned to see.
- **Absence of a fingerprint is not evidence of anything.** A hardware-clocked
  or event-driven source shows no narrow phase; a computed channel (calc
  record, unit conversion, aggregate) may show no lattice, or a lattice that
  reflects the arithmetic rather than the digitiser. The TUI keeps a measured
  phase, `unlocked` (measured but not a signature), `—` (computed, genuinely
  absent) and `…n/m` (still filling the window) as four distinct states for
  exactly that reason.
- **D3 is one-sided today.** PVAccess carries no declared-provenance field, so
  D3 fires on the observable half of the contradiction — synthesised NaN or
  all-zero `valueAlarm`/`display` limits — with nothing to compare a
  declaration against.

### Cost

Fingerprinting adds per-update work to the capture path, which is why it is off
by default. Under load it degrades in three stages and always says which one it
is in: reservoir sampling within a channel, then a round-robin subset of
channels, with channels that have no fingerprint yet always admitted. The
regime is recorded on every exported fingerprint, so a partial view is never
presented as a complete one.

## Configuration

The collector reads a TOML configuration file. Required keys:

```toml
url = "http://localhost:8086"
token = "your-token"
org = "your-org"
bucket = "your-bucket"
loki_url = "http://localhost:3100"
```

> **Note:** Do not commit secrets (tokens) to the repository. Use environment-specific secret management.

## Troubleshooting

- **No packets captured**: verify the device name (`--device`) and ensure capture permissions (`sudo` or NIC capabilities).
- **InfluxDB errors**: verify `url`, `token`, `org`, and `bucket` in the config file and confirm network connectivity.
- **Buffer size errors**: `--buffer-size` must be between 100 and 50000.
- **Diagnostics**: use `--version` to confirm build version, `--debug` for verbose logging.
