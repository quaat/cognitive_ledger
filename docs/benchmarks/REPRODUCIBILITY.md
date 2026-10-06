# Reproducibility

## Recorded with every run (`result.json`)

| field | source |
|---|---|
| `run.meta.build_rev` | `git rev-parse HEAD` |
| `run.meta.tracked_changes`, `run.meta.untracked_files` | modified tracked files and untracked files; both must be 0 for an official result, because untracked sources can reach the image build |
| `run.meta.inputs_sha256(Dockerfile,compose.yaml,Cargo.lock)` | the digest of the image and stack inputs, comparable across machines (the local `server_image` id is not) |
| `run.meta.server_toolchain`, `run.meta.docker`, `run.meta.compose` | the Dockerfile base line, and the Docker and Compose versions |
| `run.meta.acceptance_mode` | `unvalidated-development` while `compose.yaml` sets the development unvalidated-acceptance switch; accept timings are only comparable within one mode |
| `run.meta.rustc` | `rustc --version` |
| `run.meta.server_image` | id of the image the compose stack ran |
| `run.profile`, `run.harness_version`, start, finish, wall time | the harness |
| `environment` | OS, kernel, CPU model and count, RAM, PostgreSQL version, server URL |
| per dataset | id; generator and version; seed; parameters; manifest path; committed and computed checksums; licence |
| `resources.annotations.phases` | per-phase durations of the script |

A result without these fields, from a dirty tree (`tracked_changes > 0` or
`untracked_files > 0`), or with `checksum_ok = false` is not an official benchmark
result.

## Datasets
- **Generated datasets** are regenerated on every run from the manifest's seed and
  parameters. The full workload, including every expectation, is hashed into
  `output_checksum`, and must equal the committed manifest before the stack starts.
  Generation uses one SplitMix64 stream, ordered collections only, integer arithmetic, and
  no clocks or host state, so it is platform-independent.
- **Extracted datasets** (from M3 onwards) record source URL, version, licence, the pinned
  source SHA-256 and size, the extraction version and parameters, and the output checksum.
  They are cached outside the run; see [DATASETS.md](DATASETS.md).

## Reproducing a run
1. Check out `run.meta.build_rev`.
2. Run `./scripts/benchmark.sh <profile>` on a comparable host. The correctness results must
   be identical: same assertions, same counts, zero failures.
3. Compare timings only with runs of the same profile on the same host class, using the
   JSON series, not the rendered report.
