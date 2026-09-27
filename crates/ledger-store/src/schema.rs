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
pub const REQUIRED_SCHEMA_VERSION: i64 = 9;

/// What `verify` found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaReport {
    pub version: i64,
    /// Fingerprint of the content-address CHECK's stored expression tree (`conbin`, with the
    /// statement-offset `location` fields removed so re-adding the same definition from a
    /// differently formatted statement does not change it): readiness compares it with the
    /// value start-up validated (deparse + probe), lock-free.
    pub content_check_fingerprint: String,
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
    verify_constraints_and_indexes(pool).await?;
    let content_check_fingerprint = verify_content_address_check(pool).await?;
    Ok(SchemaReport {
        version: highest,
        content_check_fingerprint,
    })
}

// ---------------------------------------------------------------------------------------
// Expected database controls (derived from migrations 0004–0009)
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
/// (table, key columns, referenced table/columns), never by name. Every FOREIGN KEY, PRIMARY
/// KEY and UNIQUE constraint of migrations 0001–0009 is listed: they bind audit rows to real
/// content and real events (ADR-0013) and make the workflow's uniqueness rules facts.
struct ExpectedConstraint {
    table: &'static str,
    kind: char, // 'f' | 'p' | 'u'
    columns: &'static [&'static str],
    references: Option<(&'static str, &'static [&'static str])>,
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
    }
}
const fn pk(table: &'static str, columns: &'static [&'static str]) -> ExpectedConstraint {
    ExpectedConstraint {
        table,
        kind: 'p',
        columns,
        references: None,
    }
}
const fn uq(table: &'static str, columns: &'static [&'static str]) -> ExpectedConstraint {
    ExpectedConstraint {
        table,
        kind: 'u',
        columns,
        references: None,
    }
}

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
    uq(
        "idempotency",
        &[
            "tenant_id",
            "graph_id",
            "operation",
            "idempotency_key",
            "principal_id",
            "principal_type",
            "on_behalf_of",
        ],
    ),
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
];

/// Partial unique indexes the workflow relies on (one terminal decision per candidate /
/// proposal / ref event): (table, columns, has a WHERE predicate).
const EXPECTED_UNIQUE_INDEXES: &[(&str, &[&str], bool)] = &[
    ("decisions", &["candidate_commit"], false),
    ("decisions", &["proposal_id"], true),
    ("decisions", &["ref_event_id"], true),
];

/// Named CHECK constraints of 0002–0009 (domain rules the Rust layer also enforces; here
/// presence and validation are verified, the content-address one also by definition).
const EXPECTED_CHECKS: &[(&str, &str)] = &[
    ("immutable_objects", "immutable_objects_id_format"),
    ("immutable_objects", "immutable_objects_content_addressed"),
    ("commit_index", "commit_index_version_known"),
    ("commit_index", "commit_index_parent_count"),
    ("commit_index", "commit_index_graph_id_format"),
    ("commit_parents", "commit_parents_position"),
    ("graphs", "graphs_graph_id_format"),
    ("graphs", "graphs_tenant_id_bounds"),
    ("graphs", "graphs_kb_bounds"),
    ("graphs", "graphs_purpose_bounds"),
    ("graphs", "graphs_status_known"),
    ("refs", "refs_version_positive"),
    ("refs", "refs_branch_bounds"),
    ("proposals", "proposals_branch_bounds"),
    ("proposals", "proposals_principal_type"),
    ("proposals", "proposals_correlation_bounds"),
    ("ref_events", "ref_events_branch_bounds"),
    ("ref_events", "ref_events_operation"),
    ("ref_events", "ref_events_genesis_shape"),
    ("ref_events", "ref_events_principal_type"),
    ("ref_events", "ref_events_correlation_bounds"),
    ("decisions", "decisions_kind"),
    ("decisions", "decisions_accepted_has_event"),
    ("decisions", "decisions_reason_bounds"),
    ("decisions", "decisions_principal_type"),
    ("decisions", "decisions_correlation_bounds"),
    ("projection_outbox", "outbox_event_kind"),
    ("idempotency", "idempotency_operation"),
    ("idempotency", "idempotency_key_bounds"),
    ("idempotency", "idempotency_digest_format"),
    ("idempotency", "idempotency_result_kind"),
    ("idempotency", "idempotency_principal_type"),
];

/// Every expected FOREIGN KEY / PRIMARY KEY / UNIQUE constraint exists exactly once with the
/// expected shape and is validated and non-deferrable; every named CHECK exists and is
/// validated; every partial unique index exists. Catalog-only (attnums resolved to names),
/// so it is lock-free and runs on readiness.
async fn verify_constraints_and_indexes(pool: &PgPool) -> Result<(), LedgerError> {
    let rows = sqlx::query(
        "SELECT c.relname::text AS table_name, con.conname::text AS name, con.contype::text AS kind, \
                con.convalidated, con.condeferrable, \
                (SELECT array_agg(a.attname::text ORDER BY k.ord) \
                   FROM unnest(con.conkey) WITH ORDINALITY AS k(attnum, ord) \
                   JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.attnum) AS columns, \
                rc.relname::text AS ref_table, \
                (SELECT array_agg(a.attname::text ORDER BY k.ord) \
                   FROM unnest(con.confkey) WITH ORDINALITY AS k(attnum, ord) \
                   JOIN pg_attribute a ON a.attrelid = con.confrelid AND a.attnum = k.attnum) AS ref_columns \
         FROM pg_constraint con \
         JOIN pg_class c ON c.oid = con.conrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN pg_class rc ON rc.oid = con.confrelid \
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
        ref_table: Option<String>,
        ref_columns: Vec<String>,
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
            ref_table: row.try_get("ref_table").map_err(db_error)?,
            ref_columns: row
                .try_get::<Option<Vec<String>>, _>("ref_columns")
                .map_err(db_error)?
                .unwrap_or_default(),
        });
    }
    for e in EXPECTED_CONSTRAINTS {
        let matches: Vec<&Found> = found
            .iter()
            .filter(|f| {
                f.table == e.table
                    && f.kind == e.kind.to_string()
                    && f.columns == e.columns
                    && match e.references {
                        Some((rt, rcols)) => {
                            f.ref_table.as_deref() == Some(rt) && f.ref_columns == rcols
                        }
                        None => true,
                    }
            })
            .collect();
        let shape = match e.references {
            Some((rt, rcols)) => {
                format!("{} FOREIGN KEY {:?} -> {rt} {rcols:?}", e.table, e.columns)
            }
            None => format!(
                "{} {} {:?}",
                e.table,
                if e.kind == 'p' {
                    "PRIMARY KEY"
                } else {
                    "UNIQUE"
                },
                e.columns
            ),
        };
        match matches.as_slice() {
            [] => {
                return Err(incompatible(format!(
                    "constraint {shape} is missing; refusing to serve"
                )));
            }
            [one] => {
                if !one.validated {
                    return Err(incompatible(format!(
                        "constraint {} ({shape}) is NOT VALID; refusing to serve",
                        one.name
                    )));
                }
                if one.deferrable {
                    return Err(incompatible(format!(
                        "constraint {} ({shape}) is deferrable (the migrations define none); refusing to serve",
                        one.name
                    )));
                }
            }
            _ => {} // duplicates of an expected shape are harmless
        }
    }
    for (table, name) in EXPECTED_CHECKS {
        match found.iter().find(|f| f.table == *table && f.name == *name) {
            Some(f) if f.kind == "c" && f.validated => {}
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
        "SELECT t.relname::text AS table_name, i.indisunique, i.indpred IS NOT NULL AS partial, \
                (SELECT array_agg(a.attname::text ORDER BY k.ord) \
                   FROM unnest(i.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord) \
                   JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum) AS columns \
         FROM pg_index i JOIN pg_class t ON t.oid = i.indrelid \
         JOIN pg_namespace n ON n.oid = t.relnamespace \
         WHERE n.nspname = 'public' AND i.indisunique AND i.indisvalid",
    )
    .fetch_all(pool)
    .await
    .map_err(db_error)?;
    for (table, columns, partial) in EXPECTED_UNIQUE_INDEXES {
        let present = indexes.iter().any(|row| {
            row.try_get::<String, _>("table_name")
                .is_ok_and(|t| t == *table)
                && row
                    .try_get::<Option<Vec<String>>, _>("columns")
                    .is_ok_and(|c| c.unwrap_or_default() == *columns)
                && row
                    .try_get::<bool, _>("partial")
                    .is_ok_and(|p| p == *partial)
        });
        if !present {
            return Err(incompatible(format!(
                "unique index on {table} {columns:?}{} is missing or invalid; refusing to serve",
                if *partial { " (partial)" } else { "" }
            )));
        }
    }
    Ok(())
}

