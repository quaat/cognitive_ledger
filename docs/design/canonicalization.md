# Canonicalization protocol v1

Patch bytes begin `sculpin-rdf-patch-v1\n`. Each normalized operation is one line: `A ` or `D ` followed by canonical N-Quads and LF. The maintained `oxttl` standards parser validates each input and `oxrdf` serialization produces the stored lexical form. Operations sort by the complete encoded line; duplicates collapse; contradictory add/delete operations fail. Blank nodes in subject, object, or graph position fail before a `Quad` exists. `Quad` deserialization reuses this constructor, and `Patch` deserialization re-runs normalization.

Commit bytes begin `sculpin-commit-v1\0`. They contain a big-endian `u32` ordered-parent count (zero through two), each parent as a length-prefixed UTF-8 field, then fixed-order length-prefixed fields: patch ID, actor, message, event time, and ledger-controlled recorded time. Parent zero is the reconstruction parent; parent one is merge ancestry. Parents must be distinct. No locale, map ordering, or client wall clock enters canonicalization.

IDs are `sha256:` plus lowercase digest hex. Fixtures under `fixtures/golden/` pin logical input, inspectable canonical bytes, and expected IDs. Tests never rewrite them. ADR-0006 records the one pre-release v1 vector change from optional-parent encoding to ordered parents. After release, any changed vector requires a new protocol version, ADR, and migration/compatibility analysis.

# Canonicalization protocol v2 — `sculpin-cognitive-commit/v2` (ADR-0009)

v2 is the production provenance envelope. v1 stays readable forever; `AnyCommit` dispatches
on the header and unknown versions fail closed (`UnknownCommitVersion`). Bytes begin
`sculpin-cognitive-commit-v2\0`. Integers are big-endian; `field` is a `u32` byte length plus
UTF-8 bytes; `opt` is one tag byte, `0x00` for absent or `0x01` followed by a **non-empty**
`field` — present-but-empty is an invalid encoding, so absent and empty can never collide.

```
field  graph_id            ASCII [A-Za-z0-9._:-]{1,128}; ledger-owned (ADR-0010)
u32    parent_count        0..=2, then `field parent` × count; ordered; distinct
field  patch_id            effective patch (ADR-0008)
field  principal_id        from the authenticated principal only (ADR-0011)
u8     principal_type      0 human, 1 agent, 2 service
opt    on_behalf_of
field  activity            bounded token
opt    event_time          canonical timestamp (below); optional per policy
field  recorded_at         canonical timestamp; server-assigned
u32    evidence_count      <= 64, then `field evidence_ref` × count,
                           strictly ascending bytewise (sorted + unique — a set)
opt    source_system       caller-declared; informational, never trusted provenance
field  message             <= 4096 bytes; may be empty (it is not optional)
```

Identity-bearing tokens (`principal_id`, `on_behalf_of`, `activity`, each evidence
reference, `source_system`) are non-empty, at most 512 bytes, and contain no Unicode
control characters (category `Cc`). `correlation_id` is not in the envelope. The decoder is
strict: it re-checks every rule the encoder applies (sortedness, caps, canonical
timestamps, known type byte, no trailing bytes), so any byte string that is not the
canonical form of some logical commit is rejected rather than reinterpreted.

Identity-bearing tokens are **opaque byte strings**: no Unicode normalization (NFC and NFD
spellings are different tokens), no IRI grammar check, no trimming. Evidence references are
required to be durable by policy, not by syntax; IRI validation of RDF terms belongs to the
patch layer and request limits, never to envelope identity, so no IRI grammar is frozen
into `CommitId`.

Timestamps (`LedgerTimestamp`, ADR-0011) are parsed as ASCII RFC 3339 with an uppercase
`T` and, if present, uppercase `Z`; a numeric offset must have hours 00–23 and minutes
00–59 and is converted to UTC; 1–9 fractional digits are accepted and truncated to
microseconds **at construction**, so the in-memory value, its serialization, and its
identity always agree; leap seconds (`:60`) are rejected; the UTC year after conversion
must lie in 0001–9999 (inputs that convert outside that range are rejected, never
wrapped or panicked on). The only serialization is `YYYY-MM-DDTHH:MM:SS.ffffffZ`
(27 bytes). Inside canonical bytes a timestamp must already be in that form.

Vectors live at `fixtures/golden/commits/v2-*.{input,hex,sha256}`: the `.input` is the
logical commit as JSON, `.hex` the canonical bytes, `.sha256` the `CommitId`. They pin a
genesis commit, a linear commit with all optionals present, the same logical commit with
evidence supplied in another order plus a duplicate (identical bytes and id), a two-parent
merge with `event_time` absent, and offset/precision normalization of both timestamps.
`v2-invalid-*.hex` fixtures are bytes the decoder MUST reject (unsorted or duplicate
evidence, more than 64 evidence refs, present-but-empty `on_behalf_of`/`event_time`/
`source_system`, non-canonical `recorded_at`/`event_time`, unknown `principal_type` byte,
duplicate or three parents, trailing bytes, unknown version header). The vectors were produced by
an independent reference encoder, `scripts/golden/commit_v2_reference.py` (`check` mode
recomputes them), and are verified by `crates/ledger-core/tests/golden_v2.rs`; neither
side ever rewrites a committed vector.

A header-only patch (`sculpin-rdf-patch-v1\n`, zero operations) is a valid canonical
patch *object*; the workflow write path never commits one, because an empty effective
delta is `NO_EFFECTIVE_CHANGE` (ADR-0008). Stores require a commit's patch to hash to its
id and decode canonically; bytes that hash correctly but are not canonical are
`INVALID_PATCH`, bytes that do not hash are corruption.

Skolemization is deliberately not implemented. A future general RDF ingress may accept blank-node input only after a deterministic Sculpin-controlled skolemization protocol and hostile-input limits are accepted.
