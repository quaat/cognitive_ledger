//! Real-PostgreSQL evidence for Plan 0012 (Phase 6C): windowed retrieval of commits,
//! patches and parent edges is observably the same ledger as the scalar one — the same
//! states, digests, histories, merge relations and errors — and the indexes remain hints
//! that the verified bytes must confirm. Every test is `#[ignore]` and runs with
//! `LEDGER_TEST_DATABASE_URL`; the corruption tests seed their damage in throwaway
//! databases so the shared one stays verifiable.
//!
//! Reference: `WorkflowRepository::reconstruct_reference` (the Phase 1–6B scalar fold) and
//! `GraphParents` with an ancestry window of 1 (the Phase 4–5 walk), both kept only in
//! `test-hooks` builds.
#![cfg(feature = "postgres")]

use ledger_core::{
    Actor, AnyCommit, AuthenticatedPrincipal, Commit, CommitId, CommitV2, ContentId, GraphId,
    ImmutableStore, LedgerError, LedgerTimestamp, PrincipalId, PrincipalType, TenantId,
};
use ledger_rdf::{Operation, OperationKind, Patch, Quad};
use ledger_store::{
    AcceptRequest, BranchPolicy, CreateBranchRequest, GraphStatus, MergeClass, MergePreview,
    MergeSpec, MergeStrategy, NewGraph, PostgresImmutableStore, PostgresLedgerStore,
    PrepareRequest, ReconstructionLimits, RequestScope, RetrievalWindows, TraversalLimits,
    V1Binding, ValidationPolicy, WorkflowRepository,
};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::{
    cell::Cell,
    collections::BTreeSet,
    sync::Once,
    time::{SystemTime, UNIX_EPOCH},
};

const IGNORE: &str =
    "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL";

fn database_url() -> String {
    std::env::var("LEDGER_TEST_DATABASE_URL").expect("LEDGER_TEST_DATABASE_URL must be set")
}

fn unique(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}-{}-{nanos}", std::process::id())
}

/// The test URL with its database name replaced (`…/ledger?…` → `…/<name>?…`).
fn url_for_database(base: &str, name: &str) -> String {
    let (head, query) = match base.split_once('?') {
        Some((h, q)) => (h, Some(q)),
        None => (base, None),
    };
    let slash = head.rfind('/').expect("database url has a path");
    let mut url = format!("{}/{name}", &head[..slash]);
    if let Some(q) = query {
        url.push('?');
        url.push_str(q);
    }
    url
}

/// A throwaway database for tests that seed corruption. The shared database must stay
/// clean: the integration harness ends with `ledger-admin verify` against it.
async fn fresh_database(prefix: &str) -> String {
    let base = database_url();
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&base)
        .await
        .unwrap();
    let name = unique(prefix).replace('-', "_").to_lowercase();
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&admin)
        .await
        .unwrap();
    url_for_database(&base, &name)
}

async fn store_at(url: &str) -> PostgresLedgerStore {
    PostgresLedgerStore::connect_and_migrate(url, V1Binding::Reject)
        .await
        .unwrap()
}

fn tenant() -> TenantId {
    TenantId::new("tenant-a").unwrap()
}

async fn graph_with(store: &PostgresLedgerStore, status: GraphStatus) -> GraphId {
    let id = GraphId::new(unique("rt")).unwrap();
    store
        .graphs()
        .create(&NewGraph {
            graph_id: id.clone(),
            tenant_id: tenant(),
            knowledge_base_id: None,
            purpose: None,
            status,
        })
        .await
        .unwrap();
    id
}

async fn graph(store: &PostgresLedgerStore) -> GraphId {
    graph_with(store, GraphStatus::Active).await
}

fn q(s: &str) -> Quad {
    s.parse().unwrap()
}

fn quad(slot: usize, value: u64) -> Quad {
    q(&format!("<urn:s:{slot}> <urn:p> \"{value}\" ."))
}

fn add(quad: Quad) -> Operation {
    Operation {
        kind: OperationKind::Add,
        quad,
    }
}

fn del(quad: Quad) -> Operation {
    Operation {
        kind: OperationKind::Delete,
        quad,
    }
}

fn v2(graph: &GraphId, parents: Vec<CommitId>, patch: &Patch, message: &str) -> AnyCommit {
    AnyCommit::V2(CommitV2 {
        graph_id: graph.clone(),
        parents,
        patch: patch.id(),
        actor: Actor {
            principal_id: PrincipalId::new("urn:sculpin:agent:test").unwrap(),
            principal_type: PrincipalType::Agent,
            on_behalf_of: None,
        },
        activity: "test".into(),
        event_time: None,
        recorded_at: LedgerTimestamp::parse_rfc3339("2026-10-07T00:00:00Z").unwrap(),
        evidence_refs: vec![unique("urn:evidence")],
        source_system: None,
        message: message.into(),
    })
}

fn v1(parents: Vec<CommitId>, patch: &Patch, message: &str) -> AnyCommit {
    AnyCommit::V1(Commit {
        parents,
        patch: patch.id(),
        author: "urn:agent:test".into(),
        message: message.into(),
        event_time: "2026-10-07T00:00:00Z".into(),
        recorded_time: unique("recorded"),
    })
}

/// Publish a patch and a commit on it through the production store (indexed, verified).
async fn publish(immutable: &PostgresImmutableStore, commit: AnyCommit, patch: &Patch) -> CommitId {
    immutable
        .put_content(&patch.id().0, &patch.canonical_bytes())
        .await
        .unwrap();
    immutable.put_commit(&commit).await.unwrap()
}

/// A constant-state linear history of `n` commits (genesis first) over `slots` slots: the
/// genesis adds every slot, every later commit replaces one slot's value. Returns the
/// commit ids and the expected state after each of them.
async fn constant_state_chain(
    immutable: &PostgresImmutableStore,
    graph: &GraphId,
    n: usize,
    slots: usize,
) -> (Vec<CommitId>, Vec<BTreeSet<Quad>>) {
    assert!(n >= 1 && slots >= 1);
    let mut values: Vec<u64> = (0..slots as u64).collect();
    let mut next = slots as u64;
    let mut state: BTreeSet<Quad> = (0..slots).map(|s| quad(s, values[s])).collect();
    let genesis = Patch::new(state.iter().cloned().map(add)).unwrap();
    let mut ids = vec![publish(immutable, v2(graph, vec![], &genesis, "genesis"), &genesis).await];
    let mut states = vec![state.clone()];
    while ids.len() < n {
        let slot = ids.len() % slots;
        let old = quad(slot, values[slot]);
        values[slot] = next;
        next += 1;
        let new = quad(slot, values[slot]);
        let patch = Patch::new([add(new.clone()), del(old.clone())]).unwrap();
        state.remove(&old);
        state.insert(new);
        let id = publish(
            immutable,
            v2(graph, vec![ids.last().unwrap().clone()], &patch, "step"),
            &patch,
        )
        .await;
        ids.push(id);
        states.push(state.clone());
    }
    (ids, states)
}

fn windowed(store: &PostgresLedgerStore, windows: RetrievalWindows) -> WorkflowRepository {
    WorkflowRepository::new(
        store.pool().clone(),
        PostgresImmutableStore::from_pool_migrated(store.pool().clone(), V1Binding::Reject),
    )
    .with_retrieval_windows(windows)
}

fn windows(objects: usize, bytes: usize, ancestry: usize) -> RetrievalWindows {
    RetrievalWindows {
        objects,
        bytes,
        ancestry,
    }
}

