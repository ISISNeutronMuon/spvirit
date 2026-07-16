"""Plain-assert tests for the PV-handle Python API. Run directly:
   ./.venv/Scripts/python.exe tests/test_pv_handles.py
"""
import spvirit


def _local_client(tcp, udp):
    """Build a Client wired to a specific local server, bypassing UDP search."""
    return (
        spvirit.Client.builder()
        .server_addr(f"127.0.0.1:{tcp}")
        .udp_port(udp)
        .build()
    )


def test_constructors_and_options():
    temp = spvirit.ai("PY:TEMP", 22.5, units="degC", prec=2, desc="Temp")
    assert temp.name == "PY:TEMP"
    assert "PY:TEMP" in repr(temp)

    sp = spvirit.ao("PY:SP", 25.0, drive_limits=(0.0, 100.0), mdel=0.1)
    assert sp.name == "PY:SP"

    en = spvirit.bo("PY:EN", False)
    msg = spvirit.string_in("PY:MSG", "hello")
    assert en.name == "PY:EN" and msg.name == "PY:MSG"


def test_unbound_set_get_raise():
    pv = spvirit.ai("PY:X", 1.0)
    try:
        pv.set(2.0)
        raise AssertionError("set on unbound handle must raise")
    except RuntimeError:
        pass
    try:
        pv.get()
        raise AssertionError("get on unbound handle must raise")
    except RuntimeError:
        pass


def test_server_binds_handles():
    temp = spvirit.ai("PYS:TEMP", 22.5, units="degC")
    sp = spvirit.ao("PYS:SP", 25.0)
    server = spvirit.Server(pvs=[temp, sp], port=15075, udp_port=15076,
                            listen_ip="127.0.0.1")
    temp.set(23.5)
    assert temp.get() == 23.5
    assert sp.get() == 25.0
    # attach to a record by name, typed
    h = server.pv("PYS:TEMP")
    assert h.get() == 23.5
    try:
        server.pv("PYS:NOPE")
        raise AssertionError("must raise KeyError")
    except KeyError:
        pass


def test_server_db_string_records():
    server = spvirit.Server(
        db_string='record(ao, "PYS:DBX") {\n    field(VAL, "2.5")\n}\n',
        port=15085, udp_port=15086, listen_ip="127.0.0.1",
    )
    h = server.pv("PYS:DBX")
    assert h.get() == 2.5
    h.set(3.5)
    assert h.get() == 3.5


def test_on_put_decorator_and_wire_rejection():
    seen = []
    sp = spvirit.ao("PYW:SP", 25.0, drive_limits=(0.0, 100.0))

    @sp.on_put
    def _handle(pv, value):
        seen.append(value)
        if value > 100.0:
            return False  # reject
        return None       # accept

    tcp, udp = 15095, 15096
    server = spvirit.Server(pvs=[sp], port=tcp, udp_port=udp, listen_ip="127.0.0.1")
    server.start()
    import time
    time.sleep(0.3)

    client = _local_client(tcp, udp)
    client.put("PYW:SP", 50.0)
    assert sp.get() == 50.0
    assert seen and seen[-1] == 50.0

    try:
        client.put("PYW:SP", 500.0)
        raise AssertionError("out-of-range put must be rejected on the wire")
    except Exception as e:
        assert not isinstance(e, AssertionError)
    assert sp.get() == 50.0  # unchanged


def test_on_put_can_set_other_pvs():
    a = spvirit.ao("PYC:A", 0.0)
    b = spvirit.ai("PYC:B", 0.0)

    @a.on_put
    def _(pv, value):
        b.set(value * 2.0)

    tcp, udp = 15105, 15106
    server = spvirit.Server(pvs=[a, b], port=tcp, udp_port=udp, listen_ip="127.0.0.1")
    server.start()
    import time
    time.sleep(0.3)
    client = _local_client(tcp, udp)
    client.put("PYC:A", 21.0)
    time.sleep(0.2)
    assert b.get() == 42.0


def test_scan_decorator():
    import time
    tick = spvirit.ai("PYT:TICK", 0.0)

    @tick.scan(period=0.05)
    def _(pv):
        return (pv.get() or 0.0) + 1.0

    server = spvirit.Server(pvs=[tick], port=15115, udp_port=15116,
                            listen_ip="127.0.0.1")
    server.start()
    time.sleep(0.5)
    assert tick.get() >= 2.0


