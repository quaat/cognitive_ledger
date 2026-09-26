# Finalize unreleased v1 parent and recording-time semantics

## Status
Accepted

## Context
Bootstrap encoded one optional parent and accepted `recorded_time` from callers. The target model requires two-parent merge commits, ledger-controlled recording time, and richer provenance. No v1 data has been released, so changing golden identity now is safer than knowingly requiring a parent-format migration immediately after release. Rich provenance semantics are not yet sufficiently specified to freeze into this envelope.

## Decision
Protocol v1 encodes an ordered parent count followed by zero, one, or two parent IDs. Parent zero defines reconstruction; parent one is additional merge ancestry. Parents are distinct and order is identity-bearing. The application service generates RFC 3339 `recorded_time` when constructing the immutable envelope; HTTP clients cannot supply it. This is distinct from the eventual server-generated ref-event time that records successful CAS acceptance.

V1 retains its minimal actor, message, event-time, and recorded-time fields. Rich identity-bearing provenance, semantic context, validation references, graph identity, or activity classification will use a new versioned envelope (expected v2), with dual-read/migration design and new golden vectors. They will not be silently appended to v1.

The genesis v1 vector remains byte-identical because its former empty-parent field and new zero-parent count have the same four zero bytes. New linear and merge vectors pin one- and two-parent encoding and ordering. There is no persisted-release migration because no released history exists.

## Alternatives considered
Keeping `Option<CommitId>` would force v2 solely to represent merge commits. Adding speculative provenance fields now would freeze semantics before Sculpin integration is designed. Treating recording time as caller metadata would undermine audit chronology.

## Consequences
The core can represent future merge ancestry without implementing merge now. Reconstruction has an explicit first-parent rule. Tests and fixtures must pin parent order and server-controlled time. Rich provenance still requires deliberate protocol work rather than ad hoc metadata.
