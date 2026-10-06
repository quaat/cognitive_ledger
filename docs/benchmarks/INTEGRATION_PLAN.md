> Supplied requirements document (2026-10-06), committed verbatim as the source of the benchmark programme. Execution status lives in the execution plans (Plan 0010 onwards) and the other files in this directory; this file is not updated with progress.

# Cognitive Ledger Benchmark Integration and Validation Plan

You are working on the **Sculpin Cognitive Ledger**. Your task is to establish a rigorous, reproducible benchmark framework for validating and optimizing:

1. immutable graph/version storage,
2. commit-DAG traversal,
3. branch and merge behavior,
4. temporal RDF reconstruction,
5. snapshot and delta performance,
6. provenance/history queries,
7. replay of graph evolution,
8. GNN and temporal-GNN training/inference over ledger history,
9. scalability,
10. correctness under graph evolution.

Do **not** treat this as simply downloading datasets and writing ad-hoc benchmark scripts. Build a maintainable benchmark subsystem that can be used continuously as the Cognitive Ledger evolves.

The benchmark architecture must distinguish clearly between:

- **ledger correctness benchmarks**,
- **ledger storage/performance benchmarks**,
- **graph algorithm benchmarks**,
- **GNN predictive-quality benchmarks**,
- **scale/stress benchmarks**.

Do not alter production semantics merely to improve benchmark scores.

---

# 1. CI/CD Benchmark Dataset Pack — implement this first

Before integrating full datasets, create a deliberately reduced and deterministic benchmark bundle suitable for pull-request CI.

Call this profile something equivalent to:

```text
benchmark-ci
```

It should complete quickly enough to run on every PR while still exercising the essential behavior of the Cognitive Ledger.

Target approximately:

```text
total benchmark runtime: preferably < 5 minutes
hard upper target:       < 10 minutes
memory:                  suitable for normal CI runners
network dependency:      none during test execution
```

The exact runtime target may be adjusted after measuring the existing CI environment, but CI benchmarks must remain intentionally small.

## 1.1 CI benchmark components

Include four datasets.

### A. Synthetic Cognitive Ledger DAG

This is the most important CI dataset because we completely control its expected behavior.

Generate deterministically from a fixed seed.

Suggested initial profile:

```text
entities:               ~1,000
initial triples:        ~5,000–10,000
commits:                ~200
branches:               4–8
merges:                 >= 10
intentional conflicts:  >= 5
schema changes:         several
SHACL violations:       several intentional cases
insert/update/delete:   all represented
```

Include histories resembling:

```text
A──B──C──D────────H──I
      \          /
       E──F─────G

         ┌──K──L──┐
J────────┤        ├──O
         └──M──N──┘
```

The generator must know the expected:

- parent relationships,
- merge bases,
- branch heads,
- state of every commit,
- inserted statements,
- removed statements,
- merge result,
- conflicts,
- provenance chain.

This becomes the primary ledger correctness oracle.

---

### B. Reduced BEAR-B archive

Use BEAR-B to exercise genuine evolving RDF data.

Do not randomly sample independent triples from different versions. That would destroy the benchmark.

Instead:

1. inspect BEAR-B's available versions;
2. select a **small consecutive temporal interval**;
3. preserve version ordering;
4. preserve additions and deletions between versions;
5. deterministically extract a connected or otherwise coherent RDF subset;
6. record extraction rules and checksums.

Target approximately:

```text
8–20 consecutive versions
10k–100k triples/version depending on CI runtime
```

The reduction must retain actual changes between versions.

Benchmark:

```text
version materialization
version diff
history lookup
temporal SPARQL
incremental ingest
```

---

### C. Reduced `tkgl-smallpedia`

Create a small chronological slice of TGB's temporal knowledge graph benchmark.

Do not randomize events across time.

Suggested target:

```text
nodes:           ~2k–5k
temporal edges:  ~10k–50k
relations:       preserve multiple relation types
timestamps:      contiguous interval
```

Where possible, retain corresponding static relations associated with selected entities.

The extraction must preserve:

```text
train_time < validation_time < test_time
```

There must be no temporal leakage.

Use this CI dataset to test:

