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
