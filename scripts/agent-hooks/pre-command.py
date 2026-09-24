#!/usr/bin/env python3
import json, re, sys
try:
    data=json.load(sys.stdin)
except Exception:
    sys.exit(0)
cmd=((data.get('tool_input') or {}).get('command') or '')
patterns=[
 r'(^|[;&|]\s*)git\s+push(?:\s+[^;&|]*)?\s--force(?:-with-lease)?(?:\s|$)',
 r'(^|[;&|]\s*)git\s+reset\s+--hard(?:\s|$)',
 r'(^|[;&|]\s*)git\s+clean\s+-[^\s]*f[^\s]*d[^\s]*x(?:\s|$)',
 r'(^|[;&|]\s*)rm\s+-rf\s+/(?:\s|$)',
]
if any(re.search(p,cmd) for p in patterns):
 print(json.dumps({'decision':'block','reason':'Destructive command blocked by repository safety policy.'}))
 sys.exit(2)
