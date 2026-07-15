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


def main():
    for fn in sorted(k for k in globals() if k.startswith("test_")):
        globals()[fn]()
        print(f"{fn}: ok")
    print("ALL OK")


if __name__ == "__main__":
    main()
