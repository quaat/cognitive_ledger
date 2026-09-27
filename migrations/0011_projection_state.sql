-- Phase 3 accepted-state projection (ADR-0020, ADR-0021; Plan 0007). Additive; 0001–0010
-- untouched.
--
-- projection_state   one row per enabled projection stream (graph, ref, target): the
--                    cognitive graph it writes, the ledger version it has projected, the
--                    stream lease (fenced by lease_epoch), backoff and the last error.
--                    Created/disabled only by the owner (`ledger-admin projection enable`);
--                    the projector role updates progress, lease and error columns.
-- ref_events         gains UNIQUE (graph_id, branch, new_version, new_head): the FK target
--                    binding recorded projection progress to a real accepted ref state.
-- projection_outbox  delivery becomes monotonic (delivered_at set once, attempts never
--                    decrease); 0006 already freezes identity columns and refuses DELETE.
--
-- CHECKs are written with flat ANDs (no BETWEEN) so a logical restore re-creates them
-- byte-identically (ADR-0017 amendment). No new sequences.

ALTER TABLE ref_events
    ADD CONSTRAINT ref_events_version_head UNIQUE (graph_id, branch, new_version, new_head);

CREATE TABLE projection_state (
    graph_id              TEXT        NOT NULL,
    branch                TEXT        NOT NULL,
    target_id             TEXT        NOT NULL,
    tenant_id             TEXT        NOT NULL,
    cognitive_graph       TEXT        NOT NULL,
    status                TEXT        NOT NULL DEFAULT 'active',
    projected_commit      TEXT        NULL,
    projected_ref_version BIGINT      NULL,
    lease_owner           TEXT        NULL,
    lease_until           TIMESTAMPTZ NULL,
    lease_epoch           BIGINT      NOT NULL DEFAULT 0,
    next_attempt_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    consecutive_failures  INTEGER     NOT NULL DEFAULT 0,
    last_success_at       TIMESTAMPTZ NULL,
    last_error_at         TIMESTAMPTZ NULL,
    last_error_code       TEXT        NULL,
    rebuilds              BIGINT      NOT NULL DEFAULT 0,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT projection_state_pk PRIMARY KEY (graph_id, branch, target_id),
    -- No two streams of one target write the same cognitive graph (tenant isolation).
    CONSTRAINT projection_state_graph_unique UNIQUE (target_id, cognitive_graph),
    CONSTRAINT projection_state_graph_tenant_fk FOREIGN KEY (graph_id, tenant_id)
        REFERENCES graphs (graph_id, tenant_id),
    -- Recorded progress names a real accepted state of exactly this ref.
    CONSTRAINT projection_state_progress_fk
        FOREIGN KEY (graph_id, branch, projected_ref_version, projected_commit)
        REFERENCES ref_events (graph_id, branch, new_version, new_head),
    CONSTRAINT ps_status CHECK (status IN ('active', 'blocked', 'rebuild_required', 'disabled')),
    CONSTRAINT ps_progress_shape CHECK ((projected_commit IS NULL) = (projected_ref_version IS NULL)),
    CONSTRAINT ps_lease_shape CHECK ((lease_owner IS NULL) = (lease_until IS NULL)),
    CONSTRAINT ps_target_id_format CHECK (target_id ~ '^[A-Za-z0-9._:-]{1,128}$'),
    CONSTRAINT ps_branch_bounds CHECK (octet_length(branch) >= 1 AND octet_length(branch) <= 128
        AND branch ~ '^[A-Za-z0-9._/-]+$'),
    CONSTRAINT ps_cognitive_graph_format CHECK (octet_length(cognitive_graph) <= 2048
        AND cognitive_graph ~ '^urn:sculpin:kb:[A-Za-z0-9._~%-]+:cognitive$'),
    CONSTRAINT ps_lease_owner_bounds CHECK (lease_owner IS NULL
        OR (octet_length(lease_owner) >= 1 AND octet_length(lease_owner) <= 256)),
    CONSTRAINT ps_error_code_format CHECK (last_error_code IS NULL OR last_error_code ~ '^[A-Z_]{1,64}$'),
    CONSTRAINT ps_counters CHECK (lease_epoch >= 0 AND consecutive_failures >= 0 AND rebuilds >= 0
        AND (projected_ref_version IS NULL OR projected_ref_version >= 1))
);

-- ---- guards ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION public.projection_state_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'projection_state rows are never deleted; disable the stream instead'
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    IF NEW.graph_id <> OLD.graph_id OR NEW.branch <> OLD.branch OR NEW.target_id <> OLD.target_id
       OR NEW.tenant_id <> OLD.tenant_id OR NEW.cognitive_graph <> OLD.cognitive_graph
       OR NEW.created_at <> OLD.created_at THEN
        RAISE EXCEPTION 'projection_state identity columns are immutable'
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    IF NEW.lease_epoch < OLD.lease_epoch OR NEW.rebuilds < OLD.rebuilds THEN
        RAISE EXCEPTION 'projection_state lease epochs and rebuild counts never decrease'
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    IF OLD.projected_ref_version IS NOT NULL AND (NEW.projected_ref_version IS NULL
       OR NEW.projected_ref_version < OLD.projected_ref_version) THEN
        RAISE EXCEPTION 'projection_state progress never moves backwards'
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    RETURN NEW;
END
$$;
DROP TRIGGER IF EXISTS projection_state_guard ON projection_state;
CREATE TRIGGER projection_state_guard BEFORE UPDATE OR DELETE ON projection_state
    FOR EACH ROW EXECUTE FUNCTION projection_state_guard();

