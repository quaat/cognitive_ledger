//! Schema migration and verification (ADR-0012/0013/0016).
//!
//! Migrations are embedded in the binary (`sqlx::migrate!`), monotonic and checksummed.
//! Only the schema owner runs them, through `ledger-admin migrate` (`migrate_all_on` on a
//! dedicated connection). The runtime never migrates: `verify` demands the exact schema
//! level this build was written for and refuses behind, ahead, absent or corrupt metadata.

use crate::db_error;
use ledger_core::LedgerError;
use sqlx::{PgConnection, PgPool, Row};
use std::borrow::Cow;
use std::collections::BTreeMap;

fn migrator() -> sqlx::migrate::Migrator {
    sqlx::migrate!("../../migrations")
}

/// Apply every migration on a pool (tests and tooling).
pub async fn migrate_all(pool: &PgPool) -> Result<(), LedgerError> {
    migrator().run(pool).await.map_err(db_migrate)
}

/// Apply every migration on one dedicated connection (`ledger-admin migrate`): a failed run
/// leaves its advisory lock on this connection only, which the process then closes.
pub async fn migrate_all_on(conn: &mut PgConnection) -> Result<(), LedgerError> {
    migrator().run(conn).await.map_err(db_migrate)
}

/// Apply migrations with version `<= upto` only. Used by the filesystem→PostgreSQL cutover
/// to bring a database to the content schema (0005) before content is imported, so the
/// 0006 guard ("every ref head is an indexed commit of its graph") can then pass.
pub async fn migrate_up_to(pool: &PgPool, upto: i64) -> Result<(), LedgerError> {
    let mut migrator = migrator();
    // A database that is already past `upto` has applied versions this restricted list
    // does not contain; that is expected, not drift.
    migrator.set_ignore_missing(true);
    migrator.migrations = Cow::Owned(
        migrator
            .migrations
            .iter()
            .filter(|m| m.version <= upto)
            .cloned()
            .collect(),
    );
    migrator.run(pool).await.map_err(db_migrate)
}

fn db_migrate(e: sqlx::migrate::MigrateError) -> LedgerError {
    match e {
        sqlx::migrate::MigrateError::Execute(inner) => db_error(inner),
        other => LedgerError::Storage(other.to_string()),
    }
}

/// Grant the runtime role its privileges through the versioned `ledger_grant_runtime`
/// function installed by migration 0008 (owner connection; idempotent).
pub async fn grant_runtime_role(conn: &mut PgConnection, role: &str) -> Result<(), LedgerError> {
    sqlx::query("SELECT ledger_grant_runtime($1)")
        .bind(role)
        .execute(&mut *conn)
        .await
        .map_err(db_error)?;
    // No ledger function is executable by PUBLIC: the migrations revoke it on the functions
    // they create, a database restored with `pg_restore --no-acl` comes back with the
    // default public EXECUTE, and the grant function only shapes the runtime role's own
    // rights (its `REVOKE … FROM <role>` even materializes the default ACL). Trigger
    // functions fire regardless of EXECUTE (checked at CREATE TRIGGER, by the owner).
    sqlx::query("REVOKE ALL ON ALL FUNCTIONS IN SCHEMA public FROM PUBLIC")
        .execute(&mut *conn)
        .await
        .map_err(db_error)?;
    Ok(())
}

/// The migration version that completes the content schema (immutable objects, commit
/// index, graphs, write-once guards) but precedes the workflow schema and its refs FK.
pub const CONTENT_SCHEMA_VERSION: i64 = 5;
/// The exact schema level this build requires at runtime (startup and readiness refuse
/// anything else, ADR-0016).
pub const REQUIRED_SCHEMA_VERSION: i64 = 10;

/// What `verify` found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaReport {
    pub version: i64,
    /// Expression fingerprints (`md5` of the stored node tree with statement offsets removed)
    /// of every CHECK constraint and partial-index predicate as found now, keyed
    /// `check:<table>.<name>` / `index:<name>`: readiness compares them with the values
    /// start-up validated by deparse, lock-free.
    pub fingerprints: BTreeMap<String, String>,
}

/// Verify that the database is at exactly `REQUIRED_SCHEMA_VERSION` with intact migration
/// metadata. Runs on the runtime identity (SELECT on `_sqlx_migrations`).
pub async fn verify(pool: &PgPool) -> Result<SchemaReport, LedgerError> {
    let rows = match sqlx::query(
        "SELECT version, checksum, success FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(sqlx::Error::Database(d)) if d.code().as_deref() == Some("42501") => {
            return Err(LedgerError::RuntimeIdentity(
                "the connected role cannot read the migration metadata: it has not been \
                 granted the runtime privileges; run `ledger-admin migrate --runtime-role <role>` \
                 with the owner identity (ADR-0016)"
                    .into(),
            ));
        }
        Err(sqlx::Error::Database(d)) if d.code().as_deref() == Some("42P01") => {
            return Err(LedgerError::SchemaIncompatible(
                "migration metadata is absent (no _sqlx_migrations table): this database has \
                 never been migrated; run `ledger-admin migrate` with the owner identity"
                    .into(),
            ));
        }
        Err(e) => return Err(db_error(e)),
    };
    let embedded = migrator();
    let mut highest = 0i64;
    for row in &rows {
        let version: i64 = row.try_get("version").map_err(db_error)?;
        let checksum: Vec<u8> = row.try_get("checksum").map_err(db_error)?;
        let success: bool = row.try_get("success").map_err(db_error)?;
        if !success {
            return Err(LedgerError::SchemaIncompatible(format!(
                "migration {version:04} is recorded as failed: repair the database with the \
                 owner identity before starting the service"
            )));
        }
        match embedded.migrations.iter().find(|m| m.version == version) {
            Some(m) if m.checksum.as_ref() == checksum.as_slice() => {}
            Some(_) => {
                return Err(LedgerError::SchemaIncompatible(format!(
                    "migration {version:04} was applied with different contents than this \
                     build embeds: released migrations are immutable; refusing to serve"
                )));
            }
            None if version > REQUIRED_SCHEMA_VERSION => {}
            None => {
                return Err(LedgerError::SchemaIncompatible(format!(
                    "migration {version:04} is recorded but unknown to this build"
                )));
            }
        }
        highest = highest.max(version);
    }
    if highest < REQUIRED_SCHEMA_VERSION {
        return Err(LedgerError::SchemaIncompatible(format!(
            "database schema is at {highest:04}; this build requires {REQUIRED_SCHEMA_VERSION:04}: \
             stop the replicas and run `ledger-admin migrate` with the owner identity"
        )));
    }
    for expected in embedded
        .migrations
        .iter()
        .filter(|m| m.version <= REQUIRED_SCHEMA_VERSION)
    {
        if !rows.iter().any(|row| {
            row.try_get::<i64, _>("version")
                .is_ok_and(|v| v == expected.version)
        }) {
            return Err(LedgerError::SchemaIncompatible(format!(
                "migration {:04} is missing from the migration history: the schema is not \
                 contiguous; repair with the owner identity",
                expected.version
            )));
        }
    }
    if highest > REQUIRED_SCHEMA_VERSION {
        return Err(LedgerError::SchemaIncompatible(format!(
            "database schema is at {highest:04}, newer than the {REQUIRED_SCHEMA_VERSION:04} this \
             build supports: deploy a build that knows this schema"
        )));
    }
    // Every database integrity primitive the runtime privilege model depends on must be
    // present, enabled, attached to the intended object and semantically what this build
    // expects; a same-named replacement elsewhere or with weaker semantics is not compatible.
    verify_guard_triggers(pool).await?;
    verify_guard_functions(pool).await?;
    let fingerprints = verify_constraints_and_indexes(pool).await?;
    verify_not_null(pool).await?;
    Ok(SchemaReport {
        version: highest,
        fingerprints,
    })
}

// ---------------------------------------------------------------------------------------
// Expected database controls (derived from migrations 0004–0010)
// ---------------------------------------------------------------------------------------

/// A trigger the least-privilege model relies on, as `CREATE TRIGGER` in the migrations
/// defines it. All triggers are `FOR EACH ROW` in schema `public` with functions in `public`.
struct ExpectedTrigger {
    name: &'static str,
    table: &'static str,
    function: &'static str,
    /// `BEFORE` (true) or `AFTER` (false; every AFTER trigger here is a constraint trigger).
    before: bool,
    insert: bool,
    update: bool,
    delete: bool,
    /// `UPDATE OF <columns>`; empty means any column.
    update_columns: &'static [&'static str],
    constraint: bool,
    deferrable: bool,
    initially_deferred: bool,
}

impl ExpectedTrigger {
    /// `pg_trigger.tgtype` bit layout (TRIGGER_TYPE_*): ROW 1, BEFORE 2, INSERT 4, DELETE 8,
    /// UPDATE 16, TRUNCATE 32, INSTEAD 64.
    fn tgtype(&self) -> i16 {
        1 | if self.before { 2 } else { 0 }
            | if self.insert { 4 } else { 0 }
            | if self.delete { 8 } else { 0 }
            | if self.update { 16 } else { 0 }
    }
}

const fn before(
    name: &'static str,
    table: &'static str,
    function: &'static str,
    insert: bool,
    update: bool,
    delete: bool,
    update_columns: &'static [&'static str],
) -> ExpectedTrigger {
    ExpectedTrigger {
        name,
        table,
        function,
        before: true,
        insert,
        update,
        delete,
        update_columns,
        constraint: false,
        deferrable: false,
        initially_deferred: false,
    }
}

/// Triggers that make the ledger's write-once, identity and movement rules database facts.
const GUARD_TRIGGERS: &[ExpectedTrigger] = &[
    // 0004
    before(
        "graphs_identity_immutable",
        "graphs",
        "graphs_identity_is_immutable",
        false,
        true,
        false,
        &[],
    ),
    // 0005
    before(
        "immutable_objects_write_once",
        "immutable_objects",
        "ledger_rows_are_write_once",
        false,
        true,
        true,
        &[],
    ),
    before(
        "commit_index_write_once",
        "commit_index",
        "ledger_rows_are_write_once",
        false,
        true,
        true,
        &[],
    ),
    before(
        "commit_parents_write_once",
        "commit_parents",
        "ledger_rows_are_write_once",
        false,
        true,
        true,
        &[],
    ),
    before(
        "refs_identity_immutable",
        "refs",
        "refs_identity_is_immutable",
        false,
        true,
        false,
        &[],
    ),
    // 0006
    before(
        "refs_version_monotonic",
        "refs",
        "refs_version_is_monotonic",
        true,
        true,
        false,
        &[],
    ),
    before(
        "proposals_write_once",
        "proposals",
        "ledger_rows_are_write_once",
        false,
        true,
        true,
        &[],
    ),
    before(
        "ref_events_write_once",
        "ref_events",
        "ledger_rows_are_write_once",
        false,
        true,
        true,
        &[],
    ),
    before(
        "decisions_write_once",
        "decisions",
        "ledger_rows_are_write_once",
        false,
        true,
        true,
        &[],
    ),
    before(
        "idempotency_write_once",
        "idempotency",
        "ledger_rows_are_write_once",
        false,
        true,
        true,
        &[],
    ),
    before(
        "outbox_identity_immutable",
        "projection_outbox",
        "outbox_identity_is_immutable",
        false,
        true,
        true,
        &[],
    ),
    // 0009: audited fast-forward ref movement (deferred constraint trigger) and serialized
    // graph status changes.
    ExpectedTrigger {
        name: "refs_movement_audited",
        table: "refs",
        function: "refs_movement_is_audited",
        before: false,
        insert: true,
        update: true,
        delete: false,
        update_columns: &["head", "version"],
        constraint: true,
        deferrable: true,
        initially_deferred: true,
    },
    before(
        "graphs_status_change_serialized",
        "graphs",
        "graphs_status_change_serializes",
        false,
        true,
        false,
        &["status"],
    ),
    // 0010: Phase 2 validation persistence is write-once too.
    before(
        "semantic_execution_contexts_write_once",
        "semantic_execution_contexts",
        "ledger_rows_are_write_once",
        false,
        true,
        true,
        &[],
    ),
    before(
        "semantic_virtual_contexts_write_once",
        "semantic_virtual_contexts",
        "ledger_rows_are_write_once",
        false,
        true,
        true,
        &[],
    ),
    before(
        "validation_records_write_once",
        "validation_records",
        "ledger_rows_are_write_once",
        false,
        true,
        true,
        &[],
    ),
    before(
        "validation_violations_write_once",
        "validation_violations",
        "ledger_rows_are_write_once",
        false,
        true,
        true,
        &[],
    ),
    before(
        "decision_validations_write_once",
        "decision_validations",
        "ledger_rows_are_write_once",
        false,
        true,
        true,
        &[],
    ),
];