def test_scan_direct_method():
    import time
    tick = spvirit.ai("PYT:TICK2", 0.0)

    def _(pv):
        return (pv.get() or 0.0) + 1.0

    tick.scan(0.05, _)

    server = spvirit.Server(pvs=[tick], port=15135, udp_port=15136,
                            listen_ip="127.0.0.1")
    server.start()
    time.sleep(0.5)
    assert tick.get() >= 2.0


def test_calc():
    import time
    a = spvirit.ai("PYK:A", 1.0)
    b = spvirit.ai("PYK:B", 2.0)
    s = spvirit.calc("PYK:SUM", [a, b], lambda vals: sum(vals))
    server = spvirit.Server(pvs=[a, b, s], port=15125, udp_port=15126,
                            listen_ip="127.0.0.1")
    a.set(10.0)
    time.sleep(0.1)
    assert s.get() == 12.0


def test_pv_inference():
    assert "float" in repr(spvirit.pv("PYI:F", 1.5))
    assert "bool" in repr(spvirit.pv("PYI:B", True))
    assert "str" in repr(spvirit.pv("PYI:S", "x"))
    assert "int" in repr(spvirit.pv("PYI:I", 3))
    try:
        spvirit.pv("PYI:D", {"a": 1})
        raise AssertionError("dict must raise TypeError")
    except TypeError:
        pass


def test_int_and_enum_and_array_constructors():
    n = spvirit.longout("PYX:N", 5)
    mode = spvirit.mbbo("PYX:MODE", ["Stop", "Run", "Fault"], 0)
    wave = spvirit.waveform("PYX:WAVE", [1.0, 2.0, 3.0])
    server = spvirit.Server(pvs=[n, mode, wave], port=15145, udp_port=15146,
                            listen_ip="127.0.0.1")
    n.set(42)
    assert n.get() == 42
    mode.set(2)
    assert mode.get() == 2
    mode.set(9)              # out-of-range: no-op
    assert mode.get() == 2
    wave.set([4.0, 5.0])
    assert wave.get() == [4.0, 5.0]
    # typed re-attach picks the right kinds
    assert server.pv("PYX:N").get() == 42
    assert server.pv("PYX:WAVE").get() == [4.0, 5.0]
    assert "array" in repr(server.pv("PYX:WAVE"))


def test_pv_inference_int_and_list():
    assert "int" in repr(spvirit.pv("PYI:N", 3))
    assert "array" in repr(spvirit.pv("PYI:W", [1.0, 2.0]))


def test_set_alarm():
    t = spvirit.ai("PYA:T", 1.0)
    spvirit.Server(pvs=[t], port=15155, udp_port=15156, listen_ip="127.0.0.1")
    t.set_alarm(2, 3, "broken")   # MAJOR severity
    # no direct alarm getter on handles; absence of exception is the contract,
    # plus idempotency:
    t.set_alarm(2, 3, "broken")


def test_async_set_get():
    import asyncio

    t = spvirit.ao("PYAS:T", 1.0)
    spvirit.Server(pvs=[t], port=15165, udp_port=15166, listen_ip="127.0.0.1")

    async def flow():
        await t.set_async(6.28)
        return await t.get_async()

    assert asyncio.run(flow()) == 6.28


def _wait_for(cond, timeout=5.0):
    """Poll `cond()` until truthy or the deadline expires; return its result."""
    import time
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = cond()
        if result:
            return result
        time.sleep(0.02)
    return cond()


def test_subscribe():
    t = spvirit.ao("PYM:T", 1.0)
    tcp, udp = 15175, 15176
    server = spvirit.Server(pvs=[t], port=tcp, udp_port=udp, listen_ip="127.0.0.1")
    server.start()

    updates = []
    client = _local_client(tcp, udp)
    sub = client.subscribe("PYM:T", lambda v: updates.append(v))
    assert sub.pv_name == "PYM:T"
    assert "PYM:T" in repr(sub)

    assert _wait_for(lambda: len(updates) >= 1), "no initial monitor update"
    assert sub.is_active
    t.set(2.5)
    assert _wait_for(lambda: any(u.get("value") == 2.5 for u in updates)), \
        "value update not delivered"

    sub.close()
    assert not sub.is_active
    assert sub.error is None
    sub.close()  # idempotent
    seen = len(updates)
    t.set(9.9)
    import time
    time.sleep(0.3)
    assert len(updates) == seen, "closed subscription must not deliver updates"


