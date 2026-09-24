# Content-addressed immutable commits

## Status
Accepted

## Context
Audit history needs stable identity, reproducibility, and safe concurrency.

## Decision
Hash versioned canonical patch and commit bytes with SHA-256. Store immutable objects before CAS-moving `main`. Protocol v1 permits zero, one, or two ordered parents and rejects any missing parent; ADR-0006 finalizes parent ordering before release.

## Alternatives considered
Database-generated IDs lose content identity. Mutable commit rows weaken auditability. Snapshot-per-commit is unnecessary initially.

## Consequences
Protocol bytes become permanent compatibility surface. Failed CAS may leave safe unreachable objects.
