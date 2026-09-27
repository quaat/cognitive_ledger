#!/usr/bin/env python3
"""Independent reference for the `sculpin-rdf-state/v1` candidate state digest (ADR-0018):
header line, then every canonical N-Quads line of the dataset in bytewise ascending order,
each LF-terminated; the digest is `sha256:` over those bytes. Fixtures under
`fixtures/golden/states/` are `<name>.nq` (canonical N-Quads lines, any order, duplicates
allowed — a dataset is a set) with `<name>.sha256`.

    generate   write <name>.sha256 for every .nq without one
    check      recompute every digest and fail on any difference
"""
from __future__ import annotations

import hashlib
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "fixtures" / "golden" / "states"
HEADER = b"sculpin-rdf-state/v1\n"


def canonical_bytes(lines: list[str]) -> bytes:
    quads = sorted({line.encode("utf-8") for line in lines if line != ""})
    return HEADER + b"".join(q + b"\n" for q in quads)


def digest(path: pathlib.Path) -> str:
    lines = path.read_text(encoding="utf-8").split("\n")
    return "sha256:" + hashlib.sha256(canonical_bytes(lines)).hexdigest()


def inputs() -> list[pathlib.Path]:
    return sorted(FIXTURES.glob("*.nq"))


def generate() -> int:
    n = 0
    for path in inputs():
        target = path.with_suffix(".sha256")
        if target.exists():
            continue
        target.write_text(digest(path) + "\n")
        n += 1
        print(f"wrote {target.name}")
    print(f"generated {n} digest(s)")
    return 0


def check() -> int:
    failures = 0
    for path in inputs():
        if digest(path) != path.with_suffix(".sha256").read_text().strip():
            failures += 1
            print(f"MISMATCH {path.name}", file=sys.stderr)
    if failures:
        return 1
    print(f"all {len(inputs())} state digest vectors match the reference")
    return 0


if __name__ == "__main__":
    sys.exit({"generate": generate, "check": check}[sys.argv[1] if len(sys.argv) > 1 else "check"]())