fn incompatible(message: String) -> LedgerError {
    LedgerError::SchemaIncompatible(message)
}

/// A function the guard triggers execute, exactly as the shipped migrations define it. The
/// expectation is derived from the embedded migration SQL (the last `CREATE OR REPLACE
/// FUNCTION` for each name), so the verifier can never drift from the migrations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpectedFunction {
    pub name: String,
    pub returns: String,
    pub language: String,
    /// `SET search_path = …` clause value, if the definition pins one.
    pub search_path: Option<String>,
    /// The dollar-quoted body verbatim (PostgreSQL stores it as `pg_proc.prosrc`).
    pub body: String,
}

/// Functions whose bodies the integrity model depends on: every guard trigger's function
/// plus the lock-key helper the 0009 triggers call.
const GUARD_FUNCTIONS: &[&str] = &[
    "graphs_identity_is_immutable",
    "ledger_rows_are_write_once",
    "refs_identity_is_immutable",
    "refs_version_is_monotonic",
    "outbox_identity_is_immutable",
    "refs_movement_is_audited",
    "graphs_status_change_serializes",
    "ledger_lock_key",
];

/// Parse every `CREATE OR REPLACE FUNCTION … AS $$ … $$` in the embedded migrations (up to
/// the required version) and keep the last definition per function name.
pub fn expected_guard_functions() -> Vec<ExpectedFunction> {
    let mut found: Vec<ExpectedFunction> = Vec::new();
    for migration in migrator()
        .migrations
        .iter()
        .filter(|m| m.version <= REQUIRED_SCHEMA_VERSION)
    {
        let sql: &str = &migration.sql;
        let mut rest = sql;
        while let Some(start) = rest.find("CREATE OR REPLACE FUNCTION ") {
            let header_start = start + "CREATE OR REPLACE FUNCTION ".len();
            let Some(body_open_rel) = rest[header_start..].find("AS $$") else {
                break;
            };
            let header = &rest[header_start..header_start + body_open_rel];
            let body_start = header_start + body_open_rel + "AS $$".len();
            let Some(body_len) = rest[body_start..].find("$$") else {
                break;
            };
            let body = &rest[body_start..body_start + body_len];
            rest = &rest[body_start + body_len + 2..];
            let name = header
                .split('(')
                .next()
                .unwrap_or("")
                .trim()
                .trim_start_matches("public.")
                .to_owned();
            if !GUARD_FUNCTIONS.contains(&name.as_str()) {
                continue;
            }
            let word_after = |key: &str| -> Option<String> {
                header.find(key).map(|i| {
                    header[i + key.len()..]
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .to_owned()
                })
            };
            let returns = word_after("RETURNS ").unwrap_or_default();
            let language = word_after("LANGUAGE ").unwrap_or_default();
            let search_path = header.find("SET search_path = ").map(|i| {
                header[i + "SET search_path = ".len()..]
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_owned()
            });
            found.retain(|f| f.name != name);
            found.push(ExpectedFunction {
                name,
                returns,
                language,
                search_path,
                body: body.to_owned(),
            });
        }
    }
    found
}

/// Every guard function in the database has exactly the body, language, return type and
/// `search_path` setting the migrations gave it and is not `SECURITY DEFINER`: a same-named
/// no-op replacement would otherwise keep every trigger "present and enabled".
async fn verify_guard_functions(pool: &PgPool) -> Result<(), LedgerError> {
    let expected = expected_guard_functions();
    for name in GUARD_FUNCTIONS {
        if !expected.iter().any(|f| f.name == *name) {
            return Err(incompatible(format!(
                "this build's migrations define no function {name}; refusing to serve"
            )));
        }
    }
    for f in &expected {
        let rows = sqlx::query(
            "SELECT p.prosrc, l.lanname::text AS language, p.prosecdef, \
                    pg_catalog.format_type(p.prorettype, NULL) AS returns, p.proconfig, \
                    pg_get_userbyid(p.proowner)::text AS owner, \
                    (SELECT tableowner::text FROM pg_tables WHERE schemaname = 'public' AND tablename = 'refs') AS table_owner \
             FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
             JOIN pg_language l ON l.oid = p.prolang \
             WHERE n.nspname = 'public' AND p.proname = $1",
        )
        .bind(&f.name)
        .fetch_all(pool)
        .await
        .map_err(db_error)?;
        let row = match rows.as_slice() {
            [row] => row,
            [] => {
                return Err(incompatible(format!(
                    "integrity function public.{} is missing; refusing to serve",
                    f.name
                )));
            }
            _ => {
                return Err(incompatible(format!(
                    "integrity function public.{} is overloaded; refusing to serve",
                    f.name
                )));
            }
        };
        let prosrc: String = row.try_get("prosrc").map_err(db_error)?;
        let language: String = row.try_get("language").map_err(db_error)?;
        let secdef: bool = row.try_get("prosecdef").map_err(db_error)?;
        let returns: String = row.try_get("returns").map_err(db_error)?;
        let proconfig: Option<Vec<String>> = row.try_get("proconfig").map_err(db_error)?;
        let owner: String = row.try_get("owner").map_err(db_error)?;
        let table_owner: Option<String> = row.try_get("table_owner").map_err(db_error)?;
        let expected_config = f
            .search_path
            .as_ref()
            .map(|sp| vec![format!("search_path={sp}")]);
        let problem = if table_owner.as_deref() != Some(owner.as_str()) {
            // The schema owner owns every integrity function: a function owned by anyone
            // else (the runtime, or a role it can become) could be dropped with CASCADE,
            // taking its trigger along.
            Some(format!(
                "is owned by {owner} instead of the ledger's schema owner {}",
                table_owner.unwrap_or_else(|| "?".into())
            ))
        } else if prosrc != f.body {
            Some("has a different body than this build's migration defines".to_owned())
        } else if language != f.language {
            Some(format!(
                "is written in {language} instead of {}",
                f.language
            ))
        } else if secdef {
            Some("is SECURITY DEFINER".to_owned())
        } else if returns != f.returns {
            Some(format!("returns {returns} instead of {}", f.returns))
        } else if proconfig != expected_config {
            Some(format!(
                "has settings {proconfig:?} instead of {expected_config:?}"
            ))
        } else {
            None
        };
        if let Some(problem) = problem {
            return Err(incompatible(format!(
                "integrity function public.{} {problem}; refusing to serve",
                f.name
            )));
        }
    }
    Ok(())
}

/// Every guard trigger exists on its table, is enabled, calls its function and has exactly
/// the timing, events, column list and constraint properties the migration gave it.
async fn verify_guard_triggers(pool: &PgPool) -> Result<(), LedgerError> {
    for t in GUARD_TRIGGERS {
        let rows = sqlx::query(
            "SELECT pn.nspname AS fn_schema, p.proname AS fn_name, tg.tgenabled::text AS enabled, \
                    tg.tgtype, tg.tgconstraint <> 0 AS is_constraint, tg.tgdeferrable, tg.tginitdeferred, \
                    tg.tgqual IS NULL AS unconditional, \
                    coalesce((SELECT array_agg(a.attname::text ORDER BY a.attnum) \
                              FROM unnest(tg.tgattr::int2[]) AS x(attnum) \
                              JOIN pg_attribute a ON a.attrelid = tg.tgrelid AND a.attnum = x.attnum), \
                             ARRAY[]::text[]) AS update_columns \
             FROM pg_trigger tg \
             JOIN pg_class c ON c.oid = tg.tgrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_proc p ON p.oid = tg.tgfoid \
             JOIN pg_namespace pn ON pn.oid = p.pronamespace \
             WHERE NOT tg.tgisinternal AND n.nspname = 'public' AND c.relname = $1 AND tg.tgname = $2",
        )
        .bind(t.table)
        .bind(t.name)
        .fetch_all(pool)
        .await
        .map_err(db_error)?;
        let row = match rows.as_slice() {
            [row] => row,
            [] => {
                return Err(incompatible(format!(
                    "integrity trigger {} is missing on public.{}; refusing to serve",
                    t.name, t.table
                )));
            }
            _ => {
                return Err(incompatible(format!(
                    "integrity trigger {} is ambiguous on public.{}; refusing to serve",
                    t.name, t.table
                )));
            }
        };
        let fn_schema: String = row.try_get("fn_schema").map_err(db_error)?;
        let fn_name: String = row.try_get("fn_name").map_err(db_error)?;
        let enabled: String = row.try_get("enabled").map_err(db_error)?;
        let tgtype: i16 = row.try_get("tgtype").map_err(db_error)?;
        let is_constraint: bool = row.try_get("is_constraint").map_err(db_error)?;
        let deferrable: bool = row.try_get("tgdeferrable").map_err(db_error)?;
        let initially_deferred: bool = row.try_get("tginitdeferred").map_err(db_error)?;
        let unconditional: bool = row.try_get("unconditional").map_err(db_error)?;
        let mut update_columns: Vec<String> = row.try_get("update_columns").map_err(db_error)?;
        update_columns.sort();
        let mut expected_columns: Vec<String> =
            t.update_columns.iter().map(|c| (*c).to_owned()).collect();
        expected_columns.sort();
        let problem = if enabled != "O" {
            Some(format!("is disabled (tgenabled = {enabled:?})"))
        } else if !unconditional {
            Some("carries a WHEN condition (the migrations define none)".to_owned())
        } else if fn_schema != "public" || fn_name != t.function {
            Some(format!(
                "calls {fn_schema}.{fn_name} instead of public.{}",
                t.function
            ))
        } else if tgtype != t.tgtype() {
            Some(format!(
                "has timing/events {tgtype} instead of {} (row-level {} {}{}{})",
                t.tgtype(),
                if t.before { "BEFORE" } else { "AFTER" },
                if t.insert { "INSERT " } else { "" },
                if t.update { "UPDATE " } else { "" },
                if t.delete { "DELETE" } else { "" }
            ))
        } else if update_columns != expected_columns {
            Some(format!(
                "fires on UPDATE OF {update_columns:?} instead of {expected_columns:?}"
            ))
        } else if is_constraint != t.constraint
            || deferrable != t.deferrable
            || initially_deferred != t.initially_deferred
        {
            Some(format!(
                "constraint/deferrable/initially-deferred is {is_constraint}/{deferrable}/{initially_deferred} \
                 instead of {}/{}/{}",
                t.constraint, t.deferrable, t.initially_deferred
            ))
        } else {
            None
        };
        if let Some(problem) = problem {
            return Err(incompatible(format!(
                "integrity trigger {} on public.{} {problem}; refusing to serve",
                t.name, t.table
            )));
        }
    }
    Ok(())
}

