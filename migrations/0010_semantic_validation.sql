-- Phase 2 semantic validation persistence (ADR-0014, ADR-0018, ADR-0019; Plan 0006).
-- Additive; 0001–0009 untouched.
--
-- semantic_execution_contexts  the exact semantic context a validation ran under
--                              (content-addressed: context_id = sha256(canonical_bytes),
--                              layout sculpin-semantic-context/v1); one row per distinct
--                              context, shared by any number of validations of the same
--                              candidate.
-- semantic_virtual_contexts    relational projection of the context's Virtual A-Box
--                              references (identifying provenance only — never triples).
-- validation_records           immutable validator outcomes (content-addressed, layout
--                              sculpin-validation-record/v1); a candidate may carry many.
-- validation_violations        bounded summary of a non-conforming record (auditable
--                              without the external report).
-- decision_validations         the enforced relation between a decision and the
--                              validations it cites: PostgreSQL proves same graph and same
--                              candidate on both sides. decisions.validation_ids stays and
--                              is populated identically (informational).
-- idempotency                  gains the operation `validate` / result `validated` and the
--                              recorded validation id.
--
-- Every new row is write-once. No new sequences: content ids are text and detail rows are
-- keyed by (id, position). The runtime grant function is re-issued below with exactly the
-- columns the store's INSERT statements name (ADR-0016 amendment).

-- ---- guard: no P1 decision may already cite validations (none could exist) -----------
DO $$
DECLARE
    bad BIGINT;
BEGIN
    SELECT count(*) INTO bad FROM decisions WHERE cardinality(validation_ids) > 0;
    IF bad > 0 THEN
        RAISE EXCEPTION 'migration 0010: % decision(s) cite validation ids before any validation record existed; refusing to upgrade', bad;
    END IF;
END
$$;

-- ---- semantic_execution_contexts -----------------------------------------------------
CREATE TABLE semantic_execution_contexts (
    context_id                      TEXT        PRIMARY KEY,
    graph_id                        TEXT        NOT NULL,
    tenant_id                       TEXT        NOT NULL,
    candidate_commit                TEXT        NOT NULL,
    candidate_state_digest          TEXT        NOT NULL,
    base_kb_id                      TEXT        NOT NULL,
    base_kb_revision                TEXT        NOT NULL,
    ontology_id                     TEXT        NULL,
    ontology_version                TEXT        NULL,
    shapes_id                       TEXT        NOT NULL,
    shapes_version                  TEXT        NOT NULL,
    -- NULL together when no reasoning ran (the one spelling of "no reasoning").
    reasoning_profile               TEXT        NULL,
    reasoning_implementation        TEXT        NULL,
    reasoning_version               TEXT        NULL,
    validator_service_id            TEXT        NOT NULL,
    validator_service_version       TEXT        NOT NULL,
    validator_configuration_version TEXT        NOT NULL,
    virtual_context_count           INTEGER     NOT NULL,
    canonical_bytes                 BYTEA       NOT NULL,
    created_at                      TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT sec_context_id_format CHECK (context_id ~ '^sha256:[0-9a-f]{64}$'),
    CONSTRAINT sec_state_digest_format CHECK (candidate_state_digest ~ '^sha256:[0-9a-f]{64}$'),
    CONSTRAINT sec_content_addressed CHECK (context_id = 'sha256:' || encode(sha256(canonical_bytes), 'hex')),
    CONSTRAINT sec_reasoning_shape CHECK (
        (reasoning_profile IS NULL AND reasoning_implementation IS NULL AND reasoning_version IS NULL)
        OR (reasoning_profile IS NOT NULL AND reasoning_implementation IS NOT NULL AND reasoning_version IS NOT NULL)
    ),
    CONSTRAINT sec_ontology_shape CHECK (
        (ontology_id IS NULL AND ontology_version IS NULL)
        OR (ontology_id IS NOT NULL AND ontology_version IS NOT NULL)
    ),
    CONSTRAINT sec_token_bounds CHECK (
        octet_length(base_kb_id) BETWEEN 1 AND 512 AND octet_length(base_kb_revision) BETWEEN 1 AND 512
        AND (ontology_id IS NULL OR octet_length(ontology_id) BETWEEN 1 AND 512)
        AND (ontology_version IS NULL OR octet_length(ontology_version) BETWEEN 1 AND 512)
        AND octet_length(shapes_id) BETWEEN 1 AND 512 AND octet_length(shapes_version) BETWEEN 1 AND 512
        AND (reasoning_profile IS NULL OR octet_length(reasoning_profile) BETWEEN 1 AND 512)
        AND (reasoning_implementation IS NULL OR octet_length(reasoning_implementation) BETWEEN 1 AND 512)
        AND (reasoning_version IS NULL OR octet_length(reasoning_version) BETWEEN 1 AND 512)
        AND octet_length(validator_service_id) BETWEEN 1 AND 512
        AND octet_length(validator_service_version) BETWEEN 1 AND 512
        AND octet_length(validator_configuration_version) BETWEEN 1 AND 512
    ),
    CONSTRAINT sec_virtual_context_count CHECK (virtual_context_count BETWEEN 0 AND 64),
    CONSTRAINT sec_candidate_fk FOREIGN KEY (graph_id, candidate_commit)
        REFERENCES commit_index (graph_id, id),
    CONSTRAINT sec_graph_tenant_fk FOREIGN KEY (graph_id, tenant_id)
        REFERENCES graphs (graph_id, tenant_id),
    -- FK target: a record's (context, graph, candidate, state digest, validator) must agree.
    CONSTRAINT sec_identity UNIQUE (context_id, graph_id, candidate_commit, candidate_state_digest,
        validator_service_id, validator_service_version, validator_configuration_version)
);
CREATE INDEX IF NOT EXISTS sec_by_candidate ON semantic_execution_contexts (graph_id, candidate_commit);

