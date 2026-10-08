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
for crate in ("ledger-core", "ledger-rdf", "ledger-dag", "ledger-merge", "ledger-validation-protocol", "ledger-projection"):
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
# Qualification tools (benchmark and stress harnesses) are leaves: nothing depends on them.
for tool in ("ledger-bench", "ledger-stress"):
    for crate in members:
        if crate != tool and tool in direct(crate):
            fail(f"{crate}: depends on the qualification tool {tool}")
fluree = sorted({name(n) for n in nodes if "fluree" in name(n).lower()})
if fluree:
    fail(f"Fluree packages in the dependency graph: {fluree}")
for p in [root / "Cargo.toml", *root.glob("crates/*/Cargo.toml"), *root.glob("apps/*/Cargo.toml")]:
    if "fluree" in p.read_text().lower():
        fail(f"Fluree dependency in {p}")


def feature_tree(pkg, edges):
    """`cargo tree` of `pkg` with feature edges; `edges` selects the dependency kinds."""
    errors = []
    for extra in (["--offline"], []):
        try:
            return subprocess.run(
                ["cargo", "tree", "-p", pkg, "-e", f"features,{edges}", "--locked", "--prefix", "none", *extra],
                cwd=root, check=True, capture_output=True, text=True).stdout
        except (OSError, subprocess.CalledProcessError) as e:
            errors.append(f"{' '.join(extra) or 'online'}: {e}")
    fail(f"cargo tree failed for {pkg}: {errors}")


# Test-only fault injection and pause points (ledger-store and ledger-projector feature
# `test-hooks`: `FailPoint`, `PauseHook`, slow statements, crash windows) exist only in test
# builds: no shipped or qualification binary may enable them in its normal build graph
# (Plan 0013 F5). Two proofs: the resolved feature graph of every app, and a compile probe
# showing the interface does not exist without the feature (and does exist with it, so the
# probe cannot pass vacuously).
for hooked in ("ledger-store", "ledger-projector"):
    HOOKS = f'{hooked} feature "test-hooks"'
    if HOOKS not in feature_tree(hooked, "normal,build,dev"):
        fail(f"check broken: the {hooked} test graph does not show the test-hooks feature")
    for app in sorted(p.parent.name for p in root.glob("apps/*/Cargo.toml")):
        if HOOKS in feature_tree(app, "normal,build"):
            fail(f"{app}: its build enables {hooked}'s test-hooks feature (test-only fault injection)")


def probe(features, uses, expect_ok):
    """`cargo check` a scratch crate that depends on the hooked crates with `features` and
    names every test-only symbol in `uses`; the check must succeed iff `expect_ok`."""
    work = root / "target" / "test-hooks-probe" / ("with" if expect_ok else "without")
    (work / "src").mkdir(parents=True, exist_ok=True)
    store_feats = '"postgres"' + (', "test-hooks"' if features else "")
    proj_feats = '"test-hooks"' if features else ""
    (work / "Cargo.toml").write_text(
        "[package]\nname = \"test-hooks-probe\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n"
        "[workspace]\n[dependencies]\n"
        f"ledger-store = {{ path = \"{root / 'crates' / 'ledger-store'}\", features = [{store_feats}] }}\n"
        f"ledger-projector = {{ path = \"{root / 'apps' / 'ledger-projector'}\", features = [{proj_feats}] }}\n")
    (work / "src" / "lib.rs").write_text("".join(f"pub use {u} as Probe{i};\n" for i, u in enumerate(uses)))
    # Resolve against the workspace's pinned versions (copied on every run, so a lockfile
    # change is followed); offline first, online on a fresh runner, like `metadata()`.
    (work / "Cargo.lock").write_bytes((root / "Cargo.lock").read_bytes())
    env = dict(**__import__("os").environ, CARGO_TARGET_DIR=str(root / "target" / "test-hooks-probe" / "target"))
    r = None
    for extra in (["--offline"], []):
        r = subprocess.run(["cargo", "check", "--quiet", *extra], cwd=work, env=env, capture_output=True, text=True)
        # A resolution/download failure is distinguishable from the compile outcome we test for:
        # rustc errors name `error[E`; cargo's offline failure does not.
        if r.returncode == 0 or "error[E" in r.stderr:
            break
    if (r.returncode == 0) != expect_ok:
        fail(f"test-hooks probe ({'with' if expect_ok else 'without'} the feature) {'succeeded' if r.returncode == 0 else 'failed'} unexpectedly:\n{r.stderr[-4000:]}")
    if not expect_ok:
        # The production build must fail *only* because the probed symbols are absent: any
        # other rustc error means the production feature set itself does not compile (a
        # `test-hooks`-gated import that production code relies on, say), which this probe
        # would otherwise accept as "failed as expected" (Plan 0013 M2 review clean-up).
        others = sorted({line.split(":")[0] for line in r.stderr.splitlines()
                         if line.startswith("error[E") and not line.startswith("error[E0432]")})
        if others:
            fail(f"test-hooks probe: the production feature set does not compile ({', '.join(others)}):\n{r.stderr[-4000:]}")
        for i, u in enumerate(uses):
            # rustc echoes the offending source line under each unresolved import; matching
            # it pins every symbol by its full path, so same-named symbols of different crates
            # (both `FailPoint`s) are checked apart. (The message itself may name only the
            # unresolved module, e.g. `ledger_store::test_hooks`.)
            if f"pub use {u} as Probe{i};" not in r.stderr or "error[E0432]" not in r.stderr:
                fail(f"test-hooks probe: the production build did not report `{u}` as unresolved:\n{r.stderr[-4000:]}")


TEST_ONLY = [
    "ledger_store::FailPoint",
    "ledger_store::test_hooks::PauseHook",
    "ledger_store::test_hooks::HookPoint",
    "ledger_store::classify_db_error",
    "ledger_projector::FailPoint",
]
if "--no-probe" not in sys.argv:
    probe(True, TEST_ONLY, expect_ok=True)
    probe(False, TEST_ONLY, expect_ok=False)
print("architecture dependency checks passed")
