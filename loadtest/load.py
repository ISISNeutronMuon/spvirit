#!/usr/bin/env python3
"""Downstream load generator: real p4p (pvxs) client driving the gateway.

Each simulated client runs on its own thread with its own p4p Context (one
downstream TCP connection to the gateway, multiplexing all its channels). It
repeatedly:

  1. opens MONITORS_PER_CLIENT monitors on random *quiet* PVs,
  2. holds them for CHURN_PERIOD seconds,
  3. closes them (unsubscribe),
  4. pauses briefly, then repeats.

Why this shape: closing a monitor on a quiet PV leaves the gateway's upstream
monitor task alive (it only retires on the PV's next update, which never
comes). So each churn cycle strands more upstream fds on the gateway. Combine
with a low `nofile` ulimit on the gateway container and it hits EMFILE fast.

The fan-out alone is also enough to exhaust fds: N clients x M monitors means
N*M upstream TCP sockets on the gateway (one per monitored PV -- the gateway
does not multiplex many PVs over one upstream connection).

Set DRIVE_HOT=1 to also spin a thread that puts to the active PVs once a
second; those upstream monitors DO get retired promptly after their
subscribers leave -- the contrast case.

Config via env (defaults in brackets):
  NUM_CLIENTS          concurrent clients            [5]
  MONITORS_PER_CLIENT  monitors each holds           [200]
  CHURN_PERIOD         seconds to hold a monitor set [5]
  RAMP                 seconds between client starts  [2]
  NUM_PVS              total PVs (matches gen_db)    [2000]
  PASSIVE_FRACTION     quiet fraction (matches gen_db)[0.9]
  PV_PREFIX            name prefix                  [LOAD]
  DRIVE_HOT            1 = also put to active PVs     [0]
  CHURN_POOL           which PVs to churn: passive|active [passive]

CHURN_POOL selects the contrast:
  passive (default) -- churn monitors on QUIET PVs. Each closed monitor
    strands its upstream gateway socket (retired only on the PV's next
    update, which never comes) => gateway fds climb without bound (leak).
  active -- churn monitors on PVs driven by spsine (see sine-driver.sh).
    Those keep updating, so the gateway retires each monitor promptly on
    the next update after its subscribers leave => gateway fds plateau.
This is the A/B that isolates the monitor-linger leak to quiet PVs.
"""
import os
import random
import threading
import time

from p4p.client.thread import Context

NUM_CLIENTS = int(os.environ.get("NUM_CLIENTS", "5"))
MONITORS_PER_CLIENT = int(os.environ.get("MONITORS_PER_CLIENT", "200"))
CHURN_PERIOD = float(os.environ.get("CHURN_PERIOD", "5"))
RAMP = float(os.environ.get("RAMP", "2"))
NUM_PVS = int(os.environ.get("NUM_PVS", "2000"))
PASSIVE_FRACTION = float(os.environ.get("PASSIVE_FRACTION", "0.9"))
PV_PREFIX = os.environ.get("PV_PREFIX", "LOAD")
DRIVE_HOT = os.environ.get("DRIVE_HOT", "0") == "1"
CHURN_POOL = os.environ.get("CHURN_POOL", "passive").lower()

n_passive = int(NUM_PVS * PASSIVE_FRACTION)
PASSIVE = [f"{PV_PREFIX}:PV{i:05d}" for i in range(n_passive)]
ACTIVE = [f"{PV_PREFIX}:PV{i:05d}" for i in range(n_passive, NUM_PVS)]

# The pool each client churns monitors over (see CHURN_POOL docs above).
POOL = ACTIVE if CHURN_POOL == "active" else PASSIVE


def _noop(_value):
    # We don't care about the data, only about holding the subscription open.
    pass


def client_worker(cid):
    ctx = Context("pva")
    rnd = random.Random(cid)
    cycle = 0
    k = min(MONITORS_PER_CLIENT, len(POOL))
    while True:
        names = rnd.sample(POOL, k)
        subs = [ctx.monitor(nm, _noop, notify_disconnect=True) for nm in names]
        print(f"[client {cid}] cycle {cycle}: opened {len(subs)} monitors",
              flush=True)
        time.sleep(CHURN_PERIOD)
        for s in subs:
            s.close()
        print(f"[client {cid}] cycle {cycle}: closed {len(subs)} monitors",
              flush=True)
        cycle += 1
        time.sleep(1)


def hot_driver():
    ctx = Context("pva")
    v = 0
    while True:
        for nm in ACTIVE:
            try:
                ctx.put(nm, float(v))
            except Exception:
                pass
        v += 1
        time.sleep(1)


def main():
    print(f"loadgen: {NUM_CLIENTS} clients x {MONITORS_PER_CLIENT} monitors, "
          f"churn={CHURN_PERIOD}s ramp={RAMP}s, "
          f"{len(PASSIVE)} quiet / {len(ACTIVE)} active PVs, "
          f"CHURN_POOL={CHURN_POOL} ({len(POOL)} PVs), "
          f"DRIVE_HOT={DRIVE_HOT}", flush=True)

    if DRIVE_HOT and ACTIVE:
        threading.Thread(target=hot_driver, daemon=True).start()

    threads = []
    for cid in range(NUM_CLIENTS):
        t = threading.Thread(target=client_worker, args=(cid,), daemon=True)
        t.start()
        threads.append(t)
        time.sleep(RAMP)

    for t in threads:
        t.join()


if __name__ == "__main__":
    main()
