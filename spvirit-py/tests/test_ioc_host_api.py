"""Tier 3 from Python: build records, run them, write from the host."""

import pytest

import spvirit


def test_the_ioc_submodule_offers_every_record_kind():
    for ctor in ("ai", "ao", "bi", "bo", "longin", "longout"):
        assert hasattr(spvirit.ioc, ctor), f"spvirit.ioc.{ctor} is missing"


def test_fields_are_verbatim_epics_names():
    rec = spvirit.ioc.ai("RIG:RBV", EGU="C", HIHI=100, HHSV="MAJOR", MDEL=0.1)
    assert rec.name == "RIG:RBV"
    assert rec.fields()["EGU"] == "C"
    assert rec.fields()["HIHI"] == "100"
    assert rec.fields()["HHSV"] == "MAJOR"


def test_lowercase_kwargs_are_uppercased():
    rec = spvirit.ioc.ao("X", egu="mm")
    assert rec.fields()["EGU"] == "mm"
    assert "egu" not in rec.fields()


def test_tier_two_spellings_are_refused_by_name():
    """`units=` is tier 2's spelling. Accepting it here would quietly produce
    a record with a field nothing reads, so it has to be an error that names
    the right spelling."""
    with pytest.raises(ValueError) as e:
        spvirit.ioc.ai("X", units="C")
    assert "EGU" in str(e.value)


def test_an_unmodelled_field_is_accepted():
    """DRVH is advisory: the .db path accepts and ignores it, so this must
    too, or the two construction paths are not interchangeable."""
    rec = spvirit.ioc.ao("X", DRVH=100)
    assert rec.fields()["DRVH"] == "100"


def test_booleans_render_as_epics_menu_strings():
    assert spvirit.ioc.ai("X", PINI=True).fields()["PINI"] == "YES"
    assert spvirit.ioc.ai("X", PINI=False).fields()["PINI"] == "NO"


def test_whole_floats_render_without_a_decimal_point():
    assert spvirit.ioc.ai("X", HIHI=100.0).fields()["HIHI"] == "100"
    assert spvirit.ioc.ai("X", HYST=0.5).fields()["HYST"] == "0.5"


def test_an_ioc_can_be_built_from_records():
    sp = spvirit.ioc.ao("RIG:SP", OUT="RIG:RBV.VAL", FLNK="RIG:RBV")
    rbv = spvirit.ioc.ai("RIG:RBV", INP="RIG:SP PP", EGU="C")
    ioc = spvirit.Ioc(records=[sp, rbv])
    assert sorted(ioc.record_names()) == ["RIG:RBV", "RIG:SP"]


def test_an_ioc_can_be_built_from_db_text():
    ioc = spvirit.Ioc(db_string='record(ai, "A") {\n    field(INP, "7")\n}\n')
    assert ioc.record_names() == ["A"]


def test_building_with_neither_records_nor_db_is_an_error():
    with pytest.raises(ValueError):
        spvirit.Ioc()


def test_building_with_both_records_and_db_is_an_error():
    """Two sources of records would need a merge rule nothing has defined."""
    rec = spvirit.ioc.ai("A", INP="1")
    with pytest.raises(ValueError):
        spvirit.Ioc(records=[rec], db_string='record(ai, "B") {\n}\n')


def test_a_record_spec_belongs_to_one_ioc():
    rec = spvirit.ioc.ai("A", INP="1")
    spvirit.Ioc(records=[rec])
    with pytest.raises(RuntimeError) as e:
        spvirit.Ioc(records=[rec])
    assert "already been built" in str(e.value)


def test_adding_a_record_after_the_ioc_is_built_raises_with_the_reason():
    ioc = spvirit.Ioc(records=[spvirit.ioc.ai("A", INP="1")])
    with pytest.raises(RuntimeError) as e:
        ioc.add_record(spvirit.ioc.ai("B", INP="2"))
    msg = str(e.value)
    assert "lock set" in msg
    assert "iocInit" in msg or "dbLoadRecords" in msg


def test_an_unsupported_record_type_names_sub_project_d():
    with pytest.raises(ValueError) as e:
        spvirit.Ioc(db_string='record(calc, "A") {\n}\n')
    assert "sub-project D" in str(e.value)


def _rig():
    sp = spvirit.ioc.ao("RIG:SP", OUT="RIG:RBV.VAL", FLNK="RIG:RBV")
    rbv = spvirit.ioc.ai("RIG:RBV", INP="RIG:SP PP", EGU="C", HIHI=100, HHSV="MAJOR")
    return sp, rbv, spvirit.Ioc(records=[sp, rbv])


def test_an_outside_in_write_processes_the_chain():
    sp, rbv, _ioc = _rig()
    assert sp.set(95) is None  # Python set() returns None (Ruling 4)
    assert sp.get() == 95
    assert rbv.get() == 95


def test_set_async_is_awaitable_and_returns_none():
    import asyncio

    sp, rbv, _ioc = _rig()

    async def drive():
        result = await sp.set_async(95)
        assert result is None

    asyncio.run(drive())
    assert rbv.get() == 95


def test_reading_a_field_uses_the_epics_name():
    _sp, rbv, _ioc = _rig()
    assert rbv["EGU"] == "C"


def test_field_writes_are_refused_and_name_sub_project_b():
    _sp, rbv, _ioc = _rig()
    with pytest.raises(TypeError) as e:
        rbv["SCAN"] = "1 second"
    assert "sub-project B" in str(e.value)


def test_on_put_does_not_exist_on_tier_three():
    """Not a silent no-op: tier 2 has a known trap where attaching on_put
    after handing the PV to a server logs a warning and does nothing, and
    repeating it here would be worse, because on tier 3 it can never work."""
    _sp, rbv, _ioc = _rig()
    with pytest.raises(AttributeError) as e:
        rbv.on_put
    msg = str(e.value)
    assert "tier 2" in msg
    assert "lock set" in msg or "GIL" in msg


def test_an_unbound_record_says_so():
    rec = spvirit.ioc.ai("A", INP="1")
    with pytest.raises(RuntimeError) as e:
        rec.get()
    assert "Ioc" in str(e.value)
    with pytest.raises(RuntimeError):
        rec.set(1)


def test_an_unknown_field_reads_as_a_key_error():
    _sp, rbv, _ioc = _rig()
    with pytest.raises(KeyError):
        rbv["NOSUCHFIELD"]