- ledger population from temporal KG events,
- snapshot reconstruction,
- historical feature extraction,
- simple temporal link prediction,
- GNN integration smoke tests.

CI does **not** need to produce state-of-the-art model accuracy.

---

### D. Reduced `thgl-software`

Create a deterministic temporal heterogeneous subset of the software activity dataset.

Preserve:

- temporal ordering,
- node types,
- edge/relation types,
- identities needed for selected events.

Target approximately:

```text
nodes:           ~2k–5k
events:          ~10k–50k
multiple node types
multiple relation types
```

Avoid selecting only the most frequent edge type.

This dataset should exercise:

```text
heterogeneous graph reconstruction
event replay
typed neighborhood extraction
temporal GNN input generation
next-event prediction smoke tests
```

---

## 1.2 CI dataset artifacts

Generated CI datasets must be reproducible.

Create something equivalent to:

```text
benchmarks/
  datasets/
    manifests/
    ci/
  scripts/
    download/
    extract/
    validate/
```

Each reduced dataset must have a manifest containing at least:

```yaml
dataset:
source:
source_version:
source_url:
license:
download_date:
source_checksum:
extraction_algorithm_version:
extraction_seed:
temporal_range:
entity_count:
edge_or_triple_count:
relation_count:
output_checksum:
```

Do not rely on a developer manually preparing these datasets.

Prefer:

```text
download source
     ↓
verify checksum
     ↓
deterministic extraction
     ↓
verify extracted checksum
```

For CI itself, use a versioned cached/artifact form so the CI job does not download multi-gigabyte datasets.

---

# 2. Benchmark architecture

Before integrating the full benchmarks, inspect the existing Cognitive Ledger architecture and determine the cleanest boundary for benchmark code.

Do not scatter benchmark-specific logic through production packages.

Prefer a structure conceptually similar to:

```text
benchmarks/
├── README.md
├── pyproject.toml / package metadata as appropriate
├── configs/
│   ├── ci.yaml
│   ├── local.yaml
│   ├── nightly.yaml
│   └── scale.yaml
├── datasets/
│   ├── manifests/
│   └── adapters/
├── ledger/
│   ├── ingest
│   ├── materialize
│   ├── diff
│   ├── history
│   └── merge
├── gnn/
│   ├── adapters
│   ├── baselines
│   └── evaluation
├── runners/
├── reports/
└── scripts/
```

Adapt this to the repository rather than imposing it if the project already has an appropriate structure.

---

# Milestone 0 — Repository and architecture assessment

Before implementing anything:

1. inspect the entire Cognitive Ledger implementation relevant to:
   - commits,
   - snapshots,
   - deltas,
   - parent relationships,
   - branches,
   - merges,
   - provenance,
   - RDF representation,
   - query interfaces,
   - persistence,
   - existing tests;
2. determine what benchmark hooks already exist;
3. identify missing public/internal APIs;
4. inspect CI configuration;
5. inspect container/dev environment;
6. identify languages and ML frameworks already used.

Produce:

```text
BENCHMARK_ARCHITECTURE.md
```

It should describe:

- current architecture,
- proposed benchmark integration,
- dataset adapters,
- benchmark execution flow,
- output schema,
- CI/nightly/scale profiles,
- dependencies,
- risks.

### Gate M0

Do not proceed until the benchmark design can exercise production APIs without duplicating Cognitive Ledger behavior inside the benchmark framework.

---

# Milestone 1 — Common benchmark harness

Implement a dataset-independent benchmark runner.

The runner should support something conceptually like:

```bash
benchmark list
benchmark prepare <dataset>
benchmark validate <dataset>
benchmark run <suite> --profile ci
benchmark run <suite> --profile local
benchmark run <suite> --profile nightly
benchmark report <run>
```

Do not force this exact CLI if the repository has established conventions.

## Common metrics

All performance tests should report at least:

```text
wall time
CPU time where available
peak RSS/memory
operation count
throughput
p50
p95
p99
dataset identifier
dataset checksum
git commit
benchmark configuration
```

Ledger benchmarks should additionally capture:

