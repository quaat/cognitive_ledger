# Immutable objects and mutable ref metadata

## Status
Accepted

## Context
Objects and refs have different consistency and scaling requirements.

## Decision
Use an `ObjectStore` boundary for immutable bytes and `RefStore` for CAS metadata. Bootstrap uses filesystem objects and an atomically replaced, lock-serialized ref; production direction is PostgreSQL refs.

## Alternatives considered
Putting object bodies and refs into one mutable schema couples lifecycles. S3 is premature. Memory-only refs cannot prove restart persistence.

## Consequences
The filesystem slice is durable on one host, but cross-host correctness waits for PostgreSQL integration.