def test_subscribe_callback_false_stops():
    t = spvirit.ao("PYM:S", 1.0)
    tcp, udp = 15185, 15186
    server = spvirit.Server(pvs=[t], port=tcp, udp_port=udp, listen_ip="127.0.0.1")
    server.start()

    updates = []

    def once(value):
        updates.append(value)
        return False  # unsubscribe after the first update

    client = _local_client(tcp, udp)
    sub = client.subscribe("PYM:S", once)
    assert _wait_for(lambda: not sub.is_active), \
        "returning False from the callback must end the subscription"
    assert len(updates) == 1
    assert sub.error is None


def test_subscribe_context_manager():
    t = spvirit.ao("PYM:C", 1.0)
    tcp, udp = 15195, 15196
    server = spvirit.Server(pvs=[t], port=tcp, udp_port=udp, listen_ip="127.0.0.1")
    server.start()

    updates = []
    client = _local_client(tcp, udp)
    with client.subscribe("PYM:C", lambda v: updates.append(v)) as sub:
        assert _wait_for(lambda: len(updates) >= 1)
        assert sub.is_active
    assert not sub.is_active


def test_exception_hierarchy():
    assert issubclass(spvirit.TimeoutError, TimeoutError)          # builtin
    assert issubclass(spvirit.TimeoutError, spvirit.SpviritError)
    assert issubclass(spvirit.IoError, OSError)
    assert issubclass(spvirit.IoError, spvirit.SpviritError)
    assert issubclass(spvirit.PutRejectedError, spvirit.SpviritError)
    assert not hasattr(spvirit, "MonitorEvent")  # dead API removed


def test_handle_set_bypasses_on_put():
    # on_put validates *client* (wire) writes only; the owning process
    # writes authoritatively through the handle, like p4p's post().
    seen = []
    sp = spvirit.ao("PYR:SP", 1.0)

    @sp.on_put
    def _(pv, value):
        seen.append(value)
        return False  # would reject a wire put

    spvirit.Server(pvs=[sp], port=15205, udp_port=15206, listen_ip="127.0.0.1")
    sp.set(2.0)                # not subject to the validator
    assert sp.get() == 2.0
    assert seen == []


def test_monitor_callback_exception_propagates():
    import time
    t = spvirit.ao("PYE:T", 1.0)
    tcp, udp = 15215, 15216
    server = spvirit.Server(pvs=[t], port=tcp, udp_port=udp, listen_ip="127.0.0.1")
    server.start()
    time.sleep(0.3)
    client = _local_client(tcp, udp)

    def boom(v):
        raise ValueError("boom")

    try:
        client.monitor("PYE:T", boom)
        raise AssertionError("callback exception must propagate out of monitor")
    except ValueError as e:
        assert "boom" in str(e)


def test_subscribe_callback_exception_recorded():
    import time
    t = spvirit.ao("PYE:S", 1.0)
    tcp, udp = 15225, 15226
    server = spvirit.Server(pvs=[t], port=tcp, udp_port=udp, listen_ip="127.0.0.1")
    server.start()
    time.sleep(0.3)
    client = _local_client(tcp, udp)

    def boom(v):
        raise ValueError("kaput")

    sub = client.subscribe("PYE:S", boom)
    assert _wait_for(lambda: not sub.is_active), \
        "raising callback must end the subscription"
    assert sub.error is not None and "kaput" in sub.error


def test_builder_consumed_raises_runtime_error():
    b = spvirit.ServerBuilder().ai("PYB:X", 1.0)
    b.build()
    try:
        b.ao("PYB:Y", 2.0)
        raise AssertionError("consumed builder must raise RuntimeError")
    except RuntimeError:
        pass


def test_pv_inference_rejects_opts_on_arrays():
    try:
        spvirit.pv("PYI:WU", [1.0, 2.0], units="mm")
        raise AssertionError("array pv() with metadata opts must raise TypeError")
    except TypeError:
        pass


def test_fields_accepts_single_string():
    import time
    t = spvirit.ao("PYF:T", 5.0)
    tcp, udp = 15235, 15236
    server = spvirit.Server(pvs=[t], port=tcp, udp_port=udp, listen_ip="127.0.0.1")
    server.start()
    time.sleep(0.3)
    client = _local_client(tcp, udp)
    r = client.get("PYF:T", fields="value")   # str, not list
    assert r.value["value"] == 5.0


def main():
    for fn in sorted(k for k in globals() if k.startswith("test_")):
        globals()[fn]()
        print(f"{fn}: ok")
    print("ALL OK")


if __name__ == "__main__":
    main()
