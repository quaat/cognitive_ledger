#!/usr/bin/env python3
"""Merge workload for scripts/upgrade-p5.sh (Plan 0009 gate: Phase 4 (0012) -> Phase 5 (0013)).

Reuses scripts/upgrade-p2/workload.py (tokens, calls). On a graph upgraded from the Phase-4
release whose branch `agent/task-17` (created and advanced by the OLD release) is ahead of
`main`:

  merge <graph> <tenant> <res.json>
      preview (read; writes nothing) -> fast-forward class; propose with the token; the
      ordinary accept of the candidate is refused; validate the candidate; apply bound to the
      validation; main's state = the branch's state; repeat preview -> already_contained;
      reverse direction -> no_change; propose/apply retries replay.
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


def state(tok, graph, commit):
    s, st = call(tok, "GET", f"/v1/graphs/{graph}/commits/{commit}/state")
    expect(s == 200, f"state {commit}: {s} {st}")
    return sorted(st["quads"])


def merge(graph, tenant, res):
    tok = mint(tenant, ROLES)
    reviewer = mint(tenant, ROLES, oid=f"upgrade-reviewer-{tenant}")
    s, main = call(tok, "GET", f"/v1/graphs/{graph}/refs?name=main")
    s2, branch = call(tok, "GET", f"/v1/graphs/{graph}/branches/status?name=agent/task-17")
    expect(s == 200 and s2 == 200, f"heads: {main} {branch}")
    body = {"source": "agent/task-17", "target": "main"}
    s, p = call(tok, "POST", f"/v1/graphs/{graph}/merges/preview", None, body)
    expect(s == 200 and p["classification"] == "fast_forward" and p["merge_base"] == main["head"],
           f"preview: {s} {p}")
    s, again = call(tok, "POST", f"/v1/graphs/{graph}/merges/preview", None, body)
    expect(again["preview_token"] == p["preview_token"], "preview is deterministic")
    propose = dict(body, preview_token=p["preview_token"], message="integrate the upgraded branch")
    s, pr = call(tok, "POST", f"/v1/graphs/{graph}/merges/propose", "p5-propose", propose)
    expect(s == 201, f"propose: {s} {pr}")
    s, replay = call(tok, "POST", f"/v1/graphs/{graph}/merges/propose", "p5-propose", propose)
    expect(s == 200 and replay["replayed"] is True and replay["candidate"] == pr["candidate"], f"propose replay: {s} {replay}")
    cand = pr["candidate"]
    s, ordinary = call(reviewer, "POST", f"/v1/graphs/{graph}/proposals/{cand}/accept", "p5-ordinary",
                       {"ref": "main", "expected_head": main["head"], "reason": "ordinary"})
    expect(s == 409, f"ordinary accept of a merge candidate: {s} {ordinary}")
    s, v = call(tok, "POST", f"/v1/graphs/{graph}/proposals/{cand}/validations", "p5-validate", {"requested": REQUESTED})
    expect(s == 201 and v["conforms"] is True, f"validate: {s} {v}")
    apply = {"proposal_id": pr["proposal_id"], "preview_token": p["preview_token"],
             "validation_id": v["validation_id"], "semantic_environment_id": v["semantic_environment_id"],
             "reason": "upgrade qualification"}
    s, a = call(reviewer, "POST", f"/v1/graphs/{graph}/merges/apply", "p5-apply", apply)
    expect(s == 200 and a["head"] == cand and a["ref_version"] == main["version"] + 1, f"apply: {s} {a}")
    s, a2 = call(reviewer, "POST", f"/v1/graphs/{graph}/merges/apply", "p5-apply", apply)
    expect(s == 200 and a2["replayed"] is True, f"apply replay: {s} {a2}")
    expect(state(tok, graph, cand) == state(tok, graph, branch["head"]), "integrated state = source state")
    s, p3 = call(tok, "POST", f"/v1/graphs/{graph}/merges/preview", None, body)
    expect(p3["classification"] == "already_contained", f"repeat: {p3}")
    s, back = call(tok, "POST", f"/v1/graphs/{graph}/merges/preview", None, {"source": "main", "target": "agent/task-17"})
    expect(back["classification"] == "no_change", f"reverse: {back}")
    json.dump({"graph": graph, "merge_head": cand, "version": a["ref_version"]}, open(res, "w"))
    print(f"merge on upgraded {graph}: agent/task-17 (created by the previous release) integrated into main as "
          f"v{a['ref_version']} (fast-forward class, validated, ordinary accept refused, replays identical); "
          f"repeat contained, reverse no_change")


if __name__ == "__main__":
    cmd, args = sys.argv[1], sys.argv[2:]
    {"merge": merge}.get(cmd, lambda *_: sys.exit(f"unknown subcommand {cmd}"))(*args)
