#!/usr/bin/env python3
"""Differential adapter seam.

Computes the normalized current state a scenario should reach under the ledger's
add/delete change model, then either:

  * compares it against a reference state file (produced by querying the pinned
    Fluree reference) when one is supplied, failing loudly on any divergence; or
  * validates the scenario's own internal consistency (seam-only mode) when no
    reference is supplied.

Seam-only mode is NOT a differential pass: the live Fluree comparison is deferred
pending BUSL-1.1 sign-off (see test/reference-images.lock). The caller is
responsible for stating that distinction; this script only reports what it checked.
"""
import json
import pathlib
import sys


def normalized(values):
    return sorted(set(values))


def current_state(scenario):
    state = set(scenario["initial"])
    for change in scenario["changes"]:
        if "delete" in change:
            state.discard(change["delete"])
        if "add" in change:
            state.add(change["add"])
    return normalized(state)


def main(argv):
    scenario = json.loads(pathlib.Path(argv[1]).read_text())
    computed = current_state(scenario)
    expected = normalized(scenario["expected_current"])
    if computed != expected:
        raise SystemExit(
            f"test defect: scenario model {computed} != declared expectation {expected}"
        )
    if len(argv) >= 3:
        reference = normalized(json.loads(pathlib.Path(argv[2]).read_text()))
        if computed != reference:
            raise SystemExit(
                f"DIFFERENTIAL MISMATCH: ledger model {computed} != Fluree reference {reference}"
            )
        print(f"differential OK: ledger and Fluree agree on {computed}")
        return
    print(f"seam OK (deterministic model self-consistent): {computed}")


if __name__ == "__main__":
    main(sys.argv)
