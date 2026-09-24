#!/usr/bin/env python3
import pathlib,re,sys
root=pathlib.Path(__file__).resolve().parents[1]; bad=[]
for p in [root/'README.md',root/'AGENTS.md',root/'CLAUDE.md',root/'ARCHITECTURE.md',*sorted((root/'docs').rglob('*.md'))]:
 text=p.read_text()
 for target in re.findall(r'\[[^]]+\]\(([^)]+)\)',text):
  target=target.split('#',1)[0]
  if not target or '://' in target or target.startswith('mailto:'):continue
  if not (p.parent/target).resolve().exists():bad.append(f'{p.relative_to(root)} -> {target}')
if bad: print('\n'.join(bad),file=sys.stderr);sys.exit(1)
print(f'documentation links valid ({len(list((root/"docs").rglob("*.md")))+4} files scanned)')
