#!/usr/bin/env python3
"""Independent reference encoder for `sculpin-semantic-context/v1` and
`sculpin-validation-record/v1` (ADR-0018). Written from the ADR's layout, not from the Rust
encoder, so the vectors under `fixtures/golden/validation/` are pinned by two implementations.

    generate   write <name>.hex/.sha256 for every context-*/record-*.input without them
    check      recompute every vector (positive and negative) and fail on any difference
    negatives  write the *-invalid-*.hex fixtures (bytes the decoders MUST reject)

Golden files are never rewritten; a protocol change is a new version with new vectors.
"""
from __future__ import annotations

import hashlib
import json
import pathlib
import re
import struct
import sys
import unicodedata

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from commit_v2_reference import normalize_time  # noqa: E402  (same timestamp rules)

ROOT = pathlib.Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "fixtures" / "golden" / "validation"
CONTEXT_HEADER = b"sculpin-semantic-context-v1\0"
RECORD_HEADER = b"sculpin-validation-record-v1\0"
MAX_IDENTIFIER_BYTES = 512
MAX_SET = 64
MAX_SEVERITY_BYTES = 64
MAX_MESSAGE_BYTES = 1024
MAX_REPORT_REFERENCE_BYTES = 2048
GRAPH_ID_RE = re.compile(r"^[A-Za-z0-9._:-]{1,128}$")
CONTENT_ID_RE = re.compile(r"^sha256:[0-9a-f]{64}$")


class Invalid(ValueError):
    pass


def token(name: str, value: str, max_bytes: int = MAX_IDENTIFIER_BYTES) -> str:
    if not isinstance(value, str) or value == "":
        raise Invalid(f"{name}: must be a non-empty string")
    if len(value.encode("utf-8")) > max_bytes:
        raise Invalid(f"{name}: exceeds {max_bytes} bytes")
    if any(unicodedata.category(c) == "Cc" for c in value):
        raise Invalid(f"{name}: control character")
    return value


def content_id(name: str, value: str) -> str:
    if not CONTENT_ID_RE.fullmatch(value):
        raise Invalid(f"{name}: not a strict content id")
    return value


def field(value: str) -> bytes:
    raw = value.encode("utf-8")
    return struct.pack(">I", len(raw)) + raw


def opt(value: str | None) -> bytes:
    if value is None:
        return b"\x00"
    if value == "":
        raise Invalid("optional field present but empty")
    return b"\x01" + field(value)


def counted_set(elements: list[bytes], name: str) -> bytes:
    unique = sorted(set(elements))
    if len(unique) > MAX_SET:
        raise Invalid(f"more than {MAX_SET} {name}")
    return struct.pack(">I", len(unique)) + b"".join(unique)


def virtual_context(vc: dict) -> bytes:
    allowed = {"dataset_id", "source_version", "object_refs", "query_spec_digest", "hydration_plan_digest"}
    if set(vc) - allowed:
        raise Invalid(f"unknown virtual context fields {sorted(set(vc) - allowed)}")
    out = field(token("dataset_id", vc["dataset_id"])) + field(token("source_version", vc["source_version"]))
    refs = [field(token("object_ref", r)) for r in vc.get("object_refs", [])]
    out += counted_set(refs, "object references")
    out += field(content_id("query_spec_digest", vc["query_spec_digest"]))
    out += field(content_id("hydration_plan_digest", vc["hydration_plan_digest"]))
    return out


