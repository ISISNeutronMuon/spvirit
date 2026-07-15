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
    assert temp.name() == "PY:TEMP"
    assert "PY:TEMP" in repr(temp)

    sp = spvirit.ao("PY:SP", 25.0, drive_limits=(0.0, 100.0), mdel=0.1)
    assert sp.name() == "PY:SP"

    en = spvirit.bo("PY:EN", False)
    msg = spvirit.string_in("PY:MSG", "hello")
    assert en.name() == "PY:EN" and msg.name() == "PY:MSG"


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
    try:
        spvirit.pv("PYI:I", 3)
        raise AssertionError("int must raise TypeError")
    except TypeError:
        pass


def main():
    for fn in sorted(k for k in globals() if k.startswith("test_")):
        globals()[fn]()
        print(f"{fn}: ok")
    print("ALL OK")


if __name__ == "__main__":
    main()
