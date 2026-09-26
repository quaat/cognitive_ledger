# Backup/restore run 2026-09-27 — PASS

Produced by `scripts/backup-restore.sh 50 10` on branch `claude/p1.5-production-qualification` (run directory `target/backup/20260926T221939Z/`, not committed). Fifty writers kept prepare/accept traffic running over ten graphs (the `ledger-stress fault` load generator with no kills, so its own gate — invariants, verifier — applied to the live database too). A logical backup (`pg_dump -Fc`) and a physical base backup (`pg_basebackup -c fast -X stream` over the local socket; the development `pg_hba.conf` has no network replication entry) were taken while the load continued. The dump was restored with `pg_restore --no-owner` into a new database on the same cluster; the base backup was started as a second PostgreSQL instance. For each restore: `ledger-admin verify` printed `VERIFY OK`; every graph's ref-event chain (version, head) is an exact, contiguous prefix of the live chain; a server started against the restored database (runtime identity, verify-only start-up) served the restored heads with matching versions and reconstructed states identical to the live server for the same commit ids.

```
dump: 10 refs, 184 ref events, each chain an exact prefix of the live chain (live has 899 events)
dump: 10 restored heads served with versions matching, states identical to the live server (10 graphs, digest sha256:e176ef77257acefe)
basebackup: 10 refs, 313 ref events, each chain an exact prefix of the live chain (live has 899 events)
basebackup: 10 restored heads served with versions matching, states identical to the live server (10 graphs, digest sha256:a48c1a1d6390c697)

live load established: 102 commits
backups taken under load: dump at 209 commits, base backup at 599, load now at 861
restored: logical dump -> database restored_dump; base backup -> instance ledger-qual-backup-bb
```

Load-generator report for the live database during the run (no faults injected):

Σ refs.version = 899 = Σ ref_events = 899 = Σ accepted decisions = 899 = Σ outbox rows = 899; client-observed landings (run + replays) = 899 with 899 distinct (graph, version) pairs, 899 found as ref events with the same version and head; 0 graph(s) violate. Verifier: clean (0 violating check(s)).

Verifier output (dump restore):

```
ok   idempotency scope is unique (NULL delegation is one value) (0 violation(s))
ok   no v1 commit is indexed under a production graph (0 violation(s))
ok   every candidate proposal commit is indexed under its graph (0 violation(s))
VERIFY OK
```
