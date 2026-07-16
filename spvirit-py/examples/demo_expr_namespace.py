"""Computed PV namespace — PVs that exist only when you ask for them.

A source claims every name of the form ``EXPR:<expression>`` and evaluates
the expression on each get, so the server offers an infinite namespace:

    pvget "EXPR:2*sin(0.4)+1"
    pvget "EXPR:sqrt(2)/2"
    pvget "EXPR:tau"

The expression is evaluated in a sandbox exposing only ``math`` functions
and constants — no builtins, no names beyond the whitelist.

Run it (serves until Ctrl-C after the self-test):

    python demo_expr_namespace.py
"""
import math
import time

import spvirit

TCP, UDP = 15335, 15336
PREFIX = "EXPR:"

SAFE_NAMES = {name: getattr(math, name) for name in dir(math) if not name.startswith("_")}
SAFE_NAMES.update(abs=abs, min=min, max=max, round=round)


def evaluate(expression: str) -> float:
    return float(eval(expression, {"__builtins__": {}}, dict(SAFE_NAMES)))  # noqa: S307


class ExprSource:
    def claim(self, name):
        if not name.startswith(PREFIX):
            return None
        try:
            evaluate(name[len(PREFIX):])
        except Exception:
            return None                     # not a valid expression: decline
        return spvirit.PvInfo.nt_scalar("double")

    def get(self, name):
        return spvirit.NtScalar(evaluate(name[len(PREFIX):]))

    def put(self, name, value):
        raise PermissionError("computed PVs are read-only")


def main() -> None:
    server = spvirit.Server(sources=[("expr", 10, ExprSource())],
                            port=TCP, udp_port=UDP, listen_ip="127.0.0.1")
    server.start()
    time.sleep(0.3)

    client = (spvirit.Client.builder()
              .server_addr(f"127.0.0.1:{TCP}").udp_port(UDP).build())
    for expression in ["2*sin(0.4)+1", "sqrt(2)/2", "tau", "log(e)"]:
        value = client.get(PREFIX + expression).value["value"]
        print(f"{PREFIX}{expression:<14} = {value}")

    print(f"\nserving on 127.0.0.1:{TCP} (UDP {UDP}) - try:")
    print(f'    pvget "{PREFIX}hypot(3,4)"')
    print("Ctrl-C to stop.")
    try:
        while True:
            time.sleep(1.0)
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
