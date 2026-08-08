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


def test_on_start_runs_before_serving():
    log = []
    b = spvirit.ServerBuilder().ao("PY:SP", 0.0).port(0).udp_port(0)

    @b.on_start
    def initialise(store):
        store.set_value("PY:SP", 22.5)
        log.append("init")

    server = b.build()
    assert log == [], "on_start must not run at build()"
    server.start_background()
    _wait_for(lambda: log == ["init"])
    assert log == ["init"]
    assert server.store().get_value("PY:SP") == 22.5


def test_hooks_and_source_on_start_share_one_ordered_list():
    """The spec's 'one list' rule: a builder hook registered between two
    sources must fire between them, not before or after both."""
    log = []
    b = spvirit.ServerBuilder().ai("PY:ORD", 1.0).port(0).udp_port(0)
    b.add_source("first", -5, RecordingSource(log))

    @b.on_start
    def middle(store):
        log.append("builder-hook")

    b.add_source("last", -6, RecordingSource(log))
    server = b.build()
    server.start_background()

    _wait_for(lambda: len(log) == 3)
    assert log == ["source-on-start", "builder-hook", "source-on-start"], (
        f"hooks must interleave in registration order, got {log}"
    )


def test_on_start_decorator_returns_the_function():
    b = spvirit.ServerBuilder().ai("PY:D", 1.0).port(0).udp_port(0)

    @b.on_start
    def my_hook(store):
        pass

    assert callable(my_hook), "decorator must return the function unchanged"
    assert my_hook.__name__ == "my_hook"


def test_on_event_handler_receives_store_and_event():
    seen = []
    b = spvirit.ServerBuilder().ai("PY:E", 1.0).port(0).udp_port(0)

    @b.on_event("SHUTTER")
    def handler(store, event):
        seen.append(event)

    server = b.build()
    server.start_background()
    server.post_event("SHUTTER")
    server.drain_events()
    assert seen == ["SHUTTER"], f"expected one SHUTTER, got {seen}"


def test_post_event_with_no_handlers_is_a_noop():
    server = spvirit.ServerBuilder().ai("PY:F", 1.0).port(0).udp_port(0).build()
    server.start_background()
    server.post_event("NOBODY:LISTENING")
    server.drain_events()


def test_async_event_handler_runs():
    seen = []
    b = spvirit.ServerBuilder().ai("PY:G", 1.0).port(0).udp_port(0)

    @b.on_event("ASYNC")
    async def handler(store, event):
        seen.append(event)

    server = b.build()
    server.start_background()
    server.post_event("ASYNC")
    server.drain_events()
    assert seen == ["ASYNC"], f"async handler did not run, got {seen}"


def test_raising_handler_does_not_stop_the_dispatcher():
    seen = []
    b = spvirit.ServerBuilder().ai("PY:H", 1.0).port(0).udp_port(0)

    @b.on_event("BOOM")
    def bad(store, event):
        raise ValueError("handler failed")

    @b.on_event("BOOM")
    def good(store, event):
        seen.append("ran")

    server = b.build()
    server.start_background()
    server.post_event("BOOM")
    server.drain_events()
    assert seen == ["ran"], "handler after the raising one must still run"


def test_raising_on_start_aborts_startup():
    b = spvirit.ServerBuilder().ai("PY:I", 1.0).port(0).udp_port(0)

    @b.on_start
    def bad_init(store):
        raise ValueError("init failed")

    server = b.build()
    try:
        server.start_background()
        raise AssertionError("start must fail when on_start raises")
    except RuntimeError as e:
        assert "on_start" in str(e), f"error must name the hook, got: {e}"


def test_raising_source_on_start_aborts_startup():
    """The carried-item ruling: a raising Python source on_start must abort
    startup naming the hook too, not just a swallowed tracing::error!."""

    class BadSource:
        def claim(self, name):
            return None

        def get(self, name):
            return None

        def put(self, name, value):
            return None

        def names(self):
            return []

        def on_start(self, notifier):
            raise ValueError("source init failed")

    b = spvirit.ServerBuilder().ai("PY:J", 1.0).port(0).udp_port(0)
    b.add_source("bad", -5, BadSource())
    server = b.build()
    try:
        server.start_background()
        raise AssertionError("start must fail when a source's on_start raises")
    except RuntimeError as e:
        assert "on_start" in str(e), f"error must name the hook, got: {e}"


def test_raising_source_on_start_in_immediate_add_source_window_raises_from_add_source():
    """A source added between build() and run() has its on_start fired
    immediately (see Server.add_source's docstring). A raise there is a
    normal Python exception from add_source() itself -- there is no
    startup in flight to abort at that point."""

    class BadSource:
        def claim(self, name):
            return None

        def get(self, name):
            return None

        def put(self, name, value):
            return None

        def names(self):
            return []

        def on_start(self, notifier):
            raise ValueError("immediate init failed")

    server = spvirit.ServerBuilder().ai("PY:K", 1.0).port(0).udp_port(0).build()
    try:
        server.add_source("bad", -5, BadSource())
        raise AssertionError("add_source must propagate the source's on_start error")
    except ValueError as e:
        assert "immediate init failed" in str(e)


if __name__ == "__main__":
    for name, fn in sorted(globals().items()):
        if name.startswith("test_") and callable(fn):
            fn()
            print(f"  ok  {name}")
    print("all lifecycle/event tests passed")
