#!/usr/bin/env python3
"""Dependency boundaries (ARCHITECTURE.md): core crates stay free of HTTP/database/container
clients; the projection target adapter stays free of the ledger's storage and API layers;
only the projector process depends on the target adapter; Fluree is never a dependency."""
import pathlib,re,sys
root=pathlib.Path(__file__).resolve().parents[1]

def deps(manifest):
    """Names of every dependency (normal, dev, build, target-specific) in a Cargo.toml."""
    names,section=set(),None
    for line in manifest.read_text().splitlines():
        s=line.strip()
        m=re.fullmatch(r'\[(.+)\]',s)
        if m:
            section=m.group(1)
            t=re.fullmatch(r'(?:target\..+\.)?(?:dev-|build-)?dependencies(?:\.(.+))?',section)
            section=('table',t.group(1)) if t else None
            if section and section[1]: names.add(section[1].strip('"'))
            continue
        if section and section[1] is None:
            k=re.match(r'([A-Za-z0-9_"-]+)\s*[.=]',s)
            if k: names.add(k.group(1).strip('"'))
    return names

def refuse(crate,path,forbidden):
    hits=sorted(d for d in deps(path) if any(f in d.lower() for f in forbidden))
    if hits: print(f'{crate}: forbidden dependencies {hits}',file=sys.stderr);sys.exit(1)

core=('axum','sqlx','postgres','fuseki','docker','aws-sdk-s3','fluree','reqwest','pyshacl','jena','ledger-store','ledger-api','hyper','tokio-postgres')
for crate in ('ledger-core','ledger-rdf','ledger-validation-protocol','ledger-projection'):
    refuse(crate,root/'crates'/crate/'Cargo.toml',core)
# The target adapter speaks HTTP to the target and nothing else: no database, no ledger
# storage or API layer, no server framework.
refuse('ledger-projection-fuseki',root/'crates/ledger-projection-fuseki/Cargo.toml',
       ('sqlx','postgres','axum','ledger-store','ledger-api','ledger-server','aws-sdk','fluree','pyshacl'))
# The projector process: storage + adapter, never the HTTP API layer or the server app.
refuse('ledger-projector',root/'apps/ledger-projector/Cargo.toml',('ledger-api','ledger-server','fluree','pyshacl'))
manifests=[root/'Cargo.toml',*root.glob('crates/*/Cargo.toml'),*root.glob('apps/*/Cargo.toml')]
for p in manifests:
    if any('fluree' in d.lower() for d in deps(p)) or 'fluree' in p.read_text().lower():
        print(f'Fluree dependency in {p}',file=sys.stderr);sys.exit(1)
    name=p.parent.name
    if name not in ('ledger-projector','ledger-projection-fuseki') and p!=root/'Cargo.toml' \
       and 'ledger-projection-fuseki' in deps(p):
        print(f'{name}: only ledger-projector may depend on ledger-projection-fuseki',file=sys.stderr);sys.exit(1)
print('architecture dependency checks passed')
