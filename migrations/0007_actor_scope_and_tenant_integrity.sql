-- P1.4 schema seams (ADR-0011, ADR-0013). Additive; 0001–0006 untouched.
--
-- 1. Idempotency is scoped by the COMPLETE authenticated actor: principal_id,
--    principal_type and on_behalf_of (NULL = canonical absence, compared with
--    NULLS NOT DISTINCT). Existing rows are bound to the actor of the request they
--    recorded: a 'prepared' row to its proposal's actor (the proposer), an 'accepted' or
--    'rejected' row to its decision's actor (the reviewer, who may differ from the
--    proposer). Any row that cannot be bound unambiguously fails the migration (never
--    guessed). Requires PostgreSQL 15+ (UNIQUE NULLS NOT DISTINCT).
--    Operational note: run with every P1.3 replica stopped; a pre-0007 binary would
--    insert rows without principal_type and would not scope lookups by the full actor.
-- 2. Bounded optional correlation_id on proposals, ref_events, decisions (tracing
--    metadata, never identity).
-- 3. Tenant integrity: graphs UNIQUE (graph_id, tenant_id) and composite FKs from the
--    tenant-bearing workflow/audit tables, so PostgreSQL proves row.graph_id belongs to
--    row.tenant_id. Defence in depth; application authorization stays in place.

-- ---- 1. complete-actor idempotency ---------------------------------------------------
ALTER TABLE idempotency ADD COLUMN principal_type TEXT NULL;
ALTER TABLE idempotency ADD COLUMN on_behalf_of   TEXT NULL;

DO $$
DECLARE
    unbound BIGINT;
    mismatched BIGINT;
BEGIN
    -- A prepared row is bound through its proposal; an accepted/rejected row through its
    -- decision (P1.3 records both ids on accept/reject, but the proposal names the
    -- proposer, not the reviewer who made the request).
    SELECT count(*) INTO unbound FROM idempotency i
     WHERE (i.result_kind = 'prepared'
            AND (i.result_proposal_id IS NULL
                 OR NOT EXISTS (SELECT 1 FROM proposals p WHERE p.proposal_id = i.result_proposal_id)))
        OR (i.result_kind IN ('accepted', 'rejected')
            AND (i.result_decision_id IS NULL
                 OR NOT EXISTS (SELECT 1 FROM decisions d WHERE d.decision_id = i.result_decision_id)));
    IF unbound > 0 THEN
        RAISE EXCEPTION 'migration 0007: % idempotency row(s) cannot be bound to an actor (no proposal/decision); resolve them before upgrading', unbound;
    END IF;
    SELECT count(*) INTO mismatched FROM (
        SELECT 1 FROM idempotency i JOIN proposals p ON p.proposal_id = i.result_proposal_id
         WHERE i.result_kind = 'prepared'
           AND (p.principal_id <> i.principal_id OR p.tenant_id <> i.tenant_id OR p.graph_id <> i.graph_id)
        UNION ALL
        SELECT 1 FROM idempotency i JOIN decisions d ON d.decision_id = i.result_decision_id
         WHERE i.result_kind IN ('accepted', 'rejected')
           AND (d.principal_id <> i.principal_id OR d.tenant_id <> i.tenant_id OR d.graph_id <> i.graph_id)
    ) AS m;
    IF mismatched > 0 THEN
        RAISE EXCEPTION 'migration 0007: % idempotency row(s) disagree with their proposal/decision actor, tenant or graph; refusing to guess', mismatched;
    END IF;
END
$$;

-- The backfill is the one sanctioned rewrite of these rows: it adds scope columns
-- derived from each row's own request record without changing any result. The write-once
-- guard is suspended for exactly these statements inside the migration transaction (a
-- failure rolls back the DISABLE as well).
ALTER TABLE idempotency DISABLE TRIGGER idempotency_write_once;
UPDATE idempotency i
   SET principal_type = p.principal_type,
       on_behalf_of   = p.on_behalf_of
  FROM proposals p
 WHERE i.result_kind = 'prepared' AND p.proposal_id = i.result_proposal_id;
UPDATE idempotency i
   SET principal_type = d.principal_type,
       on_behalf_of   = d.on_behalf_of
  FROM decisions d
 WHERE i.result_kind IN ('accepted', 'rejected') AND d.decision_id = i.result_decision_id;