def encode_context(logical: dict) -> bytes:
    allowed = {"graph_id", "candidate_commit", "candidate_state_digest", "base_kb", "ontology",
               "shapes", "reasoning", "virtual_contexts", "validator"}
    if set(logical) - allowed:
        raise Invalid(f"unknown context fields {sorted(set(logical) - allowed)}")
    if not GRAPH_ID_RE.fullmatch(logical["graph_id"]):
        raise Invalid("graph_id")
    out = bytearray(CONTEXT_HEADER)
    out += field(logical["graph_id"])
    out += field(content_id("candidate_commit", logical["candidate_commit"]))
    out += field(content_id("candidate_state_digest", logical["candidate_state_digest"]))
    kb = logical["base_kb"]
    out += field(token("base_kb.kb_id", kb["kb_id"])) + field(token("base_kb.revision", kb["revision"]))
    ontology = logical.get("ontology")
    if ontology is None:
        out += b"\x00"
    else:
        out += b"\x01" + field(token("ontology.id", ontology["id"])) + field(token("ontology.version", ontology["version"]))
    shapes = logical["shapes"]
    out += field(token("shapes.id", shapes["id"])) + field(token("shapes.version", shapes["version"]))
    reasoning = logical["reasoning"]
    out += field(token("reasoning.profile", reasoning["profile"]))
    out += field(token("reasoning.implementation", reasoning["implementation"]))
    out += field(token("reasoning.version", reasoning["version"]))
    out += counted_set([virtual_context(vc) for vc in logical.get("virtual_contexts", [])], "virtual contexts")
    validator = logical["validator"]
    out += field(token("validator.service_id", validator["service_id"]))
    out += field(token("validator.service_version", validator["service_version"]))
    out += field(token("validator.configuration_version", validator["configuration_version"]))
    return bytes(out)


def violation(v: dict) -> bytes:
    allowed = {"severity", "code", "message"}
    if set(v) - allowed:
        raise Invalid(f"unknown violation fields {sorted(set(v) - allowed)}")
    message = v.get("message", "")
    if len(message.encode("utf-8")) > MAX_MESSAGE_BYTES or any(unicodedata.category(c) == "Cc" for c in message):
        raise Invalid("violation.message")
    return field(token("violation.severity", v["severity"], MAX_SEVERITY_BYTES)) + field(token("violation.code", v["code"])) + field(message)


def encode_record(logical: dict, *, summary_override: list[bytes] | None = None,
                  raw_recorded_at: str | None = None) -> bytes:
    allowed = {"graph_id", "candidate_commit", "candidate_state_digest", "semantic_execution_context_id",
               "validator", "outcome", "recorded_at", "report_digest", "report_reference"}
    if set(logical) - allowed:
        raise Invalid(f"unknown record fields {sorted(set(logical) - allowed)}")
    if not GRAPH_ID_RE.fullmatch(logical["graph_id"]):
        raise Invalid("graph_id")
    out = bytearray(RECORD_HEADER)
    out += field(logical["graph_id"])
    out += field(content_id("candidate_commit", logical["candidate_commit"]))
    out += field(content_id("candidate_state_digest", logical["candidate_state_digest"]))
    out += field(content_id("semantic_execution_context_id", logical["semantic_execution_context_id"]))
    validator = logical["validator"]
    out += field(token("validator.service_id", validator["service_id"]))
    out += field(token("validator.service_version", validator["service_version"]))
    out += field(token("validator.configuration_version", validator["configuration_version"]))
    outcome = logical["outcome"]
    kind = outcome["kind"]
    count = int(outcome.get("violation_count", 0))
    summary = sorted({violation(v) for v in outcome.get("violations", [])})
    if kind == "conforms":
        if count != 0 or summary:
            raise Invalid("conforming outcome with violations")
        out += b"\x00"
    elif kind == "violations":
        if count < 1:
            raise Invalid("violations outcome needs violation_count >= 1")
        out += b"\x01"
    else:
        raise Invalid(f"unknown outcome kind {kind!r}")
    if len(summary) > MAX_SET or len(summary) > count:
        raise Invalid("summary exceeds cap or count")
    out += struct.pack(">I", count)
    if summary_override is not None:  # negatives only
        summary = summary_override
    out += struct.pack(">I", len(summary)) + b"".join(summary)
    if raw_recorded_at is not None:  # negatives only
        out += field(raw_recorded_at)
    else:
        out += field(normalize_time(logical["recorded_at"]))
    out += field(content_id("report_digest", logical["report_digest"]))
    reference = logical.get("report_reference")
    if reference is not None:
        token("report_reference", reference, MAX_REPORT_REFERENCE_BYTES)
    out += opt(reference)
    return bytes(out)


def sha(data: bytes) -> str:
    return "sha256:" + hashlib.sha256(data).hexdigest()


