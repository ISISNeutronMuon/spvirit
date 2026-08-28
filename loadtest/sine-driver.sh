#!/bin/sh
# Drive the ACTIVE PVs with continuous sine updates using the real `spsine`
# client (one process per PV), writing straight to a single backend via
# --server (no search, deterministic target).
#
# Purpose: the *control* for the monitor-linger leak. A PV that keeps updating
# lets the gateway retire an upstream monitor promptly once its subscribers
# leave (the retirement fires on the next update). Run two of these -- one per
# backend -- so whichever backend the gateway picks for a given PV is hot.
#
# Env:
#   SINE_TARGET     backend "ip:port" to write to        (required, e.g. 172.30.0.11:5075)
#   NUM_PVS         total PVs (matches gen_db)            [1000]
#   PASSIVE_FRACTION quiet fraction (matches gen_db)      [0.9]
#   PV_PREFIX       name prefix                           [LOAD]
#   SINE_RATE       updates/sec per PV                    [10]
#   SINE_FREQ       sine frequency (Hz)                   [0.2]
#   MAX_SINE        cap on number of PVs driven           [0 = all active]
set -eu

: "${SINE_TARGET:?set SINE_TARGET to a backend ip:port}"
: "${NUM_PVS:=1000}"
: "${PASSIVE_FRACTION:=0.9}"
: "${PV_PREFIX:=LOAD}"
: "${SINE_RATE:=10}"
: "${SINE_FREQ:=0.2}"
: "${MAX_SINE:=0}"

# n_passive = int(NUM_PVS * PASSIVE_FRACTION); active PVs are [n_passive, NUM_PVS)
np=$(awk "BEGIN{printf \"%d\", $NUM_PVS * $PASSIVE_FRACTION}")

hi="$NUM_PVS"
if [ "$MAX_SINE" -gt 0 ]; then
    cap=$((np + MAX_SINE))
    [ "$cap" -lt "$hi" ] && hi="$cap"
fi

echo "sine-driver: target=$SINE_TARGET driving PV[$np..$((hi-1))] rate=${SINE_RATE}Hz freq=${SINE_FREQ}Hz" >&2

i="$np"
while [ "$i" -lt "$hi" ]; do
    pv=$(printf "%s:PV%05d" "$PV_PREFIX" "$i")
    spsine "$pv" --server "$SINE_TARGET" --rate "$SINE_RATE" --freq "$SINE_FREQ" \
        >/dev/null 2>&1 &
    i=$((i + 1))
done

echo "sine-driver: launched $((hi - np)) spsine processes -> $SINE_TARGET" >&2
wait