/// Every window configuration the reconstruction tests compare with the reference: one
/// object per window, the boundaries around small histories, the production default, and
/// byte cuts that serve one object per window whatever the object bound.
fn reconstruction_windows() -> Vec<RetrievalWindows> {
    vec![
        windows(1, 8 << 20, 1),
        windows(2, 8 << 20, 1),
        windows(3, 8 << 20, 1),
        windows(4, 8 << 20, 1),
        windows(7, 8 << 20, 1),
        windows(8, 8 << 20, 1),
        windows(9, 8 << 20, 1),
        windows(256, 1, 1),
        windows(256, 300, 1),
        RetrievalWindows::DEFAULT,
    ]
}

type Detailed = Result<(BTreeSet<Quad>, usize, usize), LedgerError>;

fn same(a: &Detailed, b: &Detailed) -> bool {
    match (a, b) {
        (Ok(x), Ok(y)) => x == y,
        (Err(x), Err(y)) => {
            std::mem::discriminant(x) == std::mem::discriminant(y) && x.to_string() == y.to_string()
        }
        _ => false,
    }
}

fn show(r: &Detailed) -> String {
    match r {
        Ok((state, bytes, depth)) => {
            format!("Ok({} quads, {bytes} bytes, depth {depth})", state.len())
        }
        Err(e) => format!("Err({e})"),
    }
}

/// Reconstruct `head` through the reference and through every window configuration;
/// every result must equal the reference (state, bytes, depth, or the identical error).
async fn assert_equivalent(
    store: &PostgresLedgerStore,
    head: &CommitId,
    limits: &ReconstructionLimits,
    context: &str,
) -> Detailed {
    let reference = store.workflows().reconstruct_reference(head, limits).await;
    for w in reconstruction_windows() {
        let got = windowed(store, w).reconstruct_detailed(head, limits).await;
        assert!(
            same(&reference, &got),
            "{context}: windows {w:?}: reference {} vs windowed {}",
            show(&reference),
            show(&got)
        );
    }
    reference
}

