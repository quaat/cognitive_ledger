#!/usr/bin/env python3
"""Independent reference encoder for `sculpin-cognitive-commit/v2` (ADR-0009).

This is a second implementation of the canonical byte layout documented in
`docs/design/canonicalization.md`, deliberately written without looking at the Rust
encoder's internals, so the golden vectors under `fixtures/golden/commits/v2-*` are
cross-checked by two implementations rather than pinned by one.

    generate   write <name>.hex and <name>.sha256 for every v2-*.input that has none
    check      recompute every vector and fail if any committed .hex/.sha256 differs
    negatives  write the v2-invalid-*.hex fixtures (bytes the decoder MUST reject)

Golden files are never rewritten by `generate`; delete them explicitly if a protocol
version change (a new ADR) requires new vectors.
"""
from __future__ import annotations

import hashlib
import json
import pathlib
import re
import struct
import sys
import unicodedata
from datetime import datetime, timedelta, timezone

ROOT = pathlib.Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "fixtures" / "golden" / "commits"

HEADER = b"sculpin-cognitive-commit-v2\0"
PRINCIPAL_TYPES = {"human": 0, "agent": 1, "service": 2}
MAX_IDENTIFIER_BYTES = 512
MAX_GRAPH_ID_BYTES = 128
MAX_MESSAGE_BYTES = 4096
MAX_EVIDENCE_REFS = 64
GRAPH_ID_RE = re.compile(r"^[A-Za-z0-9._:-]{1,128}$")
RFC3339_RE = re.compile(
    r"^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})(?:\.(\d{1,9}))?(Z|[+-]\d{2}:\d{2})$"
)
CONTENT_ID_RE = re.compile(r"^sha256:[0-9a-f]{64}$")


class Invalid(ValueError):
    pass


def token(field: str, value: str, max_bytes: int = MAX_IDENTIFIER_BYTES) -> str:
    if not isinstance(value, str) or value == "":
        raise Invalid(f"{field}: must be a non-empty string")
    if len(value.encode("utf-8")) > max_bytes:
        raise Invalid(f"{field}: exceeds {max_bytes} bytes")
    if any(unicodedata.category(c) == "Cc" for c in value):
        raise Invalid(f"{field}: control character")
    return value


def content_id(field: str, value: str) -> str:
    if not CONTENT_ID_RE.fullmatch(value):
        raise Invalid(f"{field}: not a strict content id")
    return value


def normalize_time(value: str) -> str:
    """RFC 3339 → `YYYY-MM-DDTHH:MM:SS.ffffffZ` (UTC, microseconds, truncated)."""
    m = RFC3339_RE.fullmatch(value)
    if not m or not value.isascii():
        raise Invalid(f"timestamp {value!r}: not RFC 3339 with uppercase T/Z")
    y, mo, d, h, mi, s, frac, off = m.groups()
    if s == "60":
        raise Invalid("leap second")
    micro = int((frac or "0").ljust(9, "0")[:6])
    if off == "Z":
        tz = timezone.utc
    else:
        sign = 1 if off[0] == "+" else -1
        oh, om = int(off[1:3]), int(off[4:6])
        if oh > 23 or om > 59:
            raise Invalid(f"timestamp {value!r}: offset out of range")
        tz = timezone(sign * timedelta(hours=oh, minutes=om))
    try:
        dt = datetime(int(y), int(mo), int(d), int(h), int(mi), int(s), micro, tz)
        dt = dt.astimezone(timezone.utc)
    except (ValueError, OverflowError) as exc:
        raise Invalid(f"timestamp {value!r}: {exc}") from exc
    if not 1 <= dt.year <= 9999:
        raise Invalid(f"timestamp {value!r}: UTC year outside 0001..=9999")
    return dt.strftime("%Y-%m-%dT%H:%M:%S.%f") + "Z"


def field(value: str) -> bytes:
    raw = value.encode("utf-8")
    return struct.pack(">I", len(raw)) + raw


def optional(value: str | None) -> bytes:
    if value is None:
        return b"\x00"
    if value == "":
        raise Invalid("optional field present but empty")
    return b"\x01" + field(value)


