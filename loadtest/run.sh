#!/usr/bin/env sh
# One-shot driver: generate the .db, build the HEAD image, bring the network
# up. Ctrl-C to stop. Results land in ./out/fd.csv; watch the gateway with
#   docker compose -f loadtest/docker-compose.yml logs -f gateway
set -eu

ROOT=$(git rev-parse --show-toplevel)
cd "$ROOT/loadtest"

: "${NUM_PVS:=2000}"
: "${PASSIVE_FRACTION:=0.9}"

echo "run.sh: generating pvs.db (NUM_PVS=$NUM_PVS PASSIVE_FRACTION=$PASSIVE_FRACTION)"
# Prefer python3 (Debian/Ubuntu/WSL have no bare `python`); fall back to python.
PY=$(command -v python3 || command -v python)
NUM_PVS="$NUM_PVS" PASSIVE_FRACTION="$PASSIVE_FRACTION" "$PY" gen_db.py > pvs.db

sh ./build.sh

mkdir -p out
echo "run.sh: starting network (loadgen image builds on first run)"
NUM_PVS="$NUM_PVS" PASSIVE_FRACTION="$PASSIVE_FRACTION" docker compose up --build
