"""Plain-assert tests for explicit NT value-type selection. Run directly:
   ./.venv/Scripts/python.exe tests/test_value_types.py
"""
import spvirit


def _expect(exc, fn):
    try:
        fn()
    except exc:
        return
    raise AssertionError(f"expected {exc.__name__}")


def test_ntscalar_type_selection():
    # every wire type constructible, reported via .value_type
    cases = [
        (True, "boolean", True), (-5, "byte", -5), (300, "short", 300),
        (70000, "int", 70000), (2**40, "long", 2**40), (200, "ubyte", 200),
        (60000, "ushort", 60000), (3_000_000_000, "uint", 3_000_000_000),
        (2**63 + 5, "ulong", 2**63 + 5), (1.5, "float", 1.5),
        (1.5, "double", 1.5), ("hi", "string", "hi"),
    ]
    for value, tname, expect in cases:
        nt = spvirit.NtScalar(value, type=tname)
        assert nt.value_type == tname, (tname, nt.value_type)
        assert nt.value == expect, (tname, nt.value)


def test_ntscalar_type_aliases_and_default():
    assert spvirit.NtScalar(1, type="u16").value_type == "ushort"
    assert spvirit.NtScalar(1, type="float64").value_type == "double"
    # no type= keeps today's inference: int -> long, float -> double
    assert spvirit.NtScalar(1).value_type == "long"
    assert spvirit.NtScalar(1.0).value_type == "double"
    assert spvirit.NtScalar(True).value_type == "boolean"
    assert spvirit.NtScalar("s").value_type == "string"


def test_ntscalar_widening_rules():
    # int -> float/double is allowed
    assert spvirit.NtScalar(3, type="double").value == 3.0
    assert spvirit.NtScalar(3, type="float").value == 3.0
    # integral float -> int is allowed
    assert spvirit.NtScalar(2.0, type="int").value == 2


def test_ntscalar_strict_rejections():
    _expect(OverflowError, lambda: spvirit.NtScalar(300, type="ubyte"))
    _expect(OverflowError, lambda: spvirit.NtScalar(-1, type="uint"))
    _expect(OverflowError, lambda: spvirit.NtScalar(2**63, type="long"))
    _expect(OverflowError, lambda: spvirit.NtScalar(1e300, type="float"))
    _expect(TypeError, lambda: spvirit.NtScalar(2.5, type="int"))
    _expect(TypeError, lambda: spvirit.NtScalar("x", type="int"))
    _expect(TypeError, lambda: spvirit.NtScalar(True, type="int"))
    _expect(TypeError, lambda: spvirit.NtScalar(1, type="boolean"))
    _expect(TypeError, lambda: spvirit.NtScalar(1, type="string"))
    _expect(ValueError, lambda: spvirit.NtScalar(1, type="quint"))


def test_ntscalararray_type_selection():
    a = spvirit.NtScalarArray([1, 2, 3], type="ushort")
    assert a.value_type == "ushort"
    assert a.value == [1, 2, 3]
    f = spvirit.NtScalarArray([1, 2.5], type="float")
    assert f.value == [1.0, 2.5]
    # empty list gets the requested element type (not the double fallback)
    assert spvirit.NtScalarArray([], type="uint").value_type == "uint"
    assert spvirit.NtScalarArray([]).value_type == "double"
    # bytes only for byte/ubyte element types
    b = spvirit.NtScalarArray(b"\x01\x02", type="ubyte")
    assert b.value == b"\x01\x02"
    sb = spvirit.NtScalarArray(b"\xff", type="byte")
    assert sb.value_type == "byte" and sb.value == [-1]
    _expect(TypeError, lambda: spvirit.NtScalarArray(b"\x01", type="int"))
    _expect(OverflowError, lambda: spvirit.NtScalarArray([1, 999], type="ubyte"))
    # untyped default unchanged: ints -> long[]
    assert spvirit.NtScalarArray([1, 2]).value_type == "long"


def main():
    for fn in sorted(k for k in globals() if k.startswith("test_")):
        globals()[fn]()
        print(f"{fn}: ok")
    print("ALL OK")


if __name__ == "__main__":
    main()
