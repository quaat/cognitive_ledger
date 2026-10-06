# Benchmark datasets

Every dataset has a committed manifest under `benchmark/datasets/<id>.json`. The harness
recomputes the manifest and refuses to run when any field differs (see
[REPRODUCIBILITY.md](REPRODUCIBILITY.md)).

| dataset | purpose | status |
|---|---|---|
| `synthetic-ledger-ci` | commit, branch and merge correctness oracle; small representative workload (PR CI) | **implemented** (Plan 0010) |
| `synthetic-ledger-local` | the same generator with a deeper history (≈1,500 commits, parent-0 depth ≈650) for baselines | **implemented** (Plan 0010) |
| `bear-b-ci` | temporal RDF versions: version materialization, version diff, history (genuine DBpedia Live evolution) | **implemented** (Plan 0011, M3); in the `ci` profile |
| `tkgl-smallpedia-ci` | temporal knowledge graph: snapshot reconstruction and no-leakage history access | plan below (M4) |
| `thgl-software-ci` | heterogeneous temporal event graph: event replay and typed history | plan below (M5) |
| Full BEAR, OGB, large TGB, Software Heritage, LDBC, GNN quality | nightly and scale programmes | out of scope for Phase 6A |

## `synthetic-ledger-ci` / `synthetic-ledger-local` (implemented)

Generator: `apps/ledger-bench/src/synthetic.rs`. Its version (`GENERATOR_VERSION`) is recorded in each manifest's `generator.version`, the authoritative value. It is a
single SplitMix64 stream from the seed in the manifest. The data is project-owned and
generated on every run; nothing is committed except the manifest.

**Shape of `ci`** (the parameters are in the manifest):
- ~1,000 entities with 6 statements each, plus 100 statements in the named graph
  `<urn:bench:g:1>`: 6,100 initial quads.
- The initial load is two bulk commits (≤ 5,000 statements each, below the API's
  10,000-operation limit).
- 205 commits, among them 15 integration commits, over 9 branches:
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

The common rules below apply to every external dataset. They describe what `bear-b-ci`
implements (`apps/ledger-bench/src/{bear,archive}.rs`); the TGB plans further down predate
them and must be brought in line when implemented.

- **Nothing is downloaded during a benchmark run.**
  - A separate `ledger-bench fetch <dataset>` step downloads the pinned source files into
    `target/benchmark-cache/<dataset>/source/` (file names derived from role and URL,
    validated) and checks the pinned SHA-256 and byte size before the file is renamed into
    place.
  - Transient failures (transport errors, HTTP 429/5xx) are retried twice with backoff, and
    a stalled transfer times out after 120 s. Integrity failures are never retried.
  - Redirects are followed only over https on the same host.
  - In CI, `ci-benchmark` restores `target/benchmark-cache/<dataset>/source/` from a GitHub
    Actions cache keyed by the hash of the dataset manifest. On a miss, the job's `fetch`
    downloads the source. There is no separate prepare-datasets workflow.
  - `fetch` and `prepare` re-hash a restored cache against the manifest; the cache key alone
    is never trusted.
- **The publishers provide no checksums** (verified for BEAR and TGB). The first reviewed
  download pins SHA-256 and size in the manifest, and any later mismatch fails.
- **Extraction is a pure function of the verified source and the extraction constants.**
  - `ledger-bench prepare <dataset>` writes one prepared artifact to
    `target/benchmark-cache/<dataset>/prepared/`. The manifest pins its SHA-256 and size,
    plus the workload checksum.
  - A run loads only that artifact and refuses it on any mismatch.
- **Cache cleanup.** Raw downloads are kept, not deleted after extraction, in CI as well.
  `ledger-bench clean <dataset>` removes the prepared artifact; `--all` also removes the
  downloaded sources.
- **No third-party data is committed** until its redistribution basis has been reviewed (per
  dataset below). Project-generated data and manifests are committed.
- **Archive handling.**
  - gzip is decompressed in memory with a fixed hard output cap per stream (512 MiB). The
    cap is a constant, not derived from the manifest. Exactly one gzip member is accepted,
    and trailing data is refused.
  - tar is read by a minimal ustar reader. Only regular-file entries whose names pass a
    strict pattern are selected, with a 16 MiB per-entry cap and a 256 MiB total cap. Links,
    directories, devices and extension records are skipped and counted. Duplicate names, bad
    header checksums, truncation and a missing end-of-archive marker fail.
  - Selected entries stay in memory. No path from an archive is ever used to write a file,
    so there is no path traversal (zip-slip) and no link following.
  - Archive content is never executed.
- **Blank nodes** are forbidden in persistent ledger RDF. The planned rule is to replace each
  with a deterministic skolem IRI (`urn:bench:skolem:<dataset>:<sha256 of the source file
  and node label>`) and record the count. Skolemization is **not implemented yet**: BEAR-B
  has no blank nodes, and `bear-b-ci` extraction fails on any blank node until the
  skolemization is implemented and reviewed.
- **Batching.** The public API caps a patch at 10,000 operations. A source version larger
  than that is ingested as consecutive bulk commits (`bear-b-ci` uses at most 5,000
  operations and 1.4 MB per commit), and the oracle checks state only at
  version boundaries. Limits are never raised for benchmarks.

## `bear-b-ci` — BEAR-B (DBpedia Live), implemented (Plan 0011, M3)

The authoritative values are the committed manifest `benchmark/datasets/bear-b-ci.json` (v2):
pinned source files, extraction counts and the prepared artifact's SHA-256. Code:
`apps/ledger-bench/src/bear.rs`.