CREATE TABLE semantic_virtual_contexts (
    context_id            TEXT    NOT NULL REFERENCES semantic_execution_contexts (context_id),
    position              INTEGER NOT NULL,
    dataset_id            TEXT    NOT NULL,
    source_version        TEXT    NOT NULL,
    object_refs           TEXT[]  NOT NULL DEFAULT '{}',
    query_spec_digest     TEXT    NOT NULL,
    hydration_plan_digest TEXT    NOT NULL,
    PRIMARY KEY (context_id, position),
    CONSTRAINT svc_position CHECK (position BETWEEN 0 AND 63),
    CONSTRAINT svc_token_bounds CHECK (
        octet_length(dataset_id) BETWEEN 1 AND 512 AND octet_length(source_version) BETWEEN 1 AND 512
    ),
    CONSTRAINT svc_object_refs_bound CHECK (cardinality(object_refs) BETWEEN 0 AND 64),
    CONSTRAINT svc_digest_format CHECK (
        query_spec_digest ~ '^sha256:[0-9a-f]{64}$' AND hydration_plan_digest ~ '^sha256:[0-9a-f]{64}$'
    )
);

-- ---- validation_records ----------------------------------------------------------------
CREATE TABLE validation_records (
    validation_id                   TEXT        PRIMARY KEY,
    graph_id                        TEXT        NOT NULL,
    tenant_id                       TEXT        NOT NULL,
    candidate_commit                TEXT        NOT NULL,
    candidate_state_digest          TEXT        NOT NULL,
    context_id                      TEXT        NOT NULL,
    validator_service_id            TEXT        NOT NULL,
    validator_service_version       TEXT        NOT NULL,
    validator_configuration_version TEXT        NOT NULL,
    outcome                         TEXT        NOT NULL,
    violation_count                 INTEGER     NOT NULL,
    report_digest                   TEXT        NOT NULL,
    report_reference                TEXT        NULL,
    -- Server-assigned when the response was recorded; part of the hashed record, hence
    -- written by the runtime rather than defaulted.
    recorded_at                     TIMESTAMPTZ NOT NULL,
    -- Who asked for the validation (the authenticated caller of `validate`), for audit.
    principal_id                    TEXT        NOT NULL,
    principal_type                  TEXT        NOT NULL,
    on_behalf_of                    TEXT        NULL,
    correlation_id                  TEXT        NULL,
    canonical_bytes                 BYTEA       NOT NULL,
    created_at                      TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT vr_validation_id_format CHECK (validation_id ~ '^sha256:[0-9a-f]{64}$'),
    CONSTRAINT vr_content_addressed CHECK (validation_id = 'sha256:' || encode(sha256(canonical_bytes), 'hex')),
    CONSTRAINT vr_outcome_kind CHECK (outcome IN ('conforms', 'violations')),
    CONSTRAINT vr_outcome_shape CHECK (
        (outcome = 'conforms' AND violation_count >= 0) OR (outcome = 'violations' AND violation_count >= 1)
    ),
    CONSTRAINT vr_report_digest_format CHECK (report_digest ~ '^sha256:[0-9a-f]{64}$'),
    CONSTRAINT vr_report_reference_bounds CHECK (report_reference IS NULL OR octet_length(report_reference) BETWEEN 1 AND 2048),
    CONSTRAINT vr_principal_type CHECK (principal_type IN ('human', 'agent', 'service')),
    CONSTRAINT vr_correlation_bounds CHECK (correlation_id IS NULL OR octet_length(correlation_id) BETWEEN 1 AND 128),
    CONSTRAINT vr_token_bounds CHECK (
        octet_length(validator_service_id) BETWEEN 1 AND 512
        AND octet_length(validator_service_version) BETWEEN 1 AND 512
        AND octet_length(validator_configuration_version) BETWEEN 1 AND 512
    ),
    -- The record's context is a stored context of exactly this candidate, state and validator.
    CONSTRAINT vr_context_fk FOREIGN KEY (context_id, graph_id, candidate_commit, candidate_state_digest,
            validator_service_id, validator_service_version, validator_configuration_version)
        REFERENCES semantic_execution_contexts (context_id, graph_id, candidate_commit, candidate_state_digest,
            validator_service_id, validator_service_version, validator_configuration_version),
    CONSTRAINT vr_candidate_fk FOREIGN KEY (graph_id, candidate_commit)
        REFERENCES commit_index (graph_id, id),
    CONSTRAINT vr_graph_tenant_fk FOREIGN KEY (graph_id, tenant_id)
        REFERENCES graphs (graph_id, tenant_id),
    -- FK target: a decision cites validations of its own graph and candidate.
    CONSTRAINT vr_identity UNIQUE (validation_id, graph_id, candidate_commit)
);
CREATE INDEX IF NOT EXISTS vr_by_candidate ON validation_records (graph_id, candidate_commit, created_at);

