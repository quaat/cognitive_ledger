-- 0013: Phase 5 diff and merge (ADR-0023 integration commits, ADR-0024 merge preview /
-- propose / apply). Additive: one new write-once table, the `merge` ref-event kind, the merge
-- idempotency operations, database enforcement that only the merge path installs merge
-- candidates, and the re-issued runtime grant. Migration 0009's movement invariant is
-- unchanged: an integration commit's parent 0 is the target head it replaces.

-- ---- merge proposals ---------------------------------------------------------------------
CREATE TABLE merge_proposals (
    proposal_id         BIGINT      NOT NULL,
    graph_id            TEXT        NOT NULL,
    target_branch       TEXT        NOT NULL,
    candidate_commit    TEXT        NOT NULL,
    target_head         TEXT        NOT NULL,
    source_branch       TEXT        NOT NULL,
    source_head         TEXT        NOT NULL,
    merge_base          TEXT        NOT NULL,
    base_explicit       BOOLEAN     NOT NULL,
    classification      TEXT        NOT NULL,
    strategy            TEXT        NOT NULL,
    merge_algorithm     TEXT        NOT NULL,
    conflict_count      INTEGER     NOT NULL,
    merged_state_digest TEXT        NOT NULL,
    preview_token       TEXT        NOT NULL,
    -- The accountable parties (principal ids and delegators) who proposed the commits the
    -- source has and the target lacks; `require_distinct_reviewer` on the target keeps them
    -- (and the merge proposer) from applying their own content (ADR-0024).
    source_parties      TEXT[]      NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT merge_proposals_pkey PRIMARY KEY (proposal_id),
    CONSTRAINT merge_proposals_proposal_fk FOREIGN KEY (proposal_id, graph_id, target_branch, candidate_commit)
        REFERENCES proposals (proposal_id, graph_id, branch, candidate_commit),
    CONSTRAINT merge_proposals_candidate_unique UNIQUE (candidate_commit),
    CONSTRAINT merge_proposals_target_head_fk FOREIGN KEY (graph_id, target_head) REFERENCES commit_index (graph_id, id),
    CONSTRAINT merge_proposals_source_head_fk FOREIGN KEY (graph_id, source_head) REFERENCES commit_index (graph_id, id),
    CONSTRAINT merge_proposals_base_fk FOREIGN KEY (graph_id, merge_base) REFERENCES commit_index (graph_id, id),
    CONSTRAINT merge_proposals_source_branch_fk FOREIGN KEY (graph_id, source_branch) REFERENCES branches (graph_id, branch),
    CONSTRAINT merge_proposals_target_branch_fk FOREIGN KEY (graph_id, target_branch) REFERENCES branches (graph_id, branch),
    CONSTRAINT merge_proposals_classification CHECK (classification IN ('fast_forward', 'divergent')),
    CONSTRAINT merge_proposals_strategy CHECK (strategy IN ('abort', 'take-target', 'take-source', 'union')),
    CONSTRAINT merge_proposals_algorithm CHECK (merge_algorithm = 'structural-slot/v1'),
    CONSTRAINT merge_proposals_conflicts CHECK (conflict_count >= 0),
    CONSTRAINT merge_proposals_distinct_branches CHECK (source_branch <> target_branch),
    -- A fast-forward cannot conflict; its base is the target head and its strategy is
    -- normalized to `abort`. `abort` itself never persists a conflicting merge.
    CONSTRAINT merge_proposals_fast_forward_shape CHECK (
        classification <> 'fast_forward' OR (conflict_count = 0 AND merge_base = target_head AND strategy = 'abort')),
    CONSTRAINT merge_proposals_abort_clean CHECK (strategy <> 'abort' OR conflict_count = 0),
    CONSTRAINT merge_proposals_token_format CHECK (preview_token ~ '^sha256:[0-9a-f]{64}$'),
    CONSTRAINT merge_proposals_digest_format CHECK (merged_state_digest ~ '^sha256:[0-9a-f]{64}$')
);
CREATE INDEX merge_proposals_by_target ON merge_proposals (graph_id, target_branch);
-- Not unique: the token is a confirmation digest of a preview; a rejected merge may be
-- proposed again from the same preview (ADR-0024).
CREATE INDEX merge_proposals_by_token ON merge_proposals (preview_token);

