#!/usr/bin/env python3
"""Branch workload for scripts/upgrade-p4.sh (Plan 0008 gate: Phase 3 (0011) -> Phase 4 (0012)).

Reuses scripts/upgrade-p2/workload.py (tokens, calls) and runs, through the NEW API, on a
graph upgraded from the Phase-3 release:

  branches <graph> <tenant> <res.json>
      create agent/task-17 from main's head; three prepare -> validate -> accept cycles on it;
      main unchanged; create review/hist from main's first commit (historical); an
      unreachable point refused; delete (tombstone) agent/task-17, prepare refused, restore
      with the same head/version, one more cycle; lifecycle retries replay.
  adopted <graph> <tenant> <ref>
      a pre-Phase-4 ref (adopted by 0012) is an active branch whose lifecycle starts `adopted`.
Every mismatch raises (non-zero exit).
"""
import importlib.util
import json
import os
import sys

_spec = importlib.util.spec_from_file_location(
    "p2", os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "upgrade-p2", "workload.py"))
p2 = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(p2)
call, mint, expect = p2.call, p2.mint, p2.expect
ROLES = p2.NEW_ROLES + ["ledger.admin"]
REQUESTED = {"base_kb": {"kb_id": "urn:upgrade:kb", "revision": "kbrev-upgrade-1"},
             "shapes": {"id": "urn:upgrade:shapes", "version": "1+shapes_hash:upgrade"}}


def step(tok, graph, branch, head, quad, tag):
    body = {"ref": branch, "expected_head": head, "operations": [{"op": "add", "quad": quad}],
            "activity": "upgrade-p4", "message": tag}
    s, p = call(tok, "POST", f"/v1/graphs/{graph}/proposals", f"p4-p-{tag}", body)
    expect(s == 201, f"prepare {tag}: {s} {p}")
    base = f"/v1/graphs/{graph}/proposals/{p['candidate']}"
    s, v = call(tok, "POST", f"{base}/validations", f"p4-v-{tag}", {"requested": REQUESTED})
    expect(s == 201 and v["conforms"] is True, f"validate {tag}: {s} {v}")
    s, a = call(tok, "POST", f"{base}/accept", f"p4-a-{tag}",
                {"ref": branch, "expected_head": head, "reason": tag,
                 "validation_id": v["validation_id"], "semantic_environment_id": v["semantic_environment_id"]})
    expect(s == 200, f"accept {tag}: {s} {a}")
    return a["head"], a["ref_version"]