CREATE TABLE validation_violations (
    validation_id TEXT    NOT NULL REFERENCES validation_records (validation_id),
    position      INTEGER NOT NULL,
    severity      TEXT    NOT NULL,
    code          TEXT    NOT NULL,
    message       TEXT    NOT NULL,
    PRIMARY KEY (validation_id, position),
    CONSTRAINT vv_position CHECK (position BETWEEN 0 AND 63),
    CONSTRAINT vv_bounds CHECK (
        octet_length(severity) BETWEEN 1 AND 64 AND octet_length(code) BETWEEN 1 AND 512
        AND octet_length(message) <= 1024
    )
);

-- ---- decision_validations --------------------------------------------------------------
ALTER TABLE decisions ADD CONSTRAINT decisions_identity UNIQUE (decision_id, graph_id, candidate_commit);

CREATE TABLE decision_validations (
    decision_id      BIGINT NOT NULL,
    validation_id    TEXT   NOT NULL,
    graph_id         TEXT   NOT NULL,
    candidate_commit TEXT   NOT NULL,
    PRIMARY KEY (decision_id, validation_id),
    CONSTRAINT dv_decision_fk FOREIGN KEY (decision_id, graph_id, candidate_commit)
        REFERENCES decisions (decision_id, graph_id, candidate_commit),
    CONSTRAINT dv_validation_fk FOREIGN KEY (validation_id, graph_id, candidate_commit)
        REFERENCES validation_records (validation_id, graph_id, candidate_commit)
);