/// A referential or uniqueness constraint the integrity model depends on, matched by shape
/// (table, key columns, referenced schema/table/columns), never by name. Every FOREIGN KEY,
/// PRIMARY KEY and UNIQUE constraint of migrations 0001–0010 is listed: they bind audit rows
/// to real content and real events (ADR-0013) and make the workflow's uniqueness rules facts.
struct ExpectedConstraint {
    table: &'static str,
    kind: char, // 'f' | 'p' | 'u'
    columns: &'static [&'static str],
    references: Option<(&'static str, &'static [&'static str])>,
    /// `UNIQUE NULLS NOT DISTINCT` (0007's idempotency scope): NULL `on_behalf_of` scopes
    /// must collide too.
    nulls_not_distinct: bool,
}

const fn fk(
    table: &'static str,
    columns: &'static [&'static str],
    ref_table: &'static str,
    ref_columns: &'static [&'static str],
) -> ExpectedConstraint {
    ExpectedConstraint {
        table,
        kind: 'f',
        columns,
        references: Some((ref_table, ref_columns)),
        nulls_not_distinct: false,
    }
}
const fn pk(table: &'static str, columns: &'static [&'static str]) -> ExpectedConstraint {
    ExpectedConstraint {
        table,
        kind: 'p',
        columns,
        references: None,
        nulls_not_distinct: false,
    }
}
const fn uq(table: &'static str, columns: &'static [&'static str]) -> ExpectedConstraint {
    ExpectedConstraint {
        table,
        kind: 'u',
        columns,
        references: None,
        nulls_not_distinct: false,
    }
}

/// Every column migrations 0001–0010 declare `NOT NULL`, by table. Composite foreign keys use
/// `MATCH SIMPLE`, so a key column that became nullable would let a row skip its foreign key
/// entirely; the verifier therefore checks nullability structurally like every other
/// control (a column that is additionally `NOT NULL` is harmless and accepted).
pub const EXPECTED_NOT_NULL: &[(&str, &[&str])] = &[
    (
        "commit_index",
        &[
            "graph_id",
            "id",
            "indexed_at",
            "parent_count",
            "patch_id",
            "version",
        ],
    ),
    ("commit_parents", &["commit_id", "parent_id", "position"]),
    (
        "decision_validations",
        &[
            "candidate_commit",
            "decision_id",
            "graph_id",
            "validation_id",
        ],
    ),
    (
        "decisions",
        &[
            "branch",
            "candidate_commit",
            "decided_at",
            "decision",
            "decision_id",
            "graph_id",
            "principal_id",
            "principal_type",
            "tenant_id",
            "validation_ids",
        ],
    ),
    ("graphs", &["created_at", "graph_id", "status", "tenant_id"]),
    (
        "idempotency",
        &[
            "created_at",
            "graph_id",
            "idempotency_id",
            "idempotency_key",
            "operation",
            "principal_id",
            "principal_type",
            "request_digest",
            "result_kind",
            "tenant_id",
        ],
    ),
    ("immutable_objects", &["bytes", "created_at", "id"]),
    (
        "projection_outbox",
        &[
            "attempts",
            "branch",
            "commit_id",
            "created_at",
            "event_kind",
            "graph_id",
            "outbox_id",
            "ref_event_id",
            "ref_version",
        ],
    ),
    (
        "proposals",
        &[
            "branch",
            "candidate_commit",
            "created_at",
            "effective_patch_id",
            "graph_id",
            "principal_id",
            "principal_type",
            "proposal_id",
            "requested_patch_id",
            "tenant_id",
        ],
    ),
    (
        "ref_events",
        &[
            "branch",
            "event_id",
            "graph_id",
            "new_head",
            "new_version",
            "operation",
            "principal_id",
            "principal_type",
            "recorded_at",
            "tenant_id",
        ],
    ),
    (
        "refs",
        &[
            "branch",
            "graph_id",
            "head",
            "protected",
            "updated_at",
            "version",
        ],
    ),
    (
        "semantic_execution_contexts",
        &[
            "base_kb_id",
            "base_kb_revision",
            "candidate_commit",
            "candidate_state_digest",
            "canonical_bytes",
            "context_id",
            "created_at",
            "graph_id",
            "shapes_id",
            "shapes_version",
            "tenant_id",
            "validator_configuration_version",
            "validator_service_id",
            "validator_service_version",
            "virtual_context_count",
        ],
    ),
    (
        "semantic_virtual_contexts",
        &[
            "context_id",
            "dataset_id",
            "hydration_plan_digest",
            "object_refs",
            "position",
            "query_spec_digest",
            "source_version",
        ],
    ),
    (
        "validation_records",
        &[
            "candidate_commit",
            "candidate_state_digest",
            "canonical_bytes",
            "context_id",
            "created_at",
            "graph_id",
            "outcome",
            "principal_id",
            "principal_type",
            "recorded_at",
            "report_digest",
            "tenant_id",
            "validation_id",
            "validator_configuration_version",
            "validator_service_id",
            "validator_service_version",
            "violation_count",
        ],
    ),
    (
        "validation_violations",
        &["code", "message", "position", "severity", "validation_id"],
    ),
];

const EXPECTED_CONSTRAINTS: &[ExpectedConstraint] = &[
    // 0001–0004: content, index, graphs
    pk("refs", &["graph_id", "branch"]),
    pk("immutable_objects", &["id"]),
    pk("commit_index", &["id"]),
    fk("commit_index", &["id"], "immutable_objects", &["id"]),
    fk("commit_index", &["patch_id"], "immutable_objects", &["id"]),
    uq("commit_index", &["graph_id", "id"]),
    pk("commit_parents", &["commit_id", "position"]),
    fk("commit_parents", &["commit_id"], "commit_index", &["id"]),
    fk("commit_parents", &["parent_id"], "commit_index", &["id"]),
    uq("commit_parents", &["commit_id", "parent_id"]),
    pk("graphs", &["graph_id"]),
    fk("refs", &["graph_id"], "graphs", &["graph_id"]),
    fk("commit_index", &["graph_id"], "graphs", &["graph_id"]),
    // 0006: workflow persistence
    fk(
        "refs",
        &["graph_id", "head"],
        "commit_index",
        &["graph_id", "id"],
    ),
    pk("proposals", &["proposal_id"]),
    fk("proposals", &["graph_id"], "graphs", &["graph_id"]),
    fk(
        "proposals",
        &["requested_patch_id"],
        "immutable_objects",
        &["id"],
    ),
    fk(
        "proposals",
        &["effective_patch_id"],
        "immutable_objects",
        &["id"],
    ),
    fk(
        "proposals",
        &["graph_id", "candidate_commit"],
        "commit_index",
        &["graph_id", "id"],
    ),
    uq("proposals", &["candidate_commit"]),
    uq(
        "proposals",
        &["proposal_id", "graph_id", "branch", "candidate_commit"],
    ),
    pk("ref_events", &["event_id"]),
    fk(
        "ref_events",
        &["graph_id", "branch"],
        "refs",
        &["graph_id", "branch"],
    ),
    fk(
        "ref_events",
        &["graph_id", "new_head"],
        "commit_index",
        &["graph_id", "id"],
    ),
    uq("ref_events", &["graph_id", "branch", "new_version"]),
    uq(
        "ref_events",
        &["event_id", "graph_id", "branch", "new_head"],
    ),
    uq(
        "ref_events",
        &["event_id", "graph_id", "branch", "new_version", "new_head"],
    ),
    pk("decisions", &["decision_id"]),
    fk("decisions", &["proposal_id"], "proposals", &["proposal_id"]),
    fk("decisions", &["ref_event_id"], "ref_events", &["event_id"]),
    fk(
        "decisions",
        &["graph_id", "candidate_commit"],
        "commit_index",
        &["graph_id", "id"],
    ),
    fk(
        "decisions",
        &["ref_event_id", "graph_id", "branch", "candidate_commit"],
        "ref_events",
        &["event_id", "graph_id", "branch", "new_head"],
    ),
    fk(
        "decisions",
        &["proposal_id", "graph_id", "branch", "candidate_commit"],
        "proposals",
        &["proposal_id", "graph_id", "branch", "candidate_commit"],
    ),
    pk("projection_outbox", &["outbox_id"]),
    fk(
        "projection_outbox",
        &["ref_event_id"],
        "ref_events",
        &["event_id"],
    ),
    uq("projection_outbox", &["graph_id", "branch", "ref_version"]),
    uq("projection_outbox", &["ref_event_id"]),
    fk(
        "projection_outbox",
        &["graph_id", "commit_id"],
        "commit_index",
        &["graph_id", "id"],
    ),
    fk(
        "projection_outbox",
        &["graph_id", "branch", "ref_version"],
        "ref_events",
        &["graph_id", "branch", "new_version"],
    ),
    fk(
        "projection_outbox",
        &[
            "ref_event_id",
            "graph_id",
            "branch",
            "ref_version",
            "commit_id",
        ],
        "ref_events",
        &["event_id", "graph_id", "branch", "new_version", "new_head"],
    ),
    fk(
        "idempotency",
        &["result_decision_id"],
        "decisions",
        &["decision_id"],
    ),
    fk(
        "idempotency",
        &["result_proposal_id"],
        "proposals",
        &["proposal_id"],
    ),
    // 0007: actor scope and tenant integrity
    pk("idempotency", &["idempotency_id"]),
    ExpectedConstraint {
        table: "idempotency",
        kind: 'u',
        columns: &[
            "tenant_id",
            "graph_id",
            "operation",
            "idempotency_key",
            "principal_id",
            "principal_type",
            "on_behalf_of",
        ],
        references: None,
        nulls_not_distinct: true,
    },
    uq("graphs", &["graph_id", "tenant_id"]),
    fk(
        "proposals",
        &["graph_id", "tenant_id"],
        "graphs",
        &["graph_id", "tenant_id"],
    ),
    fk(
        "ref_events",
        &["graph_id", "tenant_id"],
        "graphs",
        &["graph_id", "tenant_id"],
    ),
    fk(
        "decisions",
        &["graph_id", "tenant_id"],
        "graphs",
        &["graph_id", "tenant_id"],
    ),
    fk(
        "idempotency",
        &["graph_id", "tenant_id"],
        "graphs",
        &["graph_id", "tenant_id"],
    ),
    // 0010: semantic validation persistence (ADR-0018/0019)
    pk("semantic_execution_contexts", &["context_id"]),
    fk(
        "semantic_execution_contexts",
        &["graph_id", "candidate_commit"],
        "commit_index",
        &["graph_id", "id"],
    ),
    fk(
        "semantic_execution_contexts",
        &["graph_id", "tenant_id"],
        "graphs",
        &["graph_id", "tenant_id"],
    ),
    uq(
        "semantic_execution_contexts",
        &[
            "context_id",
            "graph_id",
            "candidate_commit",
            "candidate_state_digest",
            "validator_service_id",
            "validator_service_version",
            "validator_configuration_version",
        ],
    ),
    pk("semantic_virtual_contexts", &["context_id", "position"]),
    fk(
        "semantic_virtual_contexts",
        &["context_id"],
        "semantic_execution_contexts",
        &["context_id"],
    ),
    pk("validation_records", &["validation_id"]),
    fk(
        "validation_records",
        &[
            "context_id",
            "graph_id",
            "candidate_commit",
            "candidate_state_digest",
            "validator_service_id",
            "validator_service_version",
            "validator_configuration_version",
        ],
        "semantic_execution_contexts",
        &[
            "context_id",
            "graph_id",
            "candidate_commit",
            "candidate_state_digest",
            "validator_service_id",
            "validator_service_version",
            "validator_configuration_version",
        ],
    ),
    fk(
        "validation_records",
        &["graph_id", "candidate_commit"],
        "commit_index",
        &["graph_id", "id"],
    ),
    fk(
        "validation_records",
        &["graph_id", "tenant_id"],
        "graphs",
        &["graph_id", "tenant_id"],
    ),
    uq(
        "validation_records",
        &["validation_id", "graph_id", "candidate_commit"],
    ),
    pk("validation_violations", &["validation_id", "position"]),
    fk(
        "validation_violations",
        &["validation_id"],
        "validation_records",
        &["validation_id"],
    ),
    uq(
        "decisions",
        &["decision_id", "graph_id", "candidate_commit"],
    ),
    pk("decision_validations", &["decision_id", "validation_id"]),
    fk(
        "decision_validations",
        &["decision_id", "graph_id", "candidate_commit"],
        "decisions",
        &["decision_id", "graph_id", "candidate_commit"],
    ),
    fk(
        "decision_validations",
        &["validation_id", "graph_id", "candidate_commit"],
        "validation_records",
        &["validation_id", "graph_id", "candidate_commit"],
    ),
    fk(
        "idempotency",
        &["result_validation_id", "graph_id", "result_commit"],
        "validation_records",
        &["validation_id", "graph_id", "candidate_commit"],
    ),
];

/// Unique indexes the workflow relies on (one terminal decision per candidate / proposal /
/// ref event): (index name, table, columns, normalized `WHERE` predicate or "" for none).
/// The predicate is compared by deparse at start-up and by fingerprint on readiness.
const EXPECTED_UNIQUE_INDEXES: &[(&str, &str, &[&str], &str)] = &[
    (
        "decisions_one_per_candidate",
        "decisions",
        &["candidate_commit"],
        "",
    ),
    (
        "decisions_one_per_proposal",
        "decisions",
        &["proposal_id"],
        "(proposal_idISNOTNULL)",
    ),
    (
        "decisions_one_per_ref_event",
        "decisions",
        &["ref_event_id"],
        "(ref_event_idISNOTNULL)",
    ),
];

/// Every named CHECK constraint of 0002–0010 with its normalized `pg_get_constraintdef`
/// (whitespace and `::text` removed; identical on PostgreSQL 15 and 17). Definitions are
/// compared by deparse at start-up and by expression fingerprint on readiness, so a same-named
/// vacuous replacement cannot pass; the Rust layer enforces the same domain rules independently.
const EXPECTED_CHECKS: &[(&str, &str, &str)] = &[
    (
        "commit_index",
        "commit_index_graph_id_format",
        "CHECK((graph_id~'^[A-Za-z0-9._:-]{1,128}$'))",
    ),
    (
        "commit_index",
        "commit_index_parent_count",
        "CHECK(((parent_count>=0)AND(parent_count<=2)))",
    ),
    (
        "commit_index",
        "commit_index_version_known",
        "CHECK((version=ANY(ARRAY[1,2])))",
    ),
    (
        "commit_parents",
        "commit_parents_position",
        "CHECK((\"position\"=ANY(ARRAY[0,1])))",
    ),
    (
        "decisions",
        "decisions_accepted_has_event",
        "CHECK((((decision='accepted')AND(ref_event_idISNOTNULL))OR((decision<>'accepted')AND(ref_event_idISNULL))))",
    ),
    (
        "decisions",
        "decisions_correlation_bounds",
        "CHECK(((correlation_idISNULL)OR((octet_length(correlation_id)>=1)AND(octet_length(correlation_id)<=128))))",
    ),
    (
        "decisions",
        "decisions_kind",
        "CHECK((decision=ANY(ARRAY['accepted','rejected','superseded'])))",
    ),
    (
        "decisions",
        "decisions_principal_type",
        "CHECK((principal_type=ANY(ARRAY['human','agent','service'])))",
    ),
    (
        "decisions",
        "decisions_reason_bounds",
        "CHECK(((reasonISNULL)OR(octet_length(reason)<=4096)))",
    ),
    (
        "graphs",
        "graphs_graph_id_format",
        "CHECK((graph_id~'^[A-Za-z0-9._:-]{1,128}$'))",
    ),
    (
        "graphs",
        "graphs_kb_bounds",
        "CHECK(((knowledge_base_idISNULL)OR((octet_length(knowledge_base_id)>=1)AND(octet_length(knowledge_base_id)<=512))))",
    ),
    (
        "graphs",
        "graphs_purpose_bounds",
        "CHECK(((purposeISNULL)OR(octet_length(purpose)<=512)))",
    ),
    (
        "graphs",
        "graphs_status_known",
        "CHECK((status=ANY(ARRAY['bootstrap','active','importing','archived'])))",
    ),
    (
        "graphs",
        "graphs_tenant_id_bounds",
        "CHECK(((octet_length(tenant_id)>=1)AND(octet_length(tenant_id)<=512)))",
    ),
    (
        "idempotency",
        "idempotency_digest_format",
        "CHECK((request_digest~'^sha256:[0-9a-f]{64}$'))",
    ),
    (
        "idempotency",
        "idempotency_key_bounds",
        "CHECK(((octet_length(idempotency_key)>=1)AND(octet_length(idempotency_key)<=256)))",
    ),
    (
        "idempotency",
        "idempotency_operation",
        "CHECK((operation=ANY(ARRAY['prepare','accept','reject','validate'])))",
    ),
    (
        "idempotency",
        "idempotency_principal_type",
        "CHECK((principal_type=ANY(ARRAY['human','agent','service'])))",
    ),
    (
        "idempotency",
        "idempotency_validation_shape",
        "CHECK((((operation='validate')=(result_kind='validated'))AND((result_kind='validated')=(result_validation_idISNOTNULL))AND((result_kind<>'validated')OR(result_commitISNOTNULL))))",
    ),
    (
        "idempotency",
        "idempotency_result_kind",
        "CHECK((result_kind=ANY(ARRAY['prepared','accepted','rejected','validated'])))",
    ),
    (
        "immutable_objects",
        "immutable_objects_content_addressed",
        "CHECK((id=('sha256:'||encode(sha256(bytes),'hex'))))",
    ),
    (
        "immutable_objects",
        "immutable_objects_id_format",
        "CHECK((id~'^sha256:[0-9a-f]{64}$'))",
    ),
    (
        "projection_outbox",
        "outbox_event_kind",
        "CHECK((event_kind='ref_advanced'))",
    ),
    (
        "proposals",
        "proposals_branch_bounds",
        "CHECK((((octet_length(branch)>=1)AND(octet_length(branch)<=128))AND(branch~'^[A-Za-z0-9._/-]+$')))",
    ),
    (
        "proposals",
        "proposals_correlation_bounds",
        "CHECK(((correlation_idISNULL)OR((octet_length(correlation_id)>=1)AND(octet_length(correlation_id)<=128))))",
    ),
    (
        "proposals",
        "proposals_principal_type",
        "CHECK((principal_type=ANY(ARRAY['human','agent','service'])))",
    ),
    (
        "ref_events",
        "ref_events_branch_bounds",
        "CHECK((((octet_length(branch)>=1)AND(octet_length(branch)<=128))AND(branch~'^[A-Za-z0-9._/-]+$')))",
    ),
    (
        "ref_events",
        "ref_events_correlation_bounds",
        "CHECK(((correlation_idISNULL)OR((octet_length(correlation_id)>=1)AND(octet_length(correlation_id)<=128))))",
    ),
    (
        "ref_events",
        "ref_events_genesis_shape",
        "CHECK((((operation='genesis')AND(old_headISNULL)AND(old_versionISNULL)AND(new_version=1))OR((operation='advance')AND(old_headISNOTNULL)AND(old_versionISNOTNULL)AND(new_version=(old_version+1)))))",
    ),
    (
        "ref_events",
        "ref_events_operation",
        "CHECK((operation=ANY(ARRAY['genesis','advance'])))",
    ),
    (
        "ref_events",
        "ref_events_principal_type",
        "CHECK((principal_type=ANY(ARRAY['human','agent','service'])))",
    ),
    (
        "refs",
        "refs_branch_bounds",
        "CHECK((((octet_length(branch)>=1)AND(octet_length(branch)<=128))AND(branch~'^[A-Za-z0-9._/-]+$')))",
    ),
    ("refs", "refs_version_positive", "CHECK((version>=1))"),
    // 0010 (deparse captured on PostgreSQL 15 and 17)
    (
        "semantic_execution_contexts",
        "sec_content_addressed",
        "CHECK((context_id=('sha256:'||encode(sha256(canonical_bytes),'hex'))))",
    ),
    (
        "semantic_execution_contexts",
        "sec_context_id_format",
        "CHECK((context_id~'^sha256:[0-9a-f]{64}$'))",
    ),
    (
        "semantic_execution_contexts",
        "sec_ontology_shape",
        "CHECK((((ontology_idISNULL)AND(ontology_versionISNULL))OR((ontology_idISNOTNULL)AND(ontology_versionISNOTNULL))))",
    ),
    (
        "semantic_execution_contexts",
        "sec_reasoning_shape",
        "CHECK((((reasoning_profileISNULL)AND(reasoning_implementationISNULL)AND(reasoning_versionISNULL))OR((reasoning_profileISNOTNULL)AND(reasoning_implementationISNOTNULL)AND(reasoning_versionISNOTNULL))))",
    ),
    (
        "semantic_execution_contexts",
        "sec_sources_revision_bounds",
        "CHECK(((sources_revisionISNULL)OR((octet_length(sources_revision)>=1)AND(octet_length(sources_revision)<=512))))",
    ),
    (
        "semantic_execution_contexts",
        "sec_state_digest_format",
        "CHECK((candidate_state_digest~'^sha256:[0-9a-f]{64}$'))",
    ),
    (
        "semantic_execution_contexts",
        "sec_token_bounds",
        "CHECK((((octet_length(base_kb_id)>=1)AND(octet_length(base_kb_id)<=512))AND((octet_length(base_kb_revision)>=1)AND(octet_length(base_kb_revision)<=512))AND((ontology_idISNULL)OR((octet_length(ontology_id)>=1)AND(octet_length(ontology_id)<=512)))AND((ontology_versionISNULL)OR((octet_length(ontology_version)>=1)AND(octet_length(ontology_version)<=512)))AND((octet_length(shapes_id)>=1)AND(octet_length(shapes_id)<=512))AND((octet_length(shapes_version)>=1)AND(octet_length(shapes_version)<=512))AND((reasoning_profileISNULL)OR((octet_length(reasoning_profile)>=1)AND(octet_length(reasoning_profile)<=512)))AND((reasoning_implementationISNULL)OR((octet_length(reasoning_implementation)>=1)AND(octet_length(reasoning_implementation)<=512)))AND((reasoning_versionISNULL)OR((octet_length(reasoning_version)>=1)AND(octet_length(reasoning_version)<=512)))AND((octet_length(validator_service_id)>=1)AND(octet_length(validator_service_id)<=512))AND((octet_length(validator_service_version)>=1)AND(octet_length(validator_service_version)<=512))AND((octet_length(validator_configuration_version)>=1)AND(octet_length(validator_configuration_version)<=512))))",
    ),
    (
        "semantic_execution_contexts",
        "sec_virtual_context_count",
        "CHECK(((virtual_context_count>=0)AND(virtual_context_count<=64)))",
    ),
    (
        "semantic_virtual_contexts",
        "svc_digest_format",
        "CHECK(((query_spec_digest~'^sha256:[0-9a-f]{64}$')AND(hydration_plan_digest~'^sha256:[0-9a-f]{64}$')))",
    ),
    (
        "semantic_virtual_contexts",
        "svc_object_refs_bound",
        "CHECK(((cardinality(object_refs)>=0)AND(cardinality(object_refs)<=64)))",
    ),
    (
        "semantic_virtual_contexts",
        "svc_position",
        "CHECK(((\"position\">=0)AND(\"position\"<=63)))",
    ),
    (
        "semantic_virtual_contexts",
        "svc_token_bounds",
        "CHECK((((octet_length(dataset_id)>=1)AND(octet_length(dataset_id)<=512))AND((octet_length(source_version)>=1)AND(octet_length(source_version)<=512))))",
    ),
    (
        "validation_records",
        "vr_content_addressed",
        "CHECK((validation_id=('sha256:'||encode(sha256(canonical_bytes),'hex'))))",
    ),
    (
        "validation_records",
        "vr_correlation_bounds",
        "CHECK(((correlation_idISNULL)OR((octet_length(correlation_id)>=1)AND(octet_length(correlation_id)<=128))))",
    ),
    (
        "validation_records",
        "vr_outcome_kind",
        "CHECK((outcome=ANY(ARRAY['conforms','violations'])))",
    ),
    (
        "validation_records",
        "vr_outcome_shape",
        "CHECK((((outcome='conforms')AND(violation_count>=0))OR((outcome='violations')AND(violation_count>=1))))",
    ),
    (
        "validation_records",
        "vr_principal_type",
        "CHECK((principal_type=ANY(ARRAY['human','agent','service'])))",
    ),
    (
        "validation_records",
        "vr_report_digest_format",
        "CHECK((report_digest~'^sha256:[0-9a-f]{64}$'))",
    ),
    (
        "validation_records",
        "vr_report_reference_bounds",
        "CHECK(((report_referenceISNULL)OR((octet_length(report_reference)>=1)AND(octet_length(report_reference)<=2048))))",
    ),
    (
        "validation_records",
        "vr_token_bounds",
        "CHECK((((octet_length(validator_service_id)>=1)AND(octet_length(validator_service_id)<=512))AND((octet_length(validator_service_version)>=1)AND(octet_length(validator_service_version)<=512))AND((octet_length(validator_configuration_version)>=1)AND(octet_length(validator_configuration_version)<=512))))",
    ),
    (
        "validation_records",
        "vr_validation_id_format",
        "CHECK((validation_id~'^sha256:[0-9a-f]{64}$'))",
    ),
    (
        "validation_violations",
        "vv_bounds",
        "CHECK((((octet_length(severity)>=1)AND(octet_length(severity)<=64))AND((octet_length(code)>=1)AND(octet_length(code)<=512))AND(octet_length(message)<=1024)))",
    ),
    (
        "validation_violations",
        "vv_position",
        "CHECK(((\"position\">=0)AND(\"position\"<=63)))",
    ),
];

/// The same CHECKs as a logical restore (`pg_dump` → `pg_restore`) recreates them. A CHECK
/// written with `BETWEEN` stores a nested `AND` that deparses as `((a AND b) AND c)`; the
/// restore re-parses that text and PostgreSQL flattens it to `(a AND b AND c)`. The meaning is
/// identical and the flattened form is a fixpoint of further dump/restore cycles, so exactly
/// this alternative is accepted (ADR-0017: logical dumps are a supported recovery path).
const RESTORED_CHECKS: &[(&str, &str, &str)] = &[
    (
        "proposals",
        "proposals_branch_bounds",
        "CHECK(((octet_length(branch)>=1)AND(octet_length(branch)<=128)AND(branch~'^[A-Za-z0-9._/-]+$')))",
    ),
    (
        "ref_events",
        "ref_events_branch_bounds",
        "CHECK(((octet_length(branch)>=1)AND(octet_length(branch)<=128)AND(branch~'^[A-Za-z0-9._/-]+$')))",
    ),
    (
        "refs",
        "refs_branch_bounds",
        "CHECK(((octet_length(branch)>=1)AND(octet_length(branch)<=128)AND(branch~'^[A-Za-z0-9._/-]+$')))",
    ),
    (
        "semantic_execution_contexts",
        "sec_token_bounds",
        "CHECK(((octet_length(base_kb_id)>=1)AND(octet_length(base_kb_id)<=512)AND((octet_length(base_kb_revision)>=1)AND(octet_length(base_kb_revision)<=512))AND((ontology_idISNULL)OR((octet_length(ontology_id)>=1)AND(octet_length(ontology_id)<=512)))AND((ontology_versionISNULL)OR((octet_length(ontology_version)>=1)AND(octet_length(ontology_version)<=512)))AND((octet_length(shapes_id)>=1)AND(octet_length(shapes_id)<=512))AND((octet_length(shapes_version)>=1)AND(octet_length(shapes_version)<=512))AND((reasoning_profileISNULL)OR((octet_length(reasoning_profile)>=1)AND(octet_length(reasoning_profile)<=512)))AND((reasoning_implementationISNULL)OR((octet_length(reasoning_implementation)>=1)AND(octet_length(reasoning_implementation)<=512)))AND((reasoning_versionISNULL)OR((octet_length(reasoning_version)>=1)AND(octet_length(reasoning_version)<=512)))AND((octet_length(validator_service_id)>=1)AND(octet_length(validator_service_id)<=512))AND((octet_length(validator_service_version)>=1)AND(octet_length(validator_service_version)<=512))AND((octet_length(validator_configuration_version)>=1)AND(octet_length(validator_configuration_version)<=512))))",
    ),
    (
        "semantic_virtual_contexts",
        "svc_token_bounds",
        "CHECK(((octet_length(dataset_id)>=1)AND(octet_length(dataset_id)<=512)AND((octet_length(source_version)>=1)AND(octet_length(source_version)<=512))))",
    ),
    (
        "validation_records",
        "vr_token_bounds",
        "CHECK(((octet_length(validator_service_id)>=1)AND(octet_length(validator_service_id)<=512)AND((octet_length(validator_service_version)>=1)AND(octet_length(validator_service_version)<=512))AND((octet_length(validator_configuration_version)>=1)AND(octet_length(validator_configuration_version)<=512))))",
    ),
    (
        "validation_violations",
        "vv_bounds",
        "CHECK(((octet_length(severity)>=1)AND(octet_length(severity)<=64)AND((octet_length(code)>=1)AND(octet_length(code)<=512))AND(octet_length(message)<=1024)))",
    ),
];

/// Normalize a deparsed definition for comparison: whitespace and `::text` casts are removed
/// *outside* quoted text only. String literals (`'…'`, where `''` is an escaped quote) and
/// quoted identifiers (`"…"`, where `""` is an escaped quote) are kept byte for byte, so
/// `'^[a-z]+$'` never normalizes like `'^[a-z ]+$'` and `"position"` never like
/// `"position::text"`.
fn normalize_constraint_def(def: &str) -> String {
    let mut out = String::with_capacity(def.len());
    let mut outside = String::new();
    let mut quote: Option<char> = None;
    let flush = |outside: &mut String, out: &mut String| {
        out.push_str(&outside.replace("::text", ""));
        outside.clear();
    };
    for c in def.chars() {
        match quote {
            Some(q) => {
                out.push(c);
                if c == q {
                    quote = None;
                }
            }
            None if c == '\'' || c == '"' => {
                flush(&mut outside, &mut out);
                out.push(c);
                quote = Some(c);
            }
            None => {
                if !c.is_whitespace() {
                    outside.push(c);
                }
            }
        }
    }
    flush(&mut outside, &mut out);
    out
}

/// Key of a definition fingerprint in [`SchemaReport::fingerprints`].
fn check_key(table: &str, name: &str) -> String {
    format!("check:{table}.{name}")
}
fn index_key(name: &str) -> String {
    format!("index:{name}")
}

/// Refuse a database in which any column the migrations declare `NOT NULL` accepts NULL
/// (catalog read only; runs at start-up and on readiness). A `NOT VALID` not-null constraint
/// (PostgreSQL 18+, `contype = 'n'`) does not count: existing rows may still be NULL.
async fn verify_not_null(pool: &PgPool) -> Result<(), LedgerError> {
    let rows = sqlx::query(
        "SELECT c.relname::text AS table_name, a.attname::text AS column_name \
         FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'public' AND c.relkind = 'r' AND a.attnum > 0 \
           AND NOT a.attisdropped AND a.attnotnull \
           AND NOT EXISTS (SELECT 1 FROM pg_constraint k WHERE k.conrelid = a.attrelid \
                           AND k.contype = 'n' AND NOT k.convalidated AND a.attnum = ANY (k.conkey))",
    )
    .fetch_all(pool)
    .await
    .map_err(db_error)?;
    let mut not_null = std::collections::BTreeSet::new();
    for row in &rows {
        let table: String = row.try_get("table_name").map_err(db_error)?;
        let column: String = row.try_get("column_name").map_err(db_error)?;
        not_null.insert((table, column));
    }
    for (table, columns) in EXPECTED_NOT_NULL {
        for column in *columns {
            if !not_null.contains(&((*table).to_owned(), (*column).to_owned())) {
                return Err(incompatible(format!(
                    "column {table}.{column} must be NOT NULL (or is missing); refusing to serve"
                )));
            }
        }
    }
    Ok(())
}

/// Every expected FOREIGN KEY / PRIMARY KEY / UNIQUE constraint exists (at least one validated,
/// non-deferrable match of the expected shape, referenced tables in `public`, NULLS NOT
/// DISTINCT where required); every named CHECK exists and is validated; every unique index
/// exists with the expected columns and partiality. Catalog-only (attnums resolved to names,
/// no deparse), so it is lock-free and runs on readiness. Returns the expression fingerprints
/// of the CHECKs and partial-index predicates for readiness comparison.
async fn verify_constraints_and_indexes(
    pool: &PgPool,
) -> Result<BTreeMap<String, String>, LedgerError> {
    let rows = sqlx::query(
        "SELECT c.relname::text AS table_name, con.conname::text AS name, con.contype::text AS kind, \
                con.convalidated, con.condeferrable, \
                (SELECT array_agg(a.attname::text ORDER BY k.ord) \
                   FROM unnest(con.conkey) WITH ORDINALITY AS k(attnum, ord) \
                   JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.attnum) AS columns, \
                rn.nspname::text AS ref_schema, rc.relname::text AS ref_table, \
                (SELECT array_agg(a.attname::text ORDER BY k.ord) \
                   FROM unnest(con.confkey) WITH ORDINALITY AS k(attnum, ord) \
                   JOIN pg_attribute a ON a.attrelid = con.confrelid AND a.attnum = k.attnum) AS ref_columns, \
                coalesce(i.indnullsnotdistinct, false) AS nulls_not_distinct, \
                CASE WHEN con.contype = 'c' \
                     THEN md5(regexp_replace(con.conbin::text, ':location -?[0-9]+', '', 'g')) END AS fingerprint \
         FROM pg_constraint con \
         JOIN pg_class c ON c.oid = con.conrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN pg_class rc ON rc.oid = con.confrelid \
         LEFT JOIN pg_namespace rn ON rn.oid = rc.relnamespace \
         LEFT JOIN pg_index i ON i.indexrelid = con.conindid AND con.contype IN ('u', 'p') \
         WHERE n.nspname = 'public'",
    )
    .fetch_all(pool)
    .await
    .map_err(db_error)?;
    struct Found {
        table: String,
        name: String,
        kind: String,
        validated: bool,
        deferrable: bool,
        columns: Vec<String>,
        ref_schema: Option<String>,
        ref_table: Option<String>,
        ref_columns: Vec<String>,
        nulls_not_distinct: bool,
        fingerprint: Option<String>,
    }
    let mut found = Vec::with_capacity(rows.len());
    for row in &rows {
        found.push(Found {
            table: row.try_get("table_name").map_err(db_error)?,
            name: row.try_get("name").map_err(db_error)?,
            kind: row.try_get("kind").map_err(db_error)?,
            validated: row.try_get("convalidated").map_err(db_error)?,
            deferrable: row.try_get("condeferrable").map_err(db_error)?,
            columns: row
                .try_get::<Option<Vec<String>>, _>("columns")
                .map_err(db_error)?
                .unwrap_or_default(),
            ref_schema: row.try_get("ref_schema").map_err(db_error)?,
            ref_table: row.try_get("ref_table").map_err(db_error)?,
            ref_columns: row
                .try_get::<Option<Vec<String>>, _>("ref_columns")
                .map_err(db_error)?
                .unwrap_or_default(),
            nulls_not_distinct: row.try_get("nulls_not_distinct").map_err(db_error)?,
            fingerprint: row.try_get("fingerprint").map_err(db_error)?,
        });
    }
    for e in EXPECTED_CONSTRAINTS {
        let shape = match e.references {
            Some((rt, rcols)) => format!(
                "{} FOREIGN KEY {:?} -> public.{rt} {rcols:?}",
                e.table, e.columns
            ),
            None => format!(
                "{} {}{} {:?}",
                e.table,
                if e.kind == 'p' {
                    "PRIMARY KEY"
                } else {
                    "UNIQUE"
                },
                if e.nulls_not_distinct {
                    " NULLS NOT DISTINCT"
                } else {
                    ""
                },
                e.columns
            ),
        };
        let matches: Vec<&Found> = found
            .iter()
            .filter(|f| {
                f.table == e.table
                    && f.kind == e.kind.to_string()
                    && f.columns == e.columns
                    && match e.references {
                        Some((rt, rcols)) => {
                            f.ref_schema.as_deref() == Some("public")
                                && f.ref_table.as_deref() == Some(rt)
                                && f.ref_columns == rcols
                        }
                        None => true,
                    }
                    && (!e.nulls_not_distinct || f.nulls_not_distinct)
            })
            .collect();
        if matches.is_empty() {
            return Err(incompatible(format!(
                "constraint {shape} is missing; refusing to serve"
            )));
        }
        // Among same-shaped constraints at least one must be enforced now and immediately:
        // validated and non-deferrable (two NOT VALID copies are not a constraint).
        if !matches.iter().any(|f| f.validated && !f.deferrable) {
            return Err(incompatible(format!(
                "constraint {shape} exists only as NOT VALID or deferrable copies ({}); refusing to serve",
                matches
                    .iter()
                    .map(|f| f.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    let mut fingerprints = BTreeMap::new();
    for (table, name, _) in EXPECTED_CHECKS {
        match found.iter().find(|f| f.table == *table && f.name == *name) {
            Some(f) if f.kind == "c" && f.validated => {
                fingerprints.insert(
                    check_key(table, name),
                    f.fingerprint.clone().unwrap_or_default(),
                );
            }
            Some(f) if f.kind != "c" => {
                return Err(incompatible(format!(
                    "constraint {name} on public.{table} is not a CHECK constraint; refusing to serve"
                )));
            }
            Some(_) => {
                return Err(incompatible(format!(
                    "constraint {name} on public.{table} is NOT VALID; refusing to serve"
                )));
            }
            None => {
                return Err(incompatible(format!(
                    "CHECK constraint {name} is missing on public.{table}; refusing to serve"
                )));
            }
        }
    }
    let indexes = sqlx::query(
        "SELECT ic.relname::text AS name, t.relname::text AS table_name, i.indisunique, \
                i.indpred IS NOT NULL AS partial, \
                (SELECT array_agg(a.attname::text ORDER BY k.ord) \
                   FROM unnest(i.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord) \
                   JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum) AS columns, \
                CASE WHEN i.indpred IS NOT NULL \
                     THEN md5(regexp_replace(i.indpred::text, ':location -?[0-9]+', '', 'g')) END AS fingerprint \
         FROM pg_index i JOIN pg_class t ON t.oid = i.indrelid JOIN pg_class ic ON ic.oid = i.indexrelid \
         JOIN pg_namespace n ON n.oid = t.relnamespace \
         WHERE n.nspname = 'public' AND i.indisunique AND i.indisvalid",
    )
    .fetch_all(pool)
    .await
    .map_err(db_error)?;
    for (name, table, columns, predicate) in EXPECTED_UNIQUE_INDEXES {
        let row = indexes.iter().find(|row| {
            row.try_get::<String, _>("name").is_ok_and(|n| n == *name)
                && row
                    .try_get::<String, _>("table_name")
                    .is_ok_and(|t| t == *table)
                && row
                    .try_get::<Option<Vec<String>>, _>("columns")
                    .is_ok_and(|c| c.unwrap_or_default() == *columns)
                && row
                    .try_get::<bool, _>("partial")
                    .is_ok_and(|p| p != predicate.is_empty())
        });
        match row {
            Some(row) => {
                if !predicate.is_empty() {
                    let fp: Option<String> = row.try_get("fingerprint").map_err(db_error)?;
                    fingerprints.insert(index_key(name), fp.unwrap_or_default());
                }
            }
            None => {
                return Err(incompatible(format!(
                    "unique index {name} on {table} {columns:?}{} is missing or invalid; refusing to serve",
                    if predicate.is_empty() {
                        ""
                    } else {
                        " (partial)"
                    }
                )));
            }
        }
    }
    Ok(fingerprints)
}

/// Start-up verification of every CHECK and partial-index *definition* (deparsed and
/// normalized against the values the migrations produce on PostgreSQL 15 and 17) plus the
/// rolled-back semantic probe of the content-address CHECK. Deparsing opens relations with
/// `ACCESS SHARE` and the probe takes a row lock, so this runs at start-up only; readiness
/// compares the expression fingerprints `verify` returns with the ones validated here.
pub async fn verify_definitions_at_startup(pool: &PgPool) -> Result<(), LedgerError> {
    for (table, name, expected) in EXPECTED_CHECKS {
        let def: Option<String> = sqlx::query_scalar(
            "SELECT pg_get_constraintdef(con.oid) FROM pg_constraint con \
             JOIN pg_class c ON c.oid = con.conrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = 'public' AND c.relname = $1 AND con.conname = $2",
        )
        .bind(table)
        .bind(name)
        .fetch_optional(pool)
        .await
        .map_err(db_error)?;
        let def = def.ok_or_else(|| {
            incompatible(format!(
                "CHECK constraint {name} is missing on public.{table}; refusing to serve"
            ))
        })?;
        let normalized = normalize_constraint_def(&def);
        let restored = RESTORED_CHECKS
            .iter()
            .any(|(t, n, d)| t == table && n == name && normalized == *d);
        if normalized != *expected && !restored {
            return Err(incompatible(format!(
                "constraint {name} on public.{table} has definition {def:?} instead of the one \
                 migration-defined; refusing to serve"
            )));
        }
    }
    for (name, table, _, predicate) in EXPECTED_UNIQUE_INDEXES {
        if predicate.is_empty() {
            continue;
        }
        let def: Option<String> = sqlx::query_scalar(
            "SELECT pg_get_expr(i.indpred, i.indrelid) FROM pg_index i \
             JOIN pg_class ic ON ic.oid = i.indexrelid WHERE ic.relname = $1 \
               AND ic.relnamespace = 'public'::regnamespace",
        )
        .bind(name)
        .fetch_optional(pool)
        .await
        .map_err(db_error)?;
        let def = def.ok_or_else(|| {
            incompatible(format!(
                "unique index {name} on {table} has no WHERE predicate; refusing to serve"
            ))
        })?;
        if normalize_constraint_def(&def) != *predicate {
            return Err(incompatible(format!(
                "unique index {name} on {table} has predicate {def:?} instead of the migration's; \
                 refusing to serve"
            )));
        }
    }
    probe_content_address_check(pool).await?;
    probe_validation_content_address_checks(pool).await
}

/// Migration 0009's content-address CHECK on `immutable_objects`: the database-side
/// guarantee that no bytes are stored under a false content id. Rust re-verifies every
/// object's digest on read; this is defence in depth. Its definition is covered by
/// [`verify_definitions_at_startup`] like every other CHECK; this probe additionally makes
/// the database refuse a mislabelled object inside a rolled-back transaction, so a
/// constraint that deparses acceptably but does not enforce the condition cannot pass.
const CONTENT_ADDRESS_CHECK: &str = "immutable_objects_content_addressed";

pub async fn probe_content_address_check(pool: &PgPool) -> Result<(), LedgerError> {
    let mut tx = pool.begin().await.map_err(db_error)?;
    let probe = sqlx::query("INSERT INTO immutable_objects (id, bytes) VALUES ($1, $2)")
        .bind(format!("sha256:{}", "0".repeat(64)))
        .bind(b"not the preimage".as_slice())
        .execute(&mut *tx)
        .await;
    tx.rollback().await.map_err(db_error)?;
    match probe {
        Err(sqlx::Error::Database(d))
            if d.code().as_deref() == Some("23514")
                && d.constraint() == Some(CONTENT_ADDRESS_CHECK) =>
        {
            Ok(())
        }
        Err(sqlx::Error::Database(d)) if d.code().as_deref() == Some("42501") => {
            Err(LedgerError::RuntimeIdentity(
                "the connected role cannot probe immutable_objects (no INSERT (id, bytes) \
                 grant); run `ledger-admin migrate --runtime-role <role>` (ADR-0016)"
                    .into(),
            ))
        }
        Err(e) => Err(db_error(e)),
        Ok(_) => Err(incompatible(format!(
            "constraint {CONTENT_ADDRESS_CHECK} accepted a mislabelled object; the content-address \
             guard is not enforced; refusing to serve"
        ))),
    }
}

/// Migration 0010's content-address CHECKs on `semantic_execution_contexts` and
/// `validation_records`: the database-side guarantee that no context or record is stored
/// under a false identity (ADR-0018). Probed like the object CHECK: a mislabelled row inside
/// a rolled-back transaction must be refused with 23514. CHECK constraints are evaluated
/// before the row exists, so the probe fails on the CHECK before any foreign key could.
pub async fn probe_validation_content_address_checks(pool: &PgPool) -> Result<(), LedgerError> {
    let zero = format!("sha256:{}", "0".repeat(64));
    let probes: [(&str, &str, String); 2] = [
        (
            "sec_content_addressed",
            "INSERT INTO semantic_execution_contexts (context_id, graph_id, tenant_id, candidate_commit, \
             candidate_state_digest, base_kb_id, base_kb_revision, shapes_id, shapes_version, \
             reasoning_profile, reasoning_implementation, reasoning_version, validator_service_id, \
             validator_service_version, validator_configuration_version, virtual_context_count, canonical_bytes) \
             VALUES ($1, 'probe', 'probe', $1, $1, 'p', 'p', 'p', 'p', 'p', 'p', 'p', 'p', 'p', 'p', 0, $2)",
            zero.clone(),
        ),
        (
            "vr_content_addressed",
            "INSERT INTO validation_records (validation_id, graph_id, tenant_id, candidate_commit, \
             candidate_state_digest, context_id, validator_service_id, validator_service_version, \
             validator_configuration_version, outcome, violation_count, report_digest, recorded_at, \
             principal_id, principal_type, canonical_bytes) \
             VALUES ($1, 'probe', 'probe', $1, $1, $1, 'p', 'p', 'p', 'conforms', 0, $1, now(), 'p', 'service', $2)",
            zero,
        ),
    ];
    for (name, sql, id) in probes {
        let mut tx = pool.begin().await.map_err(db_error)?;
        let probe = sqlx::query(sql)
            .bind(&id)
            .bind(b"not the preimage".as_slice())
            .execute(&mut *tx)
            .await;
        tx.rollback().await.map_err(db_error)?;
        match probe {
            Err(sqlx::Error::Database(d))
                if d.code().as_deref() == Some("23514") && d.constraint() == Some(name) => {}
            Err(sqlx::Error::Database(d)) if d.code().as_deref() == Some("42501") => {
                return Err(LedgerError::RuntimeIdentity(format!(
                    "the connected role cannot probe {name} (no INSERT grant on the validation \
                     tables); run `ledger-admin migrate --runtime-role <role>` (ADR-0016)"
                )));
            }
            Err(e) => return Err(db_error(e)),
            Ok(_) => {
                return Err(incompatible(format!(
                    "constraint {name} accepted a mislabelled row; the content-address guard is not \
                     enforced; refusing to serve"
                )));
            }
        }
    }
    Ok(())
}

/// The runtime identity's exact table privileges (migration 0008 / `ledger_grant_runtime`):
/// whole-table SELECT, column-level INSERT and UPDATE, nothing else. Column sets are the
/// columns the store's statements name; a column added later is not writable until it is
/// granted here and in 0008's successor.
struct TablePrivileges {
    table: &'static str,
    insert_columns: &'static [&'static str],
    update_columns: &'static [&'static str],
}

const RUNTIME_TABLE_MODEL: &[TablePrivileges] = &[
    TablePrivileges {
        table: "graphs",
        insert_columns: &[],
        update_columns: &[],
    },
    TablePrivileges {
        table: "refs",
        insert_columns: &["graph_id", "branch", "head", "version"],
        update_columns: &["head", "version", "updated_at"],
    },
    TablePrivileges {
        table: "immutable_objects",
        insert_columns: &["id", "bytes"],
        update_columns: &[],
    },
    TablePrivileges {
        table: "commit_index",
        insert_columns: &["id", "graph_id", "version", "patch_id", "parent_count"],
        update_columns: &[],
    },
    TablePrivileges {
        table: "commit_parents",
        insert_columns: &["commit_id", "position", "parent_id"],
        update_columns: &[],
    },
    TablePrivileges {
        table: "proposals",
        insert_columns: &[
            "graph_id",
            "branch",
            "tenant_id",
            "principal_id",
            "principal_type",
            "on_behalf_of",
            "expected_head",
            "requested_patch_id",
            "effective_patch_id",
            "candidate_commit",
            "correlation_id",
        ],
        update_columns: &[],
    },
    TablePrivileges {
        table: "ref_events",
        insert_columns: &[
            "graph_id",
            "branch",
            "old_head",
            "new_head",
            "old_version",
            "new_version",
            "operation",
            "tenant_id",
            "principal_id",
            "principal_type",
            "on_behalf_of",
            "reason",
            "correlation_id",
        ],
        update_columns: &[],
    },
    TablePrivileges {
        table: "decisions",
        insert_columns: &[
            "proposal_id",
            "graph_id",
            "branch",
            "candidate_commit",
            "decision",
            "tenant_id",
            "principal_id",
            "principal_type",
            "on_behalf_of",
            "reason",
            "validation_ids",
            "ref_event_id",
            "correlation_id",
        ],
        update_columns: &[],
    },
    TablePrivileges {
        table: "projection_outbox",
        insert_columns: &[
            "graph_id",
            "branch",
            "commit_id",
            "ref_version",
            "event_kind",
            "ref_event_id",
        ],
        update_columns: &[],
    },
    TablePrivileges {
        table: "idempotency",
        insert_columns: &[
            "tenant_id",
            "principal_id",
            "principal_type",
            "on_behalf_of",
            "graph_id",
            "operation",
            "idempotency_key",
            "request_digest",
            "result_kind",
            "result_commit",
            "result_ref_version",
            "result_decision_id",
            "result_proposal_id",
            "result_validation_id",
        ],
        update_columns: &[],
    },
    TablePrivileges {
        table: "_sqlx_migrations",
        insert_columns: &[],
        update_columns: &[],
    },
    // 0010 (ADR-0016 amendment): validation persistence is insert-only for the runtime.
    TablePrivileges {
        table: "semantic_execution_contexts",
        insert_columns: &[
            "context_id",
            "graph_id",
            "tenant_id",
            "candidate_commit",
            "candidate_state_digest",
            "base_kb_id",
            "base_kb_revision",
            "ontology_id",
            "ontology_version",
            "shapes_id",
            "shapes_version",
            "reasoning_profile",
            "reasoning_implementation",
            "reasoning_version",
            "sources_revision",
            "validator_service_id",
            "validator_service_version",
            "validator_configuration_version",
            "virtual_context_count",
            "canonical_bytes",
        ],
        update_columns: &[],
    },
    TablePrivileges {
        table: "semantic_virtual_contexts",
        insert_columns: &[
            "context_id",
            "position",
            "dataset_id",
            "source_version",
            "object_refs",
            "query_spec_digest",
            "hydration_plan_digest",
        ],
        update_columns: &[],
    },
    TablePrivileges {
        table: "validation_records",
        insert_columns: &[
            "validation_id",
            "graph_id",
            "tenant_id",
            "candidate_commit",
            "candidate_state_digest",
            "context_id",
            "validator_service_id",
            "validator_service_version",
            "validator_configuration_version",
            "outcome",
            "violation_count",
            "report_digest",
            "report_reference",
            "recorded_at",
            "principal_id",
            "principal_type",
            "on_behalf_of",
            "correlation_id",
            "canonical_bytes",
        ],
        update_columns: &[],
    },
    TablePrivileges {
        table: "validation_violations",
        insert_columns: &["validation_id", "position", "severity", "code", "message"],
        update_columns: &[],
    },
    TablePrivileges {
        table: "decision_validations",
        insert_columns: &[
            "decision_id",
            "validation_id",
            "graph_id",
            "candidate_commit",
        ],
        update_columns: &[],
    },
];

/// Sequences the workflow inserts draw from: `USAGE` only (0008), nothing on any other one.
const RUNTIME_SEQUENCES: &[&str] = &[
    "proposals_proposal_id_seq",
    "ref_events_event_id_seq",
    "decisions_decision_id_seq",
    "projection_outbox_outbox_id_seq",
    "idempotency_idempotency_id_seq",
];

fn identity(message: String) -> LedgerError {
    LedgerError::RuntimeIdentity(message)
}

/// Verify that the connected identity is a least-privilege runtime identity (ADR-0016):
/// not a superuser, not the owner of any ledger table, without CREATE on the schema, and
/// holding exactly the privilege model of migration 0008 — whole-table SELECT, the listed
/// INSERT/UPDATE columns and no others (checked per column, so neither a missing column nor
/// a table-level grant passes), no DELETE/TRUNCATE/TRIGGER/REFERENCES anywhere, no write to
/// the migration ledger, `USAGE` on exactly the audit sequences and nothing on any other
/// sequence. The server refuses to start otherwise, so a deployment that kept the owner URL
/// or a drifted role cannot silently serve.
pub async fn verify_runtime_identity(pool: &PgPool) -> Result<(), LedgerError> {
    let row = sqlx::query(
        "SELECT current_user::text AS who, \
                (SELECT rolsuper FROM pg_roles WHERE rolname = current_user) AS super, \
                has_schema_privilege(current_user, 'public', 'CREATE') AS can_create, \
                (SELECT count(*) FROM pg_tables WHERE schemaname = 'public' \
                    AND tableowner = current_user) AS owned",
    )
    .fetch_one(pool)
    .await
    .map_err(db_error)?;
    let who: String = row.try_get("who").map_err(db_error)?;
    let is_super: Option<bool> = row.try_get("super").map_err(db_error)?;
    let can_create: bool = row.try_get("can_create").map_err(db_error)?;
    let owned: i64 = row.try_get("owned").map_err(db_error)?;
    if is_super.unwrap_or(false) {
        return Err(identity(format!(
            "role {who} is a superuser; the server must run as the least-privilege runtime \
             identity (ADR-0016)"
        )));
    }
    if owned > 0 {
        return Err(identity(format!(
            "role {who} owns {owned} ledger table(s); the server must not run as the schema \
             owner (ADR-0016)"
        )));
    }
    if can_create {
        return Err(identity(format!(
            "role {who} holds CREATE on schema public; revoke it (ADR-0016)"
        )));
    }
    verify_role_attributes_and_memberships(pool, &who).await?;
    for model in RUNTIME_TABLE_MODEL {
        verify_table_privileges(pool, &who, model).await?;
    }
    verify_sequence_privileges(pool, &who).await
}

/// The runtime role must not be able to *become* anything more privileged or to silence the
/// guards: no role attributes beyond LOGIN, `session_replication_role = origin` with no SET
/// or ALTER SYSTEM privilege on it, and no membership — inherited or merely settable (`SET
/// ROLE`), direct or transitive — in a superuser, a table or function owner, a CREATE
/// holder, a role allowed to set `session_replication_role`, a role with
/// CREATEROLE/CREATEDB/REPLICATION/BYPASSRLS, or any predefined `pg_*` role.
async fn verify_role_attributes_and_memberships(
    pool: &PgPool,
    who: &str,
) -> Result<(), LedgerError> {
    let me = sqlx::query(
        "SELECT rolcreaterole, rolcreatedb, rolreplication, rolbypassrls \
         FROM pg_roles WHERE rolname = current_user",
    )
    .fetch_one(pool)
    .await
    .map_err(db_error)?;
    for (col, attr) in [
        ("rolcreaterole", "CREATEROLE"),
        ("rolcreatedb", "CREATEDB"),
        ("rolreplication", "REPLICATION"),
        ("rolbypassrls", "BYPASSRLS"),
    ] {
        if me.try_get::<bool, _>(col).map_err(db_error)? {
            return Err(identity(format!(
                "role {who} has the {attr} attribute; the runtime identity must be a plain LOGIN \
                 role (ADR-0016)"
            )));
        }
    }
    // `session_replication_role = replica` silences every ordinary trigger (the guards are
    // `tgenabled = 'O'`); the runtime must run with `origin` and must not be able to change
    // it (PostgreSQL 15+ can grant SET on a SUSET parameter) or to `ALTER SYSTEM`.
    let params = sqlx::query(
        "SELECT current_setting('session_replication_role') AS srr, \
                has_parameter_privilege(current_user, 'session_replication_role', 'SET') AS can_set_srr, \
                has_parameter_privilege(current_user, 'session_replication_role', 'ALTER SYSTEM') AS can_alter_srr",
    )
    .fetch_one(pool)
    .await
    .map_err(db_error)?;
    let srr: String = params.try_get("srr").map_err(db_error)?;
    if srr != "origin" {
        return Err(identity(format!(
            "role {who} runs with session_replication_role = {srr}; integrity triggers would not \
             fire (ADR-0016)"
        )));
    }
    if params.try_get::<bool, _>("can_set_srr").map_err(db_error)?
        || params
            .try_get::<bool, _>("can_alter_srr")
            .map_err(db_error)?
    {
        return Err(identity(format!(
            "role {who} may SET or ALTER SYSTEM session_replication_role; it could silence every \
             integrity trigger (ADR-0016)"
        )));
    }
    // Every role the runtime can become (inherited or `SET ROLE`), transitively. PostgreSQL
    // 16 records per-membership INHERIT/SET options; on 15 every membership is settable and
    // inheritance follows the member's NOINHERIT attribute, so treat both as true.
    let version: i32 = sqlx::query_scalar("SELECT current_setting('server_version_num')::int")
        .fetch_one(pool)
        .await
        .map_err(db_error)?;
    let options = if version >= 160_000 {
        "am.inherit_option, am.set_option"
    } else {
        "true AS inherit_option, true AS set_option"
    };
    let rows = sqlx::query(&format!(
        "WITH RECURSIVE m AS ( \
             SELECT am.roleid, {options} \
             FROM pg_auth_members am WHERE am.member = (SELECT oid FROM pg_roles WHERE rolname = current_user) \
           UNION \
             SELECT am.roleid, {options} \
             FROM pg_auth_members am JOIN m ON am.member = m.roleid) \
         SELECT r.rolname::text AS name, r.rolsuper, r.rolcreaterole, r.rolcreatedb, r.rolreplication, \
                r.rolbypassrls, m.inherit_option, m.set_option, \
                (SELECT count(*) FROM pg_tables t WHERE t.schemaname = 'public' AND t.tableowner = r.rolname) AS owned, \
                (SELECT count(*) FROM pg_proc p WHERE p.pronamespace = 'public'::regnamespace AND p.proowner = r.oid) AS owned_functions, \
                has_schema_privilege(r.oid, 'public', 'CREATE') AS can_create, \
                {srr} AS can_set_srr \
         FROM m JOIN pg_roles r ON r.oid = m.roleid",
        srr = if version >= 150_000 {
            "(has_parameter_privilege(r.oid, 'session_replication_role', 'SET') \
              OR has_parameter_privilege(r.oid, 'session_replication_role', 'ALTER SYSTEM'))"
        } else {
            "false"
        }
    ))
    .fetch_all(pool)
    .await
    .map_err(db_error)?;
    for row in &rows {
        let name: String = row.try_get("name").map_err(db_error)?;
        let flags = [
            (
                row.try_get::<bool, _>("rolsuper").map_err(db_error)?,
                "a superuser",
            ),
            (
                row.try_get::<bool, _>("rolcreaterole").map_err(db_error)?,
                "CREATEROLE",
            ),
            (
                row.try_get::<bool, _>("rolcreatedb").map_err(db_error)?,
                "CREATEDB",
            ),
            (
                row.try_get::<bool, _>("rolreplication").map_err(db_error)?,
                "REPLICATION",
            ),
            (
                row.try_get::<bool, _>("rolbypassrls").map_err(db_error)?,
                "BYPASSRLS",
            ),
            (
                row.try_get::<i64, _>("owned").map_err(db_error)? > 0,
                "an owner of ledger tables",
            ),
            (
                row.try_get::<i64, _>("owned_functions").map_err(db_error)? > 0,
                "an owner of ledger functions",
            ),
            (
                row.try_get::<bool, _>("can_create").map_err(db_error)?,
                "a CREATE holder on schema public",
            ),
            (
                row.try_get::<bool, _>("can_set_srr").map_err(db_error)?,
                "allowed to SET or ALTER SYSTEM session_replication_role",
            ),
            (name.starts_with("pg_"), "a predefined pg_* role"),
        ];
        let inherit: bool = row.try_get("inherit_option").map_err(db_error)?;
        let set: bool = row.try_get("set_option").map_err(db_error)?;
        if let Some((_, what)) = flags.iter().find(|(bad, _)| *bad) {
            return Err(identity(format!(
                "role {who} is a member of {name} ({what}; inherit={inherit}, set={set}); the \
                 runtime identity must not be able to assume more privilege (ADR-0016)"
            )));
        }
        // Object privileges an assumable role holds must stay within the runtime model too:
        // a NOINHERIT/SET-only parent's grants are invisible to the current_user checks but
        // one `SET ROLE` away.
        for model in RUNTIME_TABLE_MODEL {
            verify_table_privileges_for(pool, &name, who, model, Exactness::Subset).await?;
        }
        verify_sequence_privileges_for(pool, &name, who, Exactness::Subset).await?;
        verify_no_grant_function_execute(pool, &name, who).await?;
    }
    verify_no_grant_function_execute(pool, who, who).await
}

/// Whether a role must hold the model exactly (the runtime itself) or at most the model
/// (roles the runtime can become).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Exactness {
    Exact,
    Subset,
}

/// `ledger_grant_runtime` hands out privileges; neither the runtime nor any role it can
/// become may execute it (the function refuses non-owners, but defence in depth).
async fn verify_no_grant_function_execute(
    pool: &PgPool,
    subject: &str,
    who: &str,
) -> Result<(), LedgerError> {
    let can: bool = sqlx::query_scalar(
        "SELECT has_function_privilege($1, 'public.ledger_grant_runtime(text)', 'EXECUTE')",
    )
    .bind(subject)
    .fetch_one(pool)
    .await
    .map_err(db_error)?;
    if can {
        return Err(identity(format!(
            "role {subject} (reachable by {who}) may execute ledger_grant_runtime; the runtime \
             identity must not (ADR-0016)"
        )));
    }
    Ok(())
}

async fn verify_table_privileges(
    pool: &PgPool,
    who: &str,
    model: &TablePrivileges,
) -> Result<(), LedgerError> {
    verify_table_privileges_for(pool, who, who, model, Exactness::Exact).await
}

/// `subject` is the role whose privileges are examined (the runtime, or a role it can
/// become); `who` names the runtime in messages.
async fn verify_table_privileges_for(
    pool: &PgPool,
    subject: &str,
    who: &str,
    model: &TablePrivileges,
    exactness: Exactness,
) -> Result<(), LedgerError> {
    let qualified = format!("public.{}", model.table);
    let table = sqlx::query(
        "SELECT to_regclass($1) IS NOT NULL AS present, \
                has_table_privilege($2, $1, 'SELECT') AS sel, \
                has_table_privilege($2, $1, 'INSERT') AS ins, \
                has_table_privilege($2, $1, 'UPDATE') AS upd, \
                has_table_privilege($2, $1, 'DELETE') AS del, \
                has_table_privilege($2, $1, 'TRUNCATE') AS trunc, \
                has_table_privilege($2, $1, 'TRIGGER') AS trig, \
                has_table_privilege($2, $1, 'REFERENCES') AS refs",
    )
    .bind(&qualified)
    .bind(subject)
    .fetch_one(pool)
    .await
    .map_err(db_error)?;
    let present: bool = table.try_get("present").map_err(db_error)?;
    if !present {
        return Err(incompatible(format!(
            "table {qualified} is missing; refusing to serve"
        )));
    }
    let fix = format!(
        "run `ledger-admin migrate --runtime-role {who}` with the owner identity (ADR-0016)"
    );
    let via = if subject == who {
        String::new()
    } else {
        format!(" (through membership in {subject})")
    };
    if exactness == Exactness::Exact && !table.try_get::<bool, _>("sel").map_err(db_error)? {
        return Err(identity(format!(
            "role {who} lacks SELECT on {qualified}; {fix}"
        )));
    }
    for (column, privilege) in [
        ("ins", "table-level INSERT"),
        ("upd", "table-level UPDATE"),
        ("del", "DELETE"),
        ("trunc", "TRUNCATE"),
        ("trig", "TRIGGER"),
        ("refs", "REFERENCES"),
    ] {
        if table.try_get::<bool, _>(column).map_err(db_error)? {
            return Err(identity(format!(
                "role {who} holds {privilege} on {qualified}{via}; the runtime identity must not \
                 (only the column grants of migration 0008 are allowed; ADR-0016)"
            )));
        }
    }
    // Per column: INSERT/UPDATE exactly where the model says (table-level grants are
    // excluded above, so a true here is a genuine column grant, directly or inherited).
    let columns = sqlx::query(
        "SELECT a.attname::text AS name, \
                has_column_privilege($2, a.attrelid, a.attnum, 'INSERT') AS ins, \
                has_column_privilege($2, a.attrelid, a.attnum, 'UPDATE') AS upd \
         FROM pg_attribute a \
         WHERE a.attrelid = to_regclass($1) AND a.attnum > 0 AND NOT a.attisdropped",
    )
    .bind(&qualified)
    .bind(subject)
    .fetch_all(pool)
    .await
    .map_err(db_error)?;
    let mut seen = Vec::with_capacity(columns.len());
    for column in &columns {
        let name: String = column.try_get("name").map_err(db_error)?;
        let ins: bool = column.try_get("ins").map_err(db_error)?;
        let upd: bool = column.try_get("upd").map_err(db_error)?;
        let expect_ins = model.insert_columns.contains(&name.as_str());
        let expect_upd = model.update_columns.contains(&name.as_str());
        let violates = |has: bool, expect: bool| match exactness {
            Exactness::Exact => has != expect,
            Exactness::Subset => has && !expect,
        };
        if violates(ins, expect_ins) {
            return Err(identity(format!(
                "role {who} {} INSERT on {qualified}.{name}{via}; the runtime identity's INSERT \
                 columns on {} are exactly {:?}; {fix}",
                if ins { "holds" } else { "lacks" },
                model.table,
                model.insert_columns
            )));
        }
        if violates(upd, expect_upd) {
            return Err(identity(format!(
                "role {who} {} UPDATE on {qualified}.{name}{via}; the runtime identity's UPDATE \
                 columns on {} are exactly {:?}; {fix}",
                if upd { "holds" } else { "lacks" },
                model.table,
                model.update_columns
            )));
        }
        seen.push(name);
    }
    for expected in model.insert_columns.iter().chain(model.update_columns) {
        if !seen.iter().any(|c| c == expected) {
            return Err(incompatible(format!(
                "column {qualified}.{expected} is missing; this build's privilege model does \
                 not match the schema; refusing to serve"
            )));
        }
    }
    Ok(())
}

async fn verify_sequence_privileges(pool: &PgPool, who: &str) -> Result<(), LedgerError> {
    verify_sequence_privileges_for(pool, who, who, Exactness::Exact).await
}

async fn verify_sequence_privileges_for(
    pool: &PgPool,
    subject: &str,
    who: &str,
    exactness: Exactness,
) -> Result<(), LedgerError> {
    let rows = sqlx::query(
        "SELECT c.relname::text AS name, \
                has_sequence_privilege($1, c.oid, 'USAGE') AS usage, \
                has_sequence_privilege($1, c.oid, 'SELECT') AS sel, \
                has_sequence_privilege($1, c.oid, 'UPDATE') AS upd \
         FROM pg_class c WHERE c.relkind = 'S' AND c.relnamespace = 'public'::regnamespace",
    )
    .bind(subject)
    .fetch_all(pool)
    .await
    .map_err(db_error)?;
    let via = if subject == who {
        String::new()
    } else {
        format!(" (through membership in {subject})")
    };
    let mut seen = Vec::with_capacity(rows.len());
    for row in &rows {
        let name: String = row.try_get("name").map_err(db_error)?;
        let usage: bool = row.try_get("usage").map_err(db_error)?;
        let sel: bool = row.try_get("sel").map_err(db_error)?;
        let upd: bool = row.try_get("upd").map_err(db_error)?;
        let expected = RUNTIME_SEQUENCES.contains(&name.as_str());
        let violates = match exactness {
            Exactness::Exact => usage != expected,
            Exactness::Subset => usage && !expected,
        };
        if violates {
            return Err(identity(format!(
                "role {who} {} USAGE on sequence public.{name}{via}; the runtime identity has USAGE \
                 on exactly {RUNTIME_SEQUENCES:?}; run `ledger-admin migrate --runtime-role {who}` \
                 with the owner identity (ADR-0016)",
                if usage { "holds" } else { "lacks" }
            )));
        }
        if sel || upd {
            return Err(identity(format!(
                "role {who} holds SELECT/UPDATE on sequence public.{name}{via}; only USAGE is granted \
                 to the runtime identity (ADR-0016)"
            )));
        }
        seen.push(name);
    }
    if exactness == Exactness::Exact {
        for expected in RUNTIME_SEQUENCES {
            if !seen.iter().any(|s| s == expected) {
                return Err(incompatible(format!(
                    "sequence public.{expected} is missing; refusing to serve"
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{EXPECTED_CHECKS, GUARD_TRIGGERS, normalize_constraint_def};

    #[test]
    fn constraint_definition_normalization_matches_postgres_deparse() {
        // PostgreSQL 15/17 deparse of migration 0009's content-address CHECK.
        let pg17 = "CHECK ((id = ('sha256:'::text || encode(sha256(bytes), 'hex'::text))))";
        let expected = EXPECTED_CHECKS
            .iter()
            .find(|(_, n, _)| *n == "immutable_objects_content_addressed")
            .unwrap()
            .2;
        assert_eq!(normalize_constraint_def(pg17), expected);
        assert_ne!(normalize_constraint_def("CHECK (true)"), expected);
        assert_eq!(EXPECTED_CHECKS.len(), 56);
    }

    #[test]
    fn normalization_never_touches_string_literals() {
        let expected = EXPECTED_CHECKS
            .iter()
            .find(|(_, n, _)| *n == "refs_branch_bounds")
            .unwrap()
            .2;
        let real = "CHECK ((((octet_length(branch) >= 1) AND (octet_length(branch) <= 128)) AND (branch ~ '^[A-Za-z0-9._/-]+$'::text)))";
        assert_eq!(normalize_constraint_def(real), expected);
        // A space added inside the regex admits `a b`: it must not normalize to the original.
        let widened = "CHECK ((((octet_length(branch) >= 1) AND (octet_length(branch) <= 128)) AND (branch ~ '^[A-Za-z0-9._/ -]+$'::text)))";
        assert_ne!(normalize_constraint_def(widened), expected);
        // `::text` inside a literal is content, not a cast.
        assert_eq!(
            normalize_constraint_def("CHECK ((x = 'a::text b'::text))"),
            "CHECK((x='a::text b'))"
        );
        // Quoted identifiers are compared exactly too.
        assert_ne!(
            normalize_constraint_def("CHECK ((\"position::text\" = ANY (ARRAY[0, 1])))"),
            normalize_constraint_def("CHECK ((\"position\" = ANY (ARRAY[0, 1])))")
        );
        assert_eq!(
            normalize_constraint_def("CHECK ((\"a b\" > 0))"),
            "CHECK((\"a b\">0))"
        );
        // An escaped quote keeps the literal open.
        assert_eq!(
            normalize_constraint_def("CHECK ((x = 'it''s  x'))"),
            "CHECK((x='it''s  x'))"
        );
    }

    #[test]
    fn restored_check_forms_are_the_flattened_migration_forms_only() {
        use super::RESTORED_CHECKS;
        assert_eq!(RESTORED_CHECKS.len(), 7);
        for (table, name, restored) in RESTORED_CHECKS {
            let expected = EXPECTED_CHECKS
                .iter()
                .find(|(t, n, _)| t == table && n == name)
                .unwrap_or_else(|| panic!("{table}.{name} is not an expected CHECK"))
                .2;
            assert_ne!(expected, *restored, "{name}: no-op alternative");
            // Only parentheses differ: the operands and operators are the migration's.
            let strip = |d: &str| d.replace(['(', ')'], "");
            assert_eq!(
                strip(expected),
                strip(restored),
                "{name}: more than grouping differs"
            );
        }
    }

    #[test]
    fn guard_functions_are_parsed_from_the_embedded_migrations() {
        let functions = super::expected_guard_functions();
        let names: Vec<&str> = functions.iter().map(|f| f.name.as_str()).collect();
        for expected in super::GUARD_FUNCTIONS {
            assert!(
                names.contains(expected),
                "{expected} not found in migrations"
            );
        }
        let audited = functions
            .iter()
            .find(|f| f.name == "refs_movement_is_audited")
            .unwrap();
        assert_eq!(audited.language, "plpgsql");
        assert_eq!(audited.returns, "trigger");
        assert_eq!(audited.search_path.as_deref(), Some("pg_catalog, public"));
        assert!(audited.body.contains("BEGIN") && audited.body.contains("RETURN"));
        let lock = functions
            .iter()
            .find(|f| f.name == "ledger_lock_key")
            .unwrap();
        assert_eq!(
            (lock.language.as_str(), lock.returns.as_str()),
            ("sql", "bigint")
        );
        let write_once = functions
            .iter()
            .find(|f| f.name == "ledger_rows_are_write_once")
            .unwrap();
        assert_eq!(write_once.search_path, None);
        assert!(write_once.body.contains("write-once"));
    }

    #[test]
    fn expected_trigger_types_match_the_catalog_bit_layout() {
        // Values observed in pg_trigger.tgtype for a freshly migrated database.
        let by_name = |n: &str| {
            GUARD_TRIGGERS
                .iter()
                .find(|t| t.name == n)
                .unwrap()
                .tgtype()
        };
        assert_eq!(by_name("immutable_objects_write_once"), 27); // ROW BEFORE UPDATE DELETE
        assert_eq!(by_name("graphs_identity_immutable"), 19); // ROW BEFORE UPDATE
        assert_eq!(by_name("refs_version_monotonic"), 23); // ROW BEFORE INSERT UPDATE
        assert_eq!(by_name("refs_movement_audited"), 21); // ROW AFTER INSERT UPDATE
        assert_eq!(GUARD_TRIGGERS.len(), 18);
    }
}
