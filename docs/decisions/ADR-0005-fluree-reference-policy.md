# Fluree differential-reference policy

## Status
Accepted

## Context
Fluree behavior can inform compatibility testing but its breadth and license do not belong in the product runtime.

## Decision
Use the official digest-pinned `fluree/server` image only in an optional differential test profile. Compare semantic RDF states, not IDs or performance gates. Never copy or link Fluree source.

## Alternatives considered
Embedding or vendoring Fluree violates product and dependency boundaries. Omitting comparison loses useful independent evidence.

## Consequences
Docker CI bears optional reference cost. Its license is documented as test-only and no shipped artifact depends on it.
