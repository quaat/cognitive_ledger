-- Development / integration harness ONLY (mounted into the compose PostgreSQL's
-- docker-entrypoint-initdb.d). Creates the least-privilege runtime identity the ledger
-- server connects with (ADR-0016). The owner identity is the container's POSTGRES_USER.
-- A production deployment creates this role with a managed secret; the grants are applied
-- by `ledger-admin migrate --runtime-role ledger_runtime` (migration 0008), never here.
CREATE ROLE ledger_runtime LOGIN PASSWORD 'ledger-runtime-development-only';