```text
commit ingestion rate
snapshot materialization latency
delta computation latency
historical query latency
DAG traversal latency
storage amplification
database/storage size
cold reconstruction latency
warm reconstruction latency
```

Store machine-readable output, preferably JSON.

Example:

```json
{
  "benchmark": "materialize_snapshot",
  "dataset": "bear-b-ci",
  "gitCommit": "...",
  "operations": 100,
  "p50Ms": 18.2,
  "p95Ms": 31.7,
  "p99Ms": 44.0
}
```

Generate a human-readable summary from the same source.

### Gate M1

- benchmark runner is deterministic;
- results contain enough provenance to reproduce the run;
- failures return non-zero status;
- test and benchmark logic are separated cleanly;
- CI profile works without Internet access.

---

# Milestone 2 — Synthetic Cognitive Ledger benchmark

Implement the deterministic synthetic generator described in Section 1.

This benchmark is not merely a load generator.

It must act as a **correctness oracle**.

Test:

### Commit semantics

```text
create commit
retrieve commit
parent traversal
ancestor traversal
merge base
branch head
```

### Historical graph semantics

For arbitrary commit `C`:

```text
materialize(C) == expected_graph(C)
```

For arbitrary commits `A` and `B`:

```text
diff(A, B) == expected_diff(A, B)
```

### Branching

Verify changes on one branch do not appear on another before merge.

### Merge

Test:

```text
non-conflicting additions
non-conflicting deletions
same-statement changes
delete-vs-modify
schema changes
conflicting values
```

Do not invent merge semantics. Test whatever semantics the Cognitive Ledger explicitly defines.

### Provenance

Every generated change should have traceable origin.

### SHACL / validation

Include expected-valid and expected-invalid commits.

### Gate M2

100% agreement with generator ground truth.

Any graph-content mismatch is a hard failure.

---

# Milestone 3 — BEAR integration

Integrate:

```text
BEAR-B first
BEAR-A second
BEAR-C query workloads where applicable
```

Create an adapter capable of representing a sequence:

```text
V0 → V1 → V2 → ... → Vn
```

as Cognitive Ledger commits.

Evaluate at least:

### Version materialization

```text
materialize(version N)
```

### Delta materialization

```text
diff(version N, version M)
```

### Version/history query

Determine when a statement:

```text
appeared
changed
disappeared
reappeared
```

### Storage strategies

Where supported, benchmark the current Cognitive Ledger strategy against reasonable configuration alternatives such as snapshot intervals.

Do not prematurely optimize.

Establish baseline measurements first.

### Gate M3

- reconstructed versions match source versions;
- computed deltas match source changes;
- benchmark reproducible;
- reduced CI dataset runs in CI;
- full BEAR benchmark runs locally/nightly.

---

# Milestone 4 — TGB temporal knowledge graph integration

Integrate:

```text
tkgl-smallpedia
```

before attempting `tkgl-wikidata`.

Represent static facts separately from temporal events where appropriate.

Conceptual mapping:

```text
genesis commit
    +
static knowledge

t0 commit
    +
events at t0

t1 commit
    +
events at t1
...
```

Keep both:

```text
event_time
commit_time
```

when the Ledger data model supports them.

## Anti-leakage requirement

For a prediction at commit/time `T`, a model must not access:

```text
future commits
future events
future labels
future-derived features
```

Implement explicit assertions protecting this boundary.

## Initial models

Do not start by implementing a novel GNN.

First establish trivial/reference baselines:

```text
random
frequency/popularity
last-value / persistence where meaningful
EdgeBank-style temporal baseline where applicable
```

Then integrate a small number of appropriate established graph models using the ML stack already preferred by the repository.

Possible models include:

```text
R-GCN
GraphSAGE
GAT
TGN
```

Use only models appropriate to the dataset and dependencies.

### Gate M4

- official temporal split preserved;
- no information leakage;
- model input reproducible from ledger commits;
- metrics reproducible with fixed seed;
- reduced version works in CI;
- full `tkgl-smallpedia` works outside PR CI.

---

# Milestone 5 — TGB heterogeneous software graph

Integrate:

```text
thgl-software
```

Preserve heterogeneous node and relation types.

Map temporal software events into ledger commits/events without flattening all relations into a single edge type.

