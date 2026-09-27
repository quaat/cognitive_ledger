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

Toolchain and target: `scripts/fuzz.sh` uses the dated nightly `FUZZ_TOOLCHAIN`
(default `nightly-2026-09-25`, installed with `rustup toolchain install <name> --profile minimal
--component rust-src`) and passes `--target` explicitly, derived from that toolchain's host
triple (`FUZZ_TARGET` overrides); the prebuilt cargo-fuzz binary must never pick the triple
from its own build platform. Sanitizer: `FUZZ_SANITIZER=none|address` (`ci-fuzz` runs both as a
matrix; the AddressSanitizer runtime crashed at start-up on the local qualification host, which
is why the hosted runner is the reference for ASan). The workspace forbids `unsafe`, so
memory-safety findings could only come from dependencies. Debug assertions are on (`-a`).

Corpora under `corpus/<target>/` are seeded from the frozen golden vectors (`fixtures/golden`,
valid and invalid commits) and small hand-written cases; libFuzzer adds what it finds.
Crashing inputs land in `artifacts/<target>/`: commit the minimized input as a regression
seed together with the fix, never delete it to make the run green.
