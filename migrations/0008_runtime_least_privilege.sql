-- ADR-0016: runtime least privilege. Additive; 0001–0007 untouched.
--
-- The schema owner (the identity running migrations) installs a reviewable, versioned
-- function that grants a pre-existing runtime role exactly the DML the request path
-- executes. No role is created here (managed PostgreSQL often withholds CREATEROLE from
-- application owners); the operator supplies the role and `ledger-admin migrate
-- --runtime-role <name>` calls this function after migrating. The function first REVOKEs
-- everything the role holds on the ledger schema, so the result is exactly the set below
-- however the role was configured before; re-running is idempotent.
--
-- Derived from the store's SQL (crates/ledger-store/src/*.rs):
--   reads      SELECT on every ledger table and on _sqlx_migrations (readiness/version)
--   publish    INSERT immutable_objects, commit_index, commit_parents
--   workflow   INSERT (named columns only) proposals, ref_events, decisions,
--              projection_outbox, idempotency — ids and timestamps keep their defaults
--   refs       INSERT (graph_id, branch, head, version) only — `protected` and the
--              timestamps keep their defaults; UPDATE (head, version, updated_at) only
--   sequences  USAGE on the audit BIGSERIAL sequences
-- Deliberately absent: CREATE on the schema, any ALTER/DROP/TRUNCATE, TRIGGER, UPDATE or
-- DELETE on immutable/audit tables, INSERT/UPDATE on graphs (operator provisioning),
-- UPDATE/DELETE on projection_outbox (the Phase 3 consumer gets its own grant).
--
-- Safety: the function pins search_path (it is executed by the owner on every deploy, so
-- an unqualified reference could be hijacked by an object planted in `public`), refuses a
-- superuser or a role that holds CREATE on the schema (either would defeat the boundary),
-- and never echoes the role name in an error (it may be a mistyped URL).

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
                   'public.decisions, public.projection_outbox, public.idempotency, public._sqlx_migrations TO %s', r);
    -- Column-level INSERT: exactly the columns the store's INSERT statements name, so the
    -- runtime cannot back-date audit rows, pre-mark outbox rows delivered, choose ids, or
    -- create unprotected refs.
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
                   'result_decision_id, result_proposal_id) ON TABLE public.idempotency TO %s', r);
    EXECUTE pg_catalog.format('GRANT INSERT (graph_id, branch, head, version) ON TABLE public.refs TO %s', r);
    EXECUTE pg_catalog.format('GRANT UPDATE (head, version, updated_at) ON TABLE public.refs TO %s', r);
    EXECUTE pg_catalog.format('GRANT USAGE ON SEQUENCE public.proposals_proposal_id_seq, '
                   'public.ref_events_event_id_seq, public.decisions_decision_id_seq, '
                   'public.projection_outbox_outbox_id_seq, public.idempotency_idempotency_id_seq TO %s', r);
END
$$;

-- Only the owner (and superusers) may hand out runtime privileges.
REVOKE ALL ON FUNCTION public.ledger_grant_runtime(text) FROM PUBLIC;
