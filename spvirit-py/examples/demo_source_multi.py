"""demo_source_multi.py — Several sources layered by priority.

Demonstrates ``order`` (priority) for source resolution.  Lower order
is tried first, higher order is tried last.  The built-in store is
always at order 0.

Layout:

  order  0    built-in store  (BLT:*)
  order 10    FastCacheSource (FAST:*)           — in-memory cache
  order 20    FallbackSource  (any name)          — returns 0 for everything

Since ``FallbackSource`` claims every PV name, it's kept at the lowest
priority so the specific sources can intercept first.  Try::

    spget BLT:X      # -> from built-in store
    spget FAST:Q     # -> from FastCacheSource
    spget RANDOM:W   # -> from FallbackSource (0.0)
"""

import threading
import time

import spvirit


class FastCache:
    PREFIX = "FAST:"

    def __init__(self):
        self._v = {}
        self._lock = threading.Lock()
        self._n = None

    def on_start(self, notifier):
        self._n = notifier

    def claim(self, name):
        if name.startswith(self.PREFIX):
            return spvirit.PvInfo.nt_scalar("double", writable=True)
        return None

    def get(self, name):
        if name.startswith(self.PREFIX):
            with self._lock:
                return spvirit.NtScalar(self._v.get(name, 0.0))
        return None

    def put(self, name, value):
        if not name.startswith(self.PREFIX):
            return None
        v = _f(value)
        with self._lock:
            self._v[name] = v
        if self._n is not None:
            self._n.notify(name, spvirit.NtScalar(v))
        return spvirit.NtScalar(v)

    def names(self):
        with self._lock:
            return list(self._v.keys())


class Fallback:
    """Catch-all source: always returns 0.0 for any name."""

    def claim(self, name):
        return spvirit.PvInfo.nt_scalar("double")

    def get(self, name):
        return spvirit.NtScalar(0.0)

    def put(self, name, value):
        raise RuntimeError("fallback is read-only")

    def names(self):
        return []  # dynamic — we claim any name, don't enumerate


def _f(v):
    if isinstance(v, (int, float)):
        return float(v)
    if isinstance(v, dict):
        for key in ("value", "val"):
            if key in v:
                return _f(v[key])
    return 0.0


def main():
    # ANCHOR: register
    # Lower order is asked first; the built-in record store sits at 0.
    # Fallback claims every name, so it goes last.
    server = (
        spvirit.ServerBuilder()
        .ai("BLT:X", 3.14)                        # built-in store, order 0
        .add_source("fast", 10, FastCache())
        .add_source("fallback", 100, Fallback())
        .build()
    )
    # ANCHOR_END: register
    server.start_background()

    print("Multi-source server on port 5075.")
    print("  BLT:X        -> built-in store (order 0)")
    print("  FAST:*       -> FastCache      (order 10)")
    print("  ANYTHING     -> Fallback 0.0   (order 100)")

    try:
        while True:
            time.sleep(3600)
    except KeyboardInterrupt:
        print("\nbye.")


if __name__ == "__main__":
    main()