DROP TRIGGER IF EXISTS merge_proposals_write_once ON merge_proposals;
CREATE TRIGGER merge_proposals_write_once BEFORE UPDATE OR DELETE ON merge_proposals
    FOR EACH ROW EXECUTE FUNCTION ledger_rows_are_write_once();

-- The candidate is an integration commit of exactly this preview: two parents, parent 0 the
-- target head the proposal expects, parent 1 the recorded source head.
CREATE OR REPLACE FUNCTION public.merge_proposals_match_lineage() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
DECLARE
    n smallint;
    p0 text;
    p1 text;
    expected text;
BEGIN
    SELECT c.parent_count INTO n FROM public.commit_index c WHERE c.id = NEW.candidate_commit;
    SELECT p.parent_id INTO p0 FROM public.commit_parents p WHERE p.commit_id = NEW.candidate_commit AND p.position = 0;
    SELECT p.parent_id INTO p1 FROM public.commit_parents p WHERE p.commit_id = NEW.candidate_commit AND p.position = 1;
    SELECT pr.expected_head INTO expected FROM public.proposals pr WHERE pr.proposal_id = NEW.proposal_id;
    IF n IS DISTINCT FROM 2 OR p0 IS DISTINCT FROM NEW.target_head OR p1 IS DISTINCT FROM NEW.source_head
       OR expected IS DISTINCT FROM NEW.target_head THEN
        RAISE EXCEPTION 'merge_proposals: candidate % is not the integration commit [%, %] of proposal %',
            NEW.candidate_commit, NEW.target_head, NEW.source_head, NEW.proposal_id
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    RETURN NEW;
END
$$;
DROP TRIGGER IF EXISTS merge_proposals_lineage ON merge_proposals;
CREATE TRIGGER merge_proposals_lineage BEFORE INSERT ON merge_proposals
    FOR EACH ROW EXECUTE FUNCTION public.merge_proposals_match_lineage();

-- ---- ref events: the `merge` kind ----------------------------------------------------------
ALTER TABLE ref_events DROP CONSTRAINT ref_events_operation;
ALTER TABLE ref_events ADD CONSTRAINT ref_events_operation CHECK (operation IN ('genesis', 'advance', 'merge'));
ALTER TABLE ref_events DROP CONSTRAINT ref_events_genesis_shape;
ALTER TABLE ref_events ADD CONSTRAINT ref_events_genesis_shape CHECK (
    (operation = 'genesis' AND old_head IS NULL AND old_version IS NULL AND new_version = 1)
    OR (operation IN ('advance', 'merge') AND old_head IS NOT NULL AND old_version IS NOT NULL
        AND new_version = old_version + 1));

-- Only the merge path installs merge candidates, and it always records them as `merge`: an
-- `advance` installs a candidate with at most one parent and no merge row; a `merge`
-- installs the integration commit of a merge proposal for this branch whose target head is
-- the head it replaces. `genesis` is unconstrained (a branch created at an integration
-- commit is a genesis on a two-parent commit).
CREATE OR REPLACE FUNCTION public.ref_events_kind_matches_candidate() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    IF NEW.operation = 'advance' THEN
        IF (SELECT c.parent_count FROM public.commit_index c WHERE c.id = NEW.new_head) > 1
           OR EXISTS (SELECT 1 FROM public.merge_proposals m WHERE m.candidate_commit = NEW.new_head) THEN
            RAISE EXCEPTION 'ref_events: % on %/% is a merge candidate; only a merge event installs it',
                NEW.new_head, NEW.graph_id, NEW.branch
                USING ERRCODE = 'integrity_constraint_violation';
        END IF;
    ELSIF NEW.operation = 'merge' THEN
        IF NOT EXISTS (
            SELECT 1 FROM public.merge_proposals m
             WHERE m.candidate_commit = NEW.new_head AND m.graph_id = NEW.graph_id
               AND m.target_branch = NEW.branch AND m.target_head = NEW.old_head
        ) THEN
            RAISE EXCEPTION 'ref_events: merge of % on %/% has no matching merge proposal',
                NEW.new_head, NEW.graph_id, NEW.branch
                USING ERRCODE = 'integrity_constraint_violation';
        END IF;
    END IF;
    RETURN NEW;
