# Backup/restore run 2026-09-27 — PASS

Produced by `scripts/backup-restore.sh 50 10` on branch `claude/p1.5-production-qualification`, build `f357cdf-dirty` (run directory `target/backup/20260927T030639Z/`, not committed). Fifty writers kept prepare/accept traffic running over ten graphs (the `ledger-stress fault` load generator with no kills, so its own gate — invariants, landings matched to `ref_events`, verifier — applied to the live database). A logical backup (`pg_dump -Fc`) and a physical base backup (`pg_basebackup -c fast -X stream` over the local socket; the development `pg_hba.conf` has no network replication entry) were taken while the load continued, each after recording how many commits the clients had already been acknowledged. The dump was restored on the same cluster by the documented path — `pg_restore --no-owner --no-acl` as the owner, `GRANT CONNECT` to the runtime role, `ledger-admin migrate --runtime-role` re-deriving the grants from migration 0008 — and the base backup was started as a second PostgreSQL instance. For each restore: `ledger-admin verify` printed `VERIFY OK`; the graph set equals the live one; the restored ref events are at least the commits acknowledged before the backup; every graph's ref-event chain (version, head) is an exact contiguous prefix of the live chain; every restored decision, outbox, idempotency and proposal row exists identically in the live database; no ledger function is executable by PUBLIC after the grant step; a server started against the restored database (runtime identity, verify-only start-up) served the restored heads with matching versions and reconstructed states identical to the live server for the same commit ids.

```
live load established: 177 commits
backups taken under load: dump at 275 commits, base backup at 630, load now at 876
restored: logical dump -> database restored_dump; base backup -> instance ledger-qual-backup-bb
dump: 11 graphs (same set as live), 10 refs, 249 ref events (>= 212 acknowledged before the backup), each chain an exact prefix of the live chain (live has 917 events); 2609 decision/outbox/idempotency/proposal rows all present identically in live
dump: 10 restored heads served with versions matching, states identical to the live server (10 graphs, digest sha256:9de638037b095605)
basebackup: 11 graphs (same set as live), 10 refs, 361 ref events (>= 275 acknowledged before the backup), each chain an exact prefix of the live chain (live has 917 events); 3693 decision/outbox/idempotency/proposal rows all present identically in live
basebackup: 10 restored heads served with versions matching, states identical to the live server (10 graphs, digest sha256:e879e3caec9b27d1)
BACKUP RESTORE OK
```

Restore forks history from the snapshot onward (versions are reissued for different commits); the runbook states the consequences and the open PITR/fencing decisions.
