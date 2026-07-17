"""Plain-assert tests for explicit NT value-type selection. Run directly:
   ./.venv/Scripts/python.exe tests/test_value_types.py
"""
import time

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


def test_nttable_constructor_with_types():
    t = spvirit.NtTable(
        {"name": ["a", "b"], "count": [1, 2]},
        types={"count": "uint"},
        descriptor="demo",
    )
    assert t.labels == ["name", "count"]
    assert t.columns() == {"name": ["a", "b"], "count": [1, 2]}
    assert t.column_types() == {"name": "string", "count": "uint"}
    assert t.descriptor == "demo"
    # untyped columns keep inference (ints -> long)
    t2 = spvirit.NtTable({"x": [1]})
    assert t2.column_types() == {"x": "long"}
    # mismatched column lengths rejected
    _expect(ValueError, lambda: spvirit.NtTable({"a": [1], "b": [1, 2]}))
    # custom labels
    t3 = spvirit.NtTable({"a": [1]}, labels=["Column A"])
    assert t3.labels == ["Column A"]


def test_ntndarray_constructor():
    nd = spvirit.NtNdArray([0] * 12, [4, 3], type="ushort")
    assert nd.value_type == "ushort"
    assert [d["size"] for d in nd.dimensions()] == [4, 3]
    assert [d["offset"] for d in nd.dimensions()] == [0, 0]
    assert nd.uncompressed_size == 24   # 12 elements x 2 bytes
    raw = spvirit.NtNdArray(bytes(6), [3, 2])
    assert raw.value_type == "ubyte"
    assert raw.value == bytes(6)


def test_scalar_factory_all_types_serve_and_roundtrip():
    pvs = [
        spvirit.scalar("VT:UL", 2**63 + 9, type="ulong", writable=True),
        spvirit.scalar("VT:F32", 1.5, type="float", writable=True, units="V"),
        spvirit.scalar("VT:U8", 200, type="ubyte"),
        spvirit.scalar("VT:S", "hey", type="string"),
        spvirit.scalar("VT:B", True, type="boolean"),
    ]
    assert "(ulong)" in repr(pvs[0])
    server = spvirit.Server(pvs=pvs, port=16060, udp_port=16061,
                            listen_ip="127.0.0.1")
    server.start()
    assert pvs[0].get() == 2**63 + 9
    pvs[0].set(2**64 - 1)
    assert pvs[0].get() == 2**64 - 1
    _expect(OverflowError, lambda: pvs[0].set(-1))
    _expect(TypeError, lambda: pvs[0].set("nope"))
    assert pvs[1].get() == 1.5
    assert pvs[2].get() == 200
    _expect(OverflowError, lambda: pvs[2].set(300))
    assert pvs[3].get() == "hey"
    assert pvs[4].get() is True
    # wire introspection reports the requested value type
    from spvirit.lowlevel import Channel
    with Channel.connect("VT:UL", "127.0.0.1:16060", timeout=5.0) as ch:
        desc = ch.introspect()
        assert desc.field("value").type_code == "uint64"
    with Channel.connect("VT:F32", "127.0.0.1:16060", timeout=5.0) as ch:
        assert ch.introspect().field("value").type_code == "float32"


def test_scalar_factory_validation():
    _expect(ValueError, lambda: spvirit.scalar("VT:BAD", 1, type="nope"))
    _expect(OverflowError, lambda: spvirit.scalar("VT:OV", 300, type="ubyte"))
    _expect(TypeError, lambda: spvirit.scalar("VT:K", 1.5, type="int"))


def test_scalar_factory_on_put_and_wire_write():
    seen = []
    ct = spvirit.scalar("VTW:CT", 5, type="ushort", writable=True)

    @ct.on_put
    def _check(pv, value):
        seen.append(value)
        if value > 1000:
            return False

    server = spvirit.Server(pvs=[ct], port=16062, udp_port=16063,
                            listen_ip="127.0.0.1")
    server.start()
    time.sleep(0.3)
    client = (spvirit.Client.builder()
              .server_addr("127.0.0.1:16062").udp_port(16063).build())
    client.put("VTW:CT", 42)
    assert ct.get() == 42
    assert seen == [42]
    try:
        client.put("VTW:CT", 2000)   # rejected by on_put
    except spvirit.SpviritError:
        pass
    assert ct.get() == 42


def test_scalar_factory_scan():
    hb = spvirit.scalar("VTS:HB", 0, type="uint")
    counter = iter(range(1, 100))

    @hb.scan(period=0.05)
    def _tick(pv):
        return next(counter)

    server = spvirit.Server(pvs=[hb], port=16064, udp_port=16065,
                            listen_ip="127.0.0.1")
    server.start()
    time.sleep(0.5)
    assert hb.get() >= 1


def test_typed_waveform_keeps_element_type_across_set():
    wf = spvirit.waveform("VTA:WF", [0] * 4, type="ushort")
    server = spvirit.Server(pvs=[wf], port=16066, udp_port=16067,
                            listen_ip="127.0.0.1")
    server.start()
    wf.set([1, 2, 3])                       # plain int list must stay ushort
    assert wf.get() == [1, 2, 3]
    _expect(OverflowError, lambda: wf.set([70000]))
    _expect(TypeError, lambda: wf.set(["x"]))
    from spvirit.lowlevel import Channel
    with Channel.connect("VTA:WF", "127.0.0.1:16066", timeout=5.0) as ch:
        assert ch.introspect().field("value").type_code == "uint16"


def test_typed_aai_aao_and_empty_list():
    r = spvirit.aai("VTA:R", [], type="float")
    w = spvirit.aao("VTA:W", [1.0, 2.0], type="float")
    server = spvirit.Server(pvs=[r, w], port=16068, udp_port=16069,
                            listen_ip="127.0.0.1")
    server.start()
    assert r.get() == []
    assert w.get() == [1.0, 2.0]
    _expect(ValueError, lambda: spvirit.waveform("VTA:BAD", [], type="nope"))


def test_pv_factory_type_override():
    p = spvirit.pv("VTP:U32", 7, type="uint")
    assert "(uint)" in repr(p)
    q = spvirit.pv("VTP:WF", [0] * 3, type="short")
    d = spvirit.pv("VTP:D", 7, type="double")     # maps onto the native float kind
    assert "(float)" in repr(d)
    server = spvirit.Server(pvs=[p, q, d], port=16070, udp_port=16071,
                            listen_ip="127.0.0.1")
    server.start()
    assert p.get() == 7
    assert d.get() == 7.0
    _expect(OverflowError, lambda: q.set([2**20]))


def main():
    for fn in sorted(k for k in globals() if k.startswith("test_")):
        globals()[fn]()
        print(f"{fn}: ok")
    print("ALL OK")


if __name__ == "__main__":
    main()