END
$$;
DROP TRIGGER IF EXISTS ref_events_kind ON ref_events;
CREATE TRIGGER ref_events_kind BEFORE INSERT ON ref_events
    FOR EACH ROW EXECUTE FUNCTION public.ref_events_kind_matches_candidate();

-- An accepted decision on a merge candidate is the decision of its `merge` event.
CREATE OR REPLACE FUNCTION public.decisions_merge_kind() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    IF NEW.decision = 'accepted'
       AND EXISTS (SELECT 1 FROM public.merge_proposals m WHERE m.candidate_commit = NEW.candidate_commit)
       AND (SELECT e.operation FROM public.ref_events e WHERE e.event_id = NEW.ref_event_id) IS DISTINCT FROM 'merge' THEN
        RAISE EXCEPTION 'decisions: merge candidate % must be accepted through a merge event', NEW.candidate_commit
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    RETURN NEW;
END
$$;
DROP TRIGGER IF EXISTS decisions_merge_kind ON decisions;
CREATE TRIGGER decisions_merge_kind BEFORE INSERT ON decisions
    FOR EACH ROW EXECUTE FUNCTION public.decisions_merge_kind();

-- ---- idempotency: merge operations ---------------------------------------------------------
ALTER TABLE idempotency DROP CONSTRAINT idempotency_operation;
ALTER TABLE idempotency ADD CONSTRAINT idempotency_operation
    CHECK (operation IN ('prepare', 'accept', 'reject', 'validate', 'branch_create', 'branch_delete',
                         'branch_restore', 'merge_propose', 'merge_apply'));
ALTER TABLE idempotency DROP CONSTRAINT idempotency_result_kind;
ALTER TABLE idempotency ADD CONSTRAINT idempotency_result_kind
    CHECK (result_kind IN ('prepared', 'accepted', 'rejected', 'validated', 'branch_created',
                           'branch_deleted', 'branch_restored', 'merge_proposed', 'merge_applied'));
ALTER TABLE idempotency ADD CONSTRAINT idempotency_merge_shape CHECK (
    ((operation = 'merge_propose') = (result_kind = 'merge_proposed'))
    AND ((operation = 'merge_apply') = (result_kind = 'merge_applied'))
    AND (result_kind <> 'merge_proposed' OR (result_proposal_id IS NOT NULL AND result_commit IS NOT NULL))
    AND (result_kind <> 'merge_applied'
         OR (result_decision_id IS NOT NULL AND result_ref_version IS NOT NULL AND result_commit IS NOT NULL))
);

-- ---- grants (ADR-0016 amendment, ADR-0024) -------------------------------------------------
-- Re-issued: SELECT and column INSERT on merge_proposals; everything else as in 0012.
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
                   'public.projection_state, public.branches, public.branch_events, public.merge_proposals TO %s', r);
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
    EXECUTE pg_catalog.format('GRANT INSERT (proposal_id, graph_id, target_branch, candidate_commit, target_head, '
                   'source_branch, source_head, merge_base, base_explicit, classification, strategy, '
                   'merge_algorithm, conflict_count, merged_state_digest, preview_token, source_parties) '
                   'ON TABLE public.merge_proposals TO %s', r);
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
REVOKE ALL ON FUNCTION public.merge_proposals_match_lineage() FROM PUBLIC;
REVOKE ALL ON FUNCTION public.ref_events_kind_matches_candidate() FROM PUBLIC;
REVOKE ALL ON FUNCTION public.decisions_merge_kind() FROM PUBLIC;