def branches(graph, tenant, res):
    tok = mint(tenant, ROLES)
    s, main = call(tok, "GET", f"/v1/graphs/{graph}/refs?name=main")
    expect(s == 200, f"main ref: {s} {main}")
    s, created = call(tok, "POST", f"/v1/graphs/{graph}/branches", "p4-create",
                      {"name": "agent/task-17", "source": "main"})
    expect(s == 201 and created["event"]["head"] == main["head"] and created["event"]["version"] == 1,
           f"create agent/task-17: {s} {created}")
    s, replay = call(tok, "POST", f"/v1/graphs/{graph}/branches", "p4-create",
                     {"name": "agent/task-17", "source": "main"})
    expect(s == 200 and replay["replayed"] is True and replay["event"] == created["event"], f"create replay: {s} {replay}")
    head, version = main["head"], 1
    for i in (1, 2, 3):
        head, version = step(tok, graph, "agent/task-17", head, f"<urn:up4:task17:{i}> <urn:up4:p> \"{i}\" .", f"t17-{i}")
    expect(version == 4, f"agent/task-17 at v{version}")
    s, main_after = call(tok, "GET", f"/v1/graphs/{graph}/refs?name=main")
    expect(main_after == main, f"main moved: {main} -> {main_after}")
    # Historical branch point: main's first-parent root.
    s, log = call(tok, "GET", f"/v1/graphs/{graph}/branches/log?name=main&limit=1000")
    expect(s == 200 and log["commits"][0] == main["head"], f"main log: {s} {log}")
    root = log["commits"][-1]
    s, hist = call(tok, "POST", f"/v1/graphs/{graph}/branches", "p4-hist",
                   {"name": "review/hist", "source": "main", "from_commit": root})
    expect(s == 201 and hist["event"]["head"] == root, f"historical branch: {s} {hist}")
    s, bad = call(tok, "POST", f"/v1/graphs/{graph}/branches", "p4-bad",
                  {"name": "review/bad", "source": "review/hist", "from_commit": main["head"]})
    expect(s == 422 and bad.get("code") == "BRANCH_POINT_UNREACHABLE", f"unreachable point: {s} {bad}")
    # Tombstone and restore.
    s, deleted = call(tok, "POST", f"/v1/graphs/{graph}/branches/delete", "p4-del",
                      {"name": "agent/task-17", "reason": "upgrade qualification"})
    expect(s == 200 and deleted["event"]["status"] == "deleted", f"delete: {s} {deleted}")
    s, again = call(tok, "POST", f"/v1/graphs/{graph}/branches/delete", "p4-del",
                    {"name": "agent/task-17", "reason": "upgrade qualification"})
    expect(s == 200 and again["replayed"] is True, f"delete replay: {s} {again}")
    body = {"ref": "agent/task-17", "expected_head": head, "operations": [{"op": "add", "quad": "<urn:x> <urn:y> \"z\" ."}],
            "activity": "upgrade-p4", "message": "refused"}
    s, refused = call(tok, "POST", f"/v1/graphs/{graph}/proposals", "p4-refused", body)
    expect(s == 409 and refused.get("code") == "BRANCH_DELETED", f"prepare on deleted: {s} {refused}")
    s, restored = call(tok, "POST", f"/v1/graphs/{graph}/branches/restore", "p4-res", {"name": "agent/task-17"})
    expect(s == 200 and restored["event"]["head"] == head and restored["event"]["version"] == 4, f"restore: {s} {restored}")
    head, version = step(tok, graph, "agent/task-17", head, "<urn:up4:task17:4> <urn:up4:p> \"4\" .", "t17-4")
    s, st = call(tok, "GET", f"/v1/graphs/{graph}/branches/status?name=agent/task-17")
    expect(s == 200 and st["status"] == "active" and st["version"] == 5 and st["lifecycle_version"] == 3, f"status: {s} {st}")
    s, history = call(tok, "GET", f"/v1/graphs/{graph}/branches/history?name=agent/task-17")
    ops = [e["operation"] for e in history["lifecycle"]]
    expect(ops == ["created", "deleted", "restored"], f"lifecycle {ops}")
    s, main_final = call(tok, "GET", f"/v1/graphs/{graph}/refs?name=main")
    expect(main_final == main, "main moved during the branch workflow")
    json.dump({"graph": graph, "branch_head": head, "branch_version": version, "main": main}, open(res, "w"))
    print(f"branches on upgraded {graph}: agent/task-17 from main v{main['version']} advanced to v{version} "
          f"(3 validated cycles, delete/restore with head kept, 1 more cycle); review/hist from the root; "
          f"unreachable point refused; main unchanged")


def adopted(graph, tenant, ref):
    tok = mint(tenant, ROLES)
    s, st = call(tok, "GET", f"/v1/graphs/{graph}/branches/status?name={ref}")
    expect(s == 200 and st["status"] == "active" and st["origin"] == "adopted", f"adopted {graph}/{ref}: {s} {st}")
    s, h = call(tok, "GET", f"/v1/graphs/{graph}/branches/history?name={ref}")
    expect(s == 200 and [e["operation"] for e in h["lifecycle"]] == ["adopted"], f"adopted history: {s} {h}")
    print(f"{graph}/{ref}: adopted as an active branch at v{st['version']}")


if __name__ == "__main__":
    cmd, args = sys.argv[1], sys.argv[2:]
    {"branches": branches, "adopted": adopted}.get(cmd, lambda *_: sys.exit(f"unknown subcommand {cmd}"))(*args)