def encode(
    logical: dict,
    *,
    evidence_override: list[str] | None = None,
    parents_override: list[str] | None = None,
    raw_event_time: str | None = None,
    raw_source_system: str | None = None,
) -> bytes:
    """Encode a logical commit. The `*_override` / `raw_*` arguments bypass validation and
    exist only to manufacture negative fixtures; `generate`/`check` never use them."""
    allowed = {"graph_id", "parents", "patch", "actor", "activity", "event_time",
               "recorded_at", "evidence_refs", "source_system", "message"}
    unknown = set(logical) - allowed
    if unknown:
        raise Invalid(f"unknown envelope fields {sorted(unknown)} (correlation_id is not part of v2)")
    graph_id = logical["graph_id"]
    if not GRAPH_ID_RE.fullmatch(graph_id):
        raise Invalid("graph_id")
    parents = [content_id("parent", p) for p in logical["parents"]]
    if len(parents) > 2 or len(set(parents)) != len(parents):
        raise Invalid("parents")
    if parents_override is not None:  # negatives only
        parents = parents_override
    actor = logical["actor"]
    token("activity", logical["activity"])
    evidence = [token("evidence_ref", e) for e in logical.get("evidence_refs", [])]
    canonical_evidence = sorted(set(evidence), key=lambda e: e.encode("utf-8"))
    if len(canonical_evidence) > MAX_EVIDENCE_REFS:
        raise Invalid("too many evidence refs")
    if evidence_override is not None:  # negatives only
        canonical_evidence = evidence_override
    message = logical["message"]
    if len(message.encode("utf-8")) > MAX_MESSAGE_BYTES:
        raise Invalid("message too long")
    source_system = logical.get("source_system")
    if source_system is not None:
        token("source_system", source_system)
    on_behalf_of = actor.get("on_behalf_of")
    if on_behalf_of is not None:
        token("on_behalf_of", on_behalf_of)
    event_time = logical.get("event_time")

    out = bytearray(HEADER)
    out += field(graph_id)
    out += struct.pack(">I", len(parents))
    for p in parents:
        out += field(p)
    out += field(content_id("patch", logical["patch"]))
    out += field(token("principal_id", actor["principal_id"]))
    out += bytes([PRINCIPAL_TYPES[actor["principal_type"]]])
    out += optional(on_behalf_of)
    out += field(logical["activity"])
    if raw_event_time is not None:  # negatives only
        out += b"\x01" + field(raw_event_time)
    else:
        out += optional(None if event_time is None else normalize_time(event_time))
    out += field(normalize_time(logical["recorded_at"]))
    out += struct.pack(">I", len(canonical_evidence))
    for e in canonical_evidence:
        out += field(e)
    if raw_source_system is not None:  # negatives only
        out += b"\x01" + field(raw_source_system)
    else:
        out += optional(source_system)
    out += field(message)
    return bytes(out)


def sha(data: bytes) -> str:
    return "sha256:" + hashlib.sha256(data).hexdigest()


def inputs() -> list[pathlib.Path]:
    return sorted(FIXTURES.glob("v2-*.input"))


def generate() -> int:
    written = 0
    for path in inputs():
        stem = path.with_suffix("")
        hex_path, sha_path = stem.with_suffix(".hex"), stem.with_suffix(".sha256")
        if hex_path.exists() or sha_path.exists():
            continue
        data = encode(json.loads(path.read_text()))
        hex_path.write_text(data.hex() + "\n")
        sha_path.write_text(sha(data) + "\n")
        written += 1
        print(f"wrote {hex_path.name} {sha_path.name}")
    print(f"generated {written} vector(s)")
    return 0


def check() -> int:
    failures = 0
    count = 0
    for path in inputs():
        stem = path.with_suffix("")
        data = encode(json.loads(path.read_text()))
        expected_hex = stem.with_suffix(".hex").read_text().strip()
        expected_sha = stem.with_suffix(".sha256").read_text().strip()
        count += 1
        if data.hex() != expected_hex or sha(data) != expected_sha:
            failures += 1
            print(f"MISMATCH {path.name}", file=sys.stderr)
    for name, data in negative_cases().items():
        count += 1
        if (FIXTURES / f"{name}.hex").read_text().strip() != data.hex():
            failures += 1
            print(f"MISMATCH {name}.hex", file=sys.stderr)
    if failures:
        print(f"{failures} of {count} vectors differ", file=sys.stderr)
        return 1
    print(f"all {count} commit v2 vectors (positive and negative) match the reference encoder")
    return 0


