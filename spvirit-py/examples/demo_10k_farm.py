"""A 10,000-PV server built in a comprehension, with timing.

Builds 10,000 ``ai`` channels plus 100 ``calc`` PVs (each averaging 100
channels), serves them all, then times a full 10,000-``set()`` sweep and a
few client round-trips.

    python demo_10k_farm.py
"""
import time

import spvirit

TCP, UDP = 15345, 15346
N_CHANNELS = 10_000
GROUP = 100


def main() -> None:
    t0 = time.perf_counter()
    channels = [
        spvirit.ai(f"FARM:CH{i:05d}", 0.0, units="counts", mdel=0.5)
        for i in range(N_CHANNELS)
    ]
    averages = [
        spvirit.calc(
            f"FARM:AVG{g:03d}",
            channels[g * GROUP:(g + 1) * GROUP],
            lambda vals: sum(vals) / len(vals),
        )
        for g in range(N_CHANNELS // GROUP)
    ]
    t1 = time.perf_counter()
    print(f"created {N_CHANNELS} channels + {len(averages)} calc PVs "
          f"in {t1 - t0:.2f} s")

    server = spvirit.Server(pvs=channels + averages, port=TCP, udp_port=UDP,
                            listen_ip="127.0.0.1")
    server.start()
    t2 = time.perf_counter()
    print(f"server built and serving {N_CHANNELS + len(averages)} PVs "
          f"in {t2 - t1:.2f} s")

    t3 = time.perf_counter()
    for i, ch in enumerate(channels):
        ch.set(float(i))
    t4 = time.perf_counter()
    rate = N_CHANNELS / (t4 - t3)
    print(f"swept set() over all {N_CHANNELS} channels in {t4 - t3:.2f} s "
          f"({rate:,.0f} writes/s)")

    time.sleep(0.3)  # let calc links settle
    client = (spvirit.Client.builder()
              .server_addr(f"127.0.0.1:{TCP}").udp_port(UDP).build())
    t5 = time.perf_counter()
    v_ch = client.get("FARM:CH04999").value["value"]
    v_avg = client.get("FARM:AVG049").value["value"]
    t6 = time.perf_counter()
    print(f"client round-trips: FARM:CH04999={v_ch}  FARM:AVG049={v_avg}  "
          f"({(t6 - t5) * 500:.1f} ms/get)")


if __name__ == "__main__":
    main()
