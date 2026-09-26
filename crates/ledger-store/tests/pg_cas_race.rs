//! Real-PostgreSQL proof that ref compare-and-set is a lost-update-safe primitive.
//!
//! This test is `#[ignore]`d by default: it requires a live database and is run
//! explicitly by `scripts/test-integration.sh` with `LEDGER_TEST_DATABASE_URL`
//! set. It deliberately uses two independent pools (hence two independent backend
//! connections) racing on the same expected head; correctness must come from the
//! database's transactional CAS, never from an application-side mutex.
#![cfg(feature = "postgres")]

use ledger_core::{CommitId, ContentId, LedgerError, RefStore};
use ledger_store::PgRefStore;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

fn commit_id(seed: &[u8]) -> CommitId {
    CommitId(ContentId::for_bytes(seed))
}

/// A branch name unique to this process/run so parallel or repeated runs against a
/// shared database never collide on state (no destructive TRUNCATE required).
fn unique_branch() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("cas-race-{}-{}", std::process::id(), nanos)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn two_connections_cannot_both_advance_same_head() {
    let url = std::env::var("LEDGER_TEST_DATABASE_URL")
        .expect("LEDGER_TEST_DATABASE_URL must be set for the PostgreSQL CAS race test");
    let branch = unique_branch();

    // Two pools => two genuinely independent backend connections contend for the ref.
    let store_a = Arc::new(
        PgRefStore::connect_ref(&url, "default", &branch)
            .await
            .unwrap(),
    );
    let store_b = Arc::new(
        PgRefStore::connect_ref(&url, "default", &branch)
            .await
            .unwrap(),
    );

    // Genesis via connection A; both connections must observe it.
    let genesis = commit_id(b"cas-race-genesis");
    store_a.compare_and_set(None, &genesis).await.unwrap();
    assert_eq!(store_a.head().await.unwrap(), Some(genesis.clone()));
    assert_eq!(store_b.head().await.unwrap(), Some(genesis.clone()));

    // A second genesis attempt from the other connection must lose, not silently
    // overwrite: the INSERT ... ON CONFLICT DO NOTHING affects zero rows.
    let rival_genesis = commit_id(b"cas-race-genesis-rival");
    assert!(matches!(
        store_b.compare_and_set(None, &rival_genesis).await,
        Err(LedgerError::HeadChanged { .. })
    ));

    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let target_a = commit_id(b"cas-race-writer-a");
    let target_b = commit_id(b"cas-race-writer-b");

    let handle_a = {
        let store = Arc::clone(&store_a);
        let barrier = Arc::clone(&barrier);
        let expected = genesis.clone();
        let target = target_a.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            store.compare_and_set(Some(&expected), &target).await
        })
    };
    let handle_b = {
        let store = Arc::clone(&store_b);
        let barrier = Arc::clone(&barrier);
        let expected = genesis.clone();
        let target = target_b.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            store.compare_and_set(Some(&expected), &target).await
        })
    };

    let result_a = handle_a.await.unwrap();
    let result_b = handle_b.await.unwrap();

    let winners = [&result_a, &result_b].iter().filter(|r| r.is_ok()).count();
    let losers = [&result_a, &result_b]
        .iter()
        .filter(|r| matches!(r, Err(LedgerError::HeadChanged { .. })))
        .count();
    assert_eq!(
        winners, 1,
        "exactly one writer must win the CAS race: a={result_a:?} b={result_b:?}"
    );
    assert_eq!(
        losers, 1,
        "the losing writer must observe HeadChanged: a={result_a:?} b={result_b:?}"
    );

    // The persisted head is the winner's target, observed through a fresh read on
    // each connection (no torn or divergent state).
    let expected_head = if result_a.is_ok() { target_a } else { target_b };
    assert_eq!(store_a.head().await.unwrap(), Some(expected_head.clone()));
    assert_eq!(store_b.head().await.unwrap(), Some(expected_head));
}
