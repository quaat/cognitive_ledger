-- Database-level write-once guards (ADR-0012). The application never UPDATEs or DELETEs
-- immutable content or the derived commit index; these triggers make that a schema rule
-- too, so an accidental administrative statement cannot rewrite history in place.
-- (A compromised or superuser role can still disable triggers; role separation between
-- migration and runtime is a deployment decision recorded in tech-debt, not enforced here.)
-- refs stay mutable in `head`/`updated_at` only: their identity columns are immutable.
CREATE OR REPLACE FUNCTION ledger_rows_are_write_once() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION '% rows are write-once (% attempted)', TG_TABLE_NAME, TG_OP
        USING ERRCODE = 'integrity_constraint_violation';
END
$$;

DROP TRIGGER IF EXISTS immutable_objects_write_once ON immutable_objects;
CREATE TRIGGER immutable_objects_write_once
    BEFORE UPDATE OR DELETE ON immutable_objects
    FOR EACH ROW EXECUTE FUNCTION ledger_rows_are_write_once();

DROP TRIGGER IF EXISTS commit_index_write_once ON commit_index;
CREATE TRIGGER commit_index_write_once
    BEFORE UPDATE OR DELETE ON commit_index
    FOR EACH ROW EXECUTE FUNCTION ledger_rows_are_write_once();

DROP TRIGGER IF EXISTS commit_parents_write_once ON commit_parents;
CREATE TRIGGER commit_parents_write_once
    BEFORE UPDATE OR DELETE ON commit_parents
    FOR EACH ROW EXECUTE FUNCTION ledger_rows_are_write_once();

CREATE OR REPLACE FUNCTION refs_identity_is_immutable() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.graph_id IS DISTINCT FROM OLD.graph_id OR NEW.branch IS DISTINCT FROM OLD.branch THEN
        RAISE EXCEPTION 'refs identity (graph_id, branch) is immutable; only head moves'
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    RETURN NEW;
END
$$;
DROP TRIGGER IF EXISTS refs_identity_immutable ON refs;
CREATE TRIGGER refs_identity_immutable
    BEFORE UPDATE ON refs
    FOR EACH ROW EXECUTE FUNCTION refs_identity_is_immutable();
