//! Filesystem persistence and the linear ledger application service.

use fs2::FileExt;
use ledger_core::{
    AnyCommit, COMMIT_V1_HEADER, COMMIT_V2_HEADER, Commit, CommitId, ContentId, ImmutableStore,
    LedgerError, PatchId, RefStore,
};
use ledger_rdf::{Patch, Quad};
use std::{
    collections::{BTreeSet, HashSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// Every object the ledger stores starts with a `sculpin-` header. Anything in that
/// family that is not an RDF patch is a commit envelope (v1, v2, or a version this build
/// does not know). Classifying by header lets stores fail closed on corrupt or
/// future-version commits instead of treating them as opaque content.
fn is_commit_family(bytes: &[u8]) -> bool {
    bytes.starts_with(b"sculpin-") && !bytes.starts_with(b"sculpin-rdf-patch-")
}

/// Decode a stored object as a commit, distinguishing three cases: not a commit at all
/// (`Ok(None)`), a commit this build understands (`Ok(Some)`), and a commit-family object
/// that is corrupt or of an unknown version (`Err`). Known-header bytes with an invalid
/// body are `CorruptObject`; unknown commit headers propagate `UnknownCommitVersion`.
pub fn decode_commit_object(
    id: &ContentId,
    bytes: &[u8],
) -> Result<Option<AnyCommit>, LedgerError> {
    if !is_commit_family(bytes) {
        return Ok(None);
    }
    match AnyCommit::from_canonical_bytes(bytes) {
        Ok(commit) => {
            if commit.id()?.0 != *id {
                return Err(LedgerError::CorruptObject {
                    id: id.clone(),
                    reason: "commit ID mismatch".into(),
                });
            }
            Ok(Some(commit))
        }
        Err(LedgerError::UnknownCommitVersion(header)) => {
            Err(LedgerError::UnknownCommitVersion(header))
        }
        Err(e) if bytes.starts_with(COMMIT_V1_HEADER) || bytes.starts_with(COMMIT_V2_HEADER) => {
            Err(LedgerError::CorruptObject {
                id: id.clone(),
                reason: format!("commit envelope with a known header does not decode: {e}"),
            })
        }
        Err(e) => Err(e),
    }
}

/// A commit's referenced patch must be a canonical ledger patch whose bytes hash to the
/// referenced id — not merely some stored non-commit blob (ImmutableStore contract).
/// Bytes that do not hash to the id are storage corruption (`CorruptObject`); bytes that
/// hash correctly but are not a canonical patch are `InvalidPatch`. The reason is bounded
/// and never echoes stored RDF text.
pub(crate) fn validate_patch_bytes(id: &PatchId, bytes: &[u8]) -> Result<Patch, LedgerError> {
    if ContentId::for_bytes(bytes) != id.0 {
        return Err(LedgerError::CorruptObject {
            id: id.0.clone(),
            reason: "stored bytes do not hash to the patch id".into(),
        });
    }
    Patch::from_canonical_bytes(bytes).map_err(|e| LedgerError::InvalidPatch {
        id: id.clone(),
        reason: bounded_reason(&e),
    })
}

/// Error classification only: which decode rule failed, without the offending text.
fn bounded_reason(error: &ledger_rdf::RdfError) -> String {
    match error {
        ledger_rdf::RdfError::InvalidQuad(_) => "an operation is not a canonical N-Quad".into(),
        ledger_rdf::RdfError::BlankNode => "blank nodes are forbidden".into(),
        ledger_rdf::RdfError::ConflictingOperation(_) => "a quad is both added and deleted".into(),
        ledger_rdf::RdfError::InvalidPatch(message) => {
            let mut m = message.clone();
            m.truncate(120);
            m
        }
    }
}

/// `put_content` is for patches and other non-commit objects only; commit envelopes must
/// go through `put_commit` so parent, graph and index checks always run.
pub(crate) fn reject_commit_bytes_as_content(bytes: &[u8]) -> Result<(), LedgerError> {
    if is_commit_family(bytes) {
        return Err(LedgerError::InvalidCommit(
            "commit envelopes are published through put_commit, not put_content".into(),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct FileStore {
    root: PathBuf,
}
impl FileStore {
    /// Open a store for read-only use, refusing to create anything: the object root must
    /// already exist. A mistyped path is an error, not an empty store.
    pub fn open_existing(root: impl AsRef<Path>) -> Result<Self, LedgerError> {
        let root = root.as_ref().to_owned();
        let objects = root.join("objects/sha256");
        if !objects.is_dir() {
            return Err(LedgerError::Storage(format!(
                "{} is not an existing ledger store (missing objects/sha256)",
                root.display()
            )));
        }
        Ok(Self { root })
    }
    pub fn open(root: impl AsRef<Path>) -> Result<Self, LedgerError> {
        let root = root.as_ref().to_owned();
        fs::create_dir_all(root.join("objects/sha256")).map_err(storage)?;
        fs::create_dir_all(root.join("refs")).map_err(storage)?;
        if let Some(parent) = root.parent() {
            sync_directory(parent)?;
        }
        sync_directory(&root)?;
        sync_directory(&root.join("objects"))?;
        sync_directory(&root.join("objects/sha256"))?;
        sync_directory(&root.join("refs"))?;
        Ok(Self { root })
    }
    /// Where an object's bytes live (`objects/sha256/<2 hex>/<62 hex>`). Public for
    /// migration/fault-injection tooling; ordinary code goes through `ImmutableStore`.
    pub fn object_path(&self, id: &ContentId) -> PathBuf {
        let h = id.digest_hex();
        self.root.join("objects/sha256").join(&h[..2]).join(&h[2..])
    }
    fn head_path(&self) -> PathBuf {
        self.root.join("refs/main")
    }
    fn lock(&self) -> Result<LockGuard, LedgerError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root.join("refs/main.lock"))
            .map_err(storage)?;
        file.lock_exclusive().map_err(storage)?;
        Ok(LockGuard(file))
    }
}
struct LockGuard(fs::File);
impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
fn storage(e: impl std::fmt::Display) -> LedgerError {
    LedgerError::Storage(e.to_string())
}
fn sync_directory(path: &Path) -> Result<(), LedgerError> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(storage)
}
impl FileStore {
    fn put_object_sync(&self, id: &ContentId, bytes: &[u8]) -> Result<(), LedgerError> {
        if &ContentId::for_bytes(bytes) != id {
            return Err(LedgerError::ObjectCollision(id.clone()));
        }
        let path = self.object_path(id);
        if let Some(existing) = self.get_object_sync(id)? {
            return if existing == bytes {
                Ok(())
            } else {
                Err(LedgerError::ObjectCollision(id.clone()))
            };
        }
        let parent = path.parent().expect("object path has parent");
        fs::create_dir_all(parent).map_err(storage)?;
        sync_directory(parent.parent().expect("object shard has parent"))?;
        let tmp = loop {
            let candidate = parent.join(format!(
                ".{}.tmp-{}-{}",
                id.digest_hex(),
                std::process::id(),
                TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(file) => break (candidate, file),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(storage(e)),
            }
        };
        let (tmp, mut file) = tmp;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(storage)?;
        match fs::rename(&tmp, &path) {
            Ok(()) => {
                sync_directory(parent)?;
                Ok(())
            }
            Err(_e) if path.exists() => {
                let _ = fs::remove_file(tmp);
                let existing = fs::read(path).map_err(storage)?;
                if existing == bytes {
                    Ok(())
                } else {
                    Err(LedgerError::ObjectCollision(id.clone()))
                }
            }
            Err(e) => {
                let _ = fs::remove_file(tmp);
                Err(storage(e))
            }
        }
    }
    fn get_object_sync(&self, id: &ContentId) -> Result<Option<Vec<u8>>, LedgerError> {
        let p = self.object_path(id);
        match fs::read(p) {
            Ok(bytes) => {
                if ContentId::for_bytes(&bytes) != *id {
                    return Err(LedgerError::CorruptObject {
                        id: id.clone(),
                        reason: "digest mismatch".into(),
                    });
                }
                Ok(Some(bytes))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(storage(e)),
        }
    }
    /// Cheap existence probe on the object path. Deliberately reads no bytes and performs
    /// no digest verification: it answers only "does an object file exist for this id".
    fn exists_sync(&self, id: &ContentId) -> Result<bool, LedgerError> {
        self.object_path(id).try_exists().map_err(storage)
    }
    fn put_commit_sync(&self, commit: &AnyCommit) -> Result<CommitId, LedgerError> {
        for parent in commit.parents() {
            let Some(stored) = self.get_commit_sync(parent)? else {
                return Err(LedgerError::MissingParent(parent.clone()));
            };
            // The filesystem store keeps no graph index; it can still refuse the one
            // cross-graph edge it can see, a v2 parent bound to another graph.
            if let (Some(child_graph), Some(parent_graph)) = (commit.graph_id(), stored.graph_id())
                && child_graph != parent_graph
            {
                return Err(LedgerError::CrossGraphParent {
                    parent: parent.clone(),
                    parent_graph: parent_graph.to_string(),
                    graph: child_graph.to_string(),
                });
            }
        }
        let Some(patch_bytes) = self.get_object_sync(&commit.patch().0)? else {
            return Err(LedgerError::MissingPatch(commit.patch().clone()));
        };
        validate_patch_bytes(commit.patch(), &patch_bytes)?;
        let bytes = commit.canonical_bytes()?;
        let id = commit.id()?;
        self.put_object_sync(&id.0, &bytes)?;
        Ok(id)
    }
    /// Typed read: an object that exists but is not a commit envelope (a patch, say)
    /// yields `None`, never a reinterpretation; a commit-family object that does not
    /// decode is corruption or an unknown version and is an error, not "absent".
    fn get_commit_sync(&self, id: &CommitId) -> Result<Option<AnyCommit>, LedgerError> {
        let Some(bytes) = self.get_object_sync(&id.0)? else {
            return Ok(None);
        };
        decode_commit_object(&id.0, &bytes)
    }
    /// Enumerate every immutable object id in this store (migration/verification use).
    /// Only well-formed object paths are returned; anything else under the object root —
    /// stray files, symlinks, directories posing as objects — is reported as corruption
    /// rather than skipped. In-flight `.<digest>.tmp-*` files of a concurrent writer are
    /// the one tolerated exception.
    pub fn list_objects(&self) -> Result<Vec<ContentId>, LedgerError> {
        let unexpected = |path: &Path, what: &str| LedgerError::CorruptObject {
            id: ContentId::for_bytes(path.to_string_lossy().as_bytes()),
            reason: format!("{what} in object root: {}", path.display()),
        };
        let mut ids = Vec::new();
        let base = self.root.join("objects/sha256");
        for shard in fs::read_dir(&base).map_err(storage)? {
            let shard = shard.map_err(storage)?;
            let shard_meta = fs::symlink_metadata(shard.path()).map_err(storage)?;
            if !shard_meta.is_dir() {
                return Err(unexpected(&shard.path(), "non-directory shard entry"));
            }
            let prefix = shard.file_name().to_string_lossy().into_owned();
            for entry in fs::read_dir(shard.path()).map_err(storage)? {
                let entry = entry.map_err(storage)?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') && name.contains(".tmp-") {
                    continue;
                }
                let meta = fs::symlink_metadata(entry.path()).map_err(storage)?;
                if !meta.is_file() {
                    return Err(unexpected(&entry.path(), "non-regular object entry"));
                }
                let id = format!("sha256:{prefix}{name}");
                ids.push(
                    ContentId::from_str(&id)
                        .map_err(|_| unexpected(&entry.path(), "malformed object path"))?,
                );
            }
        }
        ids.sort();
        Ok(ids)
    }
    fn head_sync(&self) -> Result<Option<CommitId>, LedgerError> {
        match fs::read_to_string(self.head_path()) {
            Ok(v) => Ok(Some(CommitId::from_str(v.trim())?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(storage(e)),
        }
    }
    fn compare_and_set_sync(
        &self,
        expected: Option<&CommitId>,
        new: &CommitId,
    ) -> Result<(), LedgerError> {
        let _guard = self.lock()?;
        let actual = self.head_sync()?;
        if actual.as_ref() != expected {
            return Err(LedgerError::HeadChanged {
                expected: expected.cloned(),
                actual,
            });
        }
        let path = self.head_path();
        let tmp = self
            .root
            .join(format!("refs/.main.tmp-{}", std::process::id()));
        let _ = fs::remove_file(&tmp);
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(storage)?;
        writeln!(f, "{new}")
            .and_then(|()| f.sync_all())
            .map_err(storage)?;
        fs::rename(&tmp, &path).map_err(storage)?;
        sync_directory(path.parent().expect("ref path has parent"))?;
        Ok(())
    }
}

async fn run_blocking<T, F>(f: F) -> Result<T, LedgerError>
where
    F: FnOnce() -> Result<T, LedgerError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| LedgerError::Storage(format!("storage task failed: {e}")))?
}

#[async_trait::async_trait]
impl ImmutableStore for FileStore {
    async fn put_content(&self, id: &ContentId, bytes: &[u8]) -> Result<(), LedgerError> {
        reject_commit_bytes_as_content(bytes)?;
        let this = self.clone();
        let id = id.clone();
        let bytes = bytes.to_vec();
        run_blocking(move || this.put_object_sync(&id, &bytes)).await
    }
    async fn get_content(&self, id: &ContentId) -> Result<Option<Vec<u8>>, LedgerError> {
        let this = self.clone();
        let id = id.clone();
        run_blocking(move || this.get_object_sync(&id)).await
    }
    async fn put_commit(&self, commit: &AnyCommit) -> Result<CommitId, LedgerError> {
        let this = self.clone();
        let commit = commit.clone();
        run_blocking(move || this.put_commit_sync(&commit)).await
    }
    async fn get_commit(&self, id: &CommitId) -> Result<Option<AnyCommit>, LedgerError> {
        let this = self.clone();
        let id = id.clone();
        run_blocking(move || this.get_commit_sync(&id)).await
    }
    async fn exists(&self, id: &ContentId) -> Result<bool, LedgerError> {
        let this = self.clone();
        let id = id.clone();
        run_blocking(move || this.exists_sync(&id)).await
    }
}
#[async_trait::async_trait]
impl RefStore for FileStore {
    async fn head(&self) -> Result<Option<CommitId>, LedgerError> {
        let this = self.clone();
        run_blocking(move || this.head_sync()).await
    }
    async fn compare_and_set(
        &self,
        expected: Option<&CommitId>,
        new: &CommitId,
    ) -> Result<(), LedgerError> {
        let this = self.clone();
        let expected = expected.cloned();
        let new = new.clone();
        run_blocking(move || this.compare_and_set_sync(expected.as_ref(), &new)).await
    }
}

#[cfg(feature = "postgres")]
pub use postgres::PgRefStore;
#[cfg(feature = "postgres")]
mod postgres_immutable;
#[cfg(feature = "postgres")]
pub mod schema;
#[cfg(feature = "postgres")]
pub use postgres_immutable::{PostgresImmutableStore, V1Binding};
#[cfg(feature = "postgres")]
mod postgres_graphs;
#[cfg(feature = "postgres")]
pub use postgres_graphs::{GraphRecord, GraphStatus, NewGraph, PgGraphs};
#[cfg(feature = "postgres")]
mod postgres_workflow;
#[cfg(feature = "postgres")]
pub use postgres_workflow::{
    AcceptRequest, Accepted, FailPoint, PostgresLedgerStore, PrepareRequest, Prepared,
    RejectRequest, Rejected, RequestScope, ValidationPolicy, WorkflowRepository,
};
#[cfg(feature = "postgres")]
mod migrate_fs_to_pg;
#[cfg(feature = "postgres")]
pub use migrate_fs_to_pg::{CutoverPhase, FsToPgMigration, MigrationOutcome, MigrationReport};

/// PostgreSQL-backed ref coordination. Immutable objects and commits stay on the
/// filesystem `FileStore`; only the mutable ref head is delegated here so that
/// horizontally scaled writers share a single transactional compare-and-set point
/// (ADR-0004, ADR-0007). Compare-and-set is expressed as single predicated
/// statements so concurrent connections cannot lose an update.
#[cfg(feature = "postgres")]
mod postgres {
    use super::{CommitId, LedgerError, RefStore, storage};
    use sqlx::{Row, postgres::PgPoolOptions};
    use std::str::FromStr;

    #[derive(Clone, Debug)]
    pub struct PgRefStore {
        pool: sqlx::PgPool,
        graph_id: String,
        branch: String,
    }

    impl PgRefStore {
        /// Connect to `database_url`, run the ref migrations, and coordinate the
        /// ('default', 'main') ref. Milestone 0002 only exercises this pair.
        pub async fn connect(database_url: &str) -> Result<Self, LedgerError> {
            Self::connect_ref(database_url, "default", "main").await
        }

        /// Connect a fresh pool coordinating a specific (graph, branch) ref. Each
        /// call owns its own pool, so two calls yield genuinely independent backend
        /// connections — the property the CAS race test depends on.
        pub async fn connect_ref(
            database_url: &str,
            graph_id: impl Into<String>,
            branch: impl Into<String>,
        ) -> Result<Self, LedgerError> {
            let pool = PgPoolOptions::new()
                .max_connections(4)
                // Fail fast under pool exhaustion instead of hanging a request handler.
                .acquire_timeout(std::time::Duration::from_secs(10))
                .connect(database_url)
                .await
                .map_err(storage)?;
            Self::with_ref(pool, graph_id, branch).await
        }

        /// Compose over an existing pool (e.g. a test connection). Runs migrations.
        pub async fn from_pool(pool: sqlx::PgPool) -> Result<Self, LedgerError> {
            Self::with_ref(pool, "default", "main").await
        }

        /// Compose over an existing pool and coordinate a specific (graph, branch)
        /// ref. Runs all migrations first.
        pub async fn with_ref(
            pool: sqlx::PgPool,
            graph_id: impl Into<String>,
            branch: impl Into<String>,
        ) -> Result<Self, LedgerError> {
            crate::schema::migrate_all(&pool).await?;
            Ok(Self::with_ref_migrated(pool, graph_id, branch))
        }

        /// Compose over a pool whose schema the caller has already migrated (an explicit
        /// migration entry point, or an administrative tool that must run against a
        /// partially upgraded database).
        pub fn with_ref_migrated(
            pool: sqlx::PgPool,
            graph_id: impl Into<String>,
            branch: impl Into<String>,
        ) -> Self {
            Self {
                pool,
                graph_id: graph_id.into(),
                branch: branch.into(),
            }
        }

        /// The graph this ref store coordinates.
        pub fn graph_id(&self) -> &str {
            &self.graph_id
        }

        /// The branch this ref store coordinates.
        pub fn branch(&self) -> &str {
            &self.branch
        }

        async fn read_head(&self) -> Result<Option<CommitId>, LedgerError> {
            let row = sqlx::query("SELECT head FROM refs WHERE graph_id = $1 AND branch = $2")
                .bind(&self.graph_id)
                .bind(&self.branch)
                .fetch_optional(&self.pool)
                .await
                .map_err(storage)?;
            match row {
                Some(row) => {
                    let head: String = row.try_get("head").map_err(storage)?;
                    Ok(Some(CommitId::from_str(&head)?))
                }
                None => Ok(None),
            }
        }
    }

    #[async_trait::async_trait]
    impl RefStore for PgRefStore {
        async fn head(&self) -> Result<Option<CommitId>, LedgerError> {
            self.read_head().await
        }

        async fn compare_and_set(
            &self,
            expected: Option<&CommitId>,
            new: &CommitId,
        ) -> Result<(), LedgerError> {
            // The raw primitive writes no ref event, so it is confined to graphs that are
            // not yet serving normal acceptance (bootstrap/importing, ADR-0013); accepted
            // transitions on active graphs go through WorkflowRepository. The status is
            // share-locked in the same transaction as the move, so an activation cannot
            // slip in between the check and the update.
            let mut tx = self.pool.begin().await.map_err(storage)?;
            let status: Option<String> =
                sqlx::query("SELECT status FROM graphs WHERE graph_id = $1 FOR SHARE")
                    .bind(&self.graph_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(storage)?
                    .map(|row| row.try_get("status").map_err(storage))
                    .transpose()?;
            match status.as_deref() {
                Some("bootstrap" | "importing") => {}
                Some(other) => {
                    return Err(LedgerError::InvalidCommit(format!(
                        "raw ref movement is not permitted on graph {} ({other}); accepted \
                         transitions go through WorkflowRepository",
                        self.graph_id
                    )));
                }
                None => return Err(LedgerError::UnknownGraph(self.graph_id.clone())),
            }
            let new_s = new.to_string();
            let rows = match expected {
                Some(expected) => sqlx::query(
                    "UPDATE refs SET head = $1, version = version + 1, updated_at = now() \
                     WHERE graph_id = $2 AND branch = $3 AND head = $4",
                )
                .bind(&new_s)
                .bind(&self.graph_id)
                .bind(&self.branch)
                .bind(expected.to_string())
                .execute(&mut *tx)
                .await
                .map_err(storage)?
                .rows_affected(),
                None => sqlx::query(
                    "INSERT INTO refs (graph_id, branch, head) VALUES ($1, $2, $3) \
                     ON CONFLICT (graph_id, branch) DO NOTHING",
                )
                .bind(&self.graph_id)
                .bind(&self.branch)
                .bind(&new_s)
                .execute(&mut *tx)
                .await
                .map_err(storage)?
                .rows_affected(),
            };
            tx.commit().await.map_err(storage)?;
            if rows == 1 {
                Ok(())
            } else {
                // Lost the race (or expectation never matched): report the head the
                // winner installed so the caller can retry against fresh state.
                Err(LedgerError::HeadChanged {
                    expected: expected.cloned(),
                    actual: self.read_head().await?,
                })
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct CommitRequest {
    pub expected_head: Option<CommitId>,
    pub patch: Patch,
    pub author: String,
    pub message: String,
    pub event_time: String,
}
#[derive(Clone)]
pub struct Ledger {
    immutable: Arc<dyn ImmutableStore>,
    refs: Arc<dyn RefStore>,
}
impl Ledger {
    /// Open a ledger whose immutable objects and mutable ref both live on the filesystem.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, LedgerError> {
        let file = Arc::new(FileStore::open(root)?);
        let immutable: Arc<dyn ImmutableStore> = file.clone();
        let refs: Arc<dyn RefStore> = file;
        Ok(Self { immutable, refs })
    }
    /// Compose the ledger with an externally provided ref backend (e.g. PostgreSQL),
    /// keeping immutable objects and commits on the filesystem `store`.
    pub fn with_ref_store(store: Arc<FileStore>, refs: Arc<dyn RefStore>) -> Self {
        Self::with_stores(store as Arc<dyn ImmutableStore>, refs)
    }
    /// Compose the ledger over any shared immutable store and ref backend (ADR-0012).
    pub fn with_stores(immutable: Arc<dyn ImmutableStore>, refs: Arc<dyn RefStore>) -> Self {
        Self { immutable, refs }
    }
    /// The immutable store this ledger reads and writes (for replica/qualification tests).
    pub fn immutable_store(&self) -> &Arc<dyn ImmutableStore> {
        &self.immutable
    }
    pub async fn head(&self) -> Result<Option<CommitId>, LedgerError> {
        self.refs.head().await
    }
    /// Startup/qualification check: the current HEAD, if any, must resolve to a commit in
    /// *this* immutable store whose patch is a valid canonical patch. A shared ref whose
    /// content lives elsewhere (the node-local filesystem of another host, an unmigrated
    /// store) or whose head predates the patch-validity rule is refused here rather than
    /// discovered on the first read.
    pub async fn verify_head(&self) -> Result<Option<CommitId>, LedgerError> {
        let head = self.refs.head().await?;
        if let Some(head) = &head {
            let Some(commit) = self.immutable.get_commit(head).await? else {
                return Err(LedgerError::MissingTarget(head.clone()));
            };
            let Some(patch_bytes) = self.immutable.get_content(&commit.patch().0).await? else {
                return Err(LedgerError::MissingPatch(commit.patch().clone()));
            };
            validate_patch_bytes(commit.patch(), &patch_bytes)?;
        }
        Ok(head)
    }
    pub async fn commit(&self, request: CommitRequest) -> Result<CommitId, LedgerError> {
        let patch_bytes = request.patch.canonical_bytes();
        let patch_id = request.patch.id();
        self.immutable
            .put_content(&patch_id.0, &patch_bytes)
            .await?;
        // The bootstrap write path still produces v1 envelopes; the v2 write path arrives
        // with the authenticated principal and graph binding (Plan 0004 P1.3/P1.4).
        let commit = AnyCommit::V1(Commit {
            parents: request.expected_head.clone().into_iter().collect(),
            patch: patch_id,
            author: request.author,
            message: request.message,
            event_time: request.event_time,
            recorded_time: OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .map_err(storage)?,
        });
        let id = self.immutable.put_commit(&commit).await?;
        self.advance_ref(request.expected_head.as_ref(), &id)
            .await?;
        Ok(id)
    }
    /// Enforce the ref-target-existence invariant uniformly for every ref backend, then
    /// perform the atomic swap. The ref store is a pure primitive that does not see commits.
    ///
    /// The check is *typed*: the target must decode as a commit envelope whose id matches,
    /// not merely exist as some immutable object. Otherwise a patch id (or any stored
    /// blob) could become HEAD and reconstruction would fail on a "valid" ref.
    pub(crate) async fn advance_ref(
        &self,
        expected: Option<&CommitId>,
        new: &CommitId,
    ) -> Result<(), LedgerError> {
        if self.immutable.get_commit(new).await?.is_none() {
            return Err(LedgerError::MissingTarget(new.clone()));
        }
        self.refs.compare_and_set(expected, new).await
    }
    pub async fn state_at(&self, id: &CommitId) -> Result<BTreeSet<Quad>, LedgerError> {
        let mut chain = Vec::new();
        let mut cursor = Some(id.clone());
        let mut seen = HashSet::new();
        while let Some(current) = cursor {
            if !seen.insert(current.clone()) {
                return Err(LedgerError::CorruptObject {
                    id: current.0,
                    reason: "commit cycle".into(),
                });
            }
            let c = self
                .immutable
                .get_commit(&current)
                .await?
                .ok_or_else(|| LedgerError::NotFound(current.0.clone()))?;
            // Parent zero is the state reconstruction parent in every envelope version.
            // Additional merge parents carry ancestry and provenance.
            cursor = c.parents().first().cloned();
            chain.push(c);
        }
        let mut state = BTreeSet::new();
        for commit in chain.iter().rev() {
            let bytes = self
                .immutable
                .get_content(&commit.patch().0)
                .await?
                .ok_or_else(|| LedgerError::NotFound(commit.patch().0.clone()))?;
            let patch = validate_patch_bytes(commit.patch(), &bytes)?;
            ledger_rdf::apply_patch(&mut state, &patch);
        }
        Ok(state)
    }
    pub async fn patch(&self, id: &PatchId) -> Result<Option<Patch>, LedgerError> {
        match self.immutable.get_content(&id.0).await? {
            Some(b) => Ok(Some(validate_patch_bytes(id, &b)?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ledger_core::PatchId;
    use ledger_rdf::{Operation, OperationKind};

    #[tokio::test]
    async fn missing_target_rejected() {
        let t = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(t.path()).unwrap();
        let id = CommitId(ContentId::for_bytes(b"missing"));
        assert!(matches!(
            ledger.advance_ref(None, &id).await,
            Err(LedgerError::MissingTarget(_))
        ));
    }
    #[tokio::test]
    async fn object_survives_reopen() {
        let t = tempfile::tempdir().unwrap();
        let id = ContentId::for_bytes(b"x");
        FileStore::open(t.path())
            .unwrap()
            .put_content(&id, b"x")
            .await
            .unwrap();
        assert_eq!(
            FileStore::open(t.path())
                .unwrap()
                .get_content(&id)
                .await
                .unwrap(),
            Some(b"x".to_vec())
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_identical_object_writes_are_idempotent() {
        let t = tempfile::tempdir().unwrap();
        let store = Arc::new(FileStore::open(t.path()).unwrap());
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let id = ContentId::for_bytes(b"same");
        let mut handles = Vec::new();
        for _ in 0..2 {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            let id = id.clone();
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                store.put_content(&id, b"same").await
            }));
        }
        for handle in handles {
            handle.await.unwrap().unwrap();
        }
        assert_eq!(
            store.get_content(&id).await.unwrap(),
            Some(b"same".to_vec())
        );
    }
    #[tokio::test]
    async fn commit_with_missing_patch_is_rejected() {
        let t = tempfile::tempdir().unwrap();
        let store = FileStore::open(t.path()).unwrap();
        let patch = PatchId(ContentId::for_bytes(b"absent"));
        let commit = Commit {
            parents: vec![],
            patch,
            author: "a".into(),
            message: "m".into(),
            event_time: "e".into(),
            recorded_time: "r".into(),
        };
        assert!(matches!(
            store.put_commit(&AnyCommit::V1(commit)).await,
            Err(LedgerError::MissingPatch(_))
        ));
    }
    #[tokio::test]
    async fn existing_non_commit_object_cannot_become_head() {
        // Regression for the `exists`-based target check: a patch is a real immutable
        // object, but it is not a commit and must never be installable as HEAD.
        let t = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(t.path()).unwrap();
        let patch = Patch::new([Operation {
            kind: OperationKind::Add,
            quad: "<urn:s> <urn:p> \"v\" .".parse().unwrap(),
        }])
        .unwrap();
        let patch_id = patch.id();
        ledger
            .immutable
            .put_content(&patch_id.0, &patch.canonical_bytes())
            .await
            .unwrap();
        assert!(ledger.immutable.exists(&patch_id.0).await.unwrap());
        let masquerading = CommitId(patch_id.0.clone());
        assert_eq!(
            ledger.immutable.get_commit(&masquerading).await.unwrap(),
            None
        );
        assert!(matches!(
            ledger.advance_ref(None, &masquerading).await,
            Err(LedgerError::MissingTarget(_))
        ));
        assert_eq!(ledger.head().await.unwrap(), None);
        // An arbitrary blob (not even a patch) is rejected the same way.
        let blob = ContentId::for_bytes(b"not a commit");
        ledger
            .immutable
            .put_content(&blob, b"not a commit")
            .await
            .unwrap();
        assert!(matches!(
            ledger.advance_ref(None, &CommitId(blob)).await,
            Err(LedgerError::MissingTarget(_))
        ));
    }
    #[tokio::test]
    async fn filesystem_store_is_strict_about_content_headers_and_layout() {
        use std::io::Write as _;
        let t = tempfile::tempdir().unwrap();
        // A mistyped path is an error, never a freshly created store.
        assert!(FileStore::open_existing(t.path().join("missing")).is_err());
        let store = FileStore::open(t.path()).unwrap();
        assert!(FileStore::open_existing(t.path()).is_ok());
        // Commit envelopes cannot be smuggled in as content.
        let patch = Patch::new([Operation {
            kind: OperationKind::Add,
            quad: "<urn:s> <urn:p> \"v\" .".parse().unwrap(),
        }])
        .unwrap();
        store
            .put_content(&patch.id().0, &patch.canonical_bytes())
            .await
            .unwrap();
        let commit = AnyCommit::V1(Commit {
            parents: vec![],
            patch: patch.id(),
            author: "a".into(),
            message: "m".into(),
            event_time: "e".into(),
            recorded_time: "r".into(),
        });
        let bytes = commit.canonical_bytes().unwrap();
        assert!(matches!(
            store.put_content(&commit.id().unwrap().0, &bytes).await,
            Err(LedgerError::InvalidCommit(_))
        ));
        // A known commit header with a broken body is corruption, not "absent"; an
        // unknown commit version fails closed.
        for (header, expect_unknown) in [
            (COMMIT_V1_HEADER, false),
            (COMMIT_V2_HEADER, false),
            (b"sculpin-cognitive-commit-v9\0".as_slice(), true),
        ] {
            let mut broken = header.to_vec();
            broken.extend_from_slice(b"garbage");
            let id = ContentId::for_bytes(&broken);
            let path = store.object_path(&id);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::File::create(&path).unwrap().write_all(&broken).unwrap();
            let result = store.get_commit(&CommitId(id)).await;
            if expect_unknown {
                assert!(matches!(result, Err(LedgerError::UnknownCommitVersion(_))));
            } else {
                assert!(matches!(result, Err(LedgerError::CorruptObject { .. })));
            }
        }
        // A commit whose patch is an arbitrary blob (existing, non-commit content) is
        // refused and the commit object is not published.
        let blob = b"not a patch at all".to_vec();
        let blob_id = ContentId::for_bytes(&blob);
        store.put_content(&blob_id, &blob).await.unwrap();
        let with_blob = AnyCommit::V1(Commit {
            parents: vec![],
            patch: PatchId(blob_id),
            author: "a".into(),
            message: "blob".into(),
            event_time: "e".into(),
            recorded_time: "r".into(),
        });
        assert!(matches!(
            store.put_commit(&with_blob).await,
            Err(LedgerError::InvalidPatch { .. })
        ));
        assert!(!store.exists(&with_blob.id().unwrap().0).await.unwrap());
        // Hash-correct but non-canonical, and malformed, patch bytes are refused the same way.
        for (content, why) in [
            (
                b"sculpin-rdf-patch-v1\nA <urn:s> <urn:p> \"b\" .\nA <urn:s> <urn:p> \"a\" .\n"
                    .to_vec(),
                "not canonical",
            ),
            (
                b"sculpin-rdf-patch-v1\nA <urn:s> <urn:p> .\n".to_vec(),
                "not a canonical N-Quad",
            ),
        ] {
            let content_id = ContentId::for_bytes(&content);
            store.put_content(&content_id, &content).await.unwrap();
            let commit = AnyCommit::V1(Commit {
                parents: vec![],
                patch: PatchId(content_id),
                author: "a".into(),
                message: why.into(),
                event_time: "e".into(),
                recorded_time: "r".into(),
            });
            match store.put_commit(&commit).await {
                Err(LedgerError::InvalidPatch { reason, .. }) => {
                    assert!(reason.contains(why), "{reason}")
                }
                other => panic!("{other:?}"),
            }
            assert!(!store.exists(&commit.id().unwrap().0).await.unwrap());
        }
        // A stored patch whose bytes no longer hash to their id is corruption.
        let healthy = Patch::new([Operation {
            kind: OperationKind::Add,
            quad: "<urn:s> <urn:p> \"healthy\" .".parse().unwrap(),
        }])
        .unwrap();
        let path = store.object_path(&healthy.id().0);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            b"sculpin-rdf-patch-v1\nA <urn:s> <urn:p> \"swapped\" .\n",
        )
        .unwrap();
        let on_corrupt = AnyCommit::V1(Commit {
            parents: vec![],
            patch: healthy.id(),
            author: "a".into(),
            message: "corrupt".into(),
            event_time: "e".into(),
            recorded_time: "r".into(),
        });
        assert!(matches!(
            store.put_commit(&on_corrupt).await,
            Err(LedgerError::CorruptObject { .. })
        ));
        assert!(!store.exists(&on_corrupt.id().unwrap().0).await.unwrap());
        // Listing is strict: a stray regular file at shard level is reported, not skipped.
        assert!(store.list_objects().is_ok());
        fs::write(t.path().join("objects/sha256/stray"), b"x").unwrap();
        assert!(matches!(
            store.list_objects(),
            Err(LedgerError::CorruptObject { .. })
        ));
    }
    #[tokio::test]
    async fn store_holds_v1_and_v2_commits_side_by_side() {
        use ledger_core::{Actor, CommitV2, GraphId, LedgerTimestamp, PrincipalId, PrincipalType};
        let t = tempfile::tempdir().unwrap();
        let store = FileStore::open(t.path()).unwrap();
        let patch = Patch::new([Operation {
            kind: OperationKind::Add,
            quad: "<urn:s> <urn:p> \"v\" .".parse().unwrap(),
        }])
        .unwrap();
        store
            .put_content(&patch.id().0, &patch.canonical_bytes())
            .await
            .unwrap();
        let v1 = AnyCommit::V1(Commit {
            parents: vec![],
            patch: patch.id(),
            author: "a".into(),
            message: "v1 genesis".into(),
            event_time: "e".into(),
            recorded_time: "r".into(),
        });
        let v1_id = store.put_commit(&v1).await.unwrap();
        let v2 = AnyCommit::V2(CommitV2 {
            graph_id: GraphId::new("graph-1").unwrap(),
            parents: vec![v1_id.clone()],
            patch: patch.id(),
            actor: Actor {
                principal_id: PrincipalId::new("urn:sculpin:agent:a").unwrap(),
                principal_type: PrincipalType::Agent,
                on_behalf_of: None,
            },
            activity: "test".into(),
            event_time: None,
            recorded_at: LedgerTimestamp::parse_rfc3339("2026-09-26T00:00:00Z").unwrap(),
            evidence_refs: vec![],
            source_system: None,
            message: "v2 child of a v1 parent".into(),
        });
        let v2_id = store.put_commit(&v2).await.unwrap();
        let read_v1 = store.get_commit(&v1_id).await.unwrap().unwrap();
        let read_v2 = store.get_commit(&v2_id).await.unwrap().unwrap();
        assert_eq!(read_v1.version(), 1);
        assert_eq!(read_v2.version(), 2);
        assert_eq!(read_v2.parents(), &[v1_id]);
        assert_eq!(read_v2.graph_id().unwrap().as_str(), "graph-1");
        assert_eq!(read_v1, v1);
        assert_eq!(read_v2, v2);
        // A v2 commit whose parent is missing is rejected like a v1 one.
        let mut orphan = v2.clone();
        if let AnyCommit::V2(inner) = &mut orphan {
            inner.parents = vec![CommitId(ContentId::for_bytes(b"absent"))];
        }
        assert!(matches!(
            store.put_commit(&orphan).await,
            Err(LedgerError::MissingParent(_))
        ));
    }
    #[tokio::test]
    async fn stale_cas_loses() {
        let t = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(t.path()).unwrap();
        let patch = Patch::new([Operation {
            kind: OperationKind::Add,
            quad: "<urn:s> <urn:p> \"v\" .".parse().unwrap(),
        }])
        .unwrap();
        let req = |expected| CommitRequest {
            expected_head: expected,
            patch: patch.clone(),
            author: "a".into(),
            message: "m".into(),
            event_time: "2026-01-01T00:00:00Z".into(),
        };
        let c1 = ledger.commit(req(None)).await.unwrap();
        let AnyCommit::V1(stored) = ledger.immutable.get_commit(&c1).await.unwrap().unwrap() else {
            panic!("bootstrap write path produces v1 envelopes");
        };
        assert!(OffsetDateTime::parse(&stored.recorded_time, &Rfc3339).is_ok());
        assert_ne!(stored.recorded_time, "2026-01-01T00:00:00Z");
        let c2 = ledger.commit(req(Some(c1.clone()))).await.unwrap();
        assert!(matches!(
            ledger.commit(req(Some(c1))).await,
            Err(LedgerError::HeadChanged { .. })
        ));
        assert_eq!(ledger.head().await.unwrap(), Some(c2));
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn simultaneous_writers_exactly_one_advances() {
        let t = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(t.path()).unwrap();
        let make_patch = |value: &str| {
            Patch::new([Operation {
                kind: OperationKind::Add,
                quad: format!("<urn:s> <urn:p> \"{value}\" .").parse().unwrap(),
            }])
            .unwrap()
        };
        let genesis = ledger
            .commit(CommitRequest {
                expected_head: None,
                patch: make_patch("base"),
                author: "a".into(),
                message: "base".into(),
                event_time: "e".into(),
            })
            .await
            .unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let mut handles = Vec::new();
        for value in ["writer-a", "writer-b"] {
            let ledger = ledger.clone();
            let barrier = Arc::clone(&barrier);
            let expected = genesis.clone();
            let patch = make_patch(value);
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                ledger
                    .commit(CommitRequest {
                        expected_head: Some(expected),
                        patch,
                        author: value.into(),
                        message: value.into(),
                        event_time: "e".into(),
                    })
                    .await
            }));
        }
        let mut results = Vec::new();
        for handle in handles {
            results.push(handle.await.unwrap());
        }
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, Err(LedgerError::HeadChanged { .. })))
                .count(),
            1
        );
    }
}