/// Migration 0009's content-address CHECK on `immutable_objects`: the database-side
/// guarantee that no bytes are stored under a false content id. Rust re-verifies every
/// object's digest on read; this is defence in depth and must be present, validated and
/// semantically the expected condition. `verify` checks the catalog (lock-free, used by
/// readiness); `probe_content_address_check` additionally exercises it at start-up.
const CONTENT_ADDRESS_CHECK: &str = "immutable_objects_content_addressed";
/// `pg_get_constraintdef` output with whitespace and `::text` casts removed (PostgreSQL 15
/// and 17 deparse the expression identically otherwise).
const CONTENT_ADDRESS_CHECK_DEF: &str = "CHECK((id=('sha256:'||encode(sha256(bytes),'hex'))))";

fn normalize_constraint_def(def: &str) -> String {
    def.chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .replace("::text", "")
}

/// Catalog facts about the CHECK (lock-free: no relation is opened): present on the table,
/// a CHECK, validated. Returns the fingerprint of its stored expression tree.
async fn verify_content_address_check(pool: &PgPool) -> Result<String, LedgerError> {
    let rows = sqlx::query(
        "SELECT con.contype::text AS contype, con.convalidated, \
                md5(regexp_replace(con.conbin::text, ':location -?[0-9]+', '', 'g')) AS fingerprint \
         FROM pg_constraint con JOIN pg_class c ON c.oid = con.conrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'public' AND c.relname = 'immutable_objects' AND con.conname = $1",
    )
    .bind(CONTENT_ADDRESS_CHECK)
    .fetch_all(pool)
    .await
    .map_err(db_error)?;
    let row = match rows.as_slice() {
        [row] => row,
        _ => {
            return Err(incompatible(format!(
                "constraint {CONTENT_ADDRESS_CHECK} is missing on public.immutable_objects; refusing to serve"
            )));
        }
    };
    let contype: String = row.try_get("contype").map_err(db_error)?;
    let validated: bool = row.try_get("convalidated").map_err(db_error)?;
    if contype != "c" {
        return Err(incompatible(format!(
            "constraint {CONTENT_ADDRESS_CHECK} is not a CHECK constraint; refusing to serve"
        )));
    }
    if !validated {
        return Err(incompatible(format!(
            "constraint {CONTENT_ADDRESS_CHECK} is NOT VALID (existing rows unverified); refusing to serve"
        )));
    }
    row.try_get("fingerprint").map_err(db_error)
}

