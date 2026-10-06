#!/usr/bin/env python3
"""Current-state facts the documentation must not contradict (Plan 0011). Each fact has one
authoritative source; prose either avoids the literal or must match it.

- The schema level a build requires (`REQUIRED_SCHEMA_VERSION` in ledger-store's schema.rs)
  is the highest migration in `migrations/`, and every migration number is contiguous.
- The operator runbook's current-state phrases (`schema at NNNN (required NNNN)`,
  `migrations 0001…NNNN`, `schema is exactly NNNN`) name that level. Historical upgrade
  descriptions ("0009 → 0010") are not matched.
- Benchmark docs that name a synthetic generator version name the current one
  (`GENERATOR_VERSION` in apps/ledger-bench/src/synthetic.rs), and every committed synthetic
  manifest records it.
- The required-CI-checks recommendation (tech-debt.md, production-qualification.md) lists the
  stable check names of the PR-gating workflows: every non-matrix job. Matrix jobs report one
  check per combination ("fuzz-sanitizer (none)"), so each must be covered by an `if:
  always()` aggregate job (self-tested on fixtures every run).
"""
import json, pathlib, re, sys

root = pathlib.Path(__file__).resolve().parents[1]
errors = []

schema_rs = (root / "crates/ledger-store/src/schema.rs").read_text()
m = re.search(r"REQUIRED_SCHEMA_VERSION: i64 = (\d+);", schema_rs)
required = int(m.group(1)) if m else None
numbers = sorted(int(p.name[:4]) for p in (root / "migrations").glob("[0-9][0-9][0-9][0-9]_*.sql"))
if required is None:
    errors.append("REQUIRED_SCHEMA_VERSION not found in schema.rs")
elif numbers != list(range(1, len(numbers) + 1)) or numbers[-1] != required:
    errors.append(f"migrations {numbers[:1]}…{numbers[-1:]} are not contiguous up to REQUIRED_SCHEMA_VERSION {required}")

runbook = (root / "docs/operations/deployment.md").read_text()
for pattern in (r"schema at (\d{4}) \(required (\d{4})\)", r"migrations 0001…(\d{4})", r"schema is exactly (\d{4})"):
    for hit in re.finditer(pattern, runbook):
        for value in hit.groups():
            if int(value) != required:
                errors.append(f"deployment.md: '{hit.group(0)}' contradicts REQUIRED_SCHEMA_VERSION {required}")

synthetic = (root / "apps/ledger-bench/src/synthetic.rs").read_text()
g = re.search(r'GENERATOR_VERSION: &str = "([^"]+)"', synthetic)
generator = g.group(1) if g else None
for doc in (root / "docs/benchmarks").glob("*.md"):
    if doc.name == "INTEGRATION_PLAN.md":
        continue
    for hit in re.findall(r"synthetic-ledger-gen/\d+", doc.read_text()):
        if hit != generator:
            errors.append(f"{doc.relative_to(root)}: names {hit}, the generator is {generator}")
for manifest in (root / "benchmark/datasets").glob("synthetic-*.json"):
    if (json.loads(manifest.read_text()).get("generator") or {}).get("version") != generator:
        errors.append(f"{manifest.relative_to(root)}: generator.version is not {generator}")

def top_level_block(text, key):
    """The lines of a top-level YAML key's block (flow or block form)."""
    m = re.search(rf"^{key}:(.*(?:\n(?:[ \t]+.*|[ \t]*))*)", text, re.M)
    return m.group(1) if m else ""


def flow_list(value):
    value = value.strip()
    if value.startswith("["):
        return [v.strip().strip("'\"") for v in value.strip("[]").split(",") if v.strip()]
    return [value.strip("'\"")] if value else []


def jobs(text):
    """Jobs of a workflow (the static subset of YAML these workflows use): id, display name,
    static matrix axes (None if no matrix; {} if not statically expandable), needs, if, and
    the job's raw text."""
    out, job, lines = [], None, top_level_block(text, "jobs").splitlines()
    for i, line in enumerate(lines):
        if m := re.match(r"^  ([A-Za-z0-9_-]+):\s*$", line):
            job = {"id": m.group(1), "name": m.group(1), "matrix": None, "needs": [], "if": "", "text": ""}
            out.append(job)
            continue
        if not job:
            continue
        job["text"] += line + "\n"
        if m := re.match(r"^    name:\s*['\"]?([^'\"#]+?)['\"]?\s*(#.*)?$", line):
            job["name"] = m.group(1)
        elif m := re.match(r"^    if:\s*(.+)$", line):
            job["if"] = m.group(1).strip()
        elif m := re.match(r"^    needs:\s*(.*)$", line):
            job["needs"] = flow_list(m.group(1))
            for follow in lines[i + 1:]:
                if not (n := re.match(r"^      -\s*(\S+)", follow)):
                    break
                job["needs"].append(n.group(1).strip("'\""))
        elif re.match(r"^      matrix:\s*$", line):
            job["matrix"] = {}
        elif job["matrix"] is not None and (m := re.match(r"^        ([A-Za-z0-9_-]+):\s*(\[.*\])\s*$", line)):
            job["matrix"][m.group(1)] = flow_list(m.group(2))
        elif job["matrix"] is not None and re.match(r"^        (include|exclude):", line):
            job["matrix"]["__dynamic__"] = []
    return out


