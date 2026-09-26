//! Explicit schema-migration entry points (ADR-0012/0013). Store constructors that take
//! a URL still migrate on connect for the bootstrap topology; administrative tools and
//! the eventual owner-role deployment call these directly instead, so runtime store
//! construction and DDL can be separated (see tech-debt: database role split).

use crate::storage;
use ledger_core::LedgerError;
use sqlx::PgPool;
use std::borrow::Cow;

/// Apply every migration.
pub async fn migrate_all(pool: &PgPool) -> Result<(), LedgerError> {
    sqlx::migrate!("../../migrations")
        .run(pool)
        .await
        .map_err(storage)
}

/// Apply migrations with version `<= upto` only. Used by the filesystem→PostgreSQL cutover
/// to bring a database to the content schema (0005) before content is imported, so the
/// 0006 guard ("every ref head is an indexed commit of its graph") can then pass.
pub async fn migrate_up_to(pool: &PgPool, upto: i64) -> Result<(), LedgerError> {
    let mut migrator = sqlx::migrate!("../../migrations");
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
    migrator.run(pool).await.map_err(storage)
}

/// The migration version that completes the content schema (immutable objects, commit
/// index, graphs, write-once guards) but precedes the workflow schema and its refs FK.
pub const CONTENT_SCHEMA_VERSION: i64 = 5;