CREATE OR REPLACE FUNCTION public.outbox_delivery_is_monotonic() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.delivered_at IS NOT NULL AND NEW.delivered_at IS DISTINCT FROM OLD.delivered_at THEN
        RAISE EXCEPTION 'projection_outbox delivered_at is set once'
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    IF NEW.attempts < OLD.attempts THEN
        RAISE EXCEPTION 'projection_outbox attempts never decrease'
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    RETURN NEW;
END
$$;
DROP TRIGGER IF EXISTS outbox_delivery_monotonic ON projection_outbox;
CREATE TRIGGER outbox_delivery_monotonic BEFORE UPDATE ON projection_outbox
    FOR EACH ROW EXECUTE FUNCTION outbox_delivery_is_monotonic();

-- ---- grants (ADR-0016 amendment, ADR-0021) --------------------------------------------
-- The runtime grant function is re-issued with SELECT on projection_state (status reads);
-- the runtime role never writes projection progress or outbox delivery columns.
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
                   'public.projection_state TO %s', r);
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
                   'result_decision_id, result_proposal_id, result_validation_id) ON TABLE public.idempotency TO %s', r);
    EXECUTE pg_catalog.format('GRANT INSERT (graph_id, branch, head, version) ON TABLE public.refs TO %s', r);
    EXECUTE pg_catalog.format('GRANT UPDATE (head, version, updated_at) ON TABLE public.refs TO %s', r);
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
                   'public.projection_outbox_outbox_id_seq, public.idempotency_idempotency_id_seq TO %s', r);
END
$$;
REVOKE ALL ON FUNCTION public.ledger_grant_runtime(text) FROM PUBLIC;

-- The projector identity: reads what reconstruction and projection need, updates only the
-- outbox delivery columns and the stream's progress/lease/error columns (ADR-0021).
CREATE OR REPLACE FUNCTION public.ledger_grant_projector(projector_role text) RETURNS void
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
DECLARE
    r text := pg_catalog.format('%I', projector_role);
    is_super boolean;
BEGIN
    SELECT rolsuper INTO is_super FROM pg_catalog.pg_roles WHERE rolname = projector_role;
    IF is_super IS NULL THEN
        RAISE EXCEPTION 'ledger_grant_projector: the projector role does not exist; create it first (ADR-0021)';
    END IF;
    IF is_super THEN
        RAISE EXCEPTION 'ledger_grant_projector: the projector role is a superuser; refusing (ADR-0021)';
    END IF;
    IF (SELECT tableowner FROM pg_catalog.pg_tables WHERE schemaname = 'public' AND tablename = 'refs')
       IS DISTINCT FROM current_user::text THEN
        RAISE EXCEPTION 'ledger_grant_projector: must be run by the owner of the ledger tables (ADR-0021)';
    END IF;
    IF pg_catalog.has_schema_privilege(projector_role, 'public', 'CREATE') THEN
        RAISE EXCEPTION 'ledger_grant_projector: the projector role holds CREATE on schema public; revoke it first (ADR-0021)';
    END IF;
    EXECUTE pg_catalog.format('REVOKE ALL ON ALL TABLES IN SCHEMA public FROM %s', r);
    EXECUTE pg_catalog.format('REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM %s', r);
    EXECUTE pg_catalog.format('REVOKE ALL ON ALL FUNCTIONS IN SCHEMA public FROM %s', r);
    EXECUTE pg_catalog.format('GRANT USAGE ON SCHEMA public TO %s', r);
    EXECUTE pg_catalog.format('GRANT SELECT ON TABLE public.graphs, public.refs, public.immutable_objects, '
                   'public.commit_index, public.commit_parents, public.ref_events, public.projection_outbox, '
                   'public.projection_state, public._sqlx_migrations TO %s', r);
    EXECUTE pg_catalog.format('GRANT UPDATE (delivered_at, attempts) ON TABLE public.projection_outbox TO %s', r);
    EXECUTE pg_catalog.format('GRANT UPDATE (status, projected_commit, projected_ref_version, lease_owner, '
                   'lease_until, lease_epoch, next_attempt_at, consecutive_failures, last_success_at, '
                   'last_error_at, last_error_code, rebuilds) ON TABLE public.projection_state TO %s', r);
END
$$;
REVOKE ALL ON FUNCTION public.ledger_grant_projector(text) FROM PUBLIC;
