"""demo_source_rpc.py — RPC channel served by a Python source.

Defines an RPC channel ``RPC:add`` whose handler receives ``{a, b}`` and
returns ``a + b`` as an NTScalar.  Invoke from a PVA-aware client that
supports RPC (e.g. the EPICS ``pvcall`` tool) or from the spvirit
Python client once RPC is added.
"""

import time

import spvirit


class AdderRpcSource:
    """A single-channel RPC endpoint."""

    CHANNEL = "RPC:add"

    def claim(self, name: str):
        if name != self.CHANNEL:
            return None
        # The descriptor for an RPC channel describes the *request* structure,
        # though most clients rely on the RPC call itself not on claim's shape.
        return spvirit.PvInfo(
            struct_id="epics:nt/NTScalar:1.0",
            fields={"value": "double"},
            writable=False,
        )

    def get(self, name):
        # RPC channels usually don't have a scalar get; return a harmless zero.
        if name == self.CHANNEL:
            return spvirit.NtScalar(0.0)
        return None

    def put(self, name, value):
        raise RuntimeError("RPC:add does not accept PUT; use RPC instead")

    # ANCHOR: rpc
    # `rpc` is optional: a source that does not define it simply has no RPC.
    def rpc(self, name: str, args):
        if name != self.CHANNEL:
            raise RuntimeError(f"unknown RPC channel: {name}")
        # `args` is a Python dict built from the decoded request structure.
        a = _as_float(args.get("a", 0.0))
        b = _as_float(args.get("b", 0.0))
        return spvirit.NtScalar(a + b)
    # ANCHOR_END: rpc

    def names(self):
        return [self.CHANNEL]


def _as_float(v):
    if isinstance(v, (int, float)):
        return float(v)
    if isinstance(v, dict):
        for key in ("value", "val"):
            if key in v:
                return _as_float(v[key])
    return 0.0


def main() -> None:
    server = (
        spvirit.ServerBuilder()
        .add_source("rpc", 10, AdderRpcSource())
        .build()
    )
    server.start_background()

    print("RPC source server running on port 5075.")
    print("  RPC channel: RPC:add  — takes {a: double, b: double}, returns a+b")
    print("  Ctrl-C to exit.")

    try:
        while True:
            time.sleep(3600)
    except KeyboardInterrupt:
        print("\nbye.")


if __name__ == "__main__":
    main()
