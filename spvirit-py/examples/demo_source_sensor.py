"""demo_source_sensor.py — Background-thread source with live monitor updates.

Shows how a Python source can push *unsolicited* updates to monitor
subscribers.  A worker thread generates sensor readings periodically;
each new reading is published to PVAccess monitor clients via
``notifier.notify(name, NtScalar(value))``.

Run, then subscribe::

    spmonitor SENSOR:TEMP SENSOR:PRESSURE SENSOR:FLOW
"""

import math
import threading
import time

import spvirit


class SensorBackend:
    """Three virtual sensors updated from a background thread."""

    def __init__(self):
        self._values = {
            "SENSOR:TEMP":     22.0,
            "SENSOR:PRESSURE": 1.0,
            "SENSOR:FLOW":     5.0,
        }
        self._lock = threading.Lock()
        self._notifier = None
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._loop, daemon=True)

    # Duck-typed methods ------------------------------------------------

    def on_start(self, notifier):
        """Called by the server right after build(); stash the notifier
        and launch the background update thread."""
        self._notifier = notifier
        self._thread.start()

    def claim(self, name):
        if name in self._values:
            return spvirit.PvInfo.nt_scalar("double")   # read-only sensor
        return None

    def get(self, name):
        with self._lock:
            if name in self._values:
                return spvirit.NtScalar(self._values[name])
        return None

    def put(self, name, value):
        raise RuntimeError(f"'{name}' is read-only")

    def names(self):
        return list(self._values.keys())

    # Background update loop -------------------------------------------

    # ANCHOR: notify
    # A Python source's `subscribe` is ignored: monitor events come from the
    # notifier the server hands to `on_start`. Nothing polls `get` on your
    # behalf, so a source that never notifies looks frozen to a subscriber.
    def _loop(self):
        t = 0.0
        while not self._stop.is_set():
            new = {
                "SENSOR:TEMP":     22.0 + 2.0 * math.sin(t),
                "SENSOR:PRESSURE": 1.0 + 0.1 * math.sin(t * 1.7),
                "SENSOR:FLOW":     5.0 + 1.0 * math.sin(t * 0.4),
            }
            with self._lock:
                self._values.update(new)
            # Publish updates — this is what delivers monitor events to clients.
            if self._notifier is not None:
                for name, v in new.items():
                    self._notifier.notify(name, spvirit.NtScalar(v))
            t += 0.2
            time.sleep(0.5)
    # ANCHOR_END: notify


def main():
    server = (
        spvirit.ServerBuilder()
        .add_source("sensors", 10, SensorBackend())
        .build()
    )
    server.start_background()

    print("Sensor source server on port 5075.")
    print("  PVs: SENSOR:TEMP, SENSOR:PRESSURE, SENSOR:FLOW (updated every 0.5s)")
    print("  Try: spmonitor SENSOR:TEMP")

    try:
        while True:
            time.sleep(3600)
    except KeyboardInterrupt:
        print("\nbye.")


if __name__ == "__main__":
    main()
