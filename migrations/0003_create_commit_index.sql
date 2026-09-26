-- Derived, verified commit index over immutable_objects (ADR-0012, P1.2 foundation).
--
-- Only PostgresImmutableStore::put_commit writes these rows, in the same transaction as
-- the object bytes, from values decoded out of those bytes after checking
-- id = sha256(bytes). Parents must already be indexed commits (typed existence) and the
-- patch must already be an immutable object. Rows are write-once. The index can be
-- re-derived from immutable_objects at any time; P1.3 uses it for transactional graph
-- and lineage predicates (refs composite FK onto (graph_id, id); parents[0] join).
--
-- graph_id: v2 commits carry it in their bytes; v1 commits have none and are indexed
-- under the binding the store was configured with, or rejected (ADR-0010 policy).
CREATE TABLE IF NOT EXISTS commit_index (
    id           TEXT        PRIMARY KEY REFERENCES immutable_objects(id),
    graph_id     TEXT        NOT NULL,
    version      SMALLINT    NOT NULL,
    patch_id     TEXT        NOT NULL REFERENCES immutable_objects(id),
    parent_count SMALLINT    NOT NULL,
    indexed_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT commit_index_version_known CHECK (version IN (1, 2)),
    CONSTRAINT commit_index_parent_count CHECK (parent_count BETWEEN 0 AND 2),
    CONSTRAINT commit_index_graph_id_format CHECK (graph_id ~ '^[A-Za-z0-9._:-]{1,128}$'),
    CONSTRAINT commit_index_graph_commit UNIQUE (graph_id, id)
);

CREATE TABLE IF NOT EXISTS commit_parents (
    commit_id TEXT     NOT NULL REFERENCES commit_index(id),
    position  SMALLINT NOT NULL,
    parent_id TEXT     NOT NULL REFERENCES commit_index(id),
    PRIMARY KEY (commit_id, position),
    CONSTRAINT commit_parents_position CHECK (position IN (0, 1)),
    CONSTRAINT commit_parents_distinct UNIQUE (commit_id, parent_id)
);

CREATE INDEX IF NOT EXISTS commit_parents_by_parent ON commit_parents (parent_id);