Test:

```text
event replay
typed neighborhood query
historical neighborhood reconstruction
next-event dataset generation
temporal batching
```

Run at least one non-neural baseline and one heterogeneous/temporal neural baseline if supported cleanly by the selected framework.

### Gate M5

For any sampled prediction at `T`, prove programmatically that all extracted features are derived only from ledger ancestry/history available at `T`.

---

# Milestone 6 — OGB WikiKG benchmark

Integrate:

```text
ogbl-wikikg2
```

This benchmark serves a different purpose from BEAR.

Use it primarily to answer:

> Can graph data reconstructed through the Cognitive Ledger be used by standard KG/GNN pipelines without loss of correctness, and how does model quality compare with conventional baselines?

Do not pretend its small number of temporal snapshots makes it a full Cognitive Ledger benchmark.

Test:

```text
KG reconstruction
relation preservation
train/validation/test split integrity
link-prediction pipeline
MRR
Hits@K where applicable
```

Run this as a nightly/explicit benchmark rather than on every PR.

### Gate M6

At least one known baseline must execute successfully and produce sane, reproducible metrics.

Do not require state-of-the-art performance.

---

# Milestone 7 — Large temporal datasets

After smaller integrations are stable, add:

```text
tkgl-wikidata
thgl-github
```

These are **scale tests**, not CI datasets.

Use them to identify:

```text
memory scaling
commit ingestion scaling
snapshot reconstruction scaling
historical query scaling
GNN preprocessing cost
training throughput
I/O bottlenecks
```

Define dataset-size sweeps where possible:

```text
1%
5%
10%
25%
50%
100%
```

Prefer coherent temporal prefixes/subgraphs rather than arbitrary random edge deletion.

Generate scaling plots/reports automatically.

### Gate M7

Produce reproducible scaling curves and identify the first dominant bottleneck rather than simply reporting a maximum graph size.

---

# Milestone 8 — Real DAG benchmark using Software Heritage

Integrate a deliberately selected subset of Software Heritage.

Do **not** download or ingest the global Software Heritage graph for normal development.

Create a small corpus of repositories exhibiting different history structures:

```text
long linear history
many short-lived branches
long-lived branches
frequent merges
deep merge ancestry
large commits
many small commits
```

Use this primarily for DAG tests rather than RDF semantics.

Benchmark:

```text
ancestor traversal
common ancestor
merge-base
history traversal
branch reconstruction
reachability
topological traversal
commit lookup
```

Where practical, compare results against the source VCS history.

### Gate M8

DAG traversal results must agree with known source histories for the selected repositories.

---

# Milestone 9 — Operational graph benchmark

Only after the ledger-specific benchmarks are stable, evaluate adding:

```text
LDBC SNB
and/or
LDBC FinBench
```

Use these to test:

```text
continuous writes
concurrent reads
large neighborhood queries
mixed read/write workload
latency under ingestion
```

Do not distort the Cognitive Ledger API merely to conform to LDBC.

Build a translation layer if necessary.

### Gate M9

Produce mixed-workload throughput and latency measurements with clearly documented workload translation.

---

# Milestone 10 — Cognitive/GNN intervention benchmark

Create an additional synthetic benchmark specifically for the future Cognitive Ledger learning objective.

Represent scenarios containing:

```text
method
tool
threshold
configuration
environment/context
observed outcome
```

Generate known relationships between interventions and outcomes.

Include:

```text
irrelevant changes
confounders
delayed effects
repeated interventions
context-dependent effects
regime changes
contradictory observations
branch-specific histories
```

Example:

```text
State S0
   │
   ├── intervention A
   │       ↓
   │    outcome X
   │
   └── intervention B
           ↓
        outcome Y
```

The generator must retain hidden ground truth so learned attention/predictions can be evaluated against known causal structure.

Do **not** describe prediction accuracy as causal discovery unless the benchmark and methodology actually justify causal claims.

Compare:

```text
current snapshot only

vs

last N snapshots

vs

changesets only

vs

full ledger history

vs

history selected by learned attention
```

This benchmark should eventually answer the important product question:

> Does access to Cognitive Ledger history provide measurably better prediction than access to only the current graph?

