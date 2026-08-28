#!/bin/sh
# Gateway container entrypoint: run spgateway AND sample its open-fd count
# once a second into /out/fd.csv. The sampler is the source of truth for the
# fd-exhaustion investigation:
#   - climbing monotonically under churn  => monitor-linger leak (lever B)
#   - high but plateauing                 => bounded per-PV fan-out (lever A)
#
# spgateway runs in the background so we can read /proc/<pid>/fd; its
# stdout/stderr still go to the container log (docker compose logs gateway).
# When it exits (e.g. the original accept-loop death on EMFILE, if this image
# was built from committed HEAD), the sampler loop ends and the container
# stops.
set -eu

CONFIG="${GW_CONFIG:-/cfg/gateway.json}"
OUT="${GW_FD_CSV:-/out/fd.csv}"

mkdir -p "$(dirname "$OUT")"

spgateway "$CONFIG" &
GW=$!

echo "ts,fd_count" > "$OUT"
echo "gw-entrypoint: spgateway pid=$GW, sampling fds -> $OUT" >&2

while kill -0 "$GW" 2>/dev/null; do
    n=$(ls "/proc/$GW/fd" 2>/dev/null | wc -l | tr -d ' ')
    echo "$(date +%s),${n:-0}" >> "$OUT"
    sleep 1
done

wait "$GW" || true
echo "gw-entrypoint: spgateway (pid=$GW) exited; fd samples in $OUT" >&2