// ------------------------------------------------------------------------------------------
// Equivalence on well-formed histories
// ------------------------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn windowed_reconstruction_equals_the_scalar_reference_at_every_depth_and_boundary() {
    let store = store_at(&database_url()).await;
    let g = graph(&store).await;
    // 19 commits: shorter than, exactly, and one over every small window above.
    let (ids, states) = constant_state_chain(store.immutable(), &g, 19, 3).await;
    for (depth, (id, want)) in ids.iter().zip(&states).enumerate() {
        let got = assert_equivalent(
            &store,
            id,
            &ReconstructionLimits::DEVELOPMENT,
            &format!("depth {depth}"),
        )
        .await
        .unwrap();
        assert_eq!(&got.0, want, "depth {depth}: state");
        assert_eq!(got.2, depth + 1, "depth {depth}: depth");
    }
    // A merge commit reconstructs along parent 0 whatever the second parent holds.
    let side = Patch::new([add(quad(9, 9))]).unwrap();
    let side_id = publish(
        store.immutable(),
        v2(&g, vec![ids[2].clone()], &side, "side"),
        &side,
    )
    .await;
    let merge_patch = Patch::new([add(quad(7, 7))]).unwrap();
    let merge = publish(
        store.immutable(),
        v2(
            &g,
            vec![ids[18].clone(), side_id.clone()],
            &merge_patch,
            "merge",
        ),
        &merge_patch,
    )
    .await;
    let got = assert_equivalent(&store, &merge, &ReconstructionLimits::DEVELOPMENT, "merge")
        .await
        .unwrap();
    let mut want = states[18].clone();
    want.insert(quad(7, 7));
    assert_eq!(got.0, want);
    assert_eq!(got.2, 20);
    // The second parent's own line is unaffected.
    let got = assert_equivalent(&store, &side_id, &ReconstructionLimits::DEVELOPMENT, "side")
        .await
        .unwrap();
    let mut want = states[2].clone();
    want.insert(quad(9, 9));
    assert_eq!(got.0, want);
    // The limits are enforced identically: the exact depth passes, one less fails with the
    // same wording; the quad and byte limits fail on the same patch with the same wording.
    for (max_depth, ok) in [(20, true), (19, false), (1, false)] {
        let limits = ReconstructionLimits {
            max_depth,
            ..ReconstructionLimits::DEVELOPMENT
        };
        let r = assert_equivalent(&store, &merge, &limits, &format!("max_depth {max_depth}")).await;
        assert_eq!(r.is_ok(), ok, "max_depth {max_depth}: {}", show(&r));
        if !ok {
            assert!(
                matches!(&r, Err(LedgerError::ResourceLimit(m)) if m == &format!("reconstruction depth exceeds {max_depth} commits")),
                "{}",
                show(&r)
            );
        }
    }
    for (limits, needle) in [
        (
            ReconstructionLimits {
                max_quads: 3,
                ..ReconstructionLimits::DEVELOPMENT
            },
            "exceeds 3 quads",
        ),
        (
            ReconstructionLimits {
                max_bytes: 40,
                ..ReconstructionLimits::DEVELOPMENT
            },
            "exceeds 40 bytes",
        ),
    ] {
        let r = assert_equivalent(&store, &merge, &limits, needle).await;
        assert!(
            matches!(&r, Err(LedgerError::ResourceLimit(m)) if m.contains(needle)),
            "{}",
            show(&r)
        );
    }
    // An unknown head, and a patch object requested as a commit, are NotFound in both.
    let absent = CommitId(ContentId::for_bytes(b"no such commit"));
    let r = assert_equivalent(
        &store,
        &absent,
        &ReconstructionLimits::DEVELOPMENT,
        "absent",
    )
    .await;
    assert!(
        matches!(&r, Err(LedgerError::NotFound(c)) if *c == absent.0),
        "{}",
        show(&r)
    );
    let patch_as_head = CommitId(side.id().0.clone());
    let r = assert_equivalent(
        &store,
        &patch_as_head,
        &ReconstructionLimits::DEVELOPMENT,
        "patch",
    )
    .await;
    assert!(matches!(&r, Err(LedgerError::NotFound(_))), "{}", show(&r));
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn windowed_reconstruction_reads_v1_imported_history_and_v2_on_top_of_it() {
    let store = store_at(&database_url()).await;
    let g = graph_with(&store, GraphStatus::Importing).await;
    let bound = PostgresImmutableStore::from_pool_migrated(
        store.pool().clone(),
        V1Binding::BindTo(g.clone()),
    );
    // Twelve v1 commits (no graph in the bytes; indexed under the binding), then four v2.
    let mut state: BTreeSet<Quad> = BTreeSet::new();
    let mut ids: Vec<CommitId> = Vec::new();
    let mut states = Vec::new();
    for i in 0..12u64 {
        let new = quad(0, i);
        let mut ops = vec![add(new.clone())];
        if let Some(old) = state.iter().next().cloned() {
            ops.push(del(old.clone()));
            state.remove(&old);
        }
        state.insert(new);
        let patch = Patch::new(ops).unwrap();
        let id = publish(
            &bound,
            v1(ids.last().cloned().into_iter().collect(), &patch, "v1"),
            &patch,
        )
        .await;
        ids.push(id);
        states.push(state.clone());
    }
    for i in 12..16u64 {
        let new = quad(1, i);
        let patch = Patch::new([add(new.clone())]).unwrap();
        state.insert(new);
        let id = publish(
            &bound,
            v2(&g, vec![ids.last().unwrap().clone()], &patch, "v2"),
            &patch,
        )
        .await;
        ids.push(id);
        states.push(state.clone());
    }
    for (depth, (id, want)) in ids.iter().zip(&states).enumerate() {
        let got = assert_equivalent(
            &store,
            id,
            &ReconstructionLimits::DEVELOPMENT,
            &format!("v1/v2 depth {depth}"),
        )
        .await
        .unwrap();
        assert_eq!(&got.0, want, "depth {depth}");
        assert_eq!(got.2, depth + 1);
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn large_objects_are_served_one_per_window_when_the_byte_budget_is_small() {
    let store = store_at(&database_url()).await;
    let g = graph(&store).await;
    // Four commits whose patches are ≈ 200 KiB each; a 100 KiB budget serves one per
    // window (the first object is always served), a 1-byte budget too, and the result is
    // the same as the reference and as the default windows.
    let mut state = BTreeSet::new();
    let mut parents = vec![];
    let mut head = None;
    for round in 0..4u64 {
        let adds: Vec<Quad> = (0..2_000u64)
            .map(|i| {
                q(&format!(
                    "<urn:big:{round}:{i}> <urn:p> \"{}\" .",
                    "x".repeat(80)
                ))
            })
            .collect();
        state.extend(adds.iter().cloned());
        let patch = Patch::new(adds.into_iter().map(add)).unwrap();
        assert!(patch.canonical_bytes().len() > 150 * 1024);
        let id = publish(
            store.immutable(),
            v2(&g, parents.clone(), &patch, "big"),
            &patch,
        )
        .await;
        parents = vec![id.clone()];
        head = Some(id);
    }
    let head = head.unwrap();
    let reference = store
        .workflows()
        .reconstruct_reference(&head, &ReconstructionLimits::DEVELOPMENT)
        .await
        .unwrap();
    assert_eq!(reference.0, state);
    for w in [
        windows(256, 100 * 1024, 1),
        windows(256, 1, 1),
        windows(2, 1 << 30, 1),
        RetrievalWindows::DEFAULT,
    ] {
        let got = windowed(&store, w)
            .reconstruct_detailed(&head, &ReconstructionLimits::DEVELOPMENT)
            .await
            .unwrap();
        assert_eq!(got, reference, "{w:?}");
    }
}

// ------------------------------------------------------------------------------------------
// Corruption and adversarial matrix: the index is a hint, the bytes decide
// ------------------------------------------------------------------------------------------

/// Owner-only damage to a throwaway database. Write-once triggers and the constraints
/// that make the damage impossible in production are removed first; `ledger-admin verify`
/// classifies every one of these databases as corrupt.
async fn unguard(pool: &PgPool) {
    for sql in [
        "ALTER TABLE immutable_objects DISABLE TRIGGER immutable_objects_write_once",
        "ALTER TABLE commit_index DISABLE TRIGGER commit_index_write_once",
        "ALTER TABLE commit_parents DISABLE TRIGGER commit_parents_write_once",
        "ALTER TABLE immutable_objects DROP CONSTRAINT IF EXISTS immutable_objects_content_addressed",
        "ALTER TABLE commit_index DROP CONSTRAINT IF EXISTS commit_index_id_fkey",
        "ALTER TABLE commit_index DROP CONSTRAINT IF EXISTS commit_index_patch_id_fkey",
        "ALTER TABLE commit_parents DROP CONSTRAINT IF EXISTS commit_parents_commit_id_fkey",
        "ALTER TABLE commit_parents DROP CONSTRAINT IF EXISTS commit_parents_parent_id_fkey",
    ] {
        sqlx::query(sql)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

async fn sql(pool: &PgPool, statement: &str, binds: &[&str]) {
    let mut query = sqlx::query(statement);
    for b in binds {
        query = query.bind(*b);
    }
    query
        .execute(pool)
        .await
        .unwrap_or_else(|e| panic!("{statement}: {e}"));
}

fn sha256_id(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

/// A symbolic bind value of a corruption statement: the id of the i-th commit or patch of
/// the fresh chain.
#[derive(Clone, Copy)]
enum Bind {
    Commit(usize),
    Patch(usize),
    /// A commit indexed under another graph of the same database.
    Foreign,
}

/// A corruption case: what to do to a fresh 8-commit chain, and what both paths must
/// then answer for the head (`Same`), or what each answers when the windowed path is
/// stricter by design (Decision 1 of Plan 0012).
enum Expect {
    /// Reference and windowed agree exactly; the predicate names the shared answer.
    Same(fn(&Detailed) -> bool),
    /// The reference follows the bytes and still succeeds; the windowed path refuses the
    /// contradicted hint as corruption.
    WindowedRefusesContradictedHint,
}

/// A corruption case: its name, the damage (statements with symbolic binds), the expectation.
type Case = (&'static str, Vec<(&'static str, Vec<Bind>)>, Expect);

const MALFORMED_HEAD: &str = "malformed commit bytes under a known header, as the head";
const NON_PATCH: &str = "patch bytes that hash correctly but are not a canonical patch";

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn corrupted_objects_and_index_rows_fail_closed_identically_or_stricter() {
    let cases: Vec<Case> = vec![
        (
            "commit bytes whose digest does not match the requested id",
            vec![(
                "UPDATE immutable_objects SET bytes = bytes || 'x'::bytea WHERE id = $1",
                vec![Bind::Commit(4)],
            )],
            Expect::Same(
                |r| matches!(r, Err(LedgerError::CorruptObject { reason, .. }) if reason == "stored bytes do not hash to id"),
            ),
        ),
        (
            MALFORMED_HEAD,
            vec![],
            Expect::Same(
                |r| matches!(r, Err(LedgerError::CorruptObject { reason, .. }) if reason.starts_with("commit envelope with a known header does not decode")),
            ),
        ),
        (
            "missing commit object (index rows intact)",
            vec![(
                "DELETE FROM immutable_objects WHERE id = $1",
                vec![Bind::Commit(3)],
            )],
            Expect::Same(|r| matches!(r, Err(LedgerError::NotFound(_)))),
        ),
        (
            "missing commit object and its index rows (the child's hint is silent)",
            vec![
                (
                    "DELETE FROM commit_parents WHERE commit_id = $1 OR parent_id = $1",
                    vec![Bind::Commit(3)],
                ),
                (
                    "DELETE FROM commit_index WHERE id = $1",
                    vec![Bind::Commit(3)],
                ),
                (
                    "DELETE FROM immutable_objects WHERE id = $1",
                    vec![Bind::Commit(3)],
                ),
            ],
            Expect::Same(|r| matches!(r, Err(LedgerError::NotFound(_)))),
        ),
        (
            "missing patch object (the oldest missing one is reported)",
            vec![(
                "DELETE FROM immutable_objects WHERE id = $1 OR id = $2",
                vec![Bind::Patch(2), Bind::Patch(5)],
            )],
            Expect::Same(|r| matches!(r, Err(LedgerError::NotFound(_)))),
        ),
        (
            "patch bytes whose digest does not match the patch id",
            vec![(
                "UPDATE immutable_objects SET bytes = bytes || 'x'::bytea WHERE id = $1",
                vec![Bind::Patch(6)],
            )],
            Expect::Same(
                |r| matches!(r, Err(LedgerError::CorruptObject { reason, .. }) if reason == "stored bytes do not hash to id"),
            ),
        ),
        (
            NON_PATCH,
            vec![],
            Expect::Same(|r| matches!(r, Err(LedgerError::InvalidPatch { .. }))),
        ),
        (
            "commit_index.patch_id disagreeing with the bytes (not consulted by either path)",
            vec![(
                "UPDATE commit_index SET patch_id = $2 WHERE id = $1",
                vec![Bind::Commit(4), Bind::Patch(0)],
            )],
            Expect::Same(|r| r.is_ok()),
        ),
        (
            "commit_parents position 0 disagreeing with the bytes",
            vec![(
                "UPDATE commit_parents SET parent_id = $2 WHERE commit_id = $1 AND position = 0",
                vec![Bind::Commit(5), Bind::Commit(1)],
            )],
            Expect::WindowedRefusesContradictedHint,
        ),
        (
            "commit_parents naming a parent for the genesis (an index cycle through the head)",
            vec![(
                "INSERT INTO commit_parents (commit_id, position, parent_id) VALUES ($1, 0, $2)",
                vec![Bind::Commit(0), Bind::Commit(7)],
            )],
            Expect::WindowedRefusesContradictedHint,
        ),
        (
            "commit_parents position 0 row missing (hint silent, bytes followed)",
            vec![(
                "DELETE FROM commit_parents WHERE commit_id = $1",
                vec![Bind::Commit(5)],
            )],
            Expect::Same(|r| r.is_ok()),
        ),
        (
            "parent_count disagreeing with the rows (not consulted by reconstruction)",
            vec![(
                "UPDATE commit_index SET parent_count = 2 WHERE id = $1",
                vec![Bind::Commit(5)],
            )],
            Expect::Same(|r| r.is_ok()),
        ),
        (
            "a foreign-graph commit named as the first parent by the index",
            vec![(
                "UPDATE commit_parents SET parent_id = $2 WHERE commit_id = $1 AND position = 0",
                vec![Bind::Commit(6), Bind::Foreign],
            )],
            Expect::WindowedRefusesContradictedHint,
        ),
    ];
    for (name, damage, expect) in cases {
        let url = fresh_database("corrupt").await;
        let store = store_at(&url).await;
        let g = graph(&store).await;
        let (ids, _) = constant_state_chain(store.immutable(), &g, 8, 2).await;
        // The patches, by commit order, re-read from the verified store.
        let mut patches = Vec::new();
        for id in &ids {
            let commit = store.immutable().get_commit(id).await.unwrap().unwrap();
            let bytes = store
                .immutable()
                .get_content(&commit.patch().0)
                .await
                .unwrap()
                .unwrap();
            patches.push(Patch::from_canonical_bytes(&bytes).unwrap());
        }
        let other = graph(&store).await;
        let foreign_patch = Patch::new([add(quad(77, 7))]).unwrap();
        let foreign = publish(
            store.immutable(),
            v2(&other, vec![], &foreign_patch, "foreign"),
            &foreign_patch,
        )
        .await;
        unguard(store.pool()).await;
        let head = match name {
            MALFORMED_HEAD => {
                let bytes = b"sculpin-cognitive-commit-v2\0this is not an envelope".to_vec();
                let id = sha256_id(&bytes);
                sqlx::query("INSERT INTO immutable_objects (id, bytes) VALUES ($1, $2)")
                    .bind(&id)
                    .bind(&bytes)
                    .execute(store.pool())
                    .await
                    .unwrap();
                id.parse().unwrap()
            }
            NON_PATCH => {
                // A commit whose "patch" is another commit object: well-hashed under its own
                // id, present, and not a canonical patch.
                let bogus = store
                    .immutable()
                    .get_content(&ids[0].0)
                    .await
                    .unwrap()
                    .unwrap();
                let bogus_id = ledger_core::PatchId(ContentId::for_bytes(&bogus));
                let AnyCommit::V2(mut c) = v2(&g, vec![ids[7].clone()], &patches[0], "bogus")
                else {
                    unreachable!()
                };
                c.patch = bogus_id;
                let c = AnyCommit::V2(c);
                let bytes = c.canonical_bytes().unwrap();
                let id = c.id().unwrap();
                sqlx::query("INSERT INTO immutable_objects (id, bytes) VALUES ($1, $2)")
                    .bind(id.to_string())
                    .bind(&bytes)
                    .execute(store.pool())
                    .await
                    .unwrap();
                sql(store.pool(), "INSERT INTO commit_index (id, graph_id, version, patch_id, parent_count) VALUES ($1, $2, 2, $3, 1)", &[&id.to_string(), g.as_str(), &ids[0].to_string()]).await;
                sql(store.pool(), "INSERT INTO commit_parents (commit_id, position, parent_id) VALUES ($1, 0, $2)", &[&id.to_string(), &ids[7].to_string()]).await;
                id
            }
            _ => ids[7].clone(),
        };
        for (statement, binds) in &damage {
            let values: Vec<String> = binds
                .iter()
                .map(|b| match *b {
                    Bind::Commit(i) => ids[i].to_string(),
                    Bind::Patch(i) => patches[i].id().to_string(),
                    Bind::Foreign => foreign.to_string(),
                })
                .collect();
            let values: Vec<&str> = values.iter().map(String::as_str).collect();
            sql(store.pool(), statement, &values).await;
        }
        let limits = ReconstructionLimits::DEVELOPMENT;
        let reference = store
            .workflows()
            .reconstruct_reference(&head, &limits)
            .await;
        for w in reconstruction_windows() {
            let got = windowed(&store, w)
                .reconstruct_detailed(&head, &limits)
                .await;
            match &expect {
                Expect::Same(predicate) => {
                    assert!(
                        predicate(&reference),
                        "{name}: reference {}",
                        show(&reference)
                    );
                    assert!(
                        same(&reference, &got),
                        "{name}: windows {w:?}: reference {} vs windowed {}",
                        show(&reference),
                        show(&got)
                    );
                }
                Expect::WindowedRefusesContradictedHint => {
                    assert!(reference.is_ok(), "{name}: reference {}", show(&reference));
                    assert!(
                        matches!(&got, Err(LedgerError::CorruptObject { reason, .. }) if reason == "commit_parents position 0 disagrees with the commit bytes"),
                        "{name}: windows {w:?}: {}",
                        show(&got)
                    );
                }
            }
        }
    }
}

// ------------------------------------------------------------------------------------------
// DAG retrieval: windowed `GraphParents` against the unwindowed walk
// ------------------------------------------------------------------------------------------

fn actor(principal: &str) -> AuthenticatedPrincipal {
    AuthenticatedPrincipal {
        principal_id: PrincipalId::new(format!("urn:it:{principal}")).unwrap(),
        principal_type: PrincipalType::Agent,
        tenant_id: tenant(),
        on_behalf_of: None,
    }
}

fn scope(graph: &GraphId, key: &str) -> RequestScope {
    RequestScope {
        principal: actor("curator"),
        graph: graph.clone(),
        idempotency_key: key.to_owned(),
        request_digest: ContentId::for_bytes(key.as_bytes()),
        correlation_id: None,
    }
}

/// Prepare + accept one patch on `branch` through `repo`; the new head.
async fn change(
    repo: &WorkflowRepository,
    g: &GraphId,
    branch: &str,
    head: Option<CommitId>,
    adds: &[Quad],
    deletes: &[Quad],
) -> CommitId {
    let key = unique("c");
    let ops = adds
        .iter()
        .cloned()
        .map(add)
        .chain(deletes.iter().cloned().map(del));
    let prepared = repo
        .prepare(&PrepareRequest {
            scope: scope(g, &format!("p-{key}")),
            branch: branch.into(),
            expected_head: head.clone(),
            requested: Patch::new(ops).unwrap(),
            activity: "cognitive-correction".into(),
            event_time: None,
            evidence_refs: vec![],
            source_system: None,
            message: key.clone(),
        })
        .await
        .unwrap();
    repo.accept(&AcceptRequest {
        scope: scope(g, &format!("a-{key}")),
        branch: branch.into(),
        expected_head: head,
        candidate: prepared.candidate.clone(),
        reason: None,
        validation: ValidationPolicy::NoValidation,
    })
    .await
    .unwrap();
    prepared.candidate
}

async fn branch_from(
    repo: &WorkflowRepository,
    g: &GraphId,
    name: &str,
    source: &str,
    from: Option<&CommitId>,
) {
    repo.create_branch(
        &CreateBranchRequest {
            scope: scope(g, &format!("cb-{name}-{}", unique(""))),
            name: name.into(),
            source: source.into(),
            from_commit: from.cloned(),
            policy: BranchPolicy::default(),
        },
        TraversalLimits::DEFAULT,
    )
    .await
    .unwrap();
}

fn spec(source: &str, target: &str) -> MergeSpec {
    MergeSpec {
        source: source.into(),
        target: target.into(),
        strategy: MergeStrategy::Abort,
        base: None,
    }
}

/// Everything a preview exposes.
fn preview_view(p: &MergePreview) -> String {
    format!(
        "{:?} base={:?} explicit={} ahead={} behind={} t={:?} s={:?} conflicts={:?} n={} trunc={} strategy={:?} digest={:?} token={:?}",
        p.class,
        p.merge_base,
        p.base_explicit,
        p.ahead,
        p.behind,
        p.target_delta,
        p.source_delta,
        p.conflicts,
        p.conflict_count,
        p.conflicts_truncated,
        p.strategy,
        p.merged_state_digest,
        p.preview_token
    )
}

fn ancestry_windows() -> Vec<usize> {
    vec![1, 2, 3, 4, 5, 7, 8, 256]
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn windowed_ancestry_gives_the_same_histories_branch_points_and_merge_previews() {
    let store = store_at(&database_url()).await;
    let g = graph(&store).await;
    let repo = store.workflows();
    // main: m1..m9 (9 deep). feature from m3: f1..f5. criss from m6: c1, c2; cross from f2: x1.
    // Then two-parent merges: feature merges main (m9) → f6 = [f5, m9]; main merges feature at
    // f4 → m10 = [m9, f4] (criss-cross between main and feature afterwards); and a merge-heavy
    // branch `heavy` that merges main repeatedly.
    let mut main = Vec::new();
    let mut head = None;
    for i in 0..9u64 {
        let c = change(
            repo,
            &g,
            "main",
            head.clone(),
            &[quad(100 + i as usize, i)],
            &[],
        )
        .await;
        head = Some(c.clone());
        main.push(c);
    }
    branch_from(repo, &g, "feature", "main", Some(&main[2])).await;
    let mut feature = Vec::new();
    let mut fhead = Some(main[2].clone());
    for i in 0..5u64 {
        let c = change(
            repo,
            &g,
            "feature",
            fhead.clone(),
            &[quad(200 + i as usize, i)],
            &[],
        )
        .await;
        fhead = Some(c.clone());
        feature.push(c);
    }
    branch_from(repo, &g, "criss", "main", Some(&main[5])).await;
    let c1 = change(
        repo,
        &g,
        "criss",
        Some(main[5].clone()),
        &[quad(300, 1)],
        &[],
    )
    .await;
    let _c2 = change(repo, &g, "criss", Some(c1.clone()), &[quad(301, 1)], &[]).await;
    branch_from(repo, &g, "cross", "feature", Some(&feature[1])).await;
    let _x1 = change(
        repo,
        &g,
        "cross",
        Some(feature[1].clone()),
        &[quad(400, 1)],
        &[],
    )
    .await;
    branch_from(repo, &g, "contained", "main", Some(&main[7])).await;
    branch_from(repo, &g, "same", "main", None).await;
    branch_from(repo, &g, "ahead", "main", None).await;
    let _a1 = change(
        repo,
        &g,
        "ahead",
        Some(main[8].clone()),
        &[quad(500, 1)],
        &[],
    )
    .await;
    // Two-parent integration commits through the merge path (feature ← main).
    let p = repo
        .merge_preview(
            &tenant(),
            &g,
            &spec("main", "feature"),
            TraversalLimits::DEFAULT,
        )
        .await
        .unwrap();
    assert_eq!(p.class, MergeClass::Divergent);
    let proposed = repo
        .merge_propose(
            &ledger_store::ProposeMergeRequest {
                scope: scope(&g, "mp-1"),
                spec: spec("main", "feature"),
                preview_token: p.preview_token.clone().unwrap(),
                message: "integrate main".into(),
                evidence_refs: vec![],
            },
            TraversalLimits::DEFAULT,
        )
        .await
        .unwrap();
    repo.merge_apply(&ledger_store::ApplyMergeRequest {
        scope: scope(&g, "ma-1"),
        proposal_id: proposed.proposal_id,
        preview_token: p.preview_token.clone().unwrap(),
        reason: None,
        validation: ValidationPolicy::NoValidation,
    })
    .await
    .unwrap();
    // A criss-cross: main now merges the pre-integration feature head f4 → ambiguous later.
    branch_from(repo, &g, "feature-old", "feature", Some(&feature[3])).await;
    let p = repo
        .merge_preview(
            &tenant(),
            &g,
            &spec("feature-old", "main"),
            TraversalLimits::DEFAULT,
        )
        .await
        .unwrap();
    assert_eq!(p.class, MergeClass::Divergent);
    let proposed = repo
        .merge_propose(
            &ledger_store::ProposeMergeRequest {
                scope: scope(&g, "mp-2"),
                spec: spec("feature-old", "main"),
                preview_token: p.preview_token.clone().unwrap(),
                message: "integrate feature-old".into(),
                evidence_refs: vec![],
            },
            TraversalLimits::DEFAULT,
        )
        .await
        .unwrap();
    repo.merge_apply(&ledger_store::ApplyMergeRequest {
        scope: scope(&g, "ma-2"),
        proposal_id: proposed.proposal_id,
        preview_token: p.preview_token.clone().unwrap(),
        reason: None,
        validation: ValidationPolicy::NoValidation,
    })
    .await
    .unwrap();

    let pairs = [
        ("feature", "main"),
        ("main", "feature"),
        ("criss", "main"),
        ("cross", "feature"),
        ("cross", "main"),
        ("contained", "main"),
        ("main", "contained"),
        ("same", "main"),
        ("ahead", "main"),
        ("main", "ahead"),
        ("feature-old", "main"),
        ("criss", "cross"),
    ];
    let branches = [
        "main",
        "feature",
        "criss",
        "cross",
        "contained",
        "same",
        "ahead",
        "feature-old",
    ];
    let reference = windowed(&store, windows(256, 8 << 20, 1));
    for w in ancestry_windows() {
        let wide = windowed(&store, windows(256, 8 << 20, w));
        for (source, target) in pairs {
            for strategy in [
                MergeStrategy::Abort,
                MergeStrategy::TakeSource,
                MergeStrategy::TakeTarget,
                MergeStrategy::Union,
            ] {
                let mut s = spec(source, target);
                s.strategy = strategy;
                let want = reference
                    .merge_preview(&tenant(), &g, &s, TraversalLimits::DEFAULT)
                    .await;
                let got = wide
                    .merge_preview(&tenant(), &g, &s, TraversalLimits::DEFAULT)
                    .await;
                match (&want, &got) {
                    (Ok(a), Ok(b)) => assert_eq!(
                        preview_view(a),
                        preview_view(b),
                        "window {w}: {source} → {target} {strategy:?}"
                    ),
                    (Err(a), Err(b)) => assert_eq!(
                        a.to_string(),
                        b.to_string(),
                        "window {w}: {source} → {target}"
                    ),
                    _ => panic!("window {w}: {source} → {target}: {want:?} vs {got:?}"),
                }
            }
        }
        for b in branches {
            let head = reference
                .branch(&tenant(), &g, b)
                .await
                .unwrap()
                .unwrap()
                .head;
            for max in [1usize, 2, 3, 4, 7, 8, 100] {
                let want = reference
                    .first_parent_history(&tenant(), &g, &head, max, TraversalLimits::DEFAULT)
                    .await
                    .unwrap();
                let got = wide
                    .first_parent_history(&tenant(), &g, &head, max, TraversalLimits::DEFAULT)
                    .await
                    .unwrap();
                assert_eq!(want, got, "window {w}: history of {b} max {max}");
            }
            // The visit limit counts entered commits in both.
            let tight = TraversalLimits {
                max_visited: 3,
                deadline: None,
            };
            let want = reference
                .first_parent_history(&tenant(), &g, &head, 100, tight)
                .await;
            let got = wide
                .first_parent_history(&tenant(), &g, &head, 100, tight)
                .await;
            assert_eq!(
                want.map_err(|e| e.to_string()),
                got.map_err(|e| e.to_string()),
                "window {w}: tight history of {b}"
            );
            let want = reference
                .merge_preview(&tenant(), &g, &spec(b, "cross"), tight)
                .await
                .map_err(|e| e.to_string());
            let got = wide
                .merge_preview(&tenant(), &g, &spec(b, "cross"), tight)
                .await
                .map_err(|e| e.to_string());
            assert_eq!(
                want.as_ref().map(preview_view),
                got.as_ref().map(preview_view),
                "window {w}: tight preview {b} → cross"
            );
        }
        // Historical branch points: reachable from the source head or refused, identically.
        for (i, point) in main.iter().chain(feature.iter()).enumerate() {
            for source in ["main", "feature", "criss"] {
                let name = format!("bp-{w}-{i}-{source}");
                let want = reference
                    .create_branch(
                        &CreateBranchRequest {
                            scope: scope(&g, &format!("ref-{name}")),
                            name: format!("ref-{name}"),
                            source: source.into(),
                            from_commit: Some(point.clone()),
                            policy: BranchPolicy::default(),
                        },
                        TraversalLimits::DEFAULT,
                    )
                    .await
                    .map(|o| o.event.head)
                    .map_err(|e| e.to_string());
                let got = wide
                    .create_branch(
                        &CreateBranchRequest {
                            scope: scope(&g, &format!("win-{name}")),
                            name: format!("win-{name}"),
                            source: source.into(),
                            from_commit: Some(point.clone()),
                            policy: BranchPolicy::default(),
                        },
                        TraversalLimits::DEFAULT,
                    )
                    .await
                    .map(|o| o.event.head)
                    .map_err(|e| e.to_string());
                assert_eq!(want, got, "window {w}: branch point {i} from {source}");
            }
        }
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn corrupted_parent_rows_fail_ancestry_walks_identically_in_every_window() {
    // Each case damages a fresh graph's index rows; the windowed walk must answer exactly
    // what the unwindowed one does (an error of the same kind and wording, or the same
    // success).
    let cases: Vec<(&str, &str, &[&str])> = vec![
        (
            "parent_count disagreement",
            "UPDATE commit_index SET parent_count = 2 WHERE id = $1",
            &["m5"],
        ),
        (
            "non-contiguous parent positions",
            "UPDATE commit_parents SET position = 1 WHERE commit_id = $1 AND position = 0",
            &["m5"],
        ),
        (
            "missing indexed parent row",
            "DELETE FROM commit_parents WHERE commit_id = $1",
            &["m5"],
        ),
        (
            "foreign-graph indexed parent",
            "UPDATE commit_parents SET parent_id = $2 WHERE commit_id = $1 AND position = 0",
            &["m3", "FOREIGN"],
        ),
        (
            "index cycle (genesis' parent is the head)",
            "INSERT INTO commit_parents (commit_id, position, parent_id) VALUES ($1, 0, $2)",
            &["m1", "m8"],
        ),
        (
            "index cycle through a second parent",
            "INSERT INTO commit_parents (commit_id, position, parent_id) VALUES ($1, 1, $2)",
            &["m4", "m7"],
        ),
    ];
    for (name, statement, args) in cases {
        let url = fresh_database("dagcorrupt").await;
        let store = store_at(&url).await;
        let g = graph(&store).await;
        let repo = store.workflows();
        let mut main = Vec::new();
        let mut head = None;
        for i in 0..8u64 {
            let c = change(repo, &g, "main", head.clone(), &[quad(i as usize, i)], &[]).await;
            head = Some(c.clone());
            main.push(c);
        }
        branch_from(repo, &g, "side", "main", Some(&main[1])).await;
        let s1 = change(repo, &g, "side", Some(main[1].clone()), &[quad(50, 1)], &[]).await;
        // A commit of another graph in the same database, for the foreign-parent case.
        let other = graph(&store).await;
        let foreign_patch = Patch::new([add(quad(77, 7))]).unwrap();
        let foreign = publish(
            store.immutable(),
            v2(&other, vec![], &foreign_patch, "foreign"),
            &foreign_patch,
        )
        .await;
        unguard(store.pool()).await;
        let by_name = |n: &str| -> String {
            if n == "FOREIGN" {
                return foreign.to_string();
            }
            let i: usize = n[1..].parse().unwrap();
            main[i - 1].to_string()
        };
        let binds: Vec<String> = args.iter().map(|a| by_name(a)).collect();
        let binds: Vec<&str> = binds.iter().map(String::as_str).collect();
        if name == "index cycle through a second parent" {
            sql(
                store.pool(),
                "UPDATE commit_index SET parent_count = 2 WHERE id = $1",
                &[binds[0]],
            )
            .await;
        }
        sql(store.pool(), statement, &binds).await;
        let reference = windowed(&store, windows(256, 8 << 20, 1));
        for w in ancestry_windows() {
            let wide = windowed(&store, windows(256, 8 << 20, w));
            for (source, target) in [("side", "main"), ("main", "side")] {
                let want = reference
                    .merge_preview(
                        &tenant(),
                        &g,
                        &spec(source, target),
                        TraversalLimits::DEFAULT,
                    )
                    .await;
                let got = wide
                    .merge_preview(
                        &tenant(),
                        &g,
                        &spec(source, target),
                        TraversalLimits::DEFAULT,
                    )
                    .await;
                let (want, got) = (
                    want.map(|p| preview_view(&p)).map_err(|e| e.to_string()),
                    got.map(|p| preview_view(&p)).map_err(|e| e.to_string()),
                );
                assert_eq!(want, got, "{name}: window {w}: preview {source} → {target}");
            }
            for head in [main[7].clone(), s1.clone()] {
                let want = reference
                    .first_parent_history(&tenant(), &g, &head, 100, TraversalLimits::DEFAULT)
                    .await
                    .map_err(|e| e.to_string());
                let got = wide
                    .first_parent_history(&tenant(), &g, &head, 100, TraversalLimits::DEFAULT)
                    .await
                    .map_err(|e| e.to_string());
                assert_eq!(want, got, "{name}: window {w}: history");
            }
            let point = main[0].clone();
            let want = reference
                .create_branch(
                    &CreateBranchRequest {
                        scope: scope(&g, &format!("r-{w}-{name}")),
                        name: format!("r-{w}"),
                        source: "main".into(),
                        from_commit: Some(point.clone()),
                        policy: BranchPolicy::default(),
                    },
                    TraversalLimits::DEFAULT,
                )
                .await
                .map(|o| o.event.head)
                .map_err(|e| e.to_string());
            let got = wide
                .create_branch(
                    &CreateBranchRequest {
                        scope: scope(&g, &format!("w-{w}-{name}")),
                        name: format!("w-{w}"),
                        source: "main".into(),
                        from_commit: Some(point),
                        policy: BranchPolicy::default(),
                    },
                    TraversalLimits::DEFAULT,
                )
                .await
                .map(|o| o.event.head)
                .map_err(|e| e.to_string());
            assert_eq!(want, got, "{name}: window {w}: branch point");
        }
        // Every case except the silent ones is an error in both.
        let r = reference
            .merge_preview(
                &tenant(),
                &g,
                &spec("side", "main"),
                TraversalLimits::DEFAULT,
            )
            .await;
        match name {
            "foreign-graph indexed parent"
            | "parent_count disagreement"
            | "non-contiguous parent positions"
            | "missing indexed parent row"
            | "index cycle (genesis' parent is the head)"
            | "index cycle through a second parent" => {
                assert!(
                    matches!(&r, Err(LedgerError::CorruptObject { .. })),
                    "{name}: {r:?}"
                );
            }
            _ => unreachable!(),
        }
    }
}

// ------------------------------------------------------------------------------------------
// Connection ownership and statement counts
// ------------------------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn prepare_reconstructs_on_its_own_transaction_connection_only() {
    // A pool of exactly one connection: the workflow transaction holds it, so any helper
    // that acquired a second connection for the windowed reconstruction would wait forever
    // (bounded here by the acquire timeout, which would surface as an error).
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&database_url())
        .await
        .unwrap();
    let store = PostgresLedgerStore::from_pool_migrated(pool.clone(), V1Binding::Reject);
    let g = graph(&store).await;
    let repo = WorkflowRepository::new(
        pool.clone(),
        PostgresImmutableStore::from_pool_migrated(pool.clone(), V1Binding::Reject),
    )
    .with_retrieval_windows(windows(3, 8 << 20, 2));
    let mut head = None;
    for i in 0..10u64 {
        let c = change(&repo, &g, "main", head.clone(), &[quad(i as usize, i)], &[]).await;
        head = Some(c);
    }
    let state = repo
        .reconstruct(head.as_ref().unwrap(), &ReconstructionLimits::DEVELOPMENT)
        .await
        .unwrap();
    assert_eq!(state.len(), 10);
    // Historical branch creation walks the DAG on its own pooled connection as before, and
    // merge preview reconstructs three states on one pooled connection.
    branch_from(
        &repo,
        &g,
        "b",
        "main",
        Some(&state.iter().next().map(|_| head.clone().unwrap()).unwrap()),
    )
    .await;
    let _ = change(&repo, &g, "b", head.clone(), &[quad(99, 1)], &[]).await;
    let p = repo
        .merge_preview(&tenant(), &g, &spec("b", "main"), TraversalLimits::DEFAULT)
        .await
        .unwrap();
    assert_eq!(p.class, MergeClass::FastForward);
}

thread_local! {
    static STATEMENTS: Cell<usize> = const { Cell::new(0) };
}

struct CountStatements;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CountStatements {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if event.metadata().target() == "sqlx::query" {
            STATEMENTS.with(|c| c.set(c.get() + 1));
        }
    }
}

/// Count the statements sqlx executes on this thread during `f` (a current-thread runtime
/// runs the future on the test thread, so the thread-local is exact for this test).
fn install_counter() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        use tracing_subscriber::layer::SubscriberExt;
        let subscriber = tracing_subscriber::registry()
            .with(
                tracing_subscriber::filter::Targets::new()
                    .with_target("sqlx::query", tracing::Level::DEBUG),
            )
            .with(CountStatements);
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

async fn counted<F: std::future::Future>(f: F) -> (F::Output, usize) {
    STATEMENTS.with(|c| c.set(0));
    let out = f.await;
    (out, STATEMENTS.with(Cell::get))
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn statement_counts_scale_with_windows_not_with_depth() {
    install_counter();
    let store = store_at(&database_url()).await;
    let g = graph(&store).await;
    const DEPTH: usize = 1_000;
    let (ids, states) = constant_state_chain(store.immutable(), &g, DEPTH, 5).await;
    let head = ids.last().unwrap().clone();
    let limits = ReconstructionLimits::DEVELOPMENT;
    // The scalar reference: 2 per ancestor.
    let (state, n) = counted(store.workflows().reconstruct_reference(&head, &limits)).await;
    assert_eq!(state.unwrap().0, states[DEPTH - 1]);
    assert_eq!(n, 2 * DEPTH, "reference");
    // Windowed: 2 × ceil(depth / objects), whatever the window; a byte cut that serves one
    // object per window is the same count as objects = 1.
    for (w, want) in [
        (RetrievalWindows::DEFAULT, 2 * DEPTH.div_ceil(256)),
        (windows(100, 8 << 20, 1), 2 * DEPTH.div_ceil(100)),
        (windows(1, 8 << 20, 1), 2 * DEPTH),
        (windows(256, 1, 1), 2 * DEPTH),
        (windows(10_000, 8 << 20, 1), 2),
    ] {
        let (state, n) = counted(windowed(&store, w).reconstruct(&head, &limits)).await;
        assert_eq!(state.unwrap(), states[DEPTH - 1]);
        assert_eq!(n, want, "{w:?}");
    }
    // Shallow histories: depth 1 is one chain window and one patch window.
    let (state, n) = counted(store.workflows().reconstruct(&ids[0], &limits)).await;
    assert_eq!(state.unwrap(), states[0]);
    assert_eq!(n, 2);
    // First-parent history (one constant readable-graph statement, then the windows).
    let (h, constant) = counted(store.workflows().first_parent_history(
        &tenant(),
        &g,
        &ids[0],
        1,
        TraversalLimits::DEFAULT,
    ))
    .await;
    assert_eq!(h.unwrap(), vec![ids[0].clone()]);
    for (w, want) in [
        (1usize, 2 * DEPTH),
        (256, DEPTH.div_ceil(256)),
        (100, DEPTH.div_ceil(100)),
        (10_000, 1),
    ] {
        let repo = windowed(&store, windows(256, 8 << 20, w));
        let (h, n) = counted(repo.first_parent_history(
            &tenant(),
            &g,
            &head,
            DEPTH,
            TraversalLimits::DEFAULT,
        ))
        .await;
        let h = h.unwrap();
        assert_eq!(h.len(), DEPTH);
        assert_eq!(h.first(), Some(&head));
        assert_eq!(n, constant - 1 + want, "ancestry window {w}");
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn merge_preview_statements_scale_with_ancestry_windows_on_a_deep_linear_history() {
    install_counter();
    let store = store_at(&database_url()).await;
    let g = graph(&store).await;
    let repo = store.workflows();
    // main: 300 deep; `behind` is main at depth 100 (contained), `fork` branches at 100 and
    // adds one commit (divergent: ancestry plus three reconstructions).
    const N: usize = 300;
    const AT: usize = 100;
    let mut main = Vec::new();
    let mut head = None;
    for i in 0..N as u64 {
        let c = change(
            repo,
            &g,
            "main",
            head.clone(),
            &[quad(i as usize % 7, i)],
            &[quad(i as usize % 7, i.wrapping_sub(7))]
                .iter()
                .filter(|_| i >= 7)
                .cloned()
                .collect::<Vec<_>>(),
        )
        .await;
        head = Some(c.clone());
        main.push(c);
    }
    branch_from(repo, &g, "behind", "main", Some(&main[AT - 1])).await;
    branch_from(repo, &g, "fork", "main", Some(&main[AT - 1])).await;
    let _f = change(
        repo,
        &g,
        "fork",
        Some(main[AT - 1].clone()),
        &[quad(900, 1)],
        &[],
    )
    .await;
    let contained = |w: usize| windowed(&store, windows(256, 8 << 20, w));
    // Contained: the walk of both sides and nothing else. The constant part is measured
    // with the unwindowed reference at the same depths subtracted by its known 2-per-commit
    // walk.
    let (p, n1) = counted(contained(1).merge_preview(
        &tenant(),
        &g,
        &spec("behind", "main"),
        TraversalLimits::DEFAULT,
    ))
    .await;
    assert_eq!(p.unwrap().class, MergeClass::AlreadyContained);
    let constant = n1 - 2 * N - 2 * AT;
    for (w, want) in [
        (256usize, N.div_ceil(256) + AT.div_ceil(256)),
        (100, N.div_ceil(100) + AT.div_ceil(100)),
        (7, N.div_ceil(7) + AT.div_ceil(7)),
    ] {
        let (p, n) = counted(contained(w).merge_preview(
            &tenant(),
            &g,
            &spec("behind", "main"),
            TraversalLimits::DEFAULT,
        ))
        .await;
        assert_eq!(p.unwrap().class, MergeClass::AlreadyContained);
        assert_eq!(n, constant + want, "contained, ancestry window {w}");
    }
    // Divergent: the walk plus three reconstructions (base at AT, target at N, source at AT + 1).
    let (p, n1) = counted(windowed(&store, windows(1, 8 << 20, 1)).merge_preview(
        &tenant(),
        &g,
        &spec("fork", "main"),
        TraversalLimits::DEFAULT,
    ))
    .await;
    let p1 = p.unwrap();
    assert_eq!(p1.class, MergeClass::Divergent);
    let constant = n1 - (2 * N + 2 * (AT + 1)) - 2 * (AT + N + AT + 1);
    for (objects, ancestry) in [(256usize, 256usize), (100, 7), (50, 50)] {
        let (p, n) = counted(
            windowed(&store, windows(objects, 8 << 20, ancestry)).merge_preview(
                &tenant(),
                &g,
                &spec("fork", "main"),
                TraversalLimits::DEFAULT,
            ),
        )
        .await;
        let p = p.unwrap();
        assert_eq!(preview_view(&p), preview_view(&p1));
        let walk = N.div_ceil(ancestry) + (AT + 1).div_ceil(ancestry);
        let recon = 2 * (AT.div_ceil(objects) + N.div_ceil(objects) + (AT + 1).div_ceil(objects));
        assert_eq!(
            n,
            constant + walk + recon,
            "divergent, objects {objects} ancestry {ancestry}"
        );
    }
}

// ------------------------------------------------------------------------------------------
// M0 diagnostics: query plans of the window statements (printed, not asserted)
// ------------------------------------------------------------------------------------------

#[tokio::test]
#[ignore = "diagnostic: prints EXPLAIN (ANALYZE, BUFFERS) of the window queries; run with --nocapture"]
async fn explain_window_queries() {
    let url = fresh_database("explain").await;
    let store = store_at(&url).await;
    let g = graph(&store).await;
    let (ids, _) = constant_state_chain(store.immutable(), &g, 3_000, 1_000).await;
    let head = ids.last().unwrap().clone();
    sqlx::query("VACUUM (ANALYZE)")
        .execute(store.pool())
        .await
        .unwrap();
    let chain = "EXPLAIN (ANALYZE, BUFFERS) WITH RECURSIVE chain(depth, id) AS ( \
         SELECT 0::bigint, $1::text UNION ALL \
         SELECT c.depth + 1, p.parent_id FROM chain c JOIN commit_parents p ON p.commit_id = c.id AND p.position = 0 \
         WHERE c.depth < $2 ), hinted AS ( SELECT depth, id, lead(id) OVER (ORDER BY depth) AS next_hint FROM chain ), \
         served AS ( SELECT h.depth, h.id, h.next_hint, o.bytes, sum(octet_length(o.bytes)) OVER (ORDER BY h.depth ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING) AS before \
         FROM hinted h LEFT JOIN immutable_objects o ON o.id = h.id WHERE h.depth < $2 ) \
         SELECT depth, id, next_hint, bytes FROM served WHERE before IS NULL OR before < $3 ORDER BY depth";
    for k in [1i64, 64, 256, 1024] {
        let rows: Vec<String> = sqlx::query_scalar(chain)
            .bind(head.to_string())
            .bind(k)
            .bind(8_i64 << 20)
            .fetch_all(store.pool())
            .await
            .unwrap();
        println!("\n== chain window K={k}\n{}", rows.join("\n"));
    }
    let objects = "EXPLAIN (ANALYZE, BUFFERS) WITH req AS ( SELECT r.id, r.ord FROM unnest($1::text[]) WITH ORDINALITY AS r(id, ord) ), \
         sized AS ( SELECT r.ord, r.id, o.bytes, sum(octet_length(o.bytes)) OVER (ORDER BY r.ord ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING) AS before \
         FROM req r LEFT JOIN immutable_objects o ON o.id = r.id ) SELECT ord, id, bytes FROM sized WHERE before IS NULL OR before < $2 ORDER BY ord";
    let patch_ids: Vec<String> = {
        let mut v = Vec::new();
        for id in &ids[..256] {
            v.push(
                store
                    .immutable()
                    .get_commit(id)
                    .await
                    .unwrap()
                    .unwrap()
                    .patch()
                    .to_string(),
            );
        }
        v
    };
    let rows: Vec<String> = sqlx::query_scalar(objects)
        .bind(&patch_ids)
        .bind(8_i64 << 20)
        .fetch_all(store.pool())
        .await
        .unwrap();
    println!("\n== object window 256 patches\n{}", rows.join("\n"));
    let ancestry = "EXPLAIN (ANALYZE, BUFFERS) WITH RECURSIVE reach(id, depth) AS ( \
         SELECT a.id, 0::bigint FROM commit_index a WHERE a.id = $1 AND a.graph_id = $2 UNION \
         SELECT p.parent_id, r.depth + 1 FROM reach r JOIN commit_parents p ON p.commit_id = r.id AND (NOT $5 OR p.position = 0) \
         JOIN commit_index ci ON ci.id = p.parent_id AND ci.graph_id = $2 WHERE r.depth + 1 < $3 ), \
         nearest AS ( SELECT DISTINCT ON (id) id, depth FROM reach ORDER BY id, depth ), w AS ( SELECT id FROM nearest ORDER BY depth, id LIMIT $4 ) \
         SELECT c.id, c.parent_count, p.position, p.parent_id FROM w JOIN commit_index c ON c.id = w.id AND c.graph_id = $2 \
         LEFT JOIN commit_parents p ON p.commit_id = c.id ORDER BY c.id, p.position";
    for (k, fp) in [(256i64, false), (256, true), (1024, false)] {
        let rows: Vec<String> = sqlx::query_scalar(ancestry)
            .bind(head.to_string())
            .bind(g.as_str())
            .bind(k)
            .bind(k)
            .bind(fp)
            .fetch_all(store.pool())
            .await
            .unwrap();
        println!(
            "\n== ancestry window K={k} first_parent_only={fp}\n{}",
            rows.join("\n")
        );
    }
    // Scalar reference statements for comparison.
    for s in [
        "EXPLAIN (ANALYZE, BUFFERS) SELECT bytes FROM immutable_objects WHERE id = $1",
        "EXPLAIN (ANALYZE, BUFFERS) SELECT parent_count FROM commit_index WHERE id = $1 AND graph_id = 'x'",
        "EXPLAIN (ANALYZE, BUFFERS) SELECT position, parent_id FROM commit_parents WHERE commit_id = $1 ORDER BY position",
    ] {
        let rows: Vec<String> = sqlx::query_scalar(s)
            .bind(head.to_string())
            .fetch_all(store.pool())
            .await
            .unwrap();
        println!("\n== scalar: {}\n{}", &s[25..], rows.join("\n"));
    }
    let _ = IGNORE;
}
