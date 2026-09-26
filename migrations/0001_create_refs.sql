-- Mutable ref coordination for the ledger.
--
-- Immutable objects (patches, commits) remain filesystem/object-store content; this
-- table holds only the mutable ref heads and is the horizontally safe compare-and-set
-- point (ADR-0004, ADR-0007). It is structurally ready for named graphs and branches,
-- but Milestone 0002 only exercises ('default', 'main').
--
-- Absence of a row means the ref has no head yet (empty ledger). `head` is therefore
-- NOT NULL: a present row always carries a real commit id. Compare-and-set is expressed
-- as single predicated statements so concurrent writers cannot lose an update:
--   * advance from an expected head:  UPDATE ... WHERE head = $expected  (row lock)
--   * first write (no expected head): INSERT ... ON CONFLICT DO NOTHING

CREATE TABLE IF NOT EXISTS refs (
    graph_id   TEXT        NOT NULL DEFAULT 'default',
    branch     TEXT        NOT NULL DEFAULT 'main',
    head       TEXT        NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (graph_id, branch)
);