def check_names(job):
    """The check-run names GitHub reports for a job: one per matrix combination."""
    if job["matrix"] is None:
        return [job["name"]]
    axes = [v for k, v in job["matrix"].items() if k != "__dynamic__"]
    if not axes or "__dynamic__" in job["matrix"]:
        return [f"{job['name']} (<dynamic matrix>)"]
    combos = [[]]
    for values in axes:
        combos = [c + [v] for c in combos for v in values]
    return [f"{job['name']} ({', '.join(c)})" for c in combos]


def required_checks(text, origin, errors):
    """Required check names of one PR-gating workflow. A matrix job's names vary with its
    axes, so it must not be required directly: a non-matrix aggregate job must depend on it
    with `if: always()` and test `needs.<id>.result`. A skipped aggregate would count as
    passing, so `always()` is what makes a failed matrix fail the required check."""
    js = jobs(text)
    for j in js:
        if j["matrix"] is None:
            continue
        aggregates = [a for a in js if a["matrix"] is None and j["id"] in a["needs"]
                      and "always()" in a["if"] and f"needs.{j['id']}.result" in a["text"]]
        if not aggregates:
            errors.append(f"{origin}: matrix job {j['id']} reports {check_names(j)}; add a non-matrix "
                          f"aggregate job with `needs: {j['id']}`, `if: always()` and a check of "
                          f"`needs.{j['id']}.result`, and require the aggregate")
    return [j["name"] for j in js if j["matrix"] is None]


def self_test():
    """Fixtures: a matrix must be aggregated correctly before its workflow can pass."""
    head = "name: w\non: [pull_request]\njobs:\n"
    matrix = "  m:\n    runs-on: x\n    strategy:\n      matrix:\n        s: [none, address]\n    steps: []\n"
    good = "  agg:\n    name: fuzz\n    needs: m\n    if: always()\n    steps:\n      - run: test \"${{ needs.m.result }}\" = success\n"
    cases = {
        "matrix without aggregate": (head + matrix, None),
        "aggregate without always()": (head + matrix + good.replace("    if: always()\n", ""), None),
        "aggregate ignoring the result": (head + matrix + good.replace("needs.m.result", "x"), None),
        "aggregate with block-list needs": (head + matrix + good.replace("    needs: m\n", "    needs:\n      - m\n"), ["fuzz"]),
        "correct aggregate": (head + matrix + good, ["fuzz"]),
    }
    for label, (text, want) in cases.items():
        errs = []
        got = required_checks(text, label, errs)
        if (want is None) != bool(errs) or (want is not None and got != want):
            sys.exit(f"check-doc-consistency self-test failed: {label}: {got} {errs}")
    expanded = check_names(jobs(head + matrix)[0])
    if expanded != ["m (none)", "m (address)"]:
        sys.exit(f"check-doc-consistency self-test failed: matrix expansion {expanded}")


self_test()
# Required checks are matched by check-run name (job name), not workflow file name.
gating = sorted(name for p in sorted((root / ".github/workflows").glob("*.y*ml"))
                for text in [p.read_text()]
                if re.search(r"\bpull_request\b", top_level_block(text, "on"))
                for name in required_checks(text, p.name, errors))
for doc in ("docs/exec-plans/tech-debt.md", "docs/quality/production-qualification.md"):
    rule = re.search(r"required CI checks \(([^)]*)\)", (root / doc).read_text())
    listed = sorted(c.strip().strip("`") for c in rule.group(1).split(",")) if rule else []
    if listed != gating:
        errors.append(f"{doc}: required CI checks {listed} differ from the PR-gating checks {gating}")

if errors:
    print("\n".join(errors), file=sys.stderr)
    sys.exit(1)
print(f"documentation consistency ok (schema {required:04}, generator {generator}, required checks {', '.join(gating)})")
