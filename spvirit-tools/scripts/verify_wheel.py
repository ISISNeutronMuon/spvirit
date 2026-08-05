#!/usr/bin/env python3
"""Assert that a built spvirit-tools wheel actually contains the tools.

The failure this guards against is quiet: if maturin's ``bindings = "bin"``
setting is lost, or a Cargo feature that gates the binaries is switched off,
the build still succeeds and produces a wheel. That wheel installs cleanly and
delivers nothing -- no error, no missing-file traceback, just an environment
where ``spget`` is not a command. CI would stay green.

Usage:

    python spvirit-tools/scripts/verify_wheel.py path/to/dist
"""

from __future__ import annotations

import sys
import zipfile
from pathlib import Path

# Every binary declared in spvirit-tools/Cargo.toml. Kept in step with the
# tool table in docs/book/src/04-tools/index.md.
EXPECTED = {
    "spget",
    "spput",
    "spmonitor",
    "spinfo",
    "splist",
    "spexplore",
    "spsearch",
    "spsine",
    "spget_compare",
    "spserver",
    "sptable",
    "spdodeca",
}


def scripts_in(wheel: Path) -> set[str]:
    """Names of the executables a wheel installs onto PATH."""
    names = set()
    with zipfile.ZipFile(wheel) as zf:
        for entry in zf.namelist():
            parts = entry.split("/")
            # Wheel data scripts live in `<name>-<version>.data/scripts/<exe>`.
            if len(parts) == 3 and parts[0].endswith(".data") and parts[1] == "scripts":
                if parts[2]:
                    names.add(Path(parts[2]).stem)
    return names


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2

    dist = Path(sys.argv[1])
    wheels = sorted(dist.glob("*.whl"))
    if not wheels:
        print(f"no wheel found in {dist}", file=sys.stderr)
        return 1

    failed = False
    for wheel in wheels:
        found = scripts_in(wheel)
        missing = EXPECTED - found
        if missing:
            failed = True
            print(
                f"{wheel.name}: missing {len(missing)} of {len(EXPECTED)} tools: "
                f"{', '.join(sorted(missing))}",
                file=sys.stderr,
            )
            continue

        extra = found - EXPECTED
        note = f" (plus {', '.join(sorted(extra))})" if extra else ""
        print(f"{wheel.name}: all {len(EXPECTED)} tools present{note}")

    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