ALTER TABLE idempotency ENABLE TRIGGER idempotency_write_once;

ALTER TABLE idempotency ALTER COLUMN principal_type SET NOT NULL;
ALTER TABLE idempotency ADD CONSTRAINT idempotency_principal_type
    CHECK (principal_type IN ('human', 'agent', 'service'));
ALTER TABLE idempotency DROP CONSTRAINT idempotency_pkey;
ALTER TABLE idempotency ADD COLUMN idempotency_id BIGSERIAL PRIMARY KEY;
-- Equality-searched columns lead so the lookup (which compares on_behalf_of with
-- IS NOT DISTINCT FROM, not indexable) is bounded by the key, not by the actor's history.
ALTER TABLE idempotency ADD CONSTRAINT idempotency_scope_unique
    UNIQUE NULLS NOT DISTINCT (tenant_id, graph_id, operation, idempotency_key, principal_id, principal_type, on_behalf_of);

-- ---- 2. correlation metadata ----------------------------------------------------------
ALTER TABLE proposals  ADD COLUMN correlation_id TEXT NULL;
ALTER TABLE ref_events ADD COLUMN correlation_id TEXT NULL;
ALTER TABLE decisions  ADD COLUMN correlation_id TEXT NULL;
ALTER TABLE proposals  ADD CONSTRAINT proposals_correlation_bounds  CHECK (correlation_id IS NULL OR octet_length(correlation_id) BETWEEN 1 AND 128);
ALTER TABLE ref_events ADD CONSTRAINT ref_events_correlation_bounds CHECK (correlation_id IS NULL OR octet_length(correlation_id) BETWEEN 1 AND 128);
ALTER TABLE decisions  ADD CONSTRAINT decisions_correlation_bounds  CHECK (correlation_id IS NULL OR octet_length(correlation_id) BETWEEN 1 AND 128);

-- ---- 3. tenant integrity --------------------------------------------------------------
ALTER TABLE graphs ADD CONSTRAINT graphs_graph_tenant UNIQUE (graph_id, tenant_id);

DO $$
DECLARE
    bad TEXT;
BEGIN
    SELECT string_agg(format('%s(%s→%s)', t, g, ten), ', ') INTO bad FROM (
        SELECT 'proposals' AS t, p.graph_id AS g, p.tenant_id AS ten FROM proposals p
          LEFT JOIN graphs gr ON gr.graph_id = p.graph_id AND gr.tenant_id = p.tenant_id WHERE gr.graph_id IS NULL
        UNION ALL
        SELECT 'ref_events', e.graph_id, e.tenant_id FROM ref_events e
          LEFT JOIN graphs gr ON gr.graph_id = e.graph_id AND gr.tenant_id = e.tenant_id WHERE gr.graph_id IS NULL
        UNION ALL
        SELECT 'decisions', d.graph_id, d.tenant_id FROM decisions d
          LEFT JOIN graphs gr ON gr.graph_id = d.graph_id AND gr.tenant_id = d.tenant_id WHERE gr.graph_id IS NULL
        UNION ALL
        SELECT 'idempotency', i.graph_id, i.tenant_id FROM idempotency i
          LEFT JOIN graphs gr ON gr.graph_id = i.graph_id AND gr.tenant_id = i.tenant_id WHERE gr.graph_id IS NULL
    ) AS s;
    IF bad IS NOT NULL THEN
        RAISE EXCEPTION 'migration 0007: audit rows whose graph does not belong to their tenant: %; refusing to upgrade', bad;
    END IF;
END
$$;

ALTER TABLE proposals   ADD CONSTRAINT proposals_graph_tenant_fk   FOREIGN KEY (graph_id, tenant_id) REFERENCES graphs (graph_id, tenant_id);
ALTER TABLE ref_events  ADD CONSTRAINT ref_events_graph_tenant_fk  FOREIGN KEY (graph_id, tenant_id) REFERENCES graphs (graph_id, tenant_id);
ALTER TABLE decisions   ADD CONSTRAINT decisions_graph_tenant_fk   FOREIGN KEY (graph_id, tenant_id) REFERENCES graphs (graph_id, tenant_id);
ALTER TABLE idempotency ADD CONSTRAINT idempotency_graph_tenant_fk FOREIGN KEY (graph_id, tenant_id) REFERENCES graphs (graph_id, tenant_id);
