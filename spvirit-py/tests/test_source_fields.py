"""A Python source may serve `.FIELD` by defining `fields()`. Run directly:
   ./.venv/Scripts/python.exe tests/test_source_fields.py
"""
import time

import spvirit


def _expect_not_found(fn):
    """Assert `fn` fails specifically with "PV not found".

    Not a bare `except Exception`: that also passes when the server never
    started, when the port is wrong, or when the PV name has a typo -- so
    the "no field PV here" tests would pass against a dead server. The Rust
    side matches on the message the same way.
    """
    try:
        fn()
    except Exception as e:
        assert "not found" in str(e), f"wrong error: {e!r}"
        return
    raise AssertionError("expected a 'PV not found' error")


def _local_client(tcp, udp):
    """Build a Client wired to a specific local server, bypassing UDP search."""
    return (
        spvirit.Client.builder()
        .server_addr(f"127.0.0.1:{tcp}")
        .udp_port(udp)
        .build()
    )


def _get(client, name):
    return client.get(name).value["value"]


class WithFields:
    """A source that exposes record metadata the way tiers 1 and 2 do."""

    def claim(self, name):
        return spvirit.PvInfo.nt_scalar("double") if name == "PY:A" else None

    def get(self, name):
        return spvirit.NtScalar(1.5) if name == "PY:A" else None

    def fields(self, name):
        if name != "PY:A":
            return None
        return {"DESC": "a python record", "RTYP": "ai", "MDEL": 0.25, "PHAS": 2}


class WithoutFields:
    def claim(self, name):
        return spvirit.PvInfo.nt_scalar("double") if name == "PY:B" else None

    def get(self, name):
        return spvirit.NtScalar(2.5) if name == "PY:B" else None


def test_fields_are_served_when_the_source_defines_them():
    tcp, udp = 15305, 15306
    server = spvirit.Server(sources=[("py", 0, WithFields())], port=tcp, udp_port=udp,
                             listen_ip="127.0.0.1")
    server.start()
    time.sleep(0.3)
    client = _local_client(tcp, udp)
    assert _get(client, "PY:A") == 1.5
    assert _get(client, "PY:A.DESC") == "a python record"
    assert _get(client, "PY:A.RTYP") == "ai"
    assert _get(client, "PY:A.MDEL") == 0.25
    assert _get(client, "PY:A.PHAS") == 2


def test_absent_fields_fall_back_to_the_dbcommon_default():
    tcp, udp = 15315, 15316
    server = spvirit.Server(sources=[("py", 0, WithFields())], port=tcp, udp_port=udp,
                             listen_ip="127.0.0.1")
    server.start()
    time.sleep(0.3)
    client = _local_client(tcp, udp)
    assert _get(client, "PY:A.PRIO") == "LOW"


def test_a_source_without_fields_serves_no_field_pvs():
    tcp, udp = 15325, 15326
    server = spvirit.Server(sources=[("py", 0, WithoutFields())], port=tcp, udp_port=udp,
                             listen_ip="127.0.0.1")
    server.start()
    time.sleep(0.3)
    client = _local_client(tcp, udp)
    assert _get(client, "PY:B") == 2.5
    _expect_not_found(lambda: client.get("PY:B.DESC"))


def test_fields_on_an_unowned_record_are_not_served():
    tcp, udp = 15335, 15336
    server = spvirit.Server(sources=[("py", 0, WithFields())], port=tcp, udp_port=udp,
                             listen_ip="127.0.0.1")
    server.start()
    time.sleep(0.3)
    client = _local_client(tcp, udp)
    _expect_not_found(lambda: client.get("PY:MISSING.DESC"))


def test_fields_are_served_for_a_source_added_after_build():
    """The `fields()` protocol applies on all three registration paths.

    `ServerBuilder.add_source` and `Server(sources=[...])` both derive the
    `{label}-fields` tier; post-`build()` `Server.add_source` used to skip
    it, so `fields()` was a silent no-op there while the README documented
    the protocol unconditionally.
    """
    tcp, udp = 15345, 15346
    server = (
        spvirit.ServerBuilder()
        .port(tcp)
        .udp_port(udp)
        .listen_ip("127.0.0.1")
        .ai("PY:ANCHOR", 0.0)
        .build()
    )
    server.add_source("late", 0, WithFields())
    server.start()
    time.sleep(0.3)
    client = _local_client(tcp, udp)
    assert _get(client, "PY:A") == 1.5
    assert _get(client, "PY:A.DESC") == "a python record"
    assert _get(client, "PY:A.PRIO") == "LOW"


def test_a_post_build_source_without_fields_serves_no_field_pvs():
    """The derived tier is still opt-in on the post-build path."""
    tcp, udp = 15355, 15356
    server = (
        spvirit.ServerBuilder()
        .port(tcp)
        .udp_port(udp)
        .listen_ip("127.0.0.1")
        .ai("PY:ANCHOR", 0.0)
        .build()
    )
    server.add_source("late", 0, WithoutFields())
    server.start()
    time.sleep(0.3)
    client = _local_client(tcp, udp)
    assert _get(client, "PY:B") == 2.5
    _expect_not_found(lambda: client.get("PY:B.DESC"))


def main():
    for fn in sorted(k for k in globals() if k.startswith("test_")):
        globals()[fn]()
        print(f"{fn}: ok")
    print("ALL OK")


if __name__ == "__main__":
    main()
