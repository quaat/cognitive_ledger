-- Phase 4 named branches (ADR-0022; Plan 0008). Additive; 0001–0011 untouched.
--
-- branches        one row per ref (the branch): lifecycle status and version, origin and
--                 provenance (source branch and commit of a created branch), and the
--                 immutable policy (require_validation, require_distinct_reviewer; protection
--                 stays `refs.protected`). Pre-Phase-4 refs are adopted.
-- branch_events   append-only lifecycle history (genesis / created / adopted / deleted /
--                 restored), one row per lifecycle version. `ref_events` remains the only
--                 authority for head movement.
-- refs            `main` is always protected; the runtime may set `protected` on insert
--                 (branch creation); the head of a deleted branch never moves.
-- proposals       no proposal on a deleted branch, nor on an unknown non-`main` branch of an
--                 active graph.
-- idempotency     branch lifecycle operations and their results.
--
-- CHECKs are written with flat ANDs (no BETWEEN) so a logical restore re-creates them
-- byte-identically (ADR-0017 amendment).

-- ---- refs ------------------------------------------------------------------------------
ALTER TABLE refs ADD CONSTRAINT refs_main_protected CHECK (branch <> 'main' OR protected);

-- ---- branches --------------------------------------------------------------------------
CREATE TABLE branches (
    graph_id                  TEXT        NOT NULL,
    branch                    TEXT        NOT NULL,
    tenant_id                 TEXT        NOT NULL,
    status                    TEXT        NOT NULL,
    lifecycle_version         BIGINT      NOT NULL,
    origin                    TEXT        NOT NULL,
    source_branch             TEXT        NULL,
    source_commit             TEXT        NULL,
    require_validation        BOOLEAN     NOT NULL,
    require_distinct_reviewer BOOLEAN     NOT NULL,
    created_at                TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at                TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT branches_pk PRIMARY KEY (graph_id, branch),
    CONSTRAINT branches_ref_fk FOREIGN KEY (graph_id, branch) REFERENCES refs (graph_id, branch),
    CONSTRAINT branches_graph_tenant_fk FOREIGN KEY (graph_id, tenant_id)
        REFERENCES graphs (graph_id, tenant_id),
    CONSTRAINT branches_source_branch_fk FOREIGN KEY (graph_id, source_branch)
        REFERENCES branches (graph_id, branch),
    CONSTRAINT branches_source_commit_fk FOREIGN KEY (graph_id, source_commit)
        REFERENCES commit_index (graph_id, id),
    CONSTRAINT branches_status CHECK (status IN ('active', 'deleted')),
    CONSTRAINT branches_lifecycle_version_positive CHECK (lifecycle_version >= 1),
    CONSTRAINT branches_origin CHECK (origin IN ('genesis', 'created', 'adopted')),
    CONSTRAINT branches_origin_shape CHECK (((origin = 'created') = (source_branch IS NOT NULL))
        AND ((source_branch IS NULL) = (source_commit IS NULL))),
    CONSTRAINT branches_genesis_is_main CHECK (origin <> 'genesis' OR branch = 'main'),
    CONSTRAINT branches_main_shape CHECK (branch <> 'main' OR (origin <> 'created' AND status = 'active')),
    CONSTRAINT branches_name_bounds CHECK (octet_length(branch) >= 1 AND octet_length(branch) <= 128
        AND branch ~ '^[A-Za-z0-9._/-]+$')
);

