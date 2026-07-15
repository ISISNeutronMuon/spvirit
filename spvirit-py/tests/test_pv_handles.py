"""Plain-assert tests for the PV-handle Python API. Run directly:
   ./.venv/Scripts/python.exe tests/test_pv_handles.py
"""
import spvirit


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


def main():
    for fn in sorted(k for k in globals() if k.startswith("test_")):
        globals()[fn]()
        print(f"{fn}: ok")
    print("ALL OK")


if __name__ == "__main__":
    main()