---

# Milestone 11 — Benchmark profiles

Establish explicit profiles.

## `ci`

Run on every pull request:

```text
synthetic-ledger-ci
bear-b-ci
tkgl-smallpedia-ci
thgl-software-ci
```

Focus on:

```text
correctness
regression detection
API compatibility
data leakage detection
basic performance sanity
```

Avoid fragile micro-performance gates.

---

## `nightly`

Run:

```text
full BEAR-B
full tkgl-smallpedia
full thgl-software
ogbl-wikikg2 where resources permit
larger synthetic workloads
```

Collect performance trends.

---

## `weekly` / `scale`

Run:

```text
BEAR-A
tkgl-wikidata
thgl-github
OGB large benchmark if resources permit
Software Heritage corpus
LDBC
```

These may require dedicated hardware.

---

# Performance regression strategy

Do not initially fail CI because something became 3% slower.

Performance measurements are noisy.

Separate:

```text
correctness gates
```

from:

```text
performance observations
```

Initially record benchmark history.

Once stable baselines exist, introduce regression gates based on statistically meaningful tolerances.

For example:

```text
hard correctness mismatch → fail

large regression > defined tolerance
confirmed over repeated runs → fail/warn according to profile
```

Document methodology before enforcing thresholds.

---

# Benchmark reproducibility

Every result must capture:

```text
repository commit
dataset
dataset checksum
dataset extraction version
benchmark configuration
random seed
dependency versions
Python/runtime version
OS
CPU where available
RAM where available
GPU where relevant
start/end time
```

For GNN experiments additionally record:

```text
model
hyperparameters
optimizer
learning rate
epochs
batch size
negative-sampling method
train/val/test ranges
random seed
device
best checkpoint criterion
```

A result without sufficient provenance should not be treated as an official benchmark.

---

# Dataset licensing and storage

Before adding each dataset:

1. verify its license and terms;
2. record attribution requirements;
3. determine whether reduced/derived versions may be redistributed;
4. do not commit large raw datasets to Git;
5. do not accidentally redistribute a dataset whose license forbids it.

Prefer:

```text
dataset manifest
download script
deterministic extraction script
```

over committed raw data.

A small generated dataset owned by the project can be committed directly.

If redistribution of an extracted third-party dataset is permitted and useful for CI, document the legal basis clearly.

Otherwise generate/cache it in CI infrastructure.

---

# Dataset adapter interface

Build a common abstraction where practical.

Conceptually:

```python
class BenchmarkDataset:
    metadata()
    download()
    verify()
    prepare()
    iter_events()
    expected_state(...)
```

Do not force all datasets into the same semantic representation.

For example:

```text
BEAR        → RDF versions
TGB TKG     → temporal typed facts
TGB THG     → heterogeneous temporal events
Software Heritage → DAG objects
LDBC        → transactional event stream
```

A common lifecycle is useful.

A fake universal graph schema is not.

---

# GNN integration architecture

Keep ML/GNN processing separated from ledger persistence.

Preferred conceptual flow:

```text
Dataset
   ↓
Cognitive Ledger
   ↓
historical graph view
   ↓
feature extraction
   ↓
GNN adapter
   ↓
model
   ↓
evaluation
```

Do not allow a GNN adapter to bypass the Cognitive Ledger when the benchmark is intended to evaluate ledger-backed learning.

At the same time, support a control path:

```text
Dataset
   ↓
direct conventional GNN pipeline
```

so that we can compare:

```text
direct dataset → GNN
```

against:

```text
dataset → Cognitive Ledger → reconstructed graph → GNN
```

This is important for detecting representation mistakes and quantifying ledger overhead.

---

# Required correctness invariant

For supported datasets, verify:

```text
direct_graph(T) == ledger_materialized_graph(T)
```

after normalization.

For temporal prediction:

```text
features(T)
⊆
information_available_at_or_before(T)
```

These should become automated assertions.

---

# Reporting

Create a benchmark report generator.

A report should make it easy to compare runs by commit.

Include at least:

