#!/usr/bin/env bash
# Supply-chain gate: cargo audit with the single documented exception, plus a mechanical
# proof that the exception's premise still holds (see .cargo/audit.toml).
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p target

# 1. The excepted crate must be unreachable in the feature-resolved build graph of every
#    target (it is a lockfile-only optional dependency of sqlx's MySQL driver).
# `cargo tree` exits non-zero when Cargo.lock does not match Cargo.toml (`--locked`) or
# cannot resolve; that must fail the gate, not be read as "nothing reachable". It prints
# only a stderr warning ("nothing to print") when the crate is unreachable.
if ! reachable=$(cargo tree --locked --target all -e normal,build -i rsa 2>target/cargo-tree-rsa.err); then
  echo "::error::cargo tree failed (lockfile drift or resolution error); the gate cannot prove the exception premise:" >&2
  cat target/cargo-tree-rsa.err >&2
  exit 1
fi
if [ -n "${reachable}" ]; then
  echo "::error::rsa is now reachable in the build graph; remove the RUSTSEC-2023-0071 exception and fix the dependency:" >&2
  echo "${reachable}" >&2
  exit 1
fi

# 2. Its lockfile dependents must be exactly sqlx-mysql and its version must still be the
#    advisory's 0.9 line; anything else means the exception must be re-evaluated.
python3 - <<'PY'
import re, sys
lock = open("Cargo.lock").read()
packages = re.split(r"\n\[\[package\]\]\n", lock)
dependents = set()
versions = set()
for p in packages:
    name = re.search(r'name = "([^"]+)"', p)
    if not name:
        continue
    if name.group(1) == "rsa":
        versions.add(re.search(r'version = "([^"]+)"', p).group(1))
    for dep in re.findall(r'^ "([^" ]+)', p, re.M):
        if dep == "rsa":
            dependents.add(name.group(1))
if not versions:
    print("rsa is no longer in Cargo.lock: delete the RUSTSEC-2023-0071 exception in .cargo/audit.toml", file=sys.stderr)
    sys.exit(1)
if dependents != {"sqlx-mysql"} or not all(v.startswith("0.9.") for v in versions):
    print(f"rsa lockfile premise changed (dependents={sorted(dependents)}, versions={sorted(versions)}); re-evaluate .cargo/audit.toml", file=sys.stderr)
    sys.exit(1)
print(f"rsa {sorted(versions)} is lockfile-only via {sorted(dependents)}; exception premise holds")
PY

# 3. The audit itself (configuration from .cargo/audit.toml; nothing else is ignored).
ignored=$(grep -E '^ignore' .cargo/audit.toml | grep -oE 'RUSTSEC-[0-9]+-[0-9]+' | wc -l | tr -d ' ')
if [ "${ignored}" != "1" ]; then
  echo "::error::.cargo/audit.toml must list exactly one advisory exception (found ${ignored})" >&2
  exit 1
fi
cargo audit

# 4. cargo-deny on the feature-resolved graph: advisories, licenses, bans (Fluree, query
#    engines, rsa), sources (crates.io only). Policy: deny.toml.
cargo deny --locked check advisories licenses bans sources

# 5. CycloneDX SBOMs of the shipped binaries (CI uploads them as artefacts; never committed).
#    cargo-cyclonedx reads `cargo metadata`, which lists lockfile-only optional dependencies
#    (sqlx-mysql, rsa, sqlx-sqlite) that are never built; the SBOM is filtered to the
#    feature-resolved build graph and must not name them.
mkdir -p target/sbom
rm -f apps/ledger-server/*.cdx.json
cargo cyclonedx --manifest-path apps/ledger-server/Cargo.toml --describe binaries --format json --spec-version 1.5
mv apps/ledger-server/*.cdx.json target/sbom/
cargo tree --locked -p ledger-server -e normal,build --target x86_64-unknown-linux-gnu --prefix none --format '{p}' \
  | sed -E 's/ \(.*//' | sort -u > target/sbom/reachable.txt
python3 - <<'PY'
import json, glob, hashlib, sys
reachable = set()
for line in open("target/sbom/reachable.txt"):
    line = line.strip()
    if not line:
        continue
    name, version = line.rsplit(" v", 1)
    reachable.add((name, version))
files = sorted(glob.glob("target/sbom/*_bin.cdx.json"))
if len(files) < 2:
    print("expected SBOMs for ledger-server and ledger-admin", file=sys.stderr); sys.exit(1)
for f in files:
    d = json.load(open(f))
    before = len(d.get("components", []))
    keep, dropped = [], []
    for c in d.get("components", []):
        key = (c.get("name"), c.get("version"))
        (keep if key in reachable else dropped).append(c)
    kept_refs = {c.get("bom-ref") for c in keep} | {d.get("metadata", {}).get("component", {}).get("bom-ref")}
    d["components"] = keep
    if "dependencies" in d:
        d["dependencies"] = [
            {**dep, "dependsOn": [r for r in dep.get("dependsOn", []) if r in kept_refs]}
            for dep in d["dependencies"] if dep.get("ref") in kept_refs
        ]
    names = {c.get("name") for c in keep}
    for forbidden in ("rsa", "sqlx-mysql", "sqlx-sqlite", "openssl", "openssl-sys", "native-tls"):
        if forbidden in names:
            print(f"{f}: {forbidden} is in the SBOM but must not be part of the shipped binary", file=sys.stderr); sys.exit(1)
    if d.get("bomFormat") != "CycloneDX" or len(keep) < 100:
        print(f"{f}: not a plausible CycloneDX SBOM ({len(keep)} components)", file=sys.stderr); sys.exit(1)
    json.dump(d, open(f, "w"), indent=2)
    digest = hashlib.sha256(open(f, "rb").read()).hexdigest()
    print(f"{f}: CycloneDX {d.get('specVersion')} {len(keep)} components (filtered {before - len(keep)} lockfile-only: {', '.join(sorted(c.get('name') for c in dropped)[:6])}) sha256:{digest[:16]}…")
PY
