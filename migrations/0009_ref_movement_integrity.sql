-- ADR-0016: database-enforced integrity that a least-privileged runtime cannot bypass.
-- Additive; 0001–0008 untouched.
--
-- 1. Every head movement of a ref on an active or archived graph must be audited in the
--    same transaction by a matching ref_events row and must be a fast-forward (the new
--    head's first parent is the old head). Bootstrap and importing graphs are exempt: they
--    are moved by the owner-only raw ref path (ADR-0010/0013) that predates ref events.
--    A runtime identity holding UPDATE (head, version) therefore cannot rewind, jump or
--    move a ref silently.
-- 2. Any status change on a graph takes the exclusive advisory lock the workflow shares,
--    so a lifecycle transition can never slip between the workflow's status check and its
--    writes, whoever issues the UPDATE.
-- 3. immutable_objects is content-addressed by constraint: id must be the SHA-256 of the
--    bytes, so a poisoned object under a predictable id cannot be inserted.
--
-- Advisory lock keys are derived identically in SQL and Rust (ledger_store::lock_key):
-- the first 8 bytes, big-endian, of SHA-256 over the UTF-8 key string.

CREATE OR REPLACE FUNCTION public.ledger_lock_key(key text) RETURNS bigint
LANGUAGE sql IMMUTABLE STRICT
SET search_path = pg_catalog, public
AS $$
    SELECT ('x' || pg_catalog.left(pg_catalog.encode(pg_catalog.sha256(pg_catalog.convert_to(key, 'UTF8')), 'hex'), 16))::bit(64)::bigint
$$;

CREATE OR REPLACE FUNCTION public.refs_movement_is_audited() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
DECLARE
    st text;
BEGIN
    SELECT status INTO st FROM public.graphs WHERE graph_id = NEW.graph_id;
    IF st IN ('bootstrap', 'importing') THEN
        RETURN NULL;
    END IF;
    IF TG_OP = 'INSERT' THEN
        IF NOT EXISTS (
            SELECT 1 FROM public.ref_events e
             WHERE e.graph_id = NEW.graph_id AND e.branch = NEW.branch
               AND e.new_version = NEW.version AND e.new_head = NEW.head AND e.old_head IS NULL
        ) THEN
            RAISE EXCEPTION 'refs: ref %/% was created without a matching ref event', NEW.graph_id, NEW.branch;
        END IF;
    ELSIF NEW.head IS DISTINCT FROM OLD.head OR NEW.version IS DISTINCT FROM OLD.version THEN
        IF NOT EXISTS (
            SELECT 1 FROM public.ref_events e
             WHERE e.graph_id = NEW.graph_id AND e.branch = NEW.branch
               AND e.new_version = NEW.version AND e.new_head = NEW.head
               AND e.old_head = OLD.head AND e.old_version = OLD.version
        ) THEN
            RAISE EXCEPTION 'refs: head of %/% moved without a matching ref event', NEW.graph_id, NEW.branch;
        END IF;
        IF NOT EXISTS (
            SELECT 1 FROM public.commit_parents p
             WHERE p.commit_id = NEW.head AND p.position = 0 AND p.parent_id = OLD.head
        ) THEN
            RAISE EXCEPTION 'refs: head of %/% did not move to a direct descendant (no rewinds or jumps)', NEW.graph_id, NEW.branch;
        END IF;
    END IF;
    RETURN NULL;
END
$$;

DROP TRIGGER IF EXISTS refs_movement_audited ON refs;
CREATE CONSTRAINT TRIGGER refs_movement_audited
    AFTER INSERT OR UPDATE OF head, version ON refs
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION public.refs_movement_is_audited();

CREATE OR REPLACE FUNCTION public.graphs_status_change_serializes() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    IF NEW.status IS DISTINCT FROM OLD.status THEN
        PERFORM pg_catalog.pg_advisory_xact_lock(public.ledger_lock_key('graph-status:' || OLD.graph_id));
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS graphs_status_change_serialized ON graphs;
CREATE TRIGGER graphs_status_change_serialized
    BEFORE UPDATE OF status ON graphs
    FOR EACH ROW EXECUTE FUNCTION public.graphs_status_change_serializes();

ALTER TABLE immutable_objects ADD CONSTRAINT immutable_objects_content_addressed
    CHECK (id = 'sha256:' || encode(sha256(bytes), 'hex'));

REVOKE ALL ON FUNCTION public.refs_movement_is_audited() FROM PUBLIC;
REVOKE ALL ON FUNCTION public.graphs_status_change_serializes() FROM PUBLIC;
