#!/usr/bin/env python3
"""Independent reference encoder for `sculpin-ledger-request/v1` (canonical HTTP request
identity that scopes idempotency). Second implementation of the layout documented in
`crates/ledger-api/src/request_identity.rs`; vectors in `fixtures/golden/requests/`.

    generate   write <name>.hex/.sha256 for every request-*.input without them
    check      recompute every vector and fail on any difference
"""
from __future__ import annotations

import hashlib
import json
import pathlib
import struct
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from commit_v2_reference import normalize_time  # noqa: E402  (same timestamp rules)

ROOT = pathlib.Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "fixtures" / "golden" / "requests"
HEADER = b"sculpin-ledger-request/v1\0"


def field(value: str) -> bytes:
    raw = value.encode("utf-8")
    return struct.pack(">I", len(raw)) + raw


def optional(value: str | None) -> bytes:
    # An empty string carries no meaning and is normalized to absence (ADR-0015).
    if value is None or value == "":
        return b"\x00"
    return b"\x01" + field(value)


def patch_id(operations: list[dict]) -> str:
    """PatchId of the canonical requested patch (sculpin-rdf-patch-v1), assuming the
    fixture already lists canonical N-Quads; sorted by encoded line, duplicates collapse."""
    lines = sorted({("A " if op["op"] == "add" else "D ") + op["quad"] + "\n" for op in operations})
    body = "sculpin-rdf-patch-v1\n" + "".join(lines)
    return "sha256:" + hashlib.sha256(body.encode()).hexdigest()


def encode(req: dict) -> bytes:
    out = bytearray(HEADER)
    op = req["operation"]
    out += field(op)
    out += field(req["graph_id"])
    out += field(req["branch"])
    if op == "prepare":
        out += optional(req.get("expected_head"))
        out += field(patch_id(req["operations"]))
        out += field(req["activity"])
        et = req.get("event_time")
        out += optional(None if et is None else normalize_time(et))
        evidence = sorted(set(req.get("evidence_refs", [])), key=lambda e: e.encode())
        out += struct.pack(">I", len(evidence))
        for e in evidence:
            out += field(e)
        out += optional(req.get("source_system"))
        out += field(req["message"])
    elif op == "accept":
        out += optional(req.get("expected_head"))
        out += field(req["candidate"])
        out += optional(req.get("reason"))
        out += field(req.get("validation_policy", "no-validation"))
    elif op == "reject":
        out += field(req["candidate"])
        out += field(req["reason"])
    else:
        raise ValueError(op)
    return bytes(out)


def sha(data: bytes) -> str:
    return "sha256:" + hashlib.sha256(data).hexdigest()


def inputs():
    return sorted(FIXTURES.glob("request-*.input"))


def generate() -> int:
    n = 0
    for path in inputs():
        stem = path.with_suffix("")
        if stem.with_suffix(".hex").exists():
            continue
        data = encode(json.loads(path.read_text()))
        stem.with_suffix(".hex").write_text(data.hex() + "\n")
        stem.with_suffix(".sha256").write_text(sha(data) + "\n")
        n += 1
        print(f"wrote {stem.name}")
    print(f"generated {n} vector(s)")
    return 0


def check() -> int:
    failures = 0
    for path in inputs():
        stem = path.with_suffix("")
        data = encode(json.loads(path.read_text()))
        if data.hex() != stem.with_suffix(".hex").read_text().strip() or sha(data) != stem.with_suffix(".sha256").read_text().strip():
            failures += 1
            print(f"MISMATCH {path.name}", file=sys.stderr)
    if failures:
        return 1
    print(f"all {len(inputs())} request identity vectors match the reference encoder")
    return 0


if __name__ == "__main__":
    sys.exit({"generate": generate, "check": check}[sys.argv[1] if len(sys.argv) > 1 else "check"]())
