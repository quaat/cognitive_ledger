use ledger_core::LedgerError;
use ledger_store::{CommitRequest, Ledger};
use ledger_testkit::{add, delete, patch};

fn request(
    expected_head: Option<ledger_core::CommitId>,
    patch: ledger_rdf::Patch,
    message: &str,
) -> CommitRequest {
    CommitRequest {
        expected_head,
        patch,
        author: "urn:agent:test".into(),
        message: message.into(),
        event_time: "2026-09-24T10:00:00Z".into(),
    }
}
#[tokio::test]
async fn two_commits_reconstruct_and_stale_writer_loses_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(dir.path()).unwrap();
    assert_eq!(ledger.head().await.unwrap(), None);
    let old = "<urn:material:a> <urn:temperature> \"80\" .";
    let new = "<urn:material:a> <urn:temperature> \"90\" .";
    let c1 = ledger
        .commit(request(None, patch([add(old)]), "initial assertion"))
        .await
        .unwrap();
    let c2 = ledger
        .commit(request(
            Some(c1.clone()),
            patch([delete(old), add(new)]),
            "correct temperature",
        ))
        .await
        .unwrap();
    assert_eq!(
        ledger
            .state_at(&c1)
            .await
            .unwrap()
            .into_iter()
            .map(|q| q.to_string())
            .collect::<Vec<_>>(),
        vec![old]
    );
    assert_eq!(
        ledger
            .state_at(&c2)
            .await
            .unwrap()
            .into_iter()
            .map(|q| q.to_string())
            .collect::<Vec<_>>(),
        vec![new]
    );
    let stale = ledger
        .commit(request(
            Some(c1),
            patch([add("<urn:other> <urn:p> <urn:o> .")]),
            "stale",
        ))
        .await;
    assert!(matches!(stale, Err(LedgerError::HeadChanged { .. })));
    assert_eq!(ledger.head().await.unwrap(), Some(c2.clone()));
    drop(ledger);
    let reopened = Ledger::open(dir.path()).unwrap();
    assert_eq!(reopened.head().await.unwrap(), Some(c2.clone()));
    assert_eq!(reopened.state_at(&c2).await.unwrap().len(), 1);
}