**Source facts, verified on the first reviewed download (2026-10-06)**, superseding the plan
written before verification.
- Distribution and versions:
  - Publisher: BEAR, WU Vienna (QADLOD).
  - Landing page <https://aic.ai.wu.ac.at/qadlod/bear.html>.
  - Day granularity: **89 versions**, 33,502 → 43,907 triples (the publisher's statistics,
    which match the IC files).
  - Archives dated 2017-04-05 (HTTP Last-Modified).
- Files used:

  | role | file | bytes | contents |
  |---|---|---:|---|
  | full versions (IC) | `day/IC/alldata.IC.nt.tar.gz` | 32,485,978 | 89 regular entries `000001.nt.gz` … `000089.nt.gz`, each a gzipped N-Triples file |
  | changesets (CB) | `day/CB/alldata.CB.nt.tar.gz` | 1,129,879 | 176 entries `data-added_k-(k+1).nt.gz` and `data-deleted_k-(k+1).nt.gz`, k = 1…88 |
  | time-annotated (TB) | `day/TB/alldata.TB.nq.gz` | 1,041,716 | N-Quads; graph `<http://example.org/v0_1_…>` lists the 0-based versions containing a triple, plus 25,172 `owl:versionInfo` statements about those graphs |

  There are no symlinks or directories in the archives. CBTB was inspected but is not used.
- **No publisher checksums exist.** SHA-256 and byte size were pinned on the first reviewed
  download and confirmed by a second, independent `fetch`. Any later mismatch fails.
- **License.** The BEAR page states no data license (checked 2026-10-06). The data derives
  from DBpedia (CC BY-SA 3.0 and GFDL; attribution, share-alike). BEAR's code is LGPL-3.0.
  Redistribution of a derived subset has **not** been reviewed, so nothing third-party is
  committed: sources and prepared artifacts live only in the local or CI cache. Attribution
  is in the manifest.
- **Blank nodes:** none. Parse failures: none. Normalization collisions: none.

**Finding: BEAR-B day publishes two internally consistent but different lineages.**
- IC file 1 plus the cumulative CB changes equals TB at **all 88 steps**: a changeset lineage.
- The IC files after version 1 drop stale values the changesets never delete (for example
  old `wikiPageLength` and `wikiPageModified` values). They disagree with both CB and TB:
  - CB-deleted lacks IC's deletions, 191 of 370 for 1→2;
  - every step carries 6–14 no-op churn triples in both CB files;
  - by the last version IC holds 43,907 triples and TB 63,993.
- The plan's cross-check "IC differences equal CB" therefore **fails on the published
  data**. That is a property of the source, not of extraction.

`bear-b-ci` follows the lineage where two independent representations agree exactly:
- **Oracle:** TB's per-version membership (full versions).
- **Hard cross-checks at preparation:**
  - TB version 0 equals IC file 1 (the shared start);
  - at every step, CB's net change (added − deleted, deleted − added) equals TB's
    difference;
  - no-op churn (in both CB files) is present in both versions;
  - split TB annotations of one triple (13 triples, 733 extra lines) have disjoint version
    lists.
  - IC ⊆ TB at **every** source version (all 89, `ic_subset_of_tb_versions_checked`): IC
    only drops statements the changesets keep.
- **IC divergence:** pinned as counts (`ic_lineage_divergence_in_window`,
  `ic_lineage_divergence_all_versions`), not hidden. An IC-lineage dataset is possible later
  and needs its own review.
- TB and CB are two encodings of the selected lineage, and BEAR probably derived TB from
  the changesets. Their agreement shows that the extraction reads both consistently. It does
  not establish an external ground truth.

**Extraction** (`bear-b-day-extract/2`, deterministic):
- Every triple is normalized through the ledger's canonical N-Quads rules
  (`ledger_rdf::Quad`). This is a labelled dependency of the oracle on the ledger's frozen
  canonical form.
- Window: the 12 consecutive versions with the most adds plus deletes, lowest start on ties.
  The rule selects **v22..=v33** (0-based TB numbering; IC files 000023…000034):
  - 36,645 → 41,316 triples;
  - 9,384 adds and 4,713 deletes;
  - 199 reappearances.

  Version 0 would have been nearly static (584 adds and 226 deletes over 12 versions, no
  reappearance).
- All triples of every selected version are kept; nothing is sampled.

**Workload:**
- Ingest: `main` ingests v22 as bulk commits, then one version per commit. Every commit
  holds at most 5,000 operations and 1.4 MB of quads (the API caps a request at 10,000
  operations and 2 MiB; limits are never raised). The last commit of each version carries
  the version label: its state must equal the source version.
- Asserted: every commit's state; 28 diffs (11 adjacent pairs and 3 wider gaps, each in both
  directions) against the source set differences (`algorithm` category); appear, disappear
  and reappear history facts; parents and provenance.

**Lifecycle** ([RUNNING_BENCHMARKS.md](RUNNING_BENCHMARKS.md)):
1. `fetch` is the only networked step.
2. `prepare` is offline extraction with the cross-checks.
3. `validate` and `run` are offline and load only the hash-pinned artifact. A missing or
   stale cache fails with exit 3; nothing downloads implicitly.

**Archive safety:**
- gzip output is capped;
- a minimal ustar reader returns only regular-file entries with validated names, in memory;
- no archive path is ever used for writing;
- links, directories and extension records are skipped;
- duplicates, truncation and bad header checksums fail;
- the prepared artifact is capped and hashed.

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

1. **M3 `bear-b-ci`.** Done in Plan 0011, together with the reconstruction characterization.
2. **Checkpoint ADR.** Draft it from the Plan 0011 measurements.
3. **`tkgl-smallpedia-ci` and `thgl-software-ci`.** Add them next. By the end of Phase 6 the
   `ci` profile should hold all four reduced datasets.