-- ---- idempotency: the validate operation ------------------------------------------------
ALTER TABLE idempotency DROP CONSTRAINT idempotency_operation;
ALTER TABLE idempotency ADD CONSTRAINT idempotency_operation
    CHECK (operation IN ('prepare', 'accept', 'reject', 'validate'));
ALTER TABLE idempotency DROP CONSTRAINT idempotency_result_kind;
ALTER TABLE idempotency ADD CONSTRAINT idempotency_result_kind
    CHECK (result_kind IN ('prepared', 'accepted', 'rejected', 'validated'));
ALTER TABLE idempotency ADD COLUMN result_validation_id TEXT NULL;
-- A `validated` result names a record of the same graph and candidate (all three columns are
-- NOT NULL for such rows by the shape CHECK, so MATCH SIMPLE cannot be bypassed).
ALTER TABLE idempotency ADD CONSTRAINT idempotency_validation_fk
    FOREIGN KEY (result_validation_id, graph_id, result_commit)
    REFERENCES validation_records (validation_id, graph_id, candidate_commit);
ALTER TABLE idempotency ADD CONSTRAINT idempotency_validation_shape CHECK (
    ((operation = 'validate') = (result_kind = 'validated'))
    AND ((result_kind = 'validated') = (result_validation_id IS NOT NULL))
    AND (result_kind <> 'validated' OR result_commit IS NOT NULL)
);

-- ---- write-once guards -------------------------------------------------------------------
DROP TRIGGER IF EXISTS semantic_execution_contexts_write_once ON semantic_execution_contexts;
CREATE TRIGGER semantic_execution_contexts_write_once BEFORE UPDATE OR DELETE ON semantic_execution_contexts
    FOR EACH ROW EXECUTE FUNCTION ledger_rows_are_write_once();
DROP TRIGGER IF EXISTS semantic_virtual_contexts_write_once ON semantic_virtual_contexts;
CREATE TRIGGER semantic_virtual_contexts_write_once BEFORE UPDATE OR DELETE ON semantic_virtual_contexts
    FOR EACH ROW EXECUTE FUNCTION ledger_rows_are_write_once();
DROP TRIGGER IF EXISTS validation_records_write_once ON validation_records;
CREATE TRIGGER validation_records_write_once BEFORE UPDATE OR DELETE ON validation_records
    FOR EACH ROW EXECUTE FUNCTION ledger_rows_are_write_once();
DROP TRIGGER IF EXISTS validation_violations_write_once ON validation_violations;
CREATE TRIGGER validation_violations_write_once BEFORE UPDATE OR DELETE ON validation_violations
    FOR EACH ROW EXECUTE FUNCTION ledger_rows_are_write_once();
DROP TRIGGER IF EXISTS decision_validations_write_once ON decision_validations;
CREATE TRIGGER decision_validations_write_once BEFORE UPDATE OR DELETE ON decision_validations
    FOR EACH ROW EXECUTE FUNCTION ledger_rows_are_write_once();

-- ---- runtime grants (ADR-0016 amendment) ------------------------------------------------
-- Same function, same safety rules as 0008 (owner-only, pinned search_path, revoke-then-
-- grant, refuses superusers and CREATE holders, never echoes the role name); the grant set is
-- the 0008 set plus the Phase-2 tables. Derived from crates/ledger-store/src/*.rs:
--   validation   INSERT (named columns only) semantic_execution_contexts,
--                semantic_virtual_contexts, validation_records, validation_violations,
--                decision_validations; idempotency additionally result_validation_id
-- Deliberately absent: any UPDATE/DELETE on the new tables (write-once), any new sequence.
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
                   'public.validation_records, public.validation_violations, public.decision_validations TO %s', r);
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
                   'reasoning_profile, reasoning_implementation, reasoning_version, validator_service_id, '
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
