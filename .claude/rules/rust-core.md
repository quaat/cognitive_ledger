---
paths:
  - "crates/ledger-core/**"
  - "crates/ledger-rdf/**"
---
Core code has no infrastructure dependencies, hidden global state, or `unsafe`. Prefer deterministic pure functions and explicit error types. Every invariant change requires positive and negative tests; protocol changes also require ADR/golden review.