```text
Dataset
Dataset size
Ledger commits
Triples/edges/events
Ingestion rate
Storage size
Snapshot p50/p95/p99
Diff p50/p95/p99
History query p50/p95/p99
GNN preprocessing time
Training time
Inference time
Prediction metric
Peak memory
```

For scaling runs include plots of:

```text
dataset size vs ingest throughput
dataset size vs storage
dataset size vs reconstruction latency
history depth vs reconstruction latency
commit count vs ancestry traversal
graph size vs model training time
```

Do not manually maintain benchmark numbers in documentation.

Generate reports from machine-readable result artifacts.

---

# Documentation

Create:

```text
docs/benchmarks/
```

with at least:

```text
README.md
DATASETS.md
RUNNING_BENCHMARKS.md
METRICS.md
REPRODUCIBILITY.md
GNN_BENCHMARKS.md
```

`DATASETS.md` should explain why each dataset exists.

For example:

| Dataset | Purpose |
|---|---|
| Synthetic Ledger | commit/branch/merge correctness |
| BEAR | temporal RDF/version performance |
| `tkgl-smallpedia` | temporal KG/GNN |
| `thgl-software` | heterogeneous temporal graph |
| `ogbl-wikikg2` | external KG benchmark |
| Software Heritage | real commit DAG |
| LDBC | operational workload |
| `tkgl-wikidata` | temporal scale |
| `thgl-github` | heterogeneous scale |

---

# Final quality gates

Do not declare the benchmark effort complete until all of the following hold.

## Correctness

- deterministic synthetic benchmark passes;
- reconstructed BEAR versions match source data;
- TGB chronological splits remain intact;
- future information cannot leak into training features;
- DAG results agree with source histories.

## Reproducibility

A fresh developer environment can reproduce benchmark preparation and execution from documentation.

## CI

The reduced benchmark pack executes automatically and deterministically.

## Separation

Benchmark tooling does not contaminate production Cognitive Ledger semantics.

## Dataset provenance

Every external dataset has source, version, checksum and license information.

## GNN validity

At least one simple conventional baseline works before experimental models are introduced.

## Performance

Baseline measurements exist before optimization begins.

## Reporting

Results are machine-readable and comparable across commits.

---

# Development approach

Work incrementally.

For each milestone:

1. inspect existing implementation;
2. write/update tests first where practical;
3. implement the smallest clean integration;
4. run existing test suite;
5. run benchmark-specific tests;
6. inspect results;
7. document findings;
8. commit logically;
9. stop and report milestone status before beginning substantially different work.

Use specialized subagents where useful for:

```text
dataset/licensing research
benchmark architecture review
RDF correctness review
temporal-ML review
performance methodology review
test review
```

The main agent remains responsible for architecture and integration decisions.

Do not allow subagents to independently redesign production architecture.

---

# Initial execution order

Implement in this order:

```text
M0  repository/architecture assessment
 ↓
M1  common benchmark harness
 ↓
M2  synthetic ledger benchmark
 ↓
CI benchmark pipeline established
 ↓
M3  BEAR-B
 ↓
M4  tkgl-smallpedia
 ↓
M5  thgl-software
 ↓
M6  ogbl-wikikg2
 ↓
M7  large TGB datasets
 ↓
M8  Software Heritage
 ↓
M9  LDBC
 ↓
M10 intervention/cognitive benchmark
 ↓
M11 benchmark profiles + mature regression gates
```

Do not begin with the largest datasets.

The goal of the first iterations is **correctness, reproducibility and trustworthy measurement**, not impressive benchmark scale.

---

# First deliverable

For the first development pass, complete only:

```text
M0
M1
M2
the CI dataset architecture
```

and prepare the deterministic extraction plan for:

```text
BEAR-B
tkgl-smallpedia
thgl-software
```

Do not download very large datasets until their adapters, storage locations, licensing, expected disk consumption and cleanup strategy have been reviewed.

At the end of this first pass report:

1. repository findings;
2. benchmark architecture;
3. files added/changed;
4. synthetic dataset characteristics;
5. tests added;
6. CI runtime;
7. benchmark runtime;
8. unresolved architectural issues;
9. dataset/download requirements for the next milestone;
10. recommendation whether M3 should proceed unchanged.

Stop there for review.