def encode(path: pathlib.Path) -> bytes:
    logical = json.loads(path.read_text())
    if path.name.startswith("context-"):
        return encode_context(logical)
    if path.name.startswith("record-"):
        return encode_record(logical)
    raise Invalid(f"unknown fixture kind {path.name}")


def inputs() -> list[pathlib.Path]:
    return sorted(FIXTURES.glob("*.input"))


def negative_cases() -> dict[str, bytes]:
    record = json.loads((FIXTURES / "record-v1-violations.input").read_text())
    context = json.loads((FIXTURES / "context-v1-basic.input").read_text())
    cases: dict[str, bytes] = {}
    summary = sorted({violation(v) for v in record["outcome"]["violations"]})
    cases["record-v1-invalid-unsorted-summary"] = encode_record(record, summary_override=list(reversed(summary)))
    cases["record-v1-invalid-duplicate-summary"] = encode_record(record, summary_override=[summary[0], summary[0]])
    cases["record-v1-invalid-noncanonical-time"] = encode_record(record, raw_recorded_at=record["recorded_at"].replace(".000000Z", "Z") if ".000000Z" in record["recorded_at"] else "2026-09-27T12:00:00Z")
    good = encode_record(record)
    cases["record-v1-invalid-trailing-bytes"] = good + b"\x00"
    # outcome byte 0 (conforms) with a nonzero count: the count follows the byte directly.
    conforms_count = bytearray(good)
    marker = field(record["validator"]["configuration_version"])
    at = good.find(marker) + len(marker)
    assert conforms_count[at] == 1
    conforms_count[at] = 0
    cases["record-v1-invalid-conforms-with-violations"] = bytes(conforms_count)
    ctx = encode_context(context)
    cases["context-v1-invalid-trailing-bytes"] = ctx + b"\x00"
    cases["context-v1-invalid-unknown-version"] = b"sculpin-semantic-context-v2\0" + ctx[len(CONTEXT_HEADER):]
    # virtual contexts out of order on the wire
    encoded = sorted({virtual_context(vc) for vc in context["virtual_contexts"]})
    assert len(encoded) >= 2
    joined = b"".join(encoded)
    at = ctx.find(joined)
    assert at > 0
    cases["context-v1-invalid-unsorted-virtual-contexts"] = ctx[:at] + b"".join(reversed(encoded)) + ctx[at + len(joined):]
    # ontology present-but-empty id
    no_ontology = dict(context, ontology=None)
    absent = encode_context(no_ontology)
    prefix = len(CONTEXT_HEADER) + len(field(context["graph_id"])) + len(field(context["candidate_commit"])) \
        + len(field(context["candidate_state_digest"])) + len(field(context["base_kb"]["kb_id"])) + len(field(context["base_kb"]["revision"]))
    assert absent[prefix] == 0
    cases["context-v1-invalid-empty-ontology"] = absent[:prefix] + b"\x01" + field("") + field("1") + absent[prefix + 1:]
    return cases


def generate() -> int:
    n = 0
    for path in inputs():
        stem = path.with_suffix("")
        if stem.with_suffix(".hex").exists() or stem.with_suffix(".sha256").exists():
            continue
        data = encode(path)
        stem.with_suffix(".hex").write_text(data.hex() + "\n")
        stem.with_suffix(".sha256").write_text(sha(data) + "\n")
        n += 1
        print(f"wrote {stem.name}")
    print(f"generated {n} vector(s)")
    return 0


def negatives() -> int:
    for name, data in negative_cases().items():
        path = FIXTURES / f"{name}.hex"
        if path.exists():
            continue
        path.write_text(data.hex() + "\n")
        print(f"wrote {path.name}")
    return 0


def check() -> int:
    failures = 0
    count = 0
    for path in inputs():
        stem = path.with_suffix("")
        data = encode(path)
        count += 1
        if data.hex() != stem.with_suffix(".hex").read_text().strip() or sha(data) != stem.with_suffix(".sha256").read_text().strip():
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
    print(f"all {count} validation protocol vectors (positive and negative) match the reference encoder")
    return 0


if __name__ == "__main__":
    mode = sys.argv[1] if len(sys.argv) > 1 else "check"
    sys.exit({"generate": generate, "check": check, "negatives": negatives}[mode]())
