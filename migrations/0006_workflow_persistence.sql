-- Atomic workflow persistence (ADR-0013, Plan 0004 P1.3). Additive; 0001–0005 untouched.
--
-- refs gains a monotonic version and a protection flag, and the planned schema
-- guarantee: a ref's head must be an indexed commit OF ITS OWN GRAPH
-- (refs(graph_id, head) -> commit_index(graph_id, id)). Existing refs whose head is not
-- such a commit make this migration FAIL with an actionable error; history is never
-- invented or repaired here.
--
-- proposals / ref_events / decisions are append-only audit history (write-once triggers);
-- projection_outbox identity columns are immutable while Phase 3 may update delivery
-- columns; idempotency rows are immutable results.

-- ---- refs -------------------------------------------------------------------------
DO $$
DECLARE
    dangling TEXT;
BEGIN
    SELECT string_agg(format('(%s, %s -> %s)', r.graph_id, r.branch, r.head), ', ' ORDER BY r.graph_id, r.branch)
      INTO dangling
      FROM refs r
      LEFT JOIN commit_index c ON c.graph_id = r.graph_id AND c.id = r.head
     WHERE c.id IS NULL;
    IF dangling IS NOT NULL THEN
        RAISE EXCEPTION 'migration 0006 (ADR-0013): refs point at heads that are not indexed commits of their graph: %. Import or verify that history (ledger-admin migrate-fs-to-pg / verify) before upgrading; nothing is repaired automatically.', dangling;
    END IF;
END
$$;

DO $$
DECLARE
    bad TEXT;
BEGIN
    SELECT string_agg(format('(%s, %s)', graph_id, branch), ', ' ORDER BY graph_id, branch)
      INTO bad
      FROM refs
     WHERE NOT (octet_length(branch) BETWEEN 1 AND 128 AND branch ~ '^[A-Za-z0-9._/-]+$');
    IF bad IS NOT NULL THEN
        RAISE EXCEPTION 'migration 0006: ref names outside [A-Za-z0-9._/-]{1,128} must be renamed before upgrading: %', bad;
    END IF;
END
$$;

ALTER TABLE refs ADD COLUMN version   BIGINT  NOT NULL DEFAULT 1;
ALTER TABLE refs ADD COLUMN protected BOOLEAN NOT NULL DEFAULT true;
ALTER TABLE refs ADD CONSTRAINT refs_version_positive CHECK (version >= 1);
ALTER TABLE refs ADD CONSTRAINT refs_branch_bounds
    CHECK (octet_length(branch) BETWEEN 1 AND 128 AND branch ~ '^[A-Za-z0-9._/-]+$');
ALTER TABLE refs
    ADD CONSTRAINT refs_head_fk FOREIGN KEY (graph_id, head) REFERENCES commit_index (graph_id, id);

-- version starts at 1 and moves by exactly one with the head; `protected` is set at
-- creation and immutable until branch policy (Phase 4) defines an audited change.
CREATE OR REPLACE FUNCTION refs_version_is_monotonic() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.version <> 1 THEN
            RAISE EXCEPTION 'a new ref starts at version 1 (%/% given %)', NEW.graph_id, NEW.branch, NEW.version
                USING ERRCODE = 'integrity_constraint_violation';
        END IF;
        RETURN NEW;
    END IF;
    IF NEW.head IS DISTINCT FROM OLD.head AND NEW.version <> OLD.version + 1 THEN
        RAISE EXCEPTION 'refs.version must increase by exactly one on every head movement (% -> % for %/%)',
            OLD.version, NEW.version, OLD.graph_id, OLD.branch
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    IF NEW.head IS NOT DISTINCT FROM OLD.head AND NEW.version <> OLD.version THEN
        RAISE EXCEPTION 'refs.version changes only with the head (%/%)', OLD.graph_id, OLD.branch
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    IF NEW.protected IS DISTINCT FROM OLD.protected THEN
        RAISE EXCEPTION 'refs.protected is immutable until branch policy defines an audited change (%/%)', OLD.graph_id, OLD.branch
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    RETURN NEW;
END
$$;
DROP TRIGGER IF EXISTS refs_version_monotonic ON refs;
CREATE TRIGGER refs_version_monotonic
    BEFORE INSERT OR UPDATE ON refs
    FOR EACH ROW EXECUTE FUNCTION refs_version_is_monotonic();

