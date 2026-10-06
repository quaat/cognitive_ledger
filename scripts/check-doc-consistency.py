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
- The required-CI-checks recommendation lists every workflow under .github/workflows that
  gates pull requests.
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

gating = sorted(p.stem for p in (root / ".github/workflows").glob("ci-*.yml")
                if re.search(r"^\s*pull_request:", p.read_text(), re.M))
debt = (root / "docs/exec-plans/tech-debt.md").read_text()
rule = re.search(r"required CI checks \(([^)]*)\)", debt)
listed = sorted(c.strip() for c in rule.group(1).split(",")) if rule else []
if listed != gating:
    errors.append(f"tech-debt.md required CI checks {listed} differ from the PR-gating workflows {gating}")

if errors:
    print("\n".join(errors), file=sys.stderr)
    sys.exit(1)
print(f"documentation consistency ok (schema {required:04}, generator {generator}, required checks {', '.join(gating)})")
