# Fuzz targets (Plan 0005 §5)

libFuzzer targets for every parser and canonical encoder that consumes untrusted bytes, run
with `scripts/fuzz.sh [seconds] [target…]` (nightly + cargo-fuzz; CI runs `ci-fuzz` bounded
on every pull request). This directory is its own Cargo workspace (excluded from the root)
so sanitizer builds never touch the release build graph.

| target | entry point | property checked besides "never panics" |
|---|---|---|
| `quad_parse` | `ledger_rdf::Quad::from_str` (N-Quads, blank nodes refused) | canonical text is a fixed point of parse ∘ print |
| `patch_canonical` | `ledger_rdf::Patch::from_canonical_bytes` | crash-only in practice: the decoder itself refuses non-canonical bytes; the target re-asserts it |
| `commit_decode` | `ledger_core::AnyCommit::from_canonical_bytes` (v1 + v2) | accepted bytes re-encode identically; id stable |
| `prepare_body` | `serde_json` → `ledger_api::PrepareBody` → `canonical_prepare` | identity unchanged by JSON key order, whitespace, operation and evidence order |
| `accept_body` | `serde_json` → `ledger_api::AcceptBody` → `canonical_accept` | identity unchanged by JSON key order and whitespace |
| `request_identity` | structured (`arbitrary`) prepare bodies through `canonical_prepare` | operation order / duplicated evidence never change the identity |
| `timestamp` | `ledger_core::LedgerTimestamp::parse_rfc3339` | canonical form satisfies the strict canonical parser |

Sanitizer: `scripts/fuzz.sh` runs with `-s none` unless `FUZZ_SANITIZER=address` is set — the
AddressSanitizer runtime crashed at start-up on the qualification host and is not validated
on the CI runner yet (recorded as deferred in Plan 0005); the workspace forbids `unsafe`, so
memory-safety findings could only come from dependencies.

Corpora under `corpus/<target>/` are seeded from the frozen golden vectors (`fixtures/golden`,
valid and invalid commits) and small hand-written cases; libFuzzer adds what it finds.
Crashing inputs land in `artifacts/<target>/`: commit the minimized input as a regression
seed together with the fix, never delete it to make the run green.