-- ---- proposals -------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS proposals (
    proposal_id        BIGSERIAL   PRIMARY KEY,
    graph_id           TEXT        NOT NULL REFERENCES graphs (graph_id),
    branch             TEXT        NOT NULL,
    tenant_id          TEXT        NOT NULL,
    principal_id       TEXT        NOT NULL,
    principal_type     TEXT        NOT NULL,
    on_behalf_of       TEXT        NULL,
    expected_head      TEXT        NULL,
    requested_patch_id TEXT        NOT NULL REFERENCES immutable_objects (id),
    effective_patch_id TEXT        NOT NULL REFERENCES immutable_objects (id),
    candidate_commit   TEXT        NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT proposals_candidate_fk FOREIGN KEY (graph_id, candidate_commit)
        REFERENCES commit_index (graph_id, id),
    CONSTRAINT proposals_candidate_unique UNIQUE (candidate_commit),
    -- Target for the decisions FK: a decision names its proposal's own graph/branch/candidate.
    CONSTRAINT proposals_identity UNIQUE (proposal_id, graph_id, branch, candidate_commit),
    CONSTRAINT proposals_branch_bounds CHECK (octet_length(branch) BETWEEN 1 AND 128 AND branch ~ '^[A-Za-z0-9._/-]+$'),
    CONSTRAINT proposals_principal_type CHECK (principal_type IN ('human', 'agent', 'service'))
);
CREATE INDEX IF NOT EXISTS proposals_by_ref ON proposals (graph_id, branch, created_at);

-- ---- ref_events -------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS ref_events (
    event_id       BIGSERIAL   PRIMARY KEY,
    graph_id       TEXT        NOT NULL,
    branch         TEXT        NOT NULL,
    old_head       TEXT        NULL,
    new_head       TEXT        NOT NULL,
    old_version    BIGINT      NULL,
    new_version    BIGINT      NOT NULL,
    operation      TEXT        NOT NULL,
    tenant_id      TEXT        NOT NULL,
    principal_id   TEXT        NOT NULL,
    principal_type TEXT        NOT NULL,
    on_behalf_of   TEXT        NULL,
    reason         TEXT        NULL,
    recorded_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT ref_events_ref_fk FOREIGN KEY (graph_id, branch) REFERENCES refs (graph_id, branch),
    CONSTRAINT ref_events_new_head_fk FOREIGN KEY (graph_id, new_head) REFERENCES commit_index (graph_id, id),
    CONSTRAINT ref_events_version_unique UNIQUE (graph_id, branch, new_version),
    -- Targets for the composite FKs that tie decisions and outbox rows to the exact event.
    CONSTRAINT ref_events_identity UNIQUE (event_id, graph_id, branch, new_head),
    CONSTRAINT ref_events_identity_versioned UNIQUE (event_id, graph_id, branch, new_version, new_head),
    CONSTRAINT ref_events_branch_bounds CHECK (octet_length(branch) BETWEEN 1 AND 128 AND branch ~ '^[A-Za-z0-9._/-]+$'),
    CONSTRAINT ref_events_operation CHECK (operation IN ('genesis', 'advance')),
    CONSTRAINT ref_events_genesis_shape CHECK (
        (operation = 'genesis' AND old_head IS NULL AND old_version IS NULL AND new_version = 1)
        OR (operation = 'advance' AND old_head IS NOT NULL AND old_version IS NOT NULL AND new_version = old_version + 1)
    ),
    CONSTRAINT ref_events_principal_type CHECK (principal_type IN ('human', 'agent', 'service'))
);

-- ---- decisions --------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS decisions (
    decision_id      BIGSERIAL   PRIMARY KEY,
    proposal_id      BIGINT      NULL REFERENCES proposals (proposal_id),
    graph_id         TEXT        NOT NULL,
    branch           TEXT        NOT NULL,
    candidate_commit TEXT        NOT NULL,
    decision         TEXT        NOT NULL,
    tenant_id        TEXT        NOT NULL,
    principal_id     TEXT        NOT NULL,
    principal_type   TEXT        NOT NULL,
    on_behalf_of     TEXT        NULL,
    reason           TEXT        NULL,
    validation_ids   TEXT[]      NOT NULL DEFAULT '{}',
    ref_event_id     BIGINT      NULL REFERENCES ref_events (event_id),
    decided_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT decisions_candidate_fk FOREIGN KEY (graph_id, candidate_commit)
        REFERENCES commit_index (graph_id, id),
    CONSTRAINT decisions_kind CHECK (decision IN ('accepted', 'rejected', 'superseded')),
    CONSTRAINT decisions_accepted_has_event CHECK (
        (decision = 'accepted' AND ref_event_id IS NOT NULL)
        OR (decision <> 'accepted' AND ref_event_id IS NULL)
    ),
    -- An accepted decision names exactly the event that moved this candidate onto this ref.
    CONSTRAINT decisions_event_fk FOREIGN KEY (ref_event_id, graph_id, branch, candidate_commit)
        REFERENCES ref_events (event_id, graph_id, branch, new_head),
    CONSTRAINT decisions_reason_bounds CHECK (reason IS NULL OR octet_length(reason) <= 4096),
    CONSTRAINT decisions_proposal_identity_fk FOREIGN KEY (proposal_id, graph_id, branch, candidate_commit)
        REFERENCES proposals (proposal_id, graph_id, branch, candidate_commit),
    CONSTRAINT decisions_principal_type CHECK (principal_type IN ('human', 'agent', 'service'))
);
-- At most one terminal decision per candidate (every decision is terminal), hence per
-- proposal, and one accepted decision per ref event.
CREATE UNIQUE INDEX IF NOT EXISTS decisions_one_per_candidate ON decisions (candidate_commit);
CREATE UNIQUE INDEX IF NOT EXISTS decisions_one_per_proposal ON decisions (proposal_id) WHERE proposal_id IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS decisions_one_per_ref_event ON decisions (ref_event_id) WHERE ref_event_id IS NOT NULL;