def negative_cases() -> dict[str, bytes]:
    base = json.loads((FIXTURES / "v2-linear.input").read_text())
    linear = encode(base)
    evidence_sorted = sorted(set(base["evidence_refs"]), key=lambda e: e.encode())
    cases: dict[str, bytes] = {}
    # 1. evidence not sorted (decoder must reject; encoder would have sorted it).
    cases["v2-invalid-unsorted-evidence"] = encode(base, evidence_override=list(reversed(evidence_sorted)))
    # 2. duplicate evidence on the wire.
    cases["v2-invalid-duplicate-evidence"] = encode(base, evidence_override=[evidence_sorted[0], evidence_sorted[0]] + evidence_sorted[1:])
    # 3. present-but-empty on_behalf_of: splice tag 0x01 + len 0 where the tag byte sits.
    absent = dict(base, actor=dict(base["actor"], on_behalf_of=None))
    absent_bytes = encode(absent)
    prefix_len = len(HEADER) + len(field(base["graph_id"])) + 4 + sum(len(field(p)) for p in base["parents"]) \
        + len(field(base["patch"])) + len(field(base["actor"]["principal_id"])) + 1
    assert absent_bytes[prefix_len] == 0
    cases["v2-invalid-empty-on-behalf-of"] = absent_bytes[:prefix_len] + b"\x01" + struct.pack(">I", 0) + absent_bytes[prefix_len + 1:]
    # 4. non-canonical timestamp inside the bytes (no fractional digits).
    noncanonical = linear.replace(field(normalize_time(base["recorded_at"])), field(base["recorded_at"].replace(".000000Z", "Z")), 1)
    assert noncanonical != linear
    cases["v2-invalid-noncanonical-time"] = noncanonical
    # 5. unknown principal_type byte.
    bad_type = bytearray(linear)
    assert bad_type[prefix_len - 1] == PRINCIPAL_TYPES[base["actor"]["principal_type"]]
    bad_type[prefix_len - 1] = 7
    cases["v2-invalid-principal-type"] = bytes(bad_type)
    # 6. trailing byte.
    cases["v2-invalid-trailing-bytes"] = linear + b"\x00"
    # 7. unknown envelope version: fails closed through dual read.
    cases["v2-invalid-unknown-version"] = b"sculpin-cognitive-commit-v3\0" + linear[len(HEADER):]
    # 8/9. present-but-empty event_time and source_system.
    cases["v2-invalid-empty-event-time"] = encode(base, raw_event_time="")
    cases["v2-invalid-empty-source-system"] = encode(base, raw_source_system="")
    # 10. non-canonical event_time (offset form inside the bytes).
    cases["v2-invalid-noncanonical-event-time"] = encode(base, raw_event_time="2026-09-24T13:00:00.000000+02:00")
    # 11/12. duplicate parents; three parents.
    p0 = base["parents"][0]
    cases["v2-invalid-duplicate-parents"] = encode(base, parents_override=[p0, p0])
    cases["v2-invalid-three-parents"] = encode(base, parents_override=[p0, sha(b"x"), sha(b"y")])
    # 13. more than 64 evidence references.
    cases["v2-invalid-too-many-evidence"] = encode(base, evidence_override=[f"urn:e:{i:03}" for i in range(MAX_EVIDENCE_REFS + 1)])
    return cases


def negatives() -> int:
    for name, data in negative_cases().items():
        path = FIXTURES / f"{name}.hex"
        if path.exists():
            continue
        path.write_text(data.hex() + "\n")
        print(f"wrote {path.name}")
    return 0


if __name__ == "__main__":
    mode = sys.argv[1] if len(sys.argv) > 1 else "check"
    sys.exit({"generate": generate, "check": check, "negatives": negatives}[mode]())
