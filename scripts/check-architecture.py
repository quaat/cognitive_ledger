#!/usr/bin/env python3
import pathlib,subprocess,sys
root=pathlib.Path(__file__).resolve().parents[1]
forbidden=('axum','sqlx','postgres','fuseki','docker','aws-sdk-s3','fluree')
for crate in ('ledger-core','ledger-rdf'):
 text=(root/'crates'/crate/'Cargo.toml').read_text().lower()
 hits=[x for x in forbidden if x in text]
 if hits: print(f'{crate}: forbidden dependencies {hits}',file=sys.stderr);sys.exit(1)
for p in [root/'Cargo.toml',*root.glob('crates/*/Cargo.toml'),*root.glob('apps/*/Cargo.toml')]:
 if 'fluree' in p.read_text().lower():print(f'Fluree dependency in {p}',file=sys.stderr);sys.exit(1)
print('architecture dependency checks passed')
