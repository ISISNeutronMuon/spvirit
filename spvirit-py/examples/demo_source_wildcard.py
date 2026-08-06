"""demo_source_wildcard.py — Dynamic prefix/wildcard source.

A Python `Source` that claims any PV whose name starts with `SCRATCH:`
and behaves like a scratch-pad: GETs return the last value stored,
PUTs record the new value and publish a monitor update.  Unknown PVs
get a default of 0.0 the first time they are read.

Run this script, then try::

    python -m spvirit.tools.spget SCRATCH:FOO
    python -m spvirit.tools.spput SCRATCH:FOO 42.0
    python -m spvirit.tools.spmonitor SCRATCH:BAR &
    python -m spvirit.tools.spput SCRATCH:BAR 1.5
"""

import threading
import time

import spvirit

PREFIX = "SCRATCH:"


# ANCHOR: claim
class WildcardSource:
    """Accept any PV under PREFIX and keep a per-name float value."""

    def __init__(self) -> None:
        self._values: dict[str, float] = {}
        self._notifier: spvirit.Notifier | None = None
        self._lock = threading.Lock()

    # Called by the server right after build() — stash the notifier.
    def on_start(self, notifier: spvirit.Notifier) -> None:
        self._notifier = notifier

    def claim(self, name: str):
        if not name.startswith(PREFIX):
            return None
        return spvirit.PvInfo.nt_scalar("double", writable=True)

    def get(self, name: str):
        if not name.startswith(PREFIX):
            return None
        with self._lock:
            val = self._values.setdefault(name, 0.0)
        return spvirit.NtScalar(val)

    def put(self, name: str, value):
        """value is a Python dict/value built from the PUT payload."""
        if not name.startswith(PREFIX):
            return None
        new_val = _coerce_float(value)
        with self._lock:
            self._values[name] = new_val
        # Publish the update to PVA monitor subscribers.
        if self._notifier is not None:
            self._notifier.notify(name, spvirit.NtScalar(new_val))
        # Return propagation list (only this PV changed).
        return spvirit.NtScalar(new_val)

    def names(self):
        # Report only the names we've seen so far. A dynamic namespace
        # cannot enumerate what does not exist yet; the PVs still serve.
        with self._lock:
            return list(self._values.keys())
# ANCHOR_END: claim


def _coerce_float(v) -> float:
    if isinstance(v, (int, float)):
        return float(v)
    if isinstance(v, dict):
        for key in ("value", "val"):
            if key in v:
                return _coerce_float(v[key])
    if isinstance(v, list) and v:
        return _coerce_float(v[0])
    raise TypeError(f"cannot coerce {v!r} to float")


def main() -> None:
    server = (
        spvirit.ServerBuilder()
        .ai("BUILTIN:HEARTBEAT", 0.0)      # regular PV at order 0
        .add_source("wildcard", 10, WildcardSource())
        .build()
    )

    store = server.start_background()

    print("Wildcard source server running on port 5075.")
    print(f"  Any PV under {PREFIX}* is served dynamically.")
    print("  Try: spput SCRATCH:FOO 42.0  then  spget SCRATCH:FOO")
    print("  Ctrl-C to exit.")

    # Heartbeat so pvmonitor on built-in PVs sees something moving.
    tick = 0
    try:
        while True:
            store.set_value("BUILTIN:HEARTBEAT", float(tick))
            tick += 1
            time.sleep(1)
    except KeyboardInterrupt:
        print("\nbye.")


if __name__ == "__main__":
    main()
