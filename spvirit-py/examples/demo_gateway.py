"""Mini-gateway — proxy another PVAccess server under a renamed namespace.

Two servers run in this one process:

  upstream   serves DEV:TEMP (read-only) and DEV:SP (writable)
  gateway    claims GW:* dynamically and forwards to upstream, with a
             put allow-list: only GW:DEV:SP may be written through it

A client then talks to the gateway only:

    GW:DEV:TEMP   -> proxied read of DEV:TEMP
    GW:DEV:SP     -> proxied read/write of DEV:SP
    GW:DEV:TEMP   put -> rejected at the gateway

Run it:

    python demo_gateway.py
"""
import time

import spvirit

UP_TCP, UP_UDP = 15315, 15316
GW_TCP, GW_UDP = 15325, 15326

PREFIX = "GW:"
PUT_ALLOWED = {"DEV:SP"}


class GatewaySource:
    """Forward claim/get/put for PREFIX-named PVs to the upstream server."""

    def __init__(self, upstream: spvirit.Client):
        self.upstream = upstream

    def _upstream_name(self, name):
        return name[len(PREFIX):] if name.startswith(PREFIX) else None

    def claim(self, name):
        up = self._upstream_name(name)
        if up is None:
            return None
        try:
            self.upstream.get(up, fields="value")   # claim only what exists
        except spvirit.SpviritError:
            return None
        return spvirit.PvInfo.nt_scalar("double", writable=up in PUT_ALLOWED)

    def get(self, name):
        up = self._upstream_name(name)
        value = self.upstream.get(up).value["value"]
        return spvirit.NtScalar(value)

    def put(self, name, value):
        up = self._upstream_name(name)
        if up not in PUT_ALLOWED:
            raise PermissionError(f"puts to {name} are blocked at the gateway")
        # `value` arrives as the decoded wire structure (a dict for NTScalar).
        scalar = value.get("value") if isinstance(value, dict) else value
        self.upstream.put(up, scalar)
        return spvirit.NtScalar(scalar)             # propagate to gateway monitors


def main() -> None:
    # ── upstream ─────────────────────────────────────────────────────────
    temp = spvirit.ai("DEV:TEMP", 21.5, units="degC")
    sp = spvirit.ao("DEV:SP", 20.0, units="degC")
    upstream_server = spvirit.Server(pvs=[temp, sp], port=UP_TCP, udp_port=UP_UDP,
                                     listen_ip="127.0.0.1")
    upstream_server.start()
    time.sleep(0.3)

    # ── gateway ──────────────────────────────────────────────────────────
    upstream_client = (spvirit.Client.builder()
                       .server_addr(f"127.0.0.1:{UP_TCP}").udp_port(UP_UDP).build())
    gateway_server = spvirit.Server(sources=[("gateway", 10, GatewaySource(upstream_client))],
                                    port=GW_TCP, udp_port=GW_UDP, listen_ip="127.0.0.1")
    gateway_server.start()
    time.sleep(0.3)

    # ── a client that only knows the gateway ────────────────────────────
    client = (spvirit.Client.builder()
              .server_addr(f"127.0.0.1:{GW_TCP}").udp_port(GW_UDP).build())

    print(f"GW:DEV:TEMP = {client.get('GW:DEV:TEMP').value['value']}")

    client.put("GW:DEV:SP", 22.5)
    print(f"GW:DEV:SP  <- 22.5 (allowed), upstream now reads {sp.get()}")

    try:
        client.put("GW:DEV:TEMP", 99.0)
        print("ERROR: put to GW:DEV:TEMP should have been blocked")
    except spvirit.SpviritError as e:
        print(f"GW:DEV:TEMP <- 99.0 blocked by the gateway: {e}")
    print(f"upstream DEV:TEMP untouched: {temp.get()}")


if __name__ == "__main__":
    main()
