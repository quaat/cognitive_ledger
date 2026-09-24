# RDF patch canonicalization

## Status
Accepted

## Context
Equivalent change input must produce identical bytes while RDF blank nodes make naïve identity unstable.

## Decision
Use a small versioned canonical N-Quads patch subset, deterministic operation sorting/deduplication, and reject blank nodes. Use a fixed length-prefixed commit envelope.

## Alternatives considered
Raw input hashing is nondeterministic. Full RDFC-1.0 is broader and more expensive than this milestone. JCS was considered but fixed binary fields reduce library and number-format ambiguity.

## Consequences
The subset is intentionally limited. Expanding syntax or skolemization requires protocol design, vectors, and an ADR.