-- ---- projection_outbox ------------------------------------------------------------
CREATE TABLE IF NOT EXISTS projection_outbox (
    outbox_id    BIGSERIAL   PRIMARY KEY,
    graph_id     TEXT        NOT NULL,
    branch       TEXT        NOT NULL,
    commit_id    TEXT        NOT NULL,
    ref_version  BIGINT      NOT NULL,
    event_kind   TEXT        NOT NULL,
    ref_event_id BIGINT      NOT NULL REFERENCES ref_events (event_id),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    delivered_at TIMESTAMPTZ NULL,
    attempts     INTEGER     NOT NULL DEFAULT 0,
    CONSTRAINT outbox_event_kind CHECK (event_kind IN ('ref_advanced')),
    CONSTRAINT outbox_per_ref_version UNIQUE (graph_id, branch, ref_version),
    CONSTRAINT outbox_ref_event_unique UNIQUE (ref_event_id),
    CONSTRAINT outbox_commit_fk FOREIGN KEY (graph_id, commit_id) REFERENCES commit_index (graph_id, id),
    CONSTRAINT outbox_version_fk FOREIGN KEY (graph_id, branch, ref_version)
        REFERENCES ref_events (graph_id, branch, new_version),
    -- The row describes exactly its event: same graph, branch, version and commit.
    CONSTRAINT outbox_event_identity_fk FOREIGN KEY (ref_event_id, graph_id, branch, ref_version, commit_id)
        REFERENCES ref_events (event_id, graph_id, branch, new_version, new_head)
);

-- ---- idempotency ------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS idempotency (
    tenant_id          TEXT        NOT NULL,
    principal_id       TEXT        NOT NULL,
    graph_id           TEXT        NOT NULL,
    operation          TEXT        NOT NULL,
    idempotency_key    TEXT        NOT NULL,
    request_digest     TEXT        NOT NULL,
    result_kind        TEXT        NOT NULL,
    result_commit      TEXT        NULL,
    result_ref_version BIGINT      NULL,
    result_decision_id BIGINT      NULL REFERENCES decisions (decision_id),
    result_proposal_id BIGINT      NULL REFERENCES proposals (proposal_id),
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, principal_id, graph_id, operation, idempotency_key),
    CONSTRAINT idempotency_operation CHECK (operation IN ('prepare', 'accept', 'reject')),
    CONSTRAINT idempotency_key_bounds CHECK (octet_length(idempotency_key) BETWEEN 1 AND 256),
    CONSTRAINT idempotency_digest_format CHECK (request_digest ~ '^sha256:[0-9a-f]{64}$'),
    CONSTRAINT idempotency_result_kind CHECK (result_kind IN ('prepared', 'accepted', 'rejected'))
);

-- ---- write-once guards ------------------------------------------------------------
DROP TRIGGER IF EXISTS proposals_write_once ON proposals;
CREATE TRIGGER proposals_write_once BEFORE UPDATE OR DELETE ON proposals
    FOR EACH ROW EXECUTE FUNCTION ledger_rows_are_write_once();
DROP TRIGGER IF EXISTS ref_events_write_once ON ref_events;
CREATE TRIGGER ref_events_write_once BEFORE UPDATE OR DELETE ON ref_events
    FOR EACH ROW EXECUTE FUNCTION ledger_rows_are_write_once();
DROP TRIGGER IF EXISTS decisions_write_once ON decisions;
CREATE TRIGGER decisions_write_once BEFORE UPDATE OR DELETE ON decisions
    FOR EACH ROW EXECUTE FUNCTION ledger_rows_are_write_once();
DROP TRIGGER IF EXISTS idempotency_write_once ON idempotency;
CREATE TRIGGER idempotency_write_once BEFORE UPDATE OR DELETE ON idempotency
    FOR EACH ROW EXECUTE FUNCTION ledger_rows_are_write_once();

CREATE OR REPLACE FUNCTION outbox_identity_is_immutable() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'projection_outbox rows are never deleted' USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    IF NEW.outbox_id <> OLD.outbox_id OR NEW.graph_id <> OLD.graph_id OR NEW.branch <> OLD.branch
       OR NEW.commit_id <> OLD.commit_id OR NEW.ref_version <> OLD.ref_version
       OR NEW.event_kind <> OLD.event_kind OR NEW.ref_event_id <> OLD.ref_event_id
       OR NEW.created_at <> OLD.created_at THEN
        RAISE EXCEPTION 'projection_outbox identity columns are immutable; only delivery columns change'
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    RETURN NEW;
END
$$;
DROP TRIGGER IF EXISTS outbox_identity_immutable ON projection_outbox;
CREATE TRIGGER outbox_identity_immutable BEFORE UPDATE OR DELETE ON projection_outbox
    FOR EACH ROW EXECUTE FUNCTION outbox_identity_is_immutable();
