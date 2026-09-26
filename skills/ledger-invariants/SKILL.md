---
name: ledger-invariants
description: Use when modifying commits, hashes, parents, refs, canonicalization, or persistence.
---

# Review ledger invariants

Read the hard invariants and relevant ADRs. Map each change to affected invariants. Verify IDs from exact bytes, parents/targets exist before reachability, writes are immutable/idempotent, refs use CAS, reconstruction is deterministic, and history is not rewritten. Add negative and concurrency tests. Report each invariant as preserved, violated, or not applicable; protocol changes require an ADR and explicit golden-vector review.
