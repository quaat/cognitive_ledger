#!/usr/bin/env bash
# Supply-chain gate: cargo audit with the single documented exception, plus a mechanical
# proof that the exception's premise still holds (see .cargo/audit.toml).
set -euo pipefail
cd "$(dirname "$0")/.."

# 1. The excepted crate must be unreachable in the feature-resolved build graph of every
#    target (it is a lockfile-only optional dependency of sqlx's MySQL driver).
reachable=$(cargo tree --locked --target all -e normal,build -i rsa 2>/dev/null || true)
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
