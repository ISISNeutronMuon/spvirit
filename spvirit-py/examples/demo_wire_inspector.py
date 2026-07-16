"""Wire inspector — see the actual PVAccess bytes behind a get.

Starts a tiny in-process server, connects a low-level ``Channel`` to it,
and inspects the exchange at three depths:

  1. the type layer: ``Channel.introspect()`` -> ``StructureDesc.dump()``
  2. a captured frame: ``GetResult.raw_pva`` decoded with ``codec.decode_packet``
  3. live frames straight off the TCP stream: ``Channel.read_until`` -> ``Packet``

Everything is self-contained; just run it:

    python demo_wire_inspector.py
"""
import time

import spvirit
from spvirit import codec
from spvirit.lowlevel import Channel, Packet

TCP, UDP = 15305, 15306


def hexdump(data: bytes, limit: int = 32) -> str:
    shown = " ".join(f"{b:02x}" for b in data[:limit])
    return shown + (" ..." if len(data) > limit else "")


def describe(pkt: Packet) -> str:
    return (
        f"cmd={pkt.command_name!r:12} flags=0x{pkt.flags:02x} "
        f"len={pkt.payload_length:<5} app={pkt.is_application} srv={pkt.is_server}"
    )


def main() -> None:
    pv = spvirit.ai("WIRE:TEMP", 21.5, units="degC", prec=2, desc="Demo PV")
    server = spvirit.Server(pvs=[pv], port=TCP, udp_port=UDP, listen_ip="127.0.0.1")
    server.start()
    time.sleep(0.3)

    with Channel.connect("WIRE:TEMP", f"127.0.0.1:{TCP}") as ch:
        # 1. The type layer: what structure does the server say this PV has?
        desc = ch.introspect()
        print("--- introspection ---")
        print(f"struct_id: {desc.struct_id}   fields: {len(desc)}")
        print(desc.dump())

        # 2. A captured frame: the raw bytes of the GET response, decoded.
        result = ch.get()
        print("--- captured GET response frame ---")
        print(f"value      : {codec.format_value(result.value)}")
        print(f"raw frame  : {len(result.raw_pva)} bytes")
        print(f"first bytes: {hexdump(result.raw_pva)}")
        decoded = codec.decode_packet(result.raw_pva)
        print(f"header     : magic=0x{decoded['magic']:02x} version={decoded['version']} "
              f"command={decoded['command_name']} payload={decoded['payload_length']}B")
        print(f"flags      : {decoded['flags']}")
        print(f"details    : {sorted(decoded['details'].keys())}")

    # 3. Live frames: drive an introspection on a fresh channel, then read
    #    whatever the server sends next directly off the stream.
    print("--- live frames (read_until) ---")
    with Channel.connect("WIRE:TEMP", f"127.0.0.1:{TCP}") as ch2:
        ch2.introspect()
        try:
            pkt = ch2.read_until(lambda p: print(f"  saw {describe(p)}") or p.is_application,
                                 timeout=0.5, max_frames=16)
            print(f"matched: {describe(pkt)}")
            print(f"payload first bytes: {hexdump(pkt.payload)}")
        except Exception as e:  # quiet servers send nothing further — that's fine
            print(f"no further frames within 0.5 s ({e})")

    print("done.")


if __name__ == "__main__":
    main()
