-- Graph authority (ADR-0010). graph_id is GLOBALLY unique (the PK is graph_id alone, not
-- (tenant_id, graph_id)): a commit embeds only graph_id, so this is what makes
-- commit -> graph -> tenant resolution unambiguous. The tenant binding is immutable
-- (trigger below); re-homing a graph is a new graph plus an audited import. Many graphs
-- may reference one knowledge_base_id (plain index, no uniqueness).
--
-- Upgrade policy: the only pre-v2 deployed topology is the bootstrap graph 'default'
-- (branch 'main'). It receives an explicit row with tenant 'bootstrap' and status
-- 'bootstrap' — NON-PRODUCTION legacy state, not a real tenant. Any other graph id already
-- present in refs or commit_index has no derivable owner, so this migration FAILS with an
-- actionable error rather than guessing a tenant; such data goes through the audited
-- graph import path first. Migrations 0001–0003 are released and are never rewritten.
CREATE TABLE IF NOT EXISTS graphs (
    graph_id          TEXT        PRIMARY KEY,
    tenant_id         TEXT        NOT NULL,
    knowledge_base_id TEXT        NULL,
    purpose           TEXT        NULL,
    status            TEXT        NOT NULL,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT graphs_graph_id_format CHECK (graph_id ~ '^[A-Za-z0-9._:-]{1,128}$'),
    CONSTRAINT graphs_tenant_id_bounds CHECK (octet_length(tenant_id) BETWEEN 1 AND 512),
    CONSTRAINT graphs_kb_bounds CHECK (knowledge_base_id IS NULL OR octet_length(knowledge_base_id) BETWEEN 1 AND 512),
    CONSTRAINT graphs_purpose_bounds CHECK (purpose IS NULL OR octet_length(purpose) <= 512),
    CONSTRAINT graphs_status_known CHECK (status IN ('bootstrap', 'active', 'importing', 'archived'))
);
CREATE INDEX IF NOT EXISTS graphs_by_knowledge_base ON graphs (knowledge_base_id);
CREATE INDEX IF NOT EXISTS graphs_by_tenant ON graphs (tenant_id);

CREATE OR REPLACE FUNCTION graphs_identity_is_immutable() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.graph_id IS DISTINCT FROM OLD.graph_id THEN
        RAISE EXCEPTION 'graphs.graph_id is immutable (graph %)', OLD.graph_id
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    IF NEW.tenant_id IS DISTINCT FROM OLD.tenant_id THEN
        RAISE EXCEPTION 'graphs.tenant_id is immutable (graph %): re-home via a new graph and an audited import (ADR-0010)', OLD.graph_id
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    RETURN NEW;
END
$$;
DROP TRIGGER IF EXISTS graphs_identity_immutable ON graphs;
CREATE TRIGGER graphs_identity_immutable
    BEFORE UPDATE ON graphs
    FOR EACH ROW EXECUTE FUNCTION graphs_identity_is_immutable();

-- Upgrade guard: refuse to invent owners for unknown graphs.
DO $$
DECLARE
    unknown TEXT;
BEGIN
    SELECT string_agg(DISTINCT g, ', ' ORDER BY g) INTO unknown FROM (
        SELECT graph_id AS g FROM refs WHERE graph_id <> 'default'
        UNION
        SELECT graph_id AS g FROM commit_index WHERE graph_id <> 'default'
    ) AS s;
    IF unknown IS NOT NULL THEN
        RAISE EXCEPTION 'migration 0004 (ADR-0010): refs/commit_index reference graphs with no derivable tenant owner: %. Only the bootstrap graph ''default'' is upgraded automatically; register these graphs through the audited graph import path before upgrading.', unknown;
    END IF;
END
$$;

-- The bootstrap graph row exists on clean install and upgrade alike, because the pre-v2
-- write path (V1Binding::BindTo('default')) still targets it. It is retired by a later
-- migration when the v2 write path replaces it.
INSERT INTO graphs (graph_id, tenant_id, knowledge_base_id, purpose, status)
VALUES ('default', 'bootstrap', NULL,
        'Bootstrap single-graph topology for the pre-v2 write path. Non-production legacy state (ADR-0010).',
        'bootstrap')
ON CONFLICT (graph_id) DO NOTHING;

-- Referential integrity: refs and indexed commits belong to registered graphs; a graph
-- with refs or commits cannot be deleted (RESTRICT), so deletion can never orphan them.
ALTER TABLE refs
    ADD CONSTRAINT refs_graph_fk FOREIGN KEY (graph_id) REFERENCES graphs (graph_id)
    ON DELETE RESTRICT ON UPDATE RESTRICT;
ALTER TABLE commit_index
    ADD CONSTRAINT commit_index_graph_fk FOREIGN KEY (graph_id) REFERENCES graphs (graph_id)
    ON DELETE RESTRICT ON UPDATE RESTRICT;
