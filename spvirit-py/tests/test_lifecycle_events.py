"""Lifecycle hooks and server-wide events. Run directly:
   ./.venv/Scripts/python.exe tests/test_lifecycle_events.py
"""
import time

import spvirit


def _wait_for(predicate, timeout=5.0, interval=0.02):
    """Poll `predicate()` until truthy or raise after `timeout` seconds.

    `start_background()` only guarantees the server thread has been
    spawned, not that its on_start hooks have run yet — hook execution is
    asynchronous relative to the Python call that kicked it off. Polling
    with a bounded timeout keeps this deterministic without ever hanging.
    """
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(interval)
    raise AssertionError(f"condition not met within {timeout}s")


class RecordingSource:
    """A minimal Python source that records when on_start fired."""

    def __init__(self, log):
        self.log = log

    def claim(self, name):
        if name == "PY:SRC":
            return spvirit.PvInfo.nt_scalar("double", writable=False)
        return None

    def get(self, name):
        return spvirit.NtScalar(1.0) if name == "PY:SRC" else None

    def put(self, name, value):
        return None

    def names(self):
        return ["PY:SRC"]

    def on_start(self, notifier):
        self.log.append("source-on-start")


def test_source_on_start_does_not_fire_at_build():
    log = []
    src = RecordingSource(log)
    server = (
        spvirit.ServerBuilder()
        .ai("LCE:A", 1.0)
        .port(0)
        .udp_port(0)
        .add_source("rec", -5, src)
        .build()
    )
    assert log == [], f"on_start must not fire during build(), got {log}"


def test_source_on_start_fires_at_server_start():
    log = []
    src = RecordingSource(log)
    server = (
        spvirit.ServerBuilder()
        .ai("LCE:B", 1.0)
        .port(0)
        .udp_port(0)
        .add_source("rec", -5, src)
        .build()
    )
    server.start_background()
    _wait_for(lambda: log == ["source-on-start"])
    assert log == ["source-on-start"], f"on_start must fire at start, got {log}"


def test_add_source_after_start_is_rejected():
    log = []
    server = (
        spvirit.ServerBuilder()
        .ai("LCE:C", 1.0)
        .port(0)
        .udp_port(0)
        .build()
    )
    server.start_background()
    try:
        server.add_source("late", -5, RecordingSource(log))
    except RuntimeError as e:
        assert "already consumed" in str(e), f"wrong error: {e}"
    else:
        raise AssertionError("add_source after start must raise")
    assert log == [], f"a rejected source must not fire on_start, got {log}"


if __name__ == "__main__":
    for name, fn in sorted(globals().items()):
        if name.startswith("test_") and callable(fn):
            fn()
            print(f"  ok  {name}")
    print("all lifecycle/event tests passed")