-- ---- branch_events -----------------------------------------------------------------------
CREATE TABLE branch_events (
    event_id          BIGSERIAL   PRIMARY KEY,
    graph_id          TEXT        NOT NULL,
    branch            TEXT        NOT NULL,
    tenant_id         TEXT        NOT NULL,
    lifecycle_version BIGINT      NOT NULL,
    operation         TEXT        NOT NULL,
    status_after      TEXT        NOT NULL,
    head              TEXT        NOT NULL,
    ref_version       BIGINT      NOT NULL,
    source_branch     TEXT        NULL,
    source_commit     TEXT        NULL,
    principal_id      TEXT        NOT NULL,
    principal_type    TEXT        NOT NULL,
    on_behalf_of      TEXT        NULL,
    reason            TEXT        NULL,
    correlation_id    TEXT        NULL,
    recorded_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT branch_events_branch_fk FOREIGN KEY (graph_id, branch) REFERENCES branches (graph_id, branch),
    CONSTRAINT branch_events_graph_tenant_fk FOREIGN KEY (graph_id, tenant_id)
        REFERENCES graphs (graph_id, tenant_id),
    CONSTRAINT branch_events_head_fk FOREIGN KEY (graph_id, head) REFERENCES commit_index (graph_id, id),
    CONSTRAINT branch_events_source_commit_fk FOREIGN KEY (graph_id, source_commit)
        REFERENCES commit_index (graph_id, id),
    CONSTRAINT branch_events_version_unique UNIQUE (graph_id, branch, lifecycle_version),
    CONSTRAINT branch_events_identity UNIQUE (event_id, graph_id),
    CONSTRAINT branch_events_operation CHECK (operation IN ('genesis', 'created', 'adopted', 'deleted', 'restored')),
    CONSTRAINT branch_events_status_after CHECK (status_after IN ('active', 'deleted')),
    CONSTRAINT branch_events_shape CHECK (
        (operation IN ('genesis', 'created', 'adopted') AND lifecycle_version = 1 AND status_after = 'active')
        OR (operation = 'deleted' AND lifecycle_version > 1 AND status_after = 'deleted')
        OR (operation = 'restored' AND lifecycle_version > 1 AND status_after = 'active')),
    CONSTRAINT branch_events_source_shape CHECK (((operation = 'created') = (source_branch IS NOT NULL))
        AND ((source_branch IS NULL) = (source_commit IS NULL))),
    CONSTRAINT branch_events_ref_version_positive CHECK (ref_version >= 1),
    CONSTRAINT branch_events_principal_type CHECK (principal_type IN ('human', 'agent', 'service')),
    CONSTRAINT branch_events_reason_bounds CHECK (reason IS NULL OR (octet_length(reason) >= 1
        AND octet_length(reason) <= 1024)),
    CONSTRAINT branch_events_correlation_bounds CHECK (correlation_id IS NULL
        OR (octet_length(correlation_id) >= 1 AND octet_length(correlation_id) <= 128))
);
CREATE INDEX branch_events_by_branch ON branch_events (graph_id, branch, lifecycle_version);

-- ---- adoption of pre-Phase-4 refs --------------------------------------------------------
INSERT INTO branches (graph_id, branch, tenant_id, status, lifecycle_version, origin,
                      require_validation, require_distinct_reviewer)
SELECT r.graph_id, r.branch, g.tenant_id, 'active', 1, 'adopted', false, false
  FROM refs r JOIN graphs g ON g.graph_id = r.graph_id;
INSERT INTO branch_events (graph_id, branch, tenant_id, lifecycle_version, operation, status_after,
                           head, ref_version, principal_id, principal_type, reason)
SELECT r.graph_id, r.branch, g.tenant_id, 1, 'adopted', 'active', r.head, r.version,
       'urn:sculpin:ledger:migration:0012', 'service', 'pre-Phase-4 ref adopted by migration 0012'
  FROM refs r JOIN graphs g ON g.graph_id = r.graph_id;

-- ---- guards -------------------------------------------------------------------------------
DROP TRIGGER IF EXISTS branch_events_write_once ON branch_events;
CREATE TRIGGER branch_events_write_once BEFORE UPDATE OR DELETE ON branch_events
    FOR EACH ROW EXECUTE FUNCTION ledger_rows_are_write_once();

