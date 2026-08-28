# spvirit gateway load-test network

A small Docker PVA network for load-testing the gateway and reproducing /
observing its file-descriptor exhaustion behavior.

```
loadgen (p4p)  --downstream-->  gateway (spgateway)  --upstream-->  backend1 + backend2 (spserver)
   172.30.0.20                   172.30.0.10                         172.30.0.11 / .12
```

- **backends** — two `spserver` instances serving PVs from a generated
  `pvs.db` (mostly *quiet* `ai` records, a few *active* `ao` records).
- **gateway** — `spgateway` proxying both backends downstream, with a
  deliberately low `nofile` ulimit (default **512**) so EMFILE reproduces in
  seconds. Prometheus metrics are on at `:9090/metrics`.
- **loadgen** — a real pvxs/`p4p` client that opens many monitors and churns
  them (subscribe → hold → unsubscribe → repeat).

## Quick start

From the repo root (Git Bash / any POSIX shell with Docker):

```sh
sh loadtest/run.sh
```

That generates `pvs.db`, builds the image **from committed git HEAD**, and
starts the network. In another terminal:

```sh
# gateway logs (fd errors / accept-loop behavior / death)
docker compose -f loadtest/docker-compose.yml logs -f gateway

# the fd count over time
cat loadtest/out/fd.csv
```

Stop with Ctrl-C, then `docker compose -f loadtest/docker-compose.yml down`.

## Reading the result

`out/fd.csv` (`ts,fd_count`, one sample/sec) is the source of truth:

- **Climbs monotonically under churn** → the upstream **monitor-linger leak**
  (a closed monitor on a quiet PV keeps its upstream socket until the PV's
  next update, which never comes). This is "lever B".
- **High but plateaus** → **bounded per-PV fan-out** (one upstream TCP socket
  per monitored PV; N clients × M monitors ≈ N·M sockets) plus a low ulimit.
  This is "lever A".

Gateway logs show what the accept loop does when fds run out:

- Built from **committed HEAD** (default): the accept loop dies on EMFILE and
  the gateway process exits — the original production failure.
- Built from the **working tree** *with the accept-loop fix*: repeated
  `TCP accept error (continuing)` lines and the server stays up. To build that
  way instead of HEAD:
  ```sh
  docker build -f loadtest/Dockerfile.spvirit -t spvirit-loadtest:head .   # from repo root
  docker compose -f loadtest/docker-compose.yml up   # skip run.sh's build step
  ```

## Tuning the load

Env vars (defaults in `docker-compose.yml`), e.g.:

```sh
GW_NOFILE=1024 NUM_CLIENTS=10 MONITORS_PER_CLIENT=300 CHURN_PERIOD=3 \
  docker compose -f loadtest/docker-compose.yml up
```

| Var | Meaning | Default |
|-----|---------|---------|
| `GW_NOFILE` | gateway open-file limit (soft=hard) | 512 |
| `NUM_PVS` | total PVs in `pvs.db` | 2000 |
| `PASSIVE_FRACTION` | fraction that are quiet `ai` | 0.9 |
| `NUM_CLIENTS` | concurrent downstream clients | 5 |
| `MONITORS_PER_CLIENT` | monitors each holds per cycle | 200 |
| `CHURN_PERIOD` | seconds a monitor set is held | 5 |
| `RAMP` | seconds between client starts | 2 |
| `DRIVE_HOT` | `1` = also put to active PVs (contrast case) | 0 |

> If you change `NUM_PVS` / `PASSIVE_FRACTION`, regenerate `pvs.db`
> (`run.sh` does this) so the loadgen's PV names match the backends'.

## Notes / limitations

- The image only contains `spgateway` and `spserver`; config, scripts, and the
  generated `pvs.db` are bind-mounted from this directory, and `out/` is where
  the fd CSV is written.
- `pvs.db` and `out/` are git-ignored (generated/runtime artifacts).
- Requires Docker with `docker compose` v2. Static IPs are on a dedicated
  `172.30.0.0/16` bridge; change the subnet in `docker-compose.yml` and
  `gateway.json` together if it collides with an existing network.
