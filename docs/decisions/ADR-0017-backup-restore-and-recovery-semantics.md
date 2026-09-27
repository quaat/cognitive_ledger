# ADR-0017: Backup, restore and recovery semantics

- Status: Accepted (Plan 0005, 2026-09-27)
- Deciders: Cognitive Ledger maintainers
- Related: ADR-0004/0007 (PostgreSQL ref head), ADR-0012 (shared immutable store), ADR-0013
  (atomic acceptance), ADR-0016 (database identities), Plan 0005 §8

## Context

The ledger's whole durable state is one PostgreSQL database: immutable objects, commit index,
refs, ref events, proposals, decisions, projection outbox and idempotency records (ADR-0012
removed node-local content). A restore therefore restores everything or nothing; there is no
partial recovery of one table. The qualification runs (`scripts/backup-restore.sh`) show that a
logical dump (`pg_dump -Fc`) and a physical base backup both restore to an exact prefix of the
live history and serve identical states, and that the least-privilege model must be re-derived
after a restore rather than trusted from dump ACLs. What they also show is what a restore does
*not* preserve: every acceptance acknowledged after the snapshot is gone, and because ref
versions are dense integers per ref, a restored ledger reissues the same
`(graph, branch, version)` for different commits unless the operator intervenes.

## Decision

1. **Recovery point.** A production deployment MUST run PostgreSQL with continuous WAL
   archiving (or streaming replication to a standby) so that point-in-time recovery to the
   last committed transaction is possible; a base backup without WAL archive has the backup
   time as its recovery point and is acceptable only for development. Logical dumps are an
   additional, portable safety net, not the primary recovery mechanism.
2. **Writer fencing.** Before any restore the operator MUST stop every ledger replica and any
   Phase-3 projection consumer (no writer may run against the old and the new database at
   once), and MUST NOT start replicas until `ledger-admin verify` on the restored database
   prints `VERIFY OK` and `ledger-admin migrate --runtime-role <role>` has re-derived the
   runtime grants (ADR-0016; a restore without ACLs is the documented path).
3. **Declared restore point.** The operator MUST record, before serving again, the restore
   point per ref (`graph_id, branch, version, head` of the restored heads, from the
   verifier's report or `SELECT graph_id, branch, version, head FROM refs`) together with the
   backup identity and the reason; this record is the authoritative statement of which
   acknowledged commits were lost.
4. **Projection reconciliation.** Every projection consumer MUST be rebuilt or rolled back to
   the declared restore point before it resumes; the outbox after a restore only contains
   events up to that point, so a consumer that had projected later commits is ahead of the
   ledger and must not be trusted. Phase 3 designs consumers to support "reset to (graph,
   branch, version)".
5. **Version and history expectations.** After a restore the ledger continues from the
   restored heads: new accepts receive versions the lost commits once held, with different
   heads. Clients holding a lost head receive `HEAD_CHANGED`; clients retrying a lost request
   with its `Idempotency-Key` get a fresh execution against the restored base (no record of
   the lost one exists). Commit identity is content-addressed and unaffected; only ref
   history is rewritten to the restore point. The ledger does not introduce an epoch or
   generation number in Plan 0005; if a deployment needs clients to detect a restore, it
   publishes the declared restore point out of band, and a later phase may add a ref
   generation to the API as a compatible extension.
6. **Verification.** `ledger-admin verify` is the post-restore gate (inspect, never repair).
   The restore smoke (`scripts/backup-restore.sh`) is run per release and must prove: verifier
   clean, graph set and pre-backup watermark, ref chains exact prefixes, audit rows present in
   live, identical reconstructed states, no PUBLIC execute on ledger functions, and refusal
   of a restore that lost a guard trigger, the content-address CHECK, a column grant or a
   sequence grant.

## Consequences

- Production qualification requires evidence of WAL archiving/PITR configuration in the
  deployment, not only of the dump/base-backup smoke (Plan 0005 records this as an
  operator-side blocker until a deployment provides it).
- The runbook (`docs/operations/deployment.md`) carries the fencing, restore-point and
  reconciliation steps; the release gate includes the restore smoke.
- No protocol, golden vector or migration changes; no backup platform is implemented.