-- Identity, provenance and policy are immutable; the lifecycle version moves by exactly one
-- with every status change; rows are never deleted; only the owner (migration) adopts.
CREATE OR REPLACE FUNCTION public.branches_guard() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'branches rows are never deleted; delete the branch (tombstone) instead'
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    IF TG_OP = 'INSERT' THEN
        IF NEW.lifecycle_version <> 1 OR NEW.status <> 'active' THEN
            RAISE EXCEPTION 'a new branch starts active at lifecycle version 1'
                USING ERRCODE = 'integrity_constraint_violation';
        END IF;
        IF NEW.origin = 'adopted'
           AND current_user::text IS DISTINCT FROM (SELECT t.tableowner::text FROM pg_catalog.pg_tables t
                                               WHERE t.schemaname = 'public' AND t.tablename = 'branches') THEN
            RAISE EXCEPTION 'only the schema owner adopts refs as branches'
                USING ERRCODE = 'insufficient_privilege';
        END IF;
        RETURN NEW;
    END IF;
    IF NEW.graph_id <> OLD.graph_id OR NEW.branch <> OLD.branch OR NEW.tenant_id <> OLD.tenant_id
       OR NEW.origin <> OLD.origin OR NEW.source_branch IS DISTINCT FROM OLD.source_branch
       OR NEW.source_commit IS DISTINCT FROM OLD.source_commit
       OR NEW.require_validation <> OLD.require_validation
       OR NEW.require_distinct_reviewer <> OLD.require_distinct_reviewer
       OR NEW.created_at <> OLD.created_at THEN
        RAISE EXCEPTION 'branch identity, provenance and policy are immutable'
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    IF (NEW.status <> OLD.status) <> (NEW.lifecycle_version = OLD.lifecycle_version + 1)
       OR (NEW.status = OLD.status AND NEW.lifecycle_version <> OLD.lifecycle_version) THEN
        RAISE EXCEPTION 'the lifecycle version moves by exactly one with every status change'
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    RETURN NEW;
END
$$;
DROP TRIGGER IF EXISTS branches_guard ON branches;
CREATE TRIGGER branches_guard BEFORE INSERT OR UPDATE OR DELETE ON branches
    FOR EACH ROW EXECUTE FUNCTION branches_guard();

-- Every branch state (creation and each status change) has exactly its lifecycle event, and
-- every lifecycle event is the branch's current state at commit (no event without a change).
CREATE OR REPLACE FUNCTION public.branches_lifecycle_is_audited() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM public.branch_events e
         WHERE e.graph_id = NEW.graph_id AND e.branch = NEW.branch
           AND e.lifecycle_version = NEW.lifecycle_version AND e.status_after = NEW.status
           AND (NEW.lifecycle_version > 1 OR (e.operation = NEW.origin
                AND e.source_branch IS NOT DISTINCT FROM NEW.source_branch
                AND e.source_commit IS NOT DISTINCT FROM NEW.source_commit))
    ) THEN
        RAISE EXCEPTION 'branches: %/% reached lifecycle version % without its lifecycle event',
            NEW.graph_id, NEW.branch, NEW.lifecycle_version
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    RETURN NULL;
END
$$;
DROP TRIGGER IF EXISTS branches_lifecycle_audited ON branches;
CREATE CONSTRAINT TRIGGER branches_lifecycle_audited
    AFTER INSERT OR UPDATE OF status, lifecycle_version ON branches
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION public.branches_lifecycle_is_audited();

CREATE OR REPLACE FUNCTION public.branch_event_is_current() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM public.branches b JOIN public.refs r ON r.graph_id = b.graph_id AND r.branch = b.branch
         WHERE b.graph_id = NEW.graph_id AND b.branch = NEW.branch
           AND b.lifecycle_version = NEW.lifecycle_version AND b.status = NEW.status_after
           AND b.tenant_id = NEW.tenant_id AND r.head = NEW.head AND r.version = NEW.ref_version
    ) THEN
        RAISE EXCEPTION 'branch_events: event %/%#% does not describe the branch state it records',
            NEW.graph_id, NEW.branch, NEW.lifecycle_version
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    RETURN NULL;
END
$$;
DROP TRIGGER IF EXISTS branch_events_current ON branch_events;
CREATE CONSTRAINT TRIGGER branch_events_current
    AFTER INSERT ON branch_events
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION public.branch_event_is_current();

