#!/usr/bin/env python3
"""Independent reference encoder for `sculpin-semantic-context/v1`,
`sculpin-semantic-environment/v1` and `sculpin-validation-record/v1` (ADR-0018). Written from the ADR's layout, not from the Rust
encoder, so the vectors under `fixtures/golden/validation/` are pinned by two implementations.

    generate   write <name>.hex/.sha256 for every context-*/environment-*/record-*.input
               without them
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
ENVIRONMENT_HEADER = b"sculpin-semantic-environment-v1\0"
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


def keys(name: str, value: dict, allowed: set[str], required: set[str] | None = None) -> dict:
    if not isinstance(value, dict):
        raise Invalid(f"{name}: must be an object")
    if set(value) - allowed:
        raise Invalid(f"{name}: unknown fields {sorted(set(value) - allowed)}")
    missing = (allowed if required is None else required) - set(value)
    if missing:
        raise Invalid(f"{name}: missing fields {sorted(missing)}")
    return value


def u32(name: str, value) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or not 0 <= value <= 0xFFFFFFFF:
        raise Invalid(f"{name}: must be an integer in u32 range")
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
    keys("virtual_context", vc, {"dataset_id", "source_version", "object_refs", "query_spec_digest", "hydration_plan_digest"},
         {"dataset_id", "source_version", "query_spec_digest", "hydration_plan_digest"})
    out = field(token("dataset_id", vc["dataset_id"])) + field(token("source_version", vc["source_version"]))
    refs = [field(token("object_ref", r)) for r in vc.get("object_refs", [])]
    out += counted_set(refs, "object references")
    out += field(content_id("query_spec_digest", vc["query_spec_digest"]))
    out += field(content_id("hydration_plan_digest", vc["hydration_plan_digest"]))
    return out


def semantics(logical: dict) -> bytes:
    """base_kb · ontology tag · shapes · reasoning tag — shared by context and environment."""
    kb = keys("base_kb", logical["base_kb"], {"kb_id", "revision"})
    out = field(token("base_kb.kb_id", kb["kb_id"])) + field(token("base_kb.revision", kb["revision"]))
    ontology = logical.get("ontology")
    if ontology is None:
        out += b"\x00"
    else:
        keys("ontology", ontology, {"id", "version"})
        out += b"\x01" + field(token("ontology.id", ontology["id"])) + field(token("ontology.version", ontology["version"]))
    shapes = keys("shapes", logical["shapes"], {"id", "version"})
    out += field(token("shapes.id", shapes["id"])) + field(token("shapes.version", shapes["version"]))
    reasoning = logical.get("reasoning")
    if reasoning is None:
        out += b"\x00"
    else:
        keys("reasoning", reasoning, {"profile", "implementation", "version"})
        out += b"\x01" + field(token("reasoning.profile", reasoning["profile"])) \
            + field(token("reasoning.implementation", reasoning["implementation"])) \
            + field(token("reasoning.version", reasoning["version"]))
    revision = logical.get("sources_revision")
    out += opt(None if revision is None else token("sources_revision", revision))
    return out


def source_pin(pin: dict) -> bytes:
    keys("source_pin", pin, {"dataset_id", "source_version"})
    return field(token("dataset_id", pin["dataset_id"])) + field(token("source_version", pin["source_version"]))


def encode_context(logical: dict) -> bytes:
    keys("context", logical, {"graph_id", "candidate_commit", "candidate_state_digest", "base_kb", "ontology",
                              "shapes", "reasoning", "sources_revision", "virtual_contexts", "validator"},
         {"graph_id", "candidate_commit", "candidate_state_digest", "base_kb", "shapes", "validator"})
    if not GRAPH_ID_RE.fullmatch(logical["graph_id"]):
        raise Invalid("graph_id")
    out = bytearray(CONTEXT_HEADER)
    out += field(logical["graph_id"])
    out += field(content_id("candidate_commit", logical["candidate_commit"]))
    out += field(content_id("candidate_state_digest", logical["candidate_state_digest"]))
    out += semantics(logical)
    out += counted_set([virtual_context(vc) for vc in logical.get("virtual_contexts", [])], "virtual contexts")
    validator = keys("validator", logical["validator"], {"service_id", "service_version", "configuration_version"})
    out += field(token("validator.service_id", validator["service_id"]))
    out += field(token("validator.service_version", validator["service_version"]))
    out += field(token("validator.configuration_version", validator["configuration_version"]))
    return bytes(out)


def encode_environment(logical: dict) -> bytes:
    keys("environment", logical, {"base_kb", "ontology", "shapes", "reasoning", "sources_revision",
                                  "validator_service_version", "validator_configuration_version"},
         {"base_kb", "shapes", "validator_service_version", "validator_configuration_version"})
    out = bytearray(ENVIRONMENT_HEADER)
    out += semantics(logical)
    out += field(token("validator.service_version", logical["validator_service_version"]))
    out += field(token("validator.configuration_version", logical["validator_configuration_version"]))
    return bytes(out)


def environment_of(context: dict) -> dict:
    """The candidate-independent projection of a context (ADR-0018/0019)."""
    env = {k: context[k] for k in ("base_kb", "shapes") }
    for k in ("ontology", "reasoning", "sources_revision"):
        if context.get(k) is not None:
            env[k] = context[k]
    env["validator_service_version"] = context["validator"]["service_version"]
    env["validator_configuration_version"] = context["validator"]["configuration_version"]
    return env


def violation(v: dict) -> bytes:
    keys("violation", v, {"severity", "code", "message"}, {"severity", "code"})
    message = v.get("message", "")
    if len(message.encode("utf-8")) > MAX_MESSAGE_BYTES or any(unicodedata.category(c) == "Cc" for c in message):
        raise Invalid("violation.message")
    return field(token("violation.severity", v["severity"], MAX_SEVERITY_BYTES)) + field(token("violation.code", v["code"])) + field(message)


def encode_record(logical: dict, *, summary_override: list[bytes] | None = None,
                  raw_recorded_at: str | None = None) -> bytes:
    keys("record", logical, {"graph_id", "candidate_commit", "candidate_state_digest", "semantic_execution_context_id",
                             "validator", "outcome", "recorded_at", "report_digest", "report_reference"},
         {"graph_id", "candidate_commit", "candidate_state_digest", "semantic_execution_context_id",
          "validator", "outcome", "recorded_at", "report_digest"})
    if not GRAPH_ID_RE.fullmatch(logical["graph_id"]):
        raise Invalid("graph_id")
    out = bytearray(RECORD_HEADER)
    out += field(logical["graph_id"])
    out += field(content_id("candidate_commit", logical["candidate_commit"]))
    out += field(content_id("candidate_state_digest", logical["candidate_state_digest"]))
    out += field(content_id("semantic_execution_context_id", logical["semantic_execution_context_id"]))
    validator = keys("validator", logical["validator"], {"service_id", "service_version", "configuration_version"})
    out += field(token("validator.service_id", validator["service_id"]))
    out += field(token("validator.service_version", validator["service_version"]))
    out += field(token("validator.configuration_version", validator["configuration_version"]))
    outcome = keys("outcome", logical["outcome"], {"kind", "violation_count", "violations"}, {"kind"})
    kind = outcome["kind"]
    count = u32("violation_count", outcome.get("violation_count", 0))
    summary = sorted({violation(v) for v in outcome.get("violations", [])})
    if kind == "conforms":
        # A conforming verdict may carry non-blocking results (e.g. warnings).
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
    if path.name.startswith("environment-"):
        return encode_environment(logical)
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
    cases["record-v1-invalid-noncanonical-time"] = encode_record(record, raw_recorded_at="2026-09-27T14:03:07.25Z")
    good = encode_record(record)
    cases["record-v1-invalid-trailing-bytes"] = good + b"\x00"
    marker = field(record["validator"]["configuration_version"])
    at = good.find(marker) + len(marker)
    assert good[at] == 1 and good[at + 1:at + 5] == struct.pack(">I", 3)
    # the summary (2 entries) exceeds a declared count of 1
    cases["record-v1-invalid-summary-exceeds-count"] = good[:at + 1] + struct.pack(">I", 1) + good[at + 5:]
    # violations outcome with count 0 and no summary
    conforming = encode_record(json.loads((FIXTURES / "record-v1-conforms.input").read_text()))
    cat = conforming.find(marker) + len(marker)
    assert conforming[cat] == 0
    cases["record-v1-invalid-violations-without-count"] = conforming[:cat] + b"\x01" + conforming[cat + 1:]
    cases["record-v1-invalid-outcome-byte"] = conforming[:cat] + b"\x02" + conforming[cat + 1:]
    ctx = encode_context(context)
    cases["context-v1-invalid-trailing-bytes"] = ctx + b"\x00"
    cases["context-v1-invalid-unknown-version"] = b"sculpin-semantic-context-v2\0" + ctx[len(CONTEXT_HEADER):]
    encoded = sorted({virtual_context(vc) for vc in context["virtual_contexts"]})
    assert len(encoded) >= 2
    joined = b"".join(encoded)
    at = ctx.find(joined)
    assert at > 0
    cases["context-v1-invalid-unsorted-virtual-contexts"] = ctx[:at] + b"".join(reversed(encoded)) + ctx[at + len(joined):]
    cases["context-v1-invalid-duplicate-virtual-contexts"] = ctx[:at - 4] + struct.pack(">I", len(encoded) + 1) + encoded[0] + joined + ctx[at + len(joined):]
    # object refs within one element in raw-string (not encoding) order
    lab_b = next(vc for vc in context["virtual_contexts"] if len(vc["object_refs"]) >= 2)
    refs = sorted({field(r) for r in lab_b["object_refs"]})
    refs_joined = b"".join(refs)
    rat = ctx.find(refs_joined)
    assert rat > 0
    cases["context-v1-invalid-unsorted-object-refs"] = ctx[:rat] + b"".join(reversed(refs)) + ctx[rat + len(refs_joined):]
    no_ontology = dict(context, ontology=None)
    absent = encode_context(no_ontology)
    prefix = len(CONTEXT_HEADER) + len(field(context["graph_id"])) + len(field(context["candidate_commit"])) \
        + len(field(context["candidate_state_digest"])) + len(field(context["base_kb"]["kb_id"])) + len(field(context["base_kb"]["revision"]))
    assert absent[prefix] == 0
    cases["context-v1-invalid-empty-ontology"] = absent[:prefix] + b"\x01" + field("") + field("1") + absent[prefix + 1:]
    cases["context-v1-invalid-ontology-tag"] = absent[:prefix] + b"\x02" + absent[prefix + 1:]
    env = encode_environment(environment_of(context))
    cases["environment-v1-invalid-trailing-bytes"] = env + b"\x00"
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
