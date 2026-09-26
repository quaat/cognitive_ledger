-- Shared immutable content for the ledger (ADR-0012). Commits and patches are
-- content-addressed objects: id = 'sha256:' || lowercase hex digest of `bytes`.
-- Rows are write-once; the application never UPDATEs or DELETEs them. A single
-- INSERT ... ON CONFLICT DO NOTHING is the whole publication step, so a partially
-- written object is never observable and repeated identical writes are idempotent.
--
-- Commits are stored as plain objects carrying their own versioned header
-- (`sculpin-commit-v1\0` or `sculpin-cognitive-commit-v2\0`, ADR-0009 dual read), so
-- there is no `kind` column here; the derived, verified index lives in 0003.
-- Migration 0001 (refs) is released and is never rewritten.
CREATE TABLE IF NOT EXISTS immutable_objects (
    id         TEXT        PRIMARY KEY,
    bytes      BYTEA       NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT immutable_objects_id_format CHECK (id ~ '^sha256:[0-9a-f]{64}$')
);
