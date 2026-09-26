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
        .execute(conn)
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
    // The integrity controls the least-privilege model relies on must be present and
    // enabled; a disabled or missing guard is not a compatible schema.
    let triggers = sqlx::query(
        "SELECT tgname, tgenabled FROM pg_trigger WHERE NOT tgisinternal AND tgname = ANY($1)",
    )
    .bind(GUARD_TRIGGERS)
    .fetch_all(pool)
    .await
    .map_err(db_error)?;
    for name in GUARD_TRIGGERS {
        match triggers
            .iter()
            .find(|r| r.try_get::<String, _>("tgname").is_ok_and(|n| n == *name))
        {
            Some(row) => {
                let enabled: i8 = row.try_get("tgenabled").map_err(db_error)?;
                if enabled as u8 as char != 'O' {
                    return Err(LedgerError::SchemaIncompatible(format!(
                        "integrity trigger {name} is disabled; refusing to serve"
                    )));
                }
            }
            None => {
                return Err(LedgerError::SchemaIncompatible(format!(
                    "integrity trigger {name} is missing; refusing to serve"
                )));
            }
        }
    }
    Ok(SchemaReport { version: highest })
}

/// Triggers that make the ledger's write-once and movement rules database facts.
const GUARD_TRIGGERS: &[&str] = &[
    "graphs_identity_immutable",
    "immutable_objects_write_once",
    "commit_index_write_once",
    "commit_parents_write_once",
    "refs_identity_immutable",
    "refs_version_monotonic",
    "proposals_write_once",
    "ref_events_write_once",
    "decisions_write_once",
    "idempotency_write_once",
    "outbox_identity_immutable",
    "refs_movement_audited",
    "graphs_status_change_serialized",
];

/// The privilege the runtime identity must have on each table, and nothing beyond.
const RUNTIME_TABLE_PRIVILEGES: &[(&str, &str)] = &[
    ("graphs", "SELECT"),
    ("refs", "SELECT"),
    ("refs", "INSERT"),
    ("immutable_objects", "SELECT"),
    ("immutable_objects", "INSERT"),
    ("commit_index", "SELECT"),
    ("commit_index", "INSERT"),
    ("commit_parents", "SELECT"),
    ("commit_parents", "INSERT"),
    ("proposals", "SELECT"),
    ("proposals", "INSERT"),
    ("ref_events", "SELECT"),
    ("ref_events", "INSERT"),
    ("decisions", "SELECT"),
    ("decisions", "INSERT"),
    ("projection_outbox", "SELECT"),
    ("projection_outbox", "INSERT"),
    ("idempotency", "SELECT"),
    ("idempotency", "INSERT"),
    ("_sqlx_migrations", "SELECT"),
];

/// Privileges the runtime identity must NOT have (any one defeats the boundary).
const RUNTIME_FORBIDDEN_PRIVILEGES: &[(&str, &str)] = &[
    ("immutable_objects", "UPDATE"),
    ("immutable_objects", "DELETE"),
    ("commit_index", "UPDATE"),
    ("commit_index", "DELETE"),
    ("commit_parents", "UPDATE"),
    ("commit_parents", "DELETE"),
    ("proposals", "UPDATE"),
    ("proposals", "DELETE"),
    ("ref_events", "UPDATE"),
    ("ref_events", "DELETE"),
    ("decisions", "UPDATE"),
    ("decisions", "DELETE"),
    ("idempotency", "UPDATE"),
    ("idempotency", "DELETE"),
    ("projection_outbox", "UPDATE"),
    ("projection_outbox", "DELETE"),
    ("graphs", "INSERT"),
    ("graphs", "UPDATE"),
    ("graphs", "DELETE"),
    ("refs", "DELETE"),
    ("refs", "TRUNCATE"),
    ("immutable_objects", "TRUNCATE"),
];

/// Verify that the connected identity is a least-privilege runtime identity (ADR-0016):
/// not a superuser, not the owner of any ledger table, without CREATE on the schema, with
/// exactly the request path's grants. The server refuses to start otherwise, so a
/// deployment that kept the owner URL cannot silently serve with owner rights.
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
        return Err(LedgerError::RuntimeIdentity(format!(
            "role {who} is a superuser; the server must run as the least-privilege runtime \
             identity (ADR-0016)"
        )));
    }
    if owned > 0 {
        return Err(LedgerError::RuntimeIdentity(format!(
            "role {who} owns {owned} ledger table(s); the server must not run as the schema \
             owner (ADR-0016)"
        )));
    }
    if can_create {
        return Err(LedgerError::RuntimeIdentity(format!(
            "role {who} holds CREATE on schema public; revoke it (ADR-0016)"
        )));
    }
    for (table, privilege) in RUNTIME_FORBIDDEN_PRIVILEGES {
        if table_privilege(pool, table, privilege).await? {
            return Err(LedgerError::RuntimeIdentity(format!(
                "role {who} holds {privilege} on {table}; the runtime identity must not \
                 (ADR-0016)"
            )));
        }
    }
    for (table, privilege) in RUNTIME_TABLE_PRIVILEGES {
        if !table_privilege(pool, table, privilege).await? {
            return Err(LedgerError::RuntimeIdentity(format!(
                "role {who} lacks {privilege} on {table}; run `ledger-admin migrate \
                 --runtime-role {who}` with the owner identity (ADR-0016)"
            )));
        }
    }
    Ok(())
}

async fn table_privilege(pool: &PgPool, table: &str, privilege: &str) -> Result<bool, LedgerError> {
    // Column-level grants count as table-level for has_table_privilege only when the whole
    // table is granted; the column grants on refs are checked through has_any_column_privilege.
    // SQL AND is not guaranteed to short-circuit: guard with CASE so DELETE/TRUNCATE never
    // reach has_any_column_privilege (which only knows SELECT/INSERT/UPDATE/REFERENCES).
    let row = sqlx::query(
        "SELECT has_table_privilege(current_user, $1, $2) \
                OR CASE WHEN $2 IN ('INSERT', 'UPDATE') \
                        THEN has_any_column_privilege(current_user, $1, $2) ELSE false END AS ok",
    )
    .bind(format!("public.{table}"))
    .bind(privilege)
    .fetch_one(pool)
    .await
    .map_err(db_error)?;
    row.try_get("ok").map_err(db_error)
}
