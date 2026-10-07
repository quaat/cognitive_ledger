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

/// Drop a throwaway database (every pool to it must be closed first).
async fn drop_database(url: &str) {
    let base = database_url();
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&base)
        .await
        .unwrap();
    let head = url.split_once('?').map_or(url, |(h, _)| h);
    let name = &head[head.rfind('/').unwrap() + 1..];
    sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
        .execute(&admin)
        .await
        .unwrap();
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
    constant_state_chain_v(immutable, graph, n, slots, false).await
}

/// [`constant_state_chain`] with v1 envelopes when `v1` (the store must bind v1 commits).
async fn constant_state_chain_v(
    immutable: &PostgresImmutableStore,
    graph: &GraphId,
    n: usize,
    slots: usize,
    v1_envelopes: bool,
) -> (Vec<CommitId>, Vec<BTreeSet<Quad>>) {
    assert!(n >= 1 && slots >= 1);
    let envelope = |parents: Vec<CommitId>, patch: &Patch, message: &str| {
        if v1_envelopes {
            v1(parents, patch, message)
        } else {
            v2(graph, parents, patch, message)
        }
    };
    let mut values: Vec<u64> = (0..slots as u64).collect();
    let mut next = slots as u64;
    let mut state: BTreeSet<Quad> = (0..slots).map(|s| quad(s, values[s])).collect();
    let genesis = Patch::new(state.iter().cloned().map(add)).unwrap();
    let mut ids = vec![publish(immutable, envelope(vec![], &genesis, "genesis"), &genesis).await];
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
            envelope(vec![ids.last().unwrap().clone()], &patch, "step"),
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
/// that make the damage impossible in production are removed first; such a database is
/// outside the ledger's correctness model (`PostgresImmutableStore::verify_commit_index`
/// re-derives every index row from the bytes and reports the disagreements).
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

/// Insert a raw object under its own digest (the content-addressed constraint still holds).
async fn insert_raw(pool: &PgPool, bytes: &[u8]) -> String {
    let id = sha256_id(bytes);
    sqlx::query("INSERT INTO immutable_objects (id, bytes) VALUES ($1, $2)")
        .bind(&id)
        .bind(bytes)
        .execute(pool)
        .await
        .unwrap();
    id
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

/// Which commit a case reconstructs.
#[derive(Clone, Copy)]
enum Head {
    /// The newest commit of the chain.
    Top,
    /// A commit-family object with a known header that does not decode.
    Malformed,
    /// A commit-family object with a header this build does not know.
    UnknownVersion,
    /// A valid commit on the chain whose patch id names a well-hashed non-patch object.
    NonPatch,
    /// A valid commit whose parent 0 is a malformed envelope, placed as the i-th ancestor
    /// so it lands at a window boundary for small windows.
    MalformedParentAt(usize),
}

/// What both paths must answer.
enum Expect {
    /// Reference and windowed agree exactly; the predicate names the shared answer.
    Same(fn(&Detailed) -> bool),
    /// The reference answers as the predicate says (it follows the bytes); the windowed
    /// path refuses the contradicted hint as corruption, blaming the commit whose bytes
    /// the index contradicts (Plan 0012 Decision 1).
    ContradictedHint {
        reference: fn(&Detailed) -> bool,
        blamed: Bind,
    },
}

/// A corruption case: name, whether the chain is v1, the damage (statements with symbolic
/// binds), the limits, the head, the expectation.
struct Case {
    name: &'static str,
    v1: bool,
    damage: Vec<(&'static str, Vec<Bind>)>,
    limits: ReconstructionLimits,
    head: Head,
    expect: Expect,
}

const DEV: ReconstructionLimits = ReconstructionLimits::DEVELOPMENT;

fn not_found(r: &Detailed) -> bool {
    matches!(r, Err(LedgerError::NotFound(_)))
}

fn corrupt_hash(r: &Detailed) -> bool {
    matches!(r, Err(LedgerError::CorruptObject { reason, .. }) if reason == "stored bytes do not hash to id")
}

fn is_ok(r: &Detailed) -> bool {
    r.is_ok()
}

fn depth_limit(r: &Detailed) -> bool {
    matches!(r, Err(LedgerError::ResourceLimit(m)) if m.starts_with("reconstruction depth exceeds"))
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn corrupted_objects_and_index_rows_fail_closed_identically_or_stricter() {
    let cases: Vec<Case> = vec![
        Case {
            name: "commit bytes whose digest does not match the requested id",
            v1: false,
            damage: vec![(
                "UPDATE immutable_objects SET bytes = bytes || 'x'::bytea WHERE id = $1",
                vec![Bind::Commit(4)],
            )],
            limits: DEV,
            head: Head::Top,
            expect: Expect::Same(corrupt_hash),
        },
        Case {
            name: "v1 commit bytes whose digest does not match the requested id",
            v1: true,
            damage: vec![(
                "UPDATE immutable_objects SET bytes = bytes || 'x'::bytea WHERE id = $1",
                vec![Bind::Commit(4)],
            )],
            limits: DEV,
            head: Head::Top,
            expect: Expect::Same(corrupt_hash),
        },
        Case {
            name: "malformed commit bytes under a known header, as the head",
            v1: false,
            damage: vec![],
            limits: DEV,
            head: Head::Malformed,
            expect: Expect::Same(
                |r| matches!(r, Err(LedgerError::CorruptObject { reason, .. }) if reason.starts_with("commit envelope with a known header does not decode")),
            ),
        },
        Case {
            name: "malformed commit bytes as the 4th ancestor (a window boundary for small windows)",
            v1: false,
            damage: vec![],
            limits: DEV,
            head: Head::MalformedParentAt(3),
            expect: Expect::Same(
                |r| matches!(r, Err(LedgerError::CorruptObject { reason, .. }) if reason.starts_with("commit envelope with a known header does not decode")),
            ),
        },
        Case {
            name: "a commit envelope of an unknown version, as the head",
            v1: false,
            damage: vec![],
            limits: DEV,
            head: Head::UnknownVersion,
            expect: Expect::Same(|r| matches!(r, Err(LedgerError::UnknownCommitVersion(_)))),
        },
        Case {
            name: "missing commit object (index rows intact)",
            v1: false,
            damage: vec![(
                "DELETE FROM immutable_objects WHERE id = $1",
                vec![Bind::Commit(3)],
            )],
            limits: DEV,
            head: Head::Top,
            expect: Expect::Same(not_found),
        },
        Case {
            name: "missing v1 commit object (index rows intact)",
            v1: true,
            damage: vec![(
                "DELETE FROM immutable_objects WHERE id = $1",
                vec![Bind::Commit(3)],
            )],
            limits: DEV,
            head: Head::Top,
            expect: Expect::Same(not_found),
        },
        Case {
            name: "missing commit object and its index rows (the child's hint is silent)",
            v1: false,
            damage: vec![
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
            limits: DEV,
            head: Head::Top,
            expect: Expect::Same(not_found),
        },
        Case {
            name: "precedence: the depth limit fires before a missing commit beyond it",
            v1: false,
            damage: vec![(
                "DELETE FROM immutable_objects WHERE id = $1",
                vec![Bind::Commit(0)],
            )],
            limits: ReconstructionLimits {
                max_depth: 7,
                ..DEV
            },
            head: Head::Top,
            expect: Expect::Same(depth_limit),
        },
        Case {
            name: "precedence: a missing commit is reported before a missing (older) patch",
            v1: false,
            damage: vec![
                (
                    "DELETE FROM immutable_objects WHERE id = $1",
                    vec![Bind::Patch(1)],
                ),
                (
                    "DELETE FROM immutable_objects WHERE id = $1",
                    vec![Bind::Commit(5)],
                ),
            ],
            limits: DEV,
            head: Head::Top,
            expect: Expect::Same(not_found),
        },
        Case {
            name: "precedence: a corrupt commit is reported before a corrupt (older) patch",
            v1: false,
            damage: vec![
                (
                    "UPDATE immutable_objects SET bytes = bytes || 'x'::bytea WHERE id = $1",
                    vec![Bind::Patch(1)],
                ),
                (
                    "UPDATE immutable_objects SET bytes = bytes || 'x'::bytea WHERE id = $1",
                    vec![Bind::Commit(6)],
                ),
            ],
            limits: DEV,
            head: Head::Top,
            expect: Expect::Same(corrupt_hash),
        },
        Case {
            name: "precedence: the quad limit at patch 3 fires before a corrupt patch 5",
            v1: false,
            damage: vec![(
                "UPDATE immutable_objects SET bytes = bytes || 'x'::bytea WHERE id = $1",
                vec![Bind::Patch(5)],
            )],
            // The genesis holds 2 quads and every later patch keeps 2: a limit of 1 fails
            // on the genesis patch, before the corrupt patch 5 is read.
            limits: ReconstructionLimits {
                max_quads: 1,
                ..DEV
            },
            head: Head::Top,
            expect: Expect::Same(
                |r| matches!(r, Err(LedgerError::ResourceLimit(m)) if m == "reconstructed state exceeds 1 quads"),
            ),
        },
        Case {
            name: "missing patch object (the oldest missing one is reported)",
            v1: false,
            damage: vec![(
                "DELETE FROM immutable_objects WHERE id = $1 OR id = $2",
                vec![Bind::Patch(2), Bind::Patch(5)],
            )],
            limits: DEV,
            head: Head::Top,
            expect: Expect::Same(not_found),
        },
        Case {
            name: "patch bytes whose digest does not match the patch id",
            v1: false,
            damage: vec![(
                "UPDATE immutable_objects SET bytes = bytes || 'x'::bytea WHERE id = $1",
                vec![Bind::Patch(6)],
            )],
            limits: DEV,
            head: Head::Top,
            expect: Expect::Same(corrupt_hash),
        },
        Case {
            name: "patch bytes that hash correctly but are not a canonical patch",
            v1: false,
            damage: vec![],
            limits: DEV,
            head: Head::NonPatch,
            expect: Expect::Same(|r| matches!(r, Err(LedgerError::InvalidPatch { .. }))),
        },
        Case {
            name: "commit_index.patch_id disagreeing with the bytes (not consulted by either path)",
            v1: false,
            damage: vec![(
                "UPDATE commit_index SET patch_id = $2 WHERE id = $1",
                vec![Bind::Commit(4), Bind::Patch(0)],
            )],
            limits: DEV,
            head: Head::Top,
            expect: Expect::Same(is_ok),
        },
        Case {
            name: "commit_parents position 0 disagreeing with the bytes",
            v1: false,
            damage: vec![(
                "UPDATE commit_parents SET parent_id = $2 WHERE commit_id = $1 AND position = 0",
                vec![Bind::Commit(5), Bind::Commit(1)],
            )],
            limits: DEV,
            head: Head::Top,
            expect: Expect::ContradictedHint {
                reference: is_ok,
                blamed: Bind::Commit(5),
            },
        },
        Case {
            name: "commit_parents position 0 naming a malformed id",
            v1: false,
            damage: vec![(
                "UPDATE commit_parents SET parent_id = 'garbage' WHERE commit_id = $1 AND position = 0",
                vec![Bind::Commit(5)],
            )],
            limits: DEV,
            head: Head::Top,
            expect: Expect::ContradictedHint {
                reference: is_ok,
                blamed: Bind::Commit(5),
            },
        },
        Case {
            name: "commit_parents naming a parent for the genesis (an index cycle through the head)",
            v1: false,
            damage: vec![(
                "INSERT INTO commit_parents (commit_id, position, parent_id) VALUES ($1, 0, $2)",
                vec![Bind::Commit(0), Bind::Commit(7)],
            )],
            limits: DEV,
            head: Head::Top,
            expect: Expect::ContradictedHint {
                reference: is_ok,
                blamed: Bind::Commit(0),
            },
        },
        Case {
            name: "a contradicted hint on the last commit the depth limit allows (Decision 1 precedes the limit)",
            v1: false,
            damage: vec![(
                "UPDATE commit_parents SET parent_id = $2 WHERE commit_id = $1 AND position = 0",
                vec![Bind::Commit(4), Bind::Commit(1)],
            )],
            limits: ReconstructionLimits {
                max_depth: 4,
                ..DEV
            },
            head: Head::Top,
            expect: Expect::ContradictedHint {
                reference: depth_limit,
                blamed: Bind::Commit(4),
            },
        },
        // ADR-0025 compatibility table: a contradicted row is reported in chain order, ahead
        // of the later error the scalar walk would have reached by following the bytes.
        Case {
            name: "ADR-0025: a contradicted hint at commit 5 precedes a missing older commit 2 (scalar: NotFound)",
            v1: false,
            damage: vec![
                (
                    "UPDATE commit_parents SET parent_id = $2 WHERE commit_id = $1 AND position = 0",
                    vec![Bind::Commit(5), Bind::Commit(1)],
                ),
                (
                    "DELETE FROM immutable_objects WHERE id = $1",
                    vec![Bind::Commit(2)],
                ),
            ],
            limits: DEV,
            head: Head::Top,
            expect: Expect::ContradictedHint {
                reference: not_found,
                blamed: Bind::Commit(5),
            },
        },
        Case {
            name: "ADR-0025: a contradicted hint at commit 5 precedes a corrupt older patch 1 (scalar: CorruptObject on the patch)",
            v1: false,
            damage: vec![
                (
                    "UPDATE commit_parents SET parent_id = $2 WHERE commit_id = $1 AND position = 0",
                    vec![Bind::Commit(5), Bind::Commit(1)],
                ),
                (
                    "UPDATE immutable_objects SET bytes = bytes || 'x'::bytea WHERE id = $1",
                    vec![Bind::Patch(1)],
                ),
            ],
            limits: DEV,
            head: Head::Top,
            expect: Expect::ContradictedHint {
                reference: corrupt_hash,
                blamed: Bind::Commit(5),
            },
        },
        Case {
            name: "ADR-0025: a contradicted hint at commit 5 precedes the quad limit the genesis patch exceeds (scalar: ResourceLimit)",
            v1: false,
            damage: vec![(
                "UPDATE commit_parents SET parent_id = $2 WHERE commit_id = $1 AND position = 0",
                vec![Bind::Commit(5), Bind::Commit(1)],
            )],
            limits: ReconstructionLimits {
                max_quads: 1,
                ..DEV
            },
            head: Head::Top,
            expect: Expect::ContradictedHint {
                reference: |r| matches!(r, Err(LedgerError::ResourceLimit(m)) if m == "reconstructed state exceeds 1 quads"),
                blamed: Bind::Commit(5),
            },
        },
        Case {
            name: "commit_parents position 0 row missing (hint silent, bytes followed)",
            v1: false,
            damage: vec![(
                "DELETE FROM commit_parents WHERE commit_id = $1",
                vec![Bind::Commit(5)],
            )],
            limits: DEV,
            head: Head::Top,
            expect: Expect::Same(is_ok),
        },
        Case {
            name: "parent_count disagreeing with the rows (not consulted by reconstruction)",
            v1: false,
            damage: vec![(
                "UPDATE commit_index SET parent_count = 2 WHERE id = $1",
                vec![Bind::Commit(5)],
            )],
            limits: DEV,
            head: Head::Top,
            expect: Expect::Same(is_ok),
        },
        Case {
            name: "a position-1 row without a position-0 row (not consulted by reconstruction)",
            v1: false,
            damage: vec![(
                "UPDATE commit_parents SET position = 1 WHERE commit_id = $1 AND position = 0",
                vec![Bind::Commit(5)],
            )],
            limits: DEV,
            head: Head::Top,
            expect: Expect::Same(is_ok),
        },
        Case {
            name: "a foreign-graph commit named as the first parent by the index",
            v1: false,
            damage: vec![(
                "UPDATE commit_parents SET parent_id = $2 WHERE commit_id = $1 AND position = 0",
                vec![Bind::Commit(6), Bind::Foreign],
            )],
            limits: DEV,
            head: Head::Top,
            expect: Expect::ContradictedHint {
                reference: is_ok,
                blamed: Bind::Commit(6),
            },
        },
    ];
    for case in cases {
        let url = fresh_database("corrupt").await;
        let store = store_at(&url).await;
        let (g, immutable) = if case.v1 {
            let g = graph_with(&store, GraphStatus::Importing).await;
            let bound = PostgresImmutableStore::from_pool_migrated(
                store.pool().clone(),
                V1Binding::BindTo(g.clone()),
            );
            (g, bound)
        } else {
            (graph(&store).await, store.immutable().clone())
        };
        let (ids, _) = constant_state_chain_v(&immutable, &g, 8, 2, case.v1).await;
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
        let head: CommitId = match case.head {
            Head::Top => ids[7].clone(),
            Head::Malformed => insert_raw(
                store.pool(),
                b"sculpin-cognitive-commit-v2\0this is not an envelope",
            )
            .await
            .parse()
            .unwrap(),
            Head::UnknownVersion => insert_raw(store.pool(), b"sculpin-cognitive-commit-v9\0{}")
                .await
                .parse()
                .unwrap(),
            Head::NonPatch => {
                // A commit whose "patch" is another commit object: well-hashed under its own
                // id, present, and not a canonical patch.
                let bogus = store
                    .immutable()
                    .get_content(&ids[0].0)
                    .await
                    .unwrap()
                    .unwrap();
                let AnyCommit::V2(mut c) = v2(&g, vec![ids[7].clone()], &patches[0], "bogus")
                else {
                    unreachable!()
                };
                c.patch = ledger_core::PatchId(ContentId::for_bytes(&bogus));
                let c = AnyCommit::V2(c);
                let id = c.id().unwrap();
                insert_raw(store.pool(), &c.canonical_bytes().unwrap()).await;
                sql(store.pool(), "INSERT INTO commit_index (id, graph_id, version, patch_id, parent_count) VALUES ($1, $2, 2, $3, 1)", &[&id.to_string(), g.as_str(), &ids[0].to_string()]).await;
                sql(store.pool(), "INSERT INTO commit_parents (commit_id, position, parent_id) VALUES ($1, 0, $2)", &[&id.to_string(), &ids[7].to_string()]).await;
                id
            }
            Head::MalformedParentAt(n) => {
                // n valid commits on top of a malformed envelope: the malformed one is the
                // (n + 1)-th row of the chain. Its index rows name no parent.
                let malformed: CommitId = insert_raw(
                    store.pool(),
                    b"sculpin-cognitive-commit-v2\0not an envelope either",
                )
                .await
                .parse()
                .unwrap();
                sql(store.pool(), "INSERT INTO commit_index (id, graph_id, version, patch_id, parent_count) VALUES ($1, $2, 2, $3, 0)", &[&malformed.to_string(), g.as_str(), &patches[0].id().to_string()]).await;
                let mut parent = malformed;
                for i in 0..n {
                    let patch = Patch::new([add(quad(90 + i, 1))]).unwrap();
                    store
                        .immutable()
                        .put_content(&patch.id().0, &patch.canonical_bytes())
                        .await
                        .unwrap();
                    let c = v2(&g, vec![parent.clone()], &patch, "on malformed");
                    let id = c.id().unwrap();
                    insert_raw(store.pool(), &c.canonical_bytes().unwrap()).await;
                    sql(store.pool(), "INSERT INTO commit_index (id, graph_id, version, patch_id, parent_count) VALUES ($1, $2, 2, $3, 1)", &[&id.to_string(), g.as_str(), &patch.id().to_string()]).await;
                    sql(store.pool(), "INSERT INTO commit_parents (commit_id, position, parent_id) VALUES ($1, 0, $2)", &[&id.to_string(), &parent.to_string()]).await;
                    parent = id;
                }
                parent
            }
        };
        let resolve = |b: Bind| -> String {
            match b {
                Bind::Commit(i) => ids[i].to_string(),
                Bind::Patch(i) => patches[i].id().to_string(),
                Bind::Foreign => foreign.to_string(),
            }
        };
        for (statement, binds) in &case.damage {
            let values: Vec<String> = binds.iter().map(|b| resolve(*b)).collect();
            let values: Vec<&str> = values.iter().map(String::as_str).collect();
            sql(store.pool(), statement, &values).await;
        }
        let name = case.name;
        let reference = store
            .workflows()
            .reconstruct_reference(&head, &case.limits)
            .await;
        for w in reconstruction_windows() {
            let got = windowed(&store, w)
                .reconstruct_detailed(&head, &case.limits)
                .await;
            match &case.expect {
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
                Expect::ContradictedHint {
                    reference: predicate,
                    blamed,
                } => {
                    assert!(
                        predicate(&reference),
                        "{name}: reference {}",
                        show(&reference)
                    );
                    let blamed = resolve(*blamed);
                    assert!(
                        matches!(&got, Err(LedgerError::CorruptObject { id, reason }) if id.to_string() == blamed && reason == "commit_parents position 0 disagrees with the commit bytes"),
                        "{name}: windows {w:?}: {}",
                        show(&got)
                    );
                }
            }
        }
        store.pool().close().await;
        drop_database(&url).await;
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn duplicate_patch_ids_and_exact_byte_cuts_reconstruct_identically() {
    let store = store_at(&database_url()).await;
    let g = graph(&store).await;
    // add x / delete x / add x / delete x …: the add and the delete patches each repeat,
    // so one window asks for the same object at several positions.
    let x = quad(1, 1);
    let adds = Patch::new([add(x.clone())]).unwrap();
    let dels = Patch::new([del(x.clone())]).unwrap();
    let mut ids = Vec::new();
    let mut parents = vec![];
    for i in 0..12 {
        let patch = if i % 2 == 0 { &adds } else { &dels };
        let id = publish(
            store.immutable(),
            v2(&g, parents.clone(), patch, "toggle"),
            patch,
        )
        .await;
        parents = vec![id.clone()];
        ids.push(id);
    }
    for (depth, id) in ids.iter().enumerate() {
        let got = assert_equivalent(&store, id, &DEV, &format!("toggle depth {depth}"))
            .await
            .unwrap();
        assert_eq!(got.0.len(), if depth % 2 == 0 { 1 } else { 0 });
        assert_eq!(got.2, depth + 1);
    }
    // Byte cuts exactly at, one below and one above the size of the first k objects of a
    // window: the served prefix changes, the result never does.
    let (chain, states) = constant_state_chain(store.immutable(), &g, 6, 3).await;
    let head = chain.last().unwrap();
    let mut commit_sizes = Vec::new();
    let mut patch_sizes = Vec::new();
    for id in chain.iter().rev() {
        let bytes = store.immutable().get_content(&id.0).await.unwrap().unwrap();
        let commit = ledger_store::decode_commit_object(&id.0, &bytes)
            .unwrap()
            .unwrap();
        commit_sizes.push(bytes.len());
        patch_sizes.push(
            store
                .immutable()
                .get_content(&commit.patch().0)
                .await
                .unwrap()
                .unwrap()
                .len(),
        );
    }
    let reference = store
        .workflows()
        .reconstruct_reference(head, &DEV)
        .await
        .unwrap();
    assert_eq!(reference.0, states[5]);
    let mut budgets = BTreeSet::new();
    for sizes in [&commit_sizes, &patch_sizes] {
        let mut sum = 0;
        for s in sizes {
            sum += s;
            budgets.extend([sum - 1, sum, sum + 1]);
        }
    }
    for bytes in budgets {
        let got = windowed(&store, windows(256, bytes, 1))
            .reconstruct_detailed(head, &DEV)
            .await
            .unwrap();
        assert_eq!(got, reference, "byte budget {bytes}");
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

/// Preview, propose and apply `source` into `target`; the integration commit.
async fn merge(
    repo: &WorkflowRepository,
    g: &GraphId,
    source: &str,
    target: &str,
    key: &str,
) -> CommitId {
    let p = repo
        .merge_preview(
            &tenant(),
            g,
            &spec(source, target),
            TraversalLimits::DEFAULT,
        )
        .await
        .unwrap();
    assert!(
        matches!(p.class, MergeClass::Divergent | MergeClass::FastForward),
        "{source} → {target}: {:?}",
        p.class
    );
    let token = p.preview_token.clone().unwrap();
    let proposed = repo
        .merge_propose(
            &ledger_store::ProposeMergeRequest {
                scope: scope(g, &format!("mp-{key}")),
                spec: spec(source, target),
                preview_token: token.clone(),
                message: format!("integrate {source}"),
                evidence_refs: vec![],
            },
            TraversalLimits::DEFAULT,
        )
        .await
        .unwrap();
    repo.merge_apply(&ledger_store::ApplyMergeRequest {
        scope: scope(g, &format!("ma-{key}")),
        proposal_id: proposed.proposal_id,
        preview_token: token,
        reason: None,
        validation: ValidationPolicy::NoValidation,
    })
    .await
    .unwrap();
    proposed.candidate
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
    // main: m1..m9 (9 deep). feature from m3: f1..f5. criss from m6: c1, c2; cross from f2:
    // x1. Then two-parent merges: feature ← main (m9) → f6 = [f5, m9]; main ← feature-old
    // (f4) → m10 = [m9, f4], a criss-cross between main and feature; and `heavy`, a
    // merge-heavy branch that merges main after every commit for ten rounds.
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
    let mut merges = Vec::new();
    merges.push(merge(repo, &g, "main", "feature", "1").await);
    branch_from(repo, &g, "feature-old", "feature", Some(&feature[3])).await;
    merges.push(merge(repo, &g, "feature-old", "main", "2").await);
    // The merge-heavy branch: ten rounds of one commit on main, one on heavy, then
    // heavy ← main (the integration commits have two parents, every round).
    branch_from(repo, &g, "heavy", "main", None).await;
    let mut mhead = repo
        .branch(&tenant(), &g, "main")
        .await
        .unwrap()
        .unwrap()
        .head;
    let mut hhead = mhead.clone();
    for round in 0..10u64 {
        mhead = change(
            repo,
            &g,
            "main",
            Some(mhead.clone()),
            &[quad(600 + round as usize, round)],
            &[],
        )
        .await;
        change(
            repo,
            &g,
            "heavy",
            Some(hhead.clone()),
            &[quad(700 + round as usize, round)],
            &[],
        )
        .await;
        hhead = merge(repo, &g, "main", "heavy", &format!("h{round}")).await;
        merges.push(hhead.clone());
    }

    // Expected classes: feature and main merged each other (a criss-cross: two best common
    // ancestors), heavy integrated main's head last (fast-forward into main), and
    // feature ← heavy is ambiguous through main's two integrations.
    let pairs = [
        ("feature", "main", "ambiguous_merge_base"),
        ("main", "feature", "ambiguous_merge_base"),
        ("criss", "main", "divergent"),
        ("cross", "feature", "divergent"),
        ("cross", "main", "divergent"),
        ("contained", "main", "already_contained"),
        ("main", "contained", "fast_forward"),
        ("same", "main", "already_contained"),
        ("ahead", "main", "divergent"),
        ("main", "ahead", "divergent"),
        ("feature-old", "main", "already_contained"),
        ("criss", "cross", "divergent"),
        ("heavy", "main", "fast_forward"),
        ("main", "heavy", "already_contained"),
        ("feature", "heavy", "ambiguous_merge_base"),
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
        "heavy",
    ];
    // Every state a preview consumes (heads, bases, integration commits) reconstructs the
    // same through the scalar reference and every reconstruction window.
    let mut inputs: Vec<CommitId> = merges.clone();
    for b in branches {
        inputs.push(repo.branch(&tenant(), &g, b).await.unwrap().unwrap().head);
    }
    inputs.extend(main.iter().cloned());
    inputs.extend(feature.iter().cloned());
    for (i, c) in inputs.iter().enumerate() {
        assert_equivalent(&store, c, &DEV, &format!("preview input {i}"))
            .await
            .unwrap();
    }
    let reference = windowed(&store, windows(256, 8 << 20, 1));
    for w in ancestry_windows() {
        let wide = windowed(&store, windows(256, 8 << 20, w));
        for (source, target, class) in &pairs {
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
                    (Ok(a), Ok(b)) => {
                        assert_eq!(
                            preview_view(a),
                            preview_view(b),
                            "window {w}: {source} → {target} {strategy:?}"
                        );
                        assert_eq!(a.class.as_str(), *class, "window {w}: {source} → {target}");
                    }
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
            for source in ["main", "feature", "criss", "heavy"] {
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

/// A DAG corruption case: name, damage (statements with symbolic binds), the reason both
/// walks must report.
type DagCase = (
    &'static str,
    Vec<(&'static str, &'static [&'static str])>,
    &'static str,
);

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn corrupted_parent_rows_fail_ancestry_walks_identically_in_every_window() {
    // Each case damages a fresh graph's index rows; the windowed walk must answer exactly
    // what the unwindowed one does (an error of the same kind and wording, or the same
    // success), and damage beyond what a bounded read needs stays invisible to both.
    let cases: Vec<DagCase> = vec![
        (
            "parent_count disagreement",
            vec![(
                "UPDATE commit_index SET parent_count = 2 WHERE id = $1",
                &["m5"],
            )],
            "commit_parents rows disagree with parent_count 2",
        ),
        (
            "non-contiguous parent positions",
            vec![(
                "UPDATE commit_parents SET position = 1 WHERE commit_id = $1 AND position = 0",
                &["m5"],
            )],
            "commit_parents rows disagree with parent_count 1",
        ),
        (
            "missing indexed parent row",
            vec![("DELETE FROM commit_parents WHERE commit_id = $1", &["m5"])],
            "commit_parents rows disagree with parent_count 1",
        ),
        (
            "parent not indexed in this graph (a parent id without an index row)",
            vec![(
                "UPDATE commit_parents SET parent_id = $2 WHERE commit_id = $1 AND position = 0",
                &["m4", "UNINDEXED"],
            )],
            "parent commit missing from the graph's index",
        ),
        (
            "foreign-graph indexed parent",
            vec![(
                "UPDATE commit_parents SET parent_id = $2 WHERE commit_id = $1 AND position = 0",
                &["m3", "FOREIGN"],
            )],
            "parent commit missing from the graph's index",
        ),
        (
            "index cycle (genesis' parent is the head)",
            vec![
                (
                    "UPDATE commit_index SET parent_count = 1 WHERE id = $1",
                    &["m1"],
                ),
                (
                    "INSERT INTO commit_parents (commit_id, position, parent_id) VALUES ($1, 0, $2)",
                    &["m1", "m8"],
                ),
            ],
            "commit cycle",
        ),
        (
            "index cycle through a second parent",
            vec![
                (
                    "UPDATE commit_index SET parent_count = 2 WHERE id = $1",
                    &["m4"],
                ),
                (
                    "INSERT INTO commit_parents (commit_id, position, parent_id) VALUES ($1, 1, $2)",
                    &["m4", "m7"],
                ),
            ],
            "commit cycle",
        ),
    ];
    for (name, damage, reason) in cases {
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
            if n == "UNINDEXED" {
                return CommitId(ContentId::for_bytes(b"never published")).to_string();
            }
            let i: usize = n[1..].parse().unwrap();
            main[i - 1].to_string()
        };
        for (statement, args) in &damage {
            let binds: Vec<String> = args.iter().map(|a| by_name(a)).collect();
            let binds: Vec<&str> = binds.iter().map(String::as_str).collect();
            sql(store.pool(), statement, &binds).await;
        }
        let reference = windowed(&store, windows(256, 8 << 20, 1));
        // The damage is at m5, m3, m1 or m4: every full walk from m8 or s1's branch point
        // reaches it, with the pinned reason in both walks.
        let r = reference
            .merge_preview(
                &tenant(),
                &g,
                &spec("side", "main"),
                TraversalLimits::DEFAULT,
            )
            .await;
        assert!(
            matches!(&r, Err(LedgerError::CorruptObject { reason: got, .. }) if got == reason),
            "{name}: {r:?}"
        );
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
            // Damage beyond what a bounded history or an early-exit reachability check reads
            // must stay invisible, exactly as in the unwindowed walk: the two-entry history
            // of main and the branch point m7 never reach m5, m4, m3 or m1.
            let short = reference
                .first_parent_history(&tenant(), &g, &main[7], 2, TraversalLimits::DEFAULT)
                .await
                .map_err(|e| e.to_string());
            let short_w = wide
                .first_parent_history(&tenant(), &g, &main[7], 2, TraversalLimits::DEFAULT)
                .await
                .map_err(|e| e.to_string());
            assert_eq!(short, short_w, "{name}: window {w}: two-entry history");
            assert_eq!(
                short,
                Ok(vec![main[7].clone(), main[6].clone()]),
                "{name}: window {w}"
            );
            let near = main[6].clone();
            let want = reference
                .create_branch(
                    &CreateBranchRequest {
                        scope: scope(&g, &format!("rn-{w}-{name}")),
                        name: format!("rn-{w}"),
                        source: "main".into(),
                        from_commit: Some(near.clone()),
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
                        scope: scope(&g, &format!("wn-{w}-{name}")),
                        name: format!("wn-{w}"),
                        source: "main".into(),
                        from_commit: Some(near.clone()),
                        policy: BranchPolicy::default(),
                    },
                    TraversalLimits::DEFAULT,
                )
                .await
                .map(|o| o.event.head)
                .map_err(|e| e.to_string());
            assert_eq!(want, got, "{name}: window {w}: near branch point");
            assert_eq!(
                want,
                Ok(main[6].clone()),
                "{name}: window {w}: near branch point reachable"
            );
            // Full reads reach the damage in both.
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
        store.pool().close().await;
        drop_database(&url).await;
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
    let migrated = store_at(&database_url()).await;
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
    let mut ids = Vec::new();
    let mut head = None;
    for i in 0..10u64 {
        let c = change(&repo, &g, "main", head.clone(), &[quad(i as usize, i)], &[]).await;
        head = Some(c.clone());
        ids.push(c);
    }
    let head = head.unwrap();
    let state = repo.reconstruct(&head, &DEV).await.unwrap();
    assert_eq!(
        state,
        migrated
            .workflows()
            .reconstruct_reference(&head, &DEV)
            .await
            .unwrap()
            .0
    );
    assert_eq!(state.len(), 10);
    // Historical branch creation walks the DAG (windows of 2) on its pooled connection
    // before its transaction; merge preview walks and reconstructs three states on one
    // pooled connection; propose and apply run their transactions on it.
    branch_from(&repo, &g, "b", "main", Some(&ids[2])).await;
    let b1 = change(&repo, &g, "b", Some(ids[2].clone()), &[quad(99, 1)], &[]).await;
    let p = repo
        .merge_preview(&tenant(), &g, &spec("b", "main"), TraversalLimits::DEFAULT)
        .await
        .unwrap();
    assert_eq!(p.class, MergeClass::Divergent);
    let integration = merge(&repo, &g, "b", "main", "one-conn").await;
    let mut want = state.clone();
    want.insert(quad(99, 1));
    assert_eq!(repo.reconstruct(&integration, &DEV).await.unwrap(), want);
    assert_eq!(
        migrated
            .workflows()
            .reconstruct_reference(&integration, &DEV)
            .await
            .unwrap()
            .0,
        want
    );
    let _ = b1;
}

thread_local! {
    static STATEMENTS: Cell<usize> = const { Cell::new(0) };
    /// The summary sqlx logs for each statement, so a count that disagrees with the
    /// formula names the statements it saw.
    static SEEN: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

struct CountStatements;

struct Summary(String);

impl tracing::field::Visit for Summary {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "summary" || field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CountStatements {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if event.metadata().target() == "sqlx::query" {
            STATEMENTS.with(|c| c.set(c.get() + 1));
            let mut summary = Summary(String::new());
            event.record(&mut summary);
            SEEN.with(|s| s.borrow_mut().push(summary.0));
        }
    }
}

/// A store over a dedicated one-connection pool for the statement-count tests: no
/// connection is ever re-established mid-test (sqlx runs set-up statements on a new
/// connection, which would be counted), and the pool never pings before acquire.
async fn counting_store() -> PostgresLedgerStore {
    let migrated = store_at(&database_url()).await;
    migrated.pool().close().await;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .min_connections(1)
        .test_before_acquire(false)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect(&database_url())
        .await
        .unwrap();
    PostgresLedgerStore::from_pool_migrated(pool, V1Binding::Reject)
}

/// The statements the last `counted` call saw, one per line.
fn seen() -> String {
    SEEN.with(|s| s.borrow().join("\n"))
}

/// Count the statements sqlx executes on this thread (a current-thread runtime runs the
/// future on the test thread, so the thread-local is exact for this test).
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
    SEEN.with(|s| s.borrow_mut().clear());
    let out = f.await;
    (out, STATEMENTS.with(Cell::get))
}

/// Statements of one ancestry walk of `n` commits in a row with ancestry window `w`: the
/// Phase-4/5 pair per commit for `w == 1`, else one per window along the ramp.
fn walk_statements(n: usize, w: usize) -> usize {
    if w <= 1 {
        2 * n
    } else {
        ledger_store::window_calls(n, w)
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn statement_counts_scale_with_windows_not_with_depth() {
    install_counter();
    let store = counting_store().await;
    let g = graph(&store).await;
    const DEPTH: usize = 1_000;
    let (ids, states) = constant_state_chain(store.immutable(), &g, DEPTH, 5).await;
    let head = ids.last().unwrap().clone();
    // The scalar reference: 2 per ancestor.
    let (state, n) = counted(store.workflows().reconstruct_reference(&head, &DEV)).await;
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
        let (state, n) = counted(windowed(&store, w).reconstruct(&head, &DEV)).await;
        assert_eq!(state.unwrap(), states[DEPTH - 1]);
        assert_eq!(n, want, "{w:?}; statements:\n{}", seen());
    }
    // Shallow histories: depth 1 is one chain window and one patch window.
    let (state, n) = counted(store.workflows().reconstruct(&ids[0], &DEV)).await;
    assert_eq!(state.unwrap(), states[0]);
    assert_eq!(n, 2);
    // First-parent history: one readable-graph statement, then the windows along the ramp
    // (1, 4, 16, 64, then full windows), never more than the entries wanted.
    const READABLE_GRAPH: usize = 1;
    for w in [1usize, 2, 7, 100, 256, 10_000] {
        let repo = windowed(&store, windows(256, 8 << 20, w));
        for wanted in [1usize, 2, 5, 17, 300, DEPTH] {
            let (h, n) = counted(repo.first_parent_history(
                &tenant(),
                &g,
                &head,
                wanted,
                TraversalLimits::DEFAULT,
            ))
            .await;
            let h = h.unwrap();
            assert_eq!(h.len(), wanted);
            assert_eq!(h.first(), Some(&head));
            assert_eq!(
                n,
                READABLE_GRAPH + walk_statements(wanted, w),
                "ancestry window {w}, {wanted} entries; statements:\n{}",
                seen()
            );
        }
    }
    // The exact development depth limit: 10,000 commits reconstruct in 80 statements, and
    // the commit beyond the limit is refused by both paths with the same wording.
    let (deep, deep_states) =
        constant_state_chain(store.immutable(), &g, DEV.max_depth + 1, 3).await;
    let at_limit = &deep[DEV.max_depth - 1];
    let (state, n) = counted(store.workflows().reconstruct(at_limit, &DEV)).await;
    assert_eq!(state.unwrap(), deep_states[DEV.max_depth - 1]);
    assert_eq!(n, 2 * DEV.max_depth.div_ceil(256));
    let beyond = deep.last().unwrap();
    let r = assert_equivalent(&store, beyond, &DEV, "beyond the development depth limit").await;
    assert!(depth_limit(&r), "{}", show(&r));
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn merge_preview_statements_scale_with_ancestry_windows_on_a_deep_linear_history() {
    install_counter();
    let store = counting_store().await;
    let g = graph(&store).await;
    let repo = store.workflows();
    // main: 300 deep; `behind` is main at depth 100 (contained), `fork` branches at 100 and
    // adds one commit (divergent: ancestry plus three reconstructions).
    const N: usize = 300;
    const AT: usize = 100;
    let mut main = Vec::new();
    let mut head = None;
    for i in 0..N as u64 {
        let deletes: Vec<Quad> = if i >= 7 {
            vec![quad(i as usize % 7, i - 7)]
        } else {
            vec![]
        };
        let c = change(
            repo,
            &g,
            "main",
            head.clone(),
            &[quad(i as usize % 7, i)],
            &deletes,
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
    // Contained: the walk of both sides and three constant statements (readable graph, two
    // branch heads).
    const CONSTANT: usize = 3;
    let contained = |w: usize| windowed(&store, windows(256, 8 << 20, w));
    for w in [1usize, 7, 100, 256] {
        let (p, n) = counted(contained(w).merge_preview(
            &tenant(),
            &g,
            &spec("behind", "main"),
            TraversalLimits::DEFAULT,
        ))
        .await;
        assert_eq!(p.unwrap().class, MergeClass::AlreadyContained);
        assert_eq!(
            n,
            CONSTANT + walk_statements(N, w) + walk_statements(AT, w),
            "contained, ancestry window {w}"
        );
    }
    // Divergent: the walk plus three reconstructions (base at AT, target at N, source at
    // AT + 1), each 2 × ceil(depth / objects).
    let (p1, n1) = counted(windowed(&store, windows(1, 8 << 20, 1)).merge_preview(
        &tenant(),
        &g,
        &spec("fork", "main"),
        TraversalLimits::DEFAULT,
    ))
    .await;
    let p1 = p1.unwrap();
    assert_eq!(p1.class, MergeClass::Divergent);
    assert_eq!(n1, CONSTANT + 2 * N + 2 * (AT + 1) + 2 * (AT + N + AT + 1));
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
        let walk = walk_statements(N, ancestry) + walk_statements(AT + 1, ancestry);
        let recon = 2 * (AT.div_ceil(objects) + N.div_ceil(objects) + (AT + 1).div_ceil(objects));
        assert_eq!(
            n,
            CONSTANT + walk + recon,
            "divergent, objects {objects} ancestry {ancestry}"
        );
    }
}

/// A merge-heavy DAG without refs: commit i has parents [i-1, i-2] (every commit after the
/// second is a two-parent merge, and every ancestor is reachable at many depths).
async fn fibonacci_dag(immutable: &PostgresImmutableStore, g: &GraphId, n: usize) -> Vec<CommitId> {
    let mut ids: Vec<CommitId> = Vec::new();
    for i in 0..n {
        let patch = Patch::new([add(quad(i, 1))]).unwrap();
        let parents: Vec<CommitId> = ids.iter().rev().take(2).cloned().collect();
        ids.push(publish(immutable, v2(g, parents, &patch, "fib"), &patch).await);
    }
    ids
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn ancestry_windows_stay_bounded_and_far_fewer_than_scalar_on_a_merge_heavy_dag() {
    install_counter();
    let store = counting_store().await;
    let g = graph(&store).await;
    const N: usize = 600;
    let ids = fibonacci_dag(store.immutable(), &g, N).await;
    let head = ids.last().unwrap();
    let (count, scalar) = counted(windowed(&store, windows(256, 8 << 20, 1)).ancestry_probe(
        &tenant(),
        &g,
        head,
        TraversalLimits::DEFAULT,
    ))
    .await;
    assert_eq!(count.unwrap(), N);
    assert_eq!(scalar, 1 + 2 * N);
    for w in [8usize, 64, 256] {
        let (count, n) = counted(windowed(&store, windows(256, 8 << 20, w)).ancestry_probe(
            &tenant(),
            &g,
            head,
            TraversalLimits::DEFAULT,
        ))
        .await;
        assert_eq!(count.unwrap(), N, "window {w}");
        // Every window serves its anchor at least, so the walk is never worse than one
        // statement per commit; a merge-heavy history fills windows with fewer distinct
        // commits than a linear one (the recursion stops after 4 pairs per requested
        // commit), so the count lies between the linear formula and N. It must stay far
        // below the scalar pair per commit.
        assert!(n > ledger_store::window_calls(N, w), "window {w}: {n}");
        assert!(n <= 1 + N, "window {w}: {n}");
        assert!(
            n * 4 < scalar,
            "window {w}: {n} statements vs {scalar} scalar"
        );
        println!("fibonacci DAG of {N}: ancestry window {w}: {n} statements (scalar {scalar})");
    }
    // The same DAG reconstructs identically (parent 0 is the previous commit).
    assert_equivalent(&store, head, &DEV, "fibonacci head")
        .await
        .unwrap();
}

// ------------------------------------------------------------------------------------------
// M0/M4 diagnostics: query plans of the window statements (printed, not asserted)
// ------------------------------------------------------------------------------------------

#[tokio::test]
#[ignore = "diagnostic: prints EXPLAIN (ANALYZE, BUFFERS) of the window queries; run with --nocapture"]
async fn explain_window_queries() {
    let url = fresh_database("explain").await;
    let store = store_at(&url).await;
    let g = graph(&store).await;
    // 15,000 commits (30,000 objects) so the planner faces a table where a sequential scan
    // of `immutable_objects` is no longer the cheap choice, plus a merge-heavy DAG.
    let (ids, _) = constant_state_chain(store.immutable(), &g, 15_000, 1_000).await;
    let head = ids.last().unwrap().clone();
    let fib = fibonacci_dag(store.immutable(), &g, 3_000).await;
    let fib_head = fib.last().unwrap().clone();
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
    println!(
        "\n== object window 256 patches (30,000-object table)\n{}",
        rows.join("\n")
    );
    let ancestry = "EXPLAIN (ANALYZE, BUFFERS) WITH RECURSIVE reach(id, depth) AS ( \
         SELECT a.id, 0::bigint FROM commit_index a WHERE a.id = $1 AND a.graph_id = $2 UNION \
         SELECT p.parent_id, r.depth + 1 FROM reach r JOIN commit_parents p ON p.commit_id = r.id AND (NOT $5 OR p.position = 0) \
         JOIN commit_index ci ON ci.id = p.parent_id AND ci.graph_id = $2 WHERE r.depth + 1 < $3 ), \
         capped AS ( SELECT id, depth FROM reach LIMIT $6 ), \
         nearest AS ( SELECT DISTINCT ON (id) id, depth FROM capped ORDER BY id, depth ), w AS ( SELECT id FROM nearest ORDER BY depth, id LIMIT $4 ) \
         SELECT c.id, c.parent_count, p.position, p.parent_id FROM w JOIN commit_index c ON c.id = w.id AND c.graph_id = $2 \
         LEFT JOIN commit_parents p ON p.commit_id = c.id ORDER BY c.id, p.position";
    for (label, anchor, k, fp) in [
        ("linear", &head, 256i64, false),
        ("linear first-parent", &head, 256, true),
        ("linear", &head, 1024, false),
        ("fibonacci", &fib_head, 256, false),
        ("fibonacci", &fib_head, 64, false),
    ] {
        let rows: Vec<String> = sqlx::query_scalar(ancestry)
            .bind(anchor.to_string())
            .bind(g.as_str())
            .bind(k)
            .bind(k)
            .bind(fp)
            .bind(k * 4)
            .fetch_all(store.pool())
            .await
            .unwrap();
        println!(
            "\n== ancestry window {label} K={k} first_parent_only={fp} cap={}\n{}",
            k * 4,
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
    store.pool().close().await;
    drop_database(&url).await;
    let _ = IGNORE;
}
