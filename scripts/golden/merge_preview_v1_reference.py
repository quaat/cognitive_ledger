#!/usr/bin/env python3
"""Independent reference encoder for `sculpin-ledger-merge-preview/v1` (ADR-0024).

Every `fixtures/golden/merge-preview/*.input` (JSON) has `.hex` (canonical bytes) and
`.token` (`sha256:<hex>`). `generate` writes missing vectors; `check` verifies all.
"""
import hashlib
import json
import pathlib
import struct
import sys

FIXTURES = pathlib.Path(__file__).resolve().parents[2] / "fixtures" / "golden" / "merge-preview"
HEADER = b"sculpin-ledger-merge-preview/v1\0"
ALGORITHM = "structural-slot/v1"
CLASSIFICATION = {"fast_forward": 1, "divergent": 2}
STRATEGY = {"abort": 0, "take-target": 1, "take-source": 2, "union": 3}


def field(value: str) -> bytes:
    raw = value.encode()
    return struct.pack(">I", len(raw)) + raw


def encode(v: dict) -> bytes:
    out = bytearray(HEADER)
    for name in ("graph_id", "source_branch", "source_head", "target_branch", "target_head", "merge_base"):
        out += field(v[name])
    out.append(CLASSIFICATION[v["classification"]])
    # A fast-forward cannot conflict: its strategy is normalized to abort.
    strategy = "abort" if v["classification"] == "fast_forward" else v["strategy"]
    out.append(STRATEGY[strategy])
    out += field(ALGORITHM)
    out += field(v["merged_state_digest"])
    return bytes(out)


def token(data: bytes) -> str:
    return "sha256:" + hashlib.sha256(data).hexdigest()


def inputs():
    return sorted(FIXTURES.glob("*.input"))


def generate() -> int:
    for path in inputs():
        stem = path.with_suffix("")
        if stem.with_suffix(".hex").exists():
            continue
        data = encode(json.loads(path.read_text()))
        stem.with_suffix(".hex").write_text(data.hex() + "\n")
        stem.with_suffix(".token").write_text(token(data) + "\n")
        print(f"wrote {stem.name}")
    return 0


def check() -> int:
    bad = 0
    for path in inputs():
        stem = path.with_suffix("")
        data = encode(json.loads(path.read_text()))
        if data.hex() != stem.with_suffix(".hex").read_text().strip() or token(data) != stem.with_suffix(".token").read_text().strip():
            bad += 1
            print(f"MISMATCH {path.name}", file=sys.stderr)
    if bad:
        return 1
    print(f"all {len(inputs())} merge preview-token vectors match the reference encoder")
    return 0


if __name__ == "__main__":
    sys.exit({"generate": generate, "check": check}[sys.argv[1] if len(sys.argv) > 1 else "check"]())
