#!/usr/bin/env python3
"""Differential adapter seam: validates scenario and normalized-state comparison.
Live Fluree transport remains blocked until the image/API contract is pinned.
"""
import json,pathlib,sys
scenario=json.loads(pathlib.Path(sys.argv[1]).read_text())
def normalized(values): return sorted(set(values))
state=set(scenario['initial'])
for change in scenario['changes']:
 if 'delete' in change: state.discard(change['delete'])
 if 'add' in change: state.add(change['add'])
if normalized(state)!=normalized(scenario['expected_current']):raise SystemExit('test defect: local scenario expectation differs')
raise SystemExit('reference adapter blocked: Fluree API/version contract is not pinned')
