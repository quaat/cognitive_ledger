# Benchmark datasets

Every dataset has a committed manifest under `benchmark/datasets/<id>.json`. The harness
recomputes the manifest and refuses to run when any field differs (see
[REPRODUCIBILITY.md](REPRODUCIBILITY.md)).

| dataset | purpose | status |
|---|---|---|
| `synthetic-ledger-ci` | commit, branch and merge correctness oracle; small representative workload (PR CI) | **implemented** (Plan 0010) |
| `synthetic-ledger-local` | the same generator with a deeper history (≈1,500 commits, parent-0 depth ≈650) for baselines | **implemented** (Plan 0010) |
| `bear-b-ci` | temporal RDF versions: version materialization, version diff, history | plan below (M3, next) |
| `tkgl-smallpedia-ci` | temporal knowledge graph: snapshot reconstruction and no-leakage history access | plan below (M4) |
| `thgl-software-ci` | heterogeneous temporal event graph: event replay and typed history | plan below (M5) |
| Full BEAR, OGB, large TGB, Software Heritage, LDBC, GNN quality | nightly and scale programmes | out of scope for Phase 6A |

## `synthetic-ledger-ci` / `synthetic-ledger-local` (implemented)

Generator: `apps/ledger-bench/src/synthetic.rs`, version `synthetic-ledger-gen/1`. It is a
single SplitMix64 stream from the seed in the manifest. The data is project-owned and
generated on every run; nothing is committed except the manifest.

**Shape of `ci`** (the parameters are in the manifest):
- ~1,000 entities with 6 statements each, plus 100 statements in the named graph
  `<urn:bench:g:1>`: 6,100 initial quads.
- The initial load is two bulk commits (≤ 5,000 statements each, below the API's
  10,000-operation limit).
- 205 commits, among them 16 integration commits, over 9 branches:
  - `main`;
  - `feature/a` and `feature/b`, forked at the historical `main@10`, forming a diamond;
  - `growth` (add-only) and `churn` (replace-only), forked at `main@5`;
  - `feature/e`, forked at the head;
  - `feature/c` and `feature/d`, carrying the designed conflicts;
  - `crisscross`, a historical branch point used only to build the criss-cross.

  See the manifest for exact counts.
- Merge classifications covered: `already_equal`, `already_contained`, `no_change` (both a
  fast-forward-class sync and a source whose changes net to nothing), `fast_forward`
  (an integration commit, ADR-0023), `divergent`, `conflicted` and `ambiguous_merge_base`.
  The ambiguous base is resolved by an explicit `base`.
- 10 designed structural conflicts cycling through five shapes, plus 2 convergent slots.
  The shapes are replace/replace; delete/modify in both directions; keep-and-add vs delete,
  where `union` must keep the deleted statement; and add/add on a multi-valued slot. They
  are previewed under `abort`, `take-target`, `take-source` and `union`, and applied with
  `union`.
- A take-target resolution that keeps the target state, recorded as an empty integration
  commit (not `no_change`).
- A merge base reachable only through a second parent.
- Repeated merge, sync-back and criss-cross sequences.
- Every commit carries generated provenance: activity, message = label, a unique evidence
  reference and the source system.

