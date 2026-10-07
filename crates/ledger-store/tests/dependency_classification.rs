//! Failure classification without a database: a PostgreSQL that cannot be reached is a
//! retryable dependency failure (`DependencyUnavailable`, 503 at the boundary), never a
//! generic `Storage` error (500), on every store surface a public request can hit.
#![cfg(feature = "postgres")]

use ledger_core::{CommitId, GraphId, ImmutableStore, LedgerError};
use ledger_store::{PostgresLedgerStore, V1Binding};
use std::str::FromStr;

fn unreachable_store() -> PostgresLedgerStore {
    // Nothing listens on port 1; the pool connects lazily so construction succeeds and
    // every operation fails at acquire time.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_secs(2))
        .connect_lazy("postgres://ledger:x@127.0.0.1:1/ledger")
        .expect("lazy pool");
    PostgresLedgerStore::from_pool_migrated(pool, V1Binding::Reject)
}

fn is_dependency(result: Result<impl std::fmt::Debug, LedgerError>) -> bool {
    matches!(result, Err(LedgerError::DependencyUnavailable(_)))
}

#[test]
fn postgres_shutdown_and_connection_sqlstates_are_unavailable_not_faults() {
    for code in [
        "08000", "08003", "08006", "08001", "08004", "57P01", "57P02", "57P03", "53300", "53400",
    ] {
        assert!(ledger_store::sqlstate_is_unavailable(code), "{code}");
    }
    for code in [
        "23505", "23503", "42P01", "22P02", "40001", "57014", "P0001", "XX000",
    ] {
        assert!(!ledger_store::sqlstate_is_unavailable(code), "{code}");
    }
}

#[tokio::test]
async fn unreachable_database_is_classified_as_a_dependency_failure_everywhere() {
    let store = unreachable_store();
    let commit = CommitId::from_str(&format!("sha256:{}", "a".repeat(64))).unwrap();
    let graph = GraphId::new("g1").unwrap();
    // Immutable publication and reads (the path the prepare transaction publishes through).
    assert!(is_dependency(
        store
            .immutable()
            .put_content(&ledger_core::ContentId::for_bytes(b"x"), b"x")
            .await
    ));
    assert!(is_dependency(
        store.immutable().get_content(&commit.0).await
    ));
    assert!(is_dependency(store.immutable().get_commit(&commit).await));
    // Graph authority, ref/commit lookups and readiness.
    assert!(is_dependency(store.graphs().get(&graph).await));
    assert!(is_dependency(store.ref_head(&graph, "main").await));
    assert!(is_dependency(store.commit_graph(&commit).await));
    assert!(is_dependency(store.ready().await));
    assert!(is_dependency(
        store
            .workflows()
            .reconstruct(&commit, &ledger_store::ReconstructionLimits::DEVELOPMENT)
            .await
    ));
}

/// A connection attempt during PostgreSQL crash recovery or shutdown fails inside the
/// start-up handshake, which sqlx reports as a `Protocol` error rather than the 57P03 the
/// server sent (observed as 165 × `500 INTERNAL` across two PostgreSQL kills in the Plan 0013
/// M1 fault run). The connection is gone and the request is retryable: `DependencyUnavailable`
/// (503), never a storage fault (500). Driver-level pool and I/O failures classify alike.
#[test]
fn driver_protocol_pool_and_io_failures_are_unavailable_not_faults() {
    for e in [
        sqlx::Error::Protocol(
            "Postgres protocol error (reading Authentication): the database system is in recovery mode"
                .into(),
        ),
        sqlx::Error::PoolTimedOut,
        sqlx::Error::PoolClosed,
        sqlx::Error::WorkerCrashed,
        sqlx::Error::Io(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof")),
    ] {
        let classified = ledger_store::classify_db_error(e);
        assert!(
            matches!(classified, LedgerError::DependencyUnavailable(_)),
            "{classified:?}"
        );
    }
    // A decoding or configuration defect stays a storage fault.
    assert!(matches!(
        ledger_store::classify_db_error(sqlx::Error::Decode("bad column".into())),
        LedgerError::Storage(_)
    ));
}
