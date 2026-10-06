#!/usr/bin/env python3
"""Dependency boundaries (ARCHITECTURE.md), checked on cargo's resolved package graph (real
package names, so renames and manifest syntax variants cannot hide a dependency):
- core crates stay free of HTTP/database/container clients and of the ledger's
  infrastructure crates, transitively (normal and build dependencies) and directly (any kind);
- the projection target adapter never reaches the database, storage or API layers;
- only the projector process depends on the target adapter, and it speaks HTTP only through it;
- Fluree is never a dependency anywhere.
A whole-text scan of the core manifests is kept as a backstop."""
import json, pathlib, subprocess, sys

root = pathlib.Path(__file__).resolve().parents[1]


def fail(msg):
    print(msg, file=sys.stderr)
    sys.exit(1)


def metadata():
    # Offline first (developer machines, sandboxes); a fresh CI runner has no registry cache
    # yet, so fall back to a normal resolve. `--locked` either way: the graph checked is the
    # lockfile's.
    errors = []
    for extra in (["--offline"], []):
        try:
            return json.loads(subprocess.run(
                ["cargo", "metadata", "--format-version", "1", "--locked", *extra],
                cwd=root, check=True, capture_output=True, text=True).stdout)
        except (OSError, subprocess.CalledProcessError) as e:
            errors.append(f"{' '.join(extra) or 'online'}: {e}")
    fail(f"cargo metadata failed (the architecture check needs the resolved graph): {errors}")


meta = metadata()

packages = {p["id"]: p for p in meta["packages"]}
members = {packages[i]["name"]: i for i in meta["workspace_members"]}
nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}


def name(pid):
    return packages[pid]["name"]


def direct(crate, kinds=(None, "dev", "build")):
    """Package names this workspace crate depends on directly with one of `kinds`."""
    return {name(d["pkg"]) for d in nodes[members[crate]]["deps"]
            if any(k["kind"] in kinds for k in d["dep_kinds"])}


def transitive(crate):
    """Package names reachable through normal and build dependencies."""
    seen, stack = set(), [members[crate]]
    while stack:
        for d in nodes[stack.pop()]["deps"]:
            if any(k["kind"] in (None, "build") for k in d["dep_kinds"]) and d["pkg"] not in seen:
                seen.add(d["pkg"])
                stack.append(d["pkg"])
    return {name(p) for p in seen}


def matches(pkg, forbidden):
    return any(pkg == f or pkg.startswith(f + "-") or (f == "postgres" and "postgres" in pkg) for f in forbidden)


def refuse(crate, names, forbidden, what):
    hits = sorted(p for p in names if matches(p, forbidden))
    if hits:
        fail(f"{crate}: forbidden {what} dependencies {hits}")


core_forbidden = ("axum", "sqlx", "postgres", "reqwest", "hyper", "bollard", "docker", "aws-sdk", "fluree",
                  "pyshacl", "jena", "ledger-store", "ledger-api", "ledger-server", "ledger-projection-fuseki",
                  "ledger-projector")
for crate in ("ledger-core", "ledger-rdf", "ledger-dag", "ledger-validation-protocol", "ledger-projection"):
    refuse(crate, transitive(crate), core_forbidden, "transitive")
    refuse(crate, direct(crate), core_forbidden, "direct")
    # Backstop: the previous whole-manifest substring scan.
    text = (root / "crates" / crate / "Cargo.toml").read_text().lower()
    hits = [x for x in ("axum", "sqlx", "postgres", "fuseki", "docker", "aws-sdk-s3", "fluree", "reqwest",
                        "pyshacl", "jena", "ledger-store", "ledger-api") if x in text]
    if hits:
        fail(f"{crate}: forbidden dependencies {hits} (manifest text)")

# The target adapter speaks HTTP to the target and nothing else.
refuse("ledger-projection-fuseki", transitive("ledger-projection-fuseki"),
       ("sqlx", "postgres", "axum", "ledger-store", "ledger-api", "ledger-server", "aws-sdk", "fluree", "pyshacl"),
       "transitive")
# The projector: storage + adapter; never the HTTP API layer or the server app; no HTTP client
# of its own outside tests (target HTTP stays in the adapter).
refuse("ledger-projector", direct("ledger-projector"), ("ledger-api", "ledger-server", "fluree", "pyshacl"), "direct")
refuse("ledger-projector", direct("ledger-projector", kinds=(None, "build")), ("reqwest", "hyper"), "direct normal")
for crate in members:
    if crate not in ("ledger-projector", "ledger-projection-fuseki") and "ledger-projection-fuseki" in direct(crate):
        fail(f"{crate}: only ledger-projector may depend on ledger-projection-fuseki")
fluree = sorted({name(n) for n in nodes if "fluree" in name(n).lower()})
if fluree:
    fail(f"Fluree packages in the dependency graph: {fluree}")
for p in [root / "Cargo.toml", *root.glob("crates/*/Cargo.toml"), *root.glob("apps/*/Cargo.toml")]:
    if "fluree" in p.read_text().lower():
        fail(f"Fluree dependency in {p}")
print("architecture dependency checks passed")