**Oracle and assertions:** see [BENCHMARK_ARCHITECTURE.md §3](BENCHMARK_ARCHITECTURE.md) and
[METRICS.md](METRICS.md#correctness-assertions).

**Semantic validity:** none is asserted. The ledger does not own SHACL or reasoning, and
the run uses the development unvalidated-acceptance switch. Validation-protocol
benchmarks, against the fake validator and later the live Sculpin service, belong to a later
milestone.

## Extraction plans for the next CI datasets (reviewed in Plan 0010; not implemented)

The rules below are common to all three:
- **Nothing is downloaded during a benchmark run.** A separate `fetch` step downloads the
  source into a cache and checks the pinned SHA-256 and byte size. Locally the cache is
  `target/benchmark-cache/<dataset>/<source-sha256>/`. In CI it is a GitHub Actions cache
  keyed by the manifest's `output_checksum`, filled by a manual or scheduled
  `prepare-datasets` workflow; PR jobs only restore it.
- **The publishers provide no checksums** (verified for BEAR and TGB). The first reviewed
  download pins SHA-256 and size in the manifest, and any later mismatch fails.
- **Extraction is a pure function of the verified source and the manifest parameters.** Its
  output (canonical N-Quads plus a version or event index) has its own `output_checksum`.
- **Raw downloads are deleted after extraction in CI.** Locally,
  `scripts/benchmark.sh clean-cache` removes them (to be added with M3).
- **No third-party data is committed** until its redistribution basis has been reviewed (per
  dataset below). Project-generated data and manifests are committed.
- **Archive handling.**
  - Archive entries are streamed by name.
  - No path from an archive is ever used to write a file, so there is no path traversal
    (zip-slip) and symlinks are ignored.
  - Decompressed bytes are capped per dataset, at twice the manifest's expected
    uncompressed size, so a decompression bomb fails instead of filling the disk.
  - A restored CI cache is re-hashed against the manifest checksums before use. The cache
    key alone is never trusted.
- **Blank nodes** are forbidden in persistent ledger RDF. Extraction replaces each with a
  deterministic skolem IRI (`urn:bench:skolem:<dataset>:<sha256 of the source file and node
  label>`). It records the count; a count above zero is reported, never silent.
- **Batching.** The public API caps a patch at 10,000 operations. A source version larger
  than that is ingested as consecutive bulk commits, and the oracle checks state only at
  version boundaries. Limits are never raised for benchmarks.

### `bear-b-ci` — BEAR-B (DBpedia Live), **next milestone (M3)**

| item | plan |
|---|---|
| Source and version | BEAR-B, the 100 most volatile DBpedia Live resources, changesets of Aug–Oct 2015; archive files dated 2017-04-05. <https://aic.ai.wu.ac.at/qadlod/bear.html> |
| Granularity | **day**: 89 versions, ~33.5k triples in version 0 growing to ~43.9k, average change 1.78% per version. Hour (1,299 versions, 489 MB compressed IC) and instant are nightly candidates |
| Files | `BEAR_B/datasets/day/IC/alldata.IC.nt.tar.gz` (32,485,978 bytes; one N-Triples file per version) and `BEAR_B/datasets/day/CB/alldata.CB.nt.tar.gz` (1,129,879 bytes; added and deleted files per version). The file names inside the tarballs are unverified until the first reviewed download |
| Licence | DBpedia data: CC BY-SA 3.0 and GFDL (attribution, share-alike). The BEAR page states no data licence (unverified); the BEAR code is LGPL-3.0. **Decision:** do not commit extracted triples; cache them. Attribution goes in the manifest and the report |
| Expected sizes | ~33.6 MB download. The extracted window (12 versions × ~35–40k triples, canonical N-Quads) is ~60–80 MB uncompressed, an estimate to be replaced by measurement |
| Extraction | **all** triples of 12 **consecutive** day versions `V_s … V_{s+11}`. No triple sampling, which would destroy the version semantics. The window rule is fixed in the extraction version: `s = 0` unless review chooses the window with the most changes; that choice is recorded either way. Each triple is normalized to the ledger's canonical N-Quads (default graph) and blank nodes are skolemized as above |
| Temporal order and identity | version index = commit order; IRIs unchanged |
| Ledger representation | linear `main`: `V_s` as bulk genesis commits, then one commit per version, applying CB's deletes and adds (cross-checked as `IC(V_{k+1}) − IC(V_k)` and vice versa). A mismatch between CB and IC fails extraction |
| Production path | prepare/accept (ingest), `GET …/state` at every version (materialization), `ledger_rdf::diff` on materialized versions (`algorithm`, until a public diff exists), branch log (history) |
| Oracle | `state(V_k) == IC(V_k)` after normalization; `diff(V_j, V_k) ==` set difference of the IC versions; the CB changes equal the per-commit diff |
| CI budget | ~12 commits of ~40k triples: ingest is dominated by state size. Estimated < 2 min, to be measured before it joins `ci` |

### `tkgl-smallpedia-ci` — TGB 2.0 temporal KG (M4)

| item | plan |
|---|---|
| Source and version | TGB 2.0 `tkgl-smallpedia`, package dataset version 1. Built from the Wikidata dump of 2024-02-20 (entity ids < 1M, links 1900–2024; arXiv:2406.09639). Fetched by `py-tgb` from `https://object-arbutus.alliancecan.ca/swift/v1/14c95234f6cd4a21a47deafe20cce2a7/tgb/tkgl-smallpedia.zip` (10,565,876 bytes; last modified 2026-07-12; the package does no integrity check). We fetch directly; no Python ML stack is needed for extraction |
| Files | `tkgl-smallpedia_edgelist.csv` (`timestamp, head, tail, relation_type`; integer year) and `tkgl-smallpedia_static_edgelist.csv` (`head, tail, relation`) |
| Full size | 47,433 nodes, 550,376 temporal edges, 283 relation types, 125 yearly timesteps; 978,315 static edges. Chronological split at timestamp quantiles 0.70/0.85 by whole timesteps |
| Licence | data under the Wikidata licence (CC0 for structured data); TGB code MIT. A derived subset is low risk to redistribute. **Decision:** cache it like the others; revisit committing the reduced CSV at M4 review |
| Reduction (target ~2–5k nodes, ~10–50k temporal edges) | 1. Take the **contiguous** year interval `[Y0, Y1]` ending at the dataset's last year, the smallest such interval holding ≥ 60k temporal edges. 2. Seed entities: the top `N` by temporal degree inside the interval (ties broken by id). 3. Keep temporal edges with both ends among the seeds. 4. Grow `N` deterministically until 10k–50k edges remain and ≥ 20 relation types are represented. 5. Add the static edges among the selected entities. No random sampling across time |
| Temporal order, splits, leakage | Re-derive train < validation < test inside the subset by the same whole-timestep quantile rule; record the boundary years. Ties within a year are ordered by `(head, relation, tail)` |
| Identities | raw string ids are kept (the loader's first-seen integer ids are not used). Rendered as `<urn:tgb:smallpedia:e:{id}>`, `<urn:tgb:smallpedia:r:{id}>` (Wikidata IRIs only if review confirms the ids are Q/P ids) |
| Ledger representation | static edges as a bulk genesis; then one commit per year adding that year's facts as quads in the named graph `<urn:tgb:smallpedia:year:{YYYY}>`. Ledger state at year `T` = every fact ≤ `T` |
| Production path | ingest; `GET …/state` at year commits (snapshot reconstruction); branch log; `ledger_rdf::diff` between years |
| Oracle | the cumulative replay of the source events. **No-leakage assertion:** the state at the commit of year `T` contains no fact of a year > `T` (the invariant `features(T) ⊆ information ≤ T`). The ML smoke tests of the requirements' M4 are out of scope for Phase 6 |

### `thgl-software-ci` — TGB 2.0 heterogeneous software graph (M5)

| item | plan |
|---|---|
| Source and version | TGB 2.0 `thgl-software`, package dataset version 1. GH Archive, January 2024; nodes with ≥ 10 interactions; node ids anonymized to integers. Fetched from `…/tgb/thgl-software.zip` (1,492,169,637 bytes, ~1.49 GB; last modified 2026-07-12) |
| Files | `thgl-software_edgelist.csv` (`timestamp, head, tail, relation_type`; UNIX seconds, integer ids) and `thgl-software_nodetype.csv` (`node_id, type`). Whether name-mapping files are in the zip is unverified |
| Full size | 681,927 nodes, 1,489,806 edges, 4 node types, 14 relation types, 689,549 one-second timesteps; chronological 70/15/15 split |
| Licence | CC BY 4.0 (content based on gharchive.org; paper Appendix C). Redistribution needs attribution. **Decision:** cache the derived subset; never commit the raw archive |
| Download and cleanup | the 1.49 GB download only happens in the manual `prepare-datasets` workflow or locally on explicit request, after a free-disk check (≥ 6 GB; the uncompressed size is unverified). Extraction streams the zip entries without full decompression to disk where possible; the raw zip is deleted after a verified extraction |
| Reduction (target ~2–5k nodes, ~10–50k events) | 1. A **contiguous** time window from the dataset start, extended in whole hours until ≥ 200k events. 2. Per relation type, select seed nodes by activity inside the window, with a **per-type quota** so that all 14 relation types and all 4 node types survive (not just the most frequent edge type). 3. Keep events with both endpoints selected. 4. Adjust the quota deterministically to land in 10k–50k events |
| Temporal order and identity | events ordered by `(timestamp, head, relation, tail)`; integer ids kept as `<urn:tgb:software:n:{id}>` with a type quad per node (`rdf:type <urn:tgb:software:type:{t}>`) |
| Ledger representation | node types as a bulk genesis; one commit per hour bucket, adding that hour's events as quads in `<urn:tgb:software:hour:{unix-hour}>` |
| Production path | ingest; snapshot reconstruction at hour commits (typed history); branch log |
| Oracle | the cumulative event replay; the type of every node constant over time; the same no-leakage assertion per hour as above |

## Recommended sequence

1. **M3 `bear-b-ci`.** Integrate it unchanged first: it directly exercises version
   materialization, version diff and history. Record pre-checkpoint reconstruction baselines
   on it.
2. **Checkpoint ADR.** Draft it from the synthetic and BEAR measurements ([METRICS.md](METRICS.md)).
3. **`tkgl-smallpedia-ci` and `thgl-software-ci`.** Add them next. By the end of Phase 6 the
   `ci` profile should hold all four reduced datasets.
