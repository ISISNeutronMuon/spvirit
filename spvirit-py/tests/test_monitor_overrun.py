"""MonitorUpdate exposes the changed and overrun bitsets to Python.

Plain-assert tests. Run directly:
   ./.venv/Scripts/python.exe tests/test_monitor_overrun.py
"""
import time

import spvirit

TCP, UDP = 16090, 16091
TCP2, UDP2 = 16092, 16093


def _wait_for(cond, timeout=5.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if cond():
            return True
        time.sleep(0.02)
    return cond()


def _assert_update_shape(u):
    assert type(u).__name__ == "MonitorUpdate", type(u).__name__
    assert isinstance(u.value, dict), u.value
    assert isinstance(u.changed, list), u.changed
    assert isinstance(u.overrun, list), u.overrun
    assert all(isinstance(p, str) for p in u.changed)
    assert all(isinstance(p, str) for p in u.overrun)
    # A quiet channel drops nothing.
    assert u.has_overrun is False
    assert u.overrun == []
    assert "MonitorUpdate" in repr(u)
    assert "changed=" in repr(u) and "overrun=" in repr(u)


def test_subscribe_yields_monitor_update():
    t = spvirit.ao("OVR:TARGET", 1.0)
    server = spvirit.Server(pvs=[t], port=TCP, udp_port=UDP,
                            listen_ip="127.0.0.1")
    server.start()

    updates = []
    client = (spvirit.Client.builder()
              .server_addr(f"127.0.0.1:{TCP}").udp_port(UDP).build())
    with client.subscribe("OVR:TARGET", lambda u: updates.append(u)):
        assert _wait_for(lambda: len(updates) >= 1), "no initial monitor update"
        _assert_update_shape(updates[0])
        # The initial update carries the whole structure, so "value" is in it.
        assert "value" in updates[0].value

        t.set(2.5)
        assert _wait_for(
            lambda: any(u.value.get("value") == 2.5 for u in updates)
        ), "value update not delivered"

    delta = next(u for u in updates if u.value.get("value") == 2.5)
    assert "value" in delta.changed, delta.changed


def test_channel_monitor_yields_monitor_update():
    t = spvirit.ao("OVR:CH", 1.0)
    server = spvirit.Server(pvs=[t], port=TCP2, udp_port=UDP2,
                            listen_ip="127.0.0.1")
    server.start()
    time.sleep(0.3)

    from spvirit.lowlevel import Channel, MonitorUpdate

    seen = []
    with Channel.connect("OVR:CH", f"127.0.0.1:{TCP2}", timeout=5.0) as ch:
        def once(update):
            seen.append(update)
            return False  # stop after the first update

        ch.monitor(once)

    assert len(seen) == 1
    assert isinstance(seen[0], MonitorUpdate)
    _assert_update_shape(seen[0])


def main():
    for fn in sorted(k for k in globals() if k.startswith("test_")):
        globals()[fn]()
        print(f"{fn}: ok")
    print("ALL OK")


if __name__ == "__main__":
    main()