-- On an active graph every ref is a branch (created with it in the same transaction).
CREATE OR REPLACE FUNCTION public.refs_are_branches() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    IF (SELECT g.status FROM public.graphs g WHERE g.graph_id = NEW.graph_id) = 'active'
       AND NOT EXISTS (SELECT 1 FROM public.branches b
                        WHERE b.graph_id = NEW.graph_id AND b.branch = NEW.branch) THEN
        RAISE EXCEPTION 'refs: ref %/% of an active graph was created without its branch',
            NEW.graph_id, NEW.branch
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    RETURN NULL;
END
$$;
DROP TRIGGER IF EXISTS refs_are_branches ON refs;
CREATE CONSTRAINT TRIGGER refs_are_branches
    AFTER INSERT ON refs
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION public.refs_are_branches();

-- A deleted branch's head never moves.
CREATE OR REPLACE FUNCTION public.refs_branch_is_active() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    IF NEW.head IS DISTINCT FROM OLD.head AND EXISTS (
        SELECT 1 FROM public.branches b
         WHERE b.graph_id = NEW.graph_id AND b.branch = NEW.branch AND b.status <> 'active'
    ) THEN
        RAISE EXCEPTION 'refs: branch %/% is deleted; its head does not move', NEW.graph_id, NEW.branch
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    RETURN NEW;
END
$$;
DROP TRIGGER IF EXISTS refs_branch_active ON refs;
CREATE TRIGGER refs_branch_active BEFORE UPDATE OF head ON refs
    FOR EACH ROW EXECUTE FUNCTION public.refs_branch_is_active();

-- No proposal on a deleted branch, nor on an unknown branch of an active graph other than
-- `main` before its genesis.
CREATE OR REPLACE FUNCTION public.proposals_branch_is_active() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
DECLARE
    st text;
BEGIN
    IF (SELECT g.status FROM public.graphs g WHERE g.graph_id = NEW.graph_id) IS DISTINCT FROM 'active' THEN
        RETURN NEW;
    END IF;
    SELECT b.status INTO st FROM public.branches b WHERE b.graph_id = NEW.graph_id AND b.branch = NEW.branch;
    IF st IS NULL AND NEW.branch <> 'main' THEN
        RAISE EXCEPTION 'proposals: branch %/% does not exist; create it first', NEW.graph_id, NEW.branch
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    IF st IS NOT NULL AND st <> 'active' THEN
        RAISE EXCEPTION 'proposals: branch %/% is deleted', NEW.graph_id, NEW.branch
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    RETURN NEW;
END
$$;
DROP TRIGGER IF EXISTS proposals_branch_active ON proposals;
CREATE TRIGGER proposals_branch_active BEFORE INSERT ON proposals
    FOR EACH ROW EXECUTE FUNCTION public.proposals_branch_is_active();

-- ---- idempotency: branch lifecycle operations ---------------------------------------------
ALTER TABLE idempotency DROP CONSTRAINT idempotency_operation;
ALTER TABLE idempotency ADD CONSTRAINT idempotency_operation
    CHECK (operation IN ('prepare', 'accept', 'reject', 'validate', 'branch_create', 'branch_delete',
                         'branch_restore'));
ALTER TABLE idempotency DROP CONSTRAINT idempotency_result_kind;
ALTER TABLE idempotency ADD CONSTRAINT idempotency_result_kind
    CHECK (result_kind IN ('prepared', 'accepted', 'rejected', 'validated', 'branch_created',
                           'branch_deleted', 'branch_restored'));
ALTER TABLE idempotency ADD COLUMN result_branch_event_id BIGINT NULL;
ALTER TABLE idempotency ADD CONSTRAINT idempotency_branch_event_fk
    FOREIGN KEY (result_branch_event_id, graph_id) REFERENCES branch_events (event_id, graph_id);
