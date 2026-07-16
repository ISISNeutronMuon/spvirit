"""pvfind — locate a PV and show its full introspection.

Given a PV name, find which server hosts it (UDP broadcast search, or a
directly specified address), connect, and print everything the server
reports about it: the structure layout, field table, and current value.

    python demo_pvfind.py PV_NAME               # locate via UDP search
    python demo_pvfind.py PV_NAME 10.0.0.5:5075 # or ask a specific server
"""
import sys

from spvirit import codec
from spvirit.lowlevel import Channel, search_pv


def walk(desc, indent=0):
    """Yield (name, type, is_array) rows for every field, depth-first."""
    for field in desc.fields:
        yield "  " * indent + field.name, field.field_type, field.is_array
        if field.struct_desc is not None:
            yield from walk(field.struct_desc, indent + 1)


def main() -> None:
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(1)
    pv_name = sys.argv[1]

    if len(sys.argv) > 2:
        addr = sys.argv[2]
        print(f"using server {addr}")
    else:
        print(f"searching for {pv_name} ...")
        addr = search_pv(pv_name)
        print(f"found on {addr}")

    with Channel.connect(pv_name, addr) as ch:
        desc = ch.introspect()

        print(f"\nPV        : {pv_name}")
        print(f"server    : {addr}  (sid={ch.sid})")
        print(f"struct_id : {desc.struct_id}")
        print(f"fields    : {len(desc)}\n")

        width = max(len(name) for name, _, _ in walk(desc))
        for name, ftype, is_array in walk(desc):
            suffix = "[]" if is_array and not ftype.endswith("[]") else ""
            print(f"  {name:<{width}}  {ftype}{suffix}")

        result = ch.get()
        print(f"\ncurrent value: {codec.format_value(result.value)}")


if __name__ == "__main__":
    main()
