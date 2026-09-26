#!/usr/bin/env python3
import json, pathlib, sys
try: data=json.load(sys.stdin)
except Exception: sys.exit(0)
ti=data.get('tool_input') or {}
path=str(ti.get('file_path') or ti.get('path') or '')
name=pathlib.PurePath(path).name.lower()
blocked=(name=='.env' or (name.startswith('.env.') and name!='.env.example') or name.endswith(('.pem','.key')) or name.startswith('credentials.'))
if blocked:
 print(json.dumps({'decision':'block','reason':'Credential-like files must not be written in the repository.'}))
 sys.exit(2)