ALTER TABLE idempotency ADD CONSTRAINT idempotency_branch_shape CHECK (
    ((operation = 'branch_create') = (result_kind = 'branch_created'))
    AND ((operation = 'branch_delete') = (result_kind = 'branch_deleted'))
    AND ((operation = 'branch_restore') = (result_kind = 'branch_restored'))
    AND ((operation IN ('branch_create', 'branch_delete', 'branch_restore'))
         = (result_branch_event_id IS NOT NULL))
);

-- ---- grants (ADR-0016 amendment, ADR-0022) -------------------------------------------------
-- The runtime grant function is re-issued: SELECT on the branch tables; column INSERT on
-- branches / branch_events; UPDATE of the lifecycle columns only; `protected` on ref insert;
-- idempotency's branch result column; the branch_events sequence.
CREATE OR REPLACE FUNCTION public.ledger_grant_runtime(runtime_role text) RETURNS void
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
DECLARE
    r text := pg_catalog.format('%I', runtime_role);
    is_super boolean;
BEGIN
    SELECT rolsuper INTO is_super FROM pg_catalog.pg_roles WHERE rolname = runtime_role;
    IF is_super IS NULL THEN
        RAISE EXCEPTION 'ledger_grant_runtime: the runtime role does not exist; create it first (ADR-0016)';
    END IF;
    IF is_super THEN
        RAISE EXCEPTION 'ledger_grant_runtime: the runtime role is a superuser; refusing (ADR-0016)';
    END IF;
    IF (SELECT tableowner FROM pg_catalog.pg_tables WHERE schemaname = 'public' AND tablename = 'refs')
       IS DISTINCT FROM current_user::text THEN
        RAISE EXCEPTION 'ledger_grant_runtime: must be run by the owner of the ledger tables (ADR-0016)';
    END IF;
    IF pg_catalog.has_schema_privilege(runtime_role, 'public', 'CREATE') THEN
        RAISE EXCEPTION 'ledger_grant_runtime: the runtime role holds CREATE on schema public; revoke it first (ADR-0016)';
    END IF;
    EXECUTE pg_catalog.format('REVOKE ALL ON ALL TABLES IN SCHEMA public FROM %s', r);
    EXECUTE pg_catalog.format('REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM %s', r);
    EXECUTE pg_catalog.format('REVOKE ALL ON ALL FUNCTIONS IN SCHEMA public FROM %s', r);
    EXECUTE pg_catalog.format('GRANT USAGE ON SCHEMA public TO %s', r);
    EXECUTE pg_catalog.format('GRANT SELECT ON TABLE public.graphs, public.refs, public.immutable_objects, '
                   'public.commit_index, public.commit_parents, public.proposals, public.ref_events, '
                   'public.decisions, public.projection_outbox, public.idempotency, public._sqlx_migrations, '
                   'public.semantic_execution_contexts, public.semantic_virtual_contexts, '
                   'public.validation_records, public.validation_violations, public.decision_validations, '
                   'public.projection_state, public.branches, public.branch_events TO %s', r);
    EXECUTE pg_catalog.format('GRANT INSERT (id, bytes) ON TABLE public.immutable_objects TO %s', r);
    EXECUTE pg_catalog.format('GRANT INSERT (id, graph_id, version, patch_id, parent_count) ON TABLE public.commit_index TO %s', r);
    EXECUTE pg_catalog.format('GRANT INSERT (commit_id, position, parent_id) ON TABLE public.commit_parents TO %s', r);
    EXECUTE pg_catalog.format('GRANT INSERT (graph_id, branch, tenant_id, principal_id, principal_type, on_behalf_of, '
                   'expected_head, requested_patch_id, effective_patch_id, candidate_commit, correlation_id) '
                   'ON TABLE public.proposals TO %s', r);
    EXECUTE pg_catalog.format('GRANT INSERT (graph_id, branch, old_head, new_head, old_version, new_version, operation, '
                   'tenant_id, principal_id, principal_type, on_behalf_of, reason, correlation_id) '
                   'ON TABLE public.ref_events TO %s', r);
    EXECUTE pg_catalog.format('GRANT INSERT (proposal_id, graph_id, branch, candidate_commit, decision, tenant_id, '
                   'principal_id, principal_type, on_behalf_of, reason, validation_ids, ref_event_id, correlation_id) '
                   'ON TABLE public.decisions TO %s', r);
    EXECUTE pg_catalog.format('GRANT INSERT (graph_id, branch, commit_id, ref_version, event_kind, ref_event_id) '
                   'ON TABLE public.projection_outbox TO %s', r);
    EXECUTE pg_catalog.format('GRANT INSERT (tenant_id, principal_id, principal_type, on_behalf_of, graph_id, operation, '
                   'idempotency_key, request_digest, result_kind, result_commit, result_ref_version, '
                   'result_decision_id, result_proposal_id, result_validation_id, result_branch_event_id) '
                   'ON TABLE public.idempotency TO %s', r);
    EXECUTE pg_catalog.format('GRANT INSERT (graph_id, branch, head, version, protected) ON TABLE public.refs TO %s', r);
    EXECUTE pg_catalog.format('GRANT UPDATE (head, version, updated_at) ON TABLE public.refs TO %s', r);
    EXECUTE pg_catalog.format('GRANT INSERT (graph_id, branch, tenant_id, status, lifecycle_version, origin, '
                   'source_branch, source_commit, require_validation, require_distinct_reviewer) '
                   'ON TABLE public.branches TO %s', r);
    EXECUTE pg_catalog.format('GRANT UPDATE (status, lifecycle_version, updated_at) ON TABLE public.branches TO %s', r);
    EXECUTE pg_catalog.format('GRANT INSERT (graph_id, branch, tenant_id, lifecycle_version, operation, status_after, '
                   'head, ref_version, source_branch, source_commit, principal_id, principal_type, on_behalf_of, '
                   'reason, correlation_id) ON TABLE public.branch_events TO %s', r);
    EXECUTE pg_catalog.format('GRANT INSERT (context_id, graph_id, tenant_id, candidate_commit, candidate_state_digest, '
                   'base_kb_id, base_kb_revision, ontology_id, ontology_version, shapes_id, shapes_version, '
                   'reasoning_profile, reasoning_implementation, reasoning_version, sources_revision, validator_service_id, '
                   'validator_service_version, validator_configuration_version, virtual_context_count, canonical_bytes) '
                   'ON TABLE public.semantic_execution_contexts TO %s', r);
    EXECUTE pg_catalog.format('GRANT INSERT (context_id, position, dataset_id, source_version, object_refs, '
                   'query_spec_digest, hydration_plan_digest) ON TABLE public.semantic_virtual_contexts TO %s', r);
    EXECUTE pg_catalog.format('GRANT INSERT (validation_id, graph_id, tenant_id, candidate_commit, candidate_state_digest, '
                   'context_id, validator_service_id, validator_service_version, validator_configuration_version, '
                   'outcome, violation_count, report_digest, report_reference, recorded_at, principal_id, '
                   'principal_type, on_behalf_of, correlation_id, canonical_bytes) '
                   'ON TABLE public.validation_records TO %s', r);
    EXECUTE pg_catalog.format('GRANT INSERT (validation_id, position, severity, code, message) '
                   'ON TABLE public.validation_violations TO %s', r);
    EXECUTE pg_catalog.format('GRANT INSERT (decision_id, validation_id, graph_id, candidate_commit) '
                   'ON TABLE public.decision_validations TO %s', r);
    EXECUTE pg_catalog.format('GRANT USAGE ON SEQUENCE public.proposals_proposal_id_seq, '
                   'public.ref_events_event_id_seq, public.decisions_decision_id_seq, '
                   'public.projection_outbox_outbox_id_seq, public.idempotency_idempotency_id_seq, '
                   'public.branch_events_event_id_seq TO %s', r);
END
$$;
REVOKE ALL ON FUNCTION public.ledger_grant_runtime(text) FROM PUBLIC;