/// Start-up verification of the content-address CHECK's semantics: its deparsed definition
/// must normalize to the content-address condition, and a mislabelled object must be refused
/// by the database itself (rolled-back probe), so a same-named constraint with a weaker
/// condition cannot pass. Runs at server start-up only: deparsing opens the relation with
/// `ACCESS SHARE` and the probe takes a row lock, neither of which belongs in readiness, which
/// instead compares the expression fingerprint `verify` returns with the one validated here.
pub async fn probe_content_address_check(pool: &PgPool) -> Result<(), LedgerError> {
    let def: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(con.oid) FROM pg_constraint con \
         JOIN pg_class c ON c.oid = con.conrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'public' AND c.relname = 'immutable_objects' AND con.conname = $1",
    )
    .bind(CONTENT_ADDRESS_CHECK)
    .fetch_one(pool)
    .await
    .map_err(db_error)?;
    if normalize_constraint_def(&def) != CONTENT_ADDRESS_CHECK_DEF {
        return Err(incompatible(format!(
            "constraint {CONTENT_ADDRESS_CHECK} has definition {def:?} instead of the content-address \
             condition; refusing to serve"
        )));
    }
    let mut tx = pool.begin().await.map_err(db_error)?;
    let probe = sqlx::query("INSERT INTO immutable_objects (id, bytes) VALUES ($1, $2)")
        .bind(format!("sha256:{}", "0".repeat(64)))
        .bind(b"not the preimage".as_slice())
        .execute(&mut *tx)
        .await;
    tx.rollback().await.map_err(db_error)?;
    match probe {
        Err(sqlx::Error::Database(d)) if d.code().as_deref() == Some("23514") => Ok(()),
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
        ],
        update_columns: &[],
    },
    TablePrivileges {
        table: "_sqlx_migrations",
        insert_columns: &[],
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
    use super::{CONTENT_ADDRESS_CHECK_DEF, GUARD_TRIGGERS, normalize_constraint_def};

    #[test]
    fn constraint_definition_normalization_matches_postgres_deparse() {
        // PostgreSQL 17 deparse of migration 0009's CHECK.
        let pg17 = "CHECK ((id = ('sha256:'::text || encode(sha256(bytes), 'hex'::text))))";
        assert_eq!(normalize_constraint_def(pg17), CONTENT_ADDRESS_CHECK_DEF);
        assert_ne!(
            normalize_constraint_def("CHECK (true)"),
            CONTENT_ADDRESS_CHECK_DEF
        );
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
        assert_eq!(GUARD_TRIGGERS.len(), 13);
    }
}
