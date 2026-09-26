//! Filesystem persistence and the linear ledger application service.

use fs2::FileExt;
use ledger_core::{
    Commit, CommitId, CommitStore, ContentId, LedgerError, ObjectStore, PatchId, RefStore,
};
use ledger_rdf::{OperationKind, Patch, Quad};
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

#[derive(Clone, Debug)]
pub struct FileStore {
    root: PathBuf,
}
impl FileStore {
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
    fn object_path(&self, id: &ContentId) -> PathBuf {
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
    fn put_commit_sync(&self, commit: &Commit) -> Result<CommitId, LedgerError> {
        for parent in &commit.parents {
            if self.get_commit_sync(parent)?.is_none() {
                return Err(LedgerError::MissingParent(parent.clone()));
            }
        }
        if self.get_object_sync(&commit.patch.0)?.is_none() {
            return Err(LedgerError::MissingPatch(commit.patch.clone()));
        }
        let bytes = commit.canonical_bytes()?;
        let id = commit.id()?;
        self.put_object_sync(&id.0, &bytes)?;
        Ok(id)
    }
    fn get_commit_sync(&self, id: &CommitId) -> Result<Option<Commit>, LedgerError> {
        let Some(bytes) = self.get_object_sync(&id.0)? else {
            return Ok(None);
        };
        let c = Commit::from_canonical_bytes(&bytes)?;
        if c.id()? != *id {
            return Err(LedgerError::CorruptObject {
                id: id.0.clone(),
                reason: "commit ID mismatch".into(),
            });
        }
        Ok(Some(c))
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
impl ObjectStore for FileStore {
    async fn put(&self, id: &ContentId, bytes: &[u8]) -> Result<(), LedgerError> {
        let this = self.clone();
        let id = id.clone();
        let bytes = bytes.to_vec();
        run_blocking(move || this.put_object_sync(&id, &bytes)).await
    }
    async fn get(&self, id: &ContentId) -> Result<Option<Vec<u8>>, LedgerError> {
        let this = self.clone();
        let id = id.clone();
        run_blocking(move || this.get_object_sync(&id)).await
    }
}
#[async_trait::async_trait]
impl CommitStore for FileStore {
    async fn put_commit(&self, commit: &Commit) -> Result<CommitId, LedgerError> {
        let this = self.clone();
        let commit = commit.clone();
        run_blocking(move || this.put_commit_sync(&commit)).await
    }
    async fn get_commit(&self, id: &CommitId) -> Result<Option<Commit>, LedgerError> {
        let this = self.clone();
        let id = id.clone();
        run_blocking(move || this.get_commit_sync(&id)).await
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
        /// ref. Runs migrations. The schema is structurally ready for named graphs
        /// and branches; Milestone 0002 only exercises ('default', 'main').
        pub async fn with_ref(
            pool: sqlx::PgPool,
            graph_id: impl Into<String>,
            branch: impl Into<String>,
        ) -> Result<Self, LedgerError> {
            sqlx::migrate!("../../migrations")
                .run(&pool)
                .await
                .map_err(storage)?;
            Ok(Self {
                pool,
                graph_id: graph_id.into(),
                branch: branch.into(),
            })
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
            let new_s = new.to_string();
            let rows = match expected {
                Some(expected) => sqlx::query(
                    "UPDATE refs SET head = $1, updated_at = now() \
                     WHERE graph_id = $2 AND branch = $3 AND head = $4",
                )
                .bind(&new_s)
                .bind(&self.graph_id)
                .bind(&self.branch)
                .bind(expected.to_string())
                .execute(&self.pool)
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
                .execute(&self.pool)
                .await
                .map_err(storage)?
                .rows_affected(),
            };
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
    store: Arc<FileStore>,
    refs: Arc<dyn RefStore>,
}
impl Ledger {
    /// Open a ledger whose immutable objects and mutable ref both live on the filesystem.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, LedgerError> {
        let store = Arc::new(FileStore::open(root)?);
        let refs: Arc<dyn RefStore> = store.clone();
        Ok(Self { store, refs })
    }
    /// Compose the ledger with an externally provided ref backend (e.g. PostgreSQL),
    /// keeping immutable objects and commits on the filesystem `store`.
    pub fn with_ref_store(store: Arc<FileStore>, refs: Arc<dyn RefStore>) -> Self {
        Self { store, refs }
    }
    pub async fn head(&self) -> Result<Option<CommitId>, LedgerError> {
        self.refs.head().await
    }
    pub async fn commit(&self, request: CommitRequest) -> Result<CommitId, LedgerError> {
        let patch_bytes = request.patch.canonical_bytes();
        let patch_id = request.patch.id();
        self.store.put(&patch_id.0, &patch_bytes).await?;
        let commit = Commit {
            parents: request.expected_head.clone().into_iter().collect(),
            patch: patch_id,
            author: request.author,
            message: request.message,
            event_time: request.event_time,
            recorded_time: OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .map_err(storage)?,
        };
        let id = self.store.put_commit(&commit).await?;
        self.advance_ref(request.expected_head.as_ref(), &id)
            .await?;
        Ok(id)
    }
    /// Enforce the ref-target-existence invariant uniformly for every ref backend, then
    /// perform the atomic swap. The ref store is a pure primitive that does not see commits.
    pub(crate) async fn advance_ref(
        &self,
        expected: Option<&CommitId>,
        new: &CommitId,
    ) -> Result<(), LedgerError> {
        if self.store.get_commit(new).await?.is_none() {
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
                .store
                .get_commit(&current)
                .await?
                .ok_or_else(|| LedgerError::NotFound(current.0.clone()))?;
            // Protocol v1 defines parent zero as the state reconstruction parent.
            // Additional merge parents carry ancestry and provenance.
            cursor = c.parents.first().cloned();
            chain.push(c);
        }
        let mut state = BTreeSet::new();
        for commit in chain.iter().rev() {
            let bytes = self
                .store
                .get(&commit.patch.0)
                .await?
                .ok_or_else(|| LedgerError::NotFound(commit.patch.0.clone()))?;
            let patch = Patch::from_canonical_bytes(&bytes).map_err(storage)?;
            for op in patch.operations() {
                match op.kind {
                    OperationKind::Add => {
                        state.insert(op.quad.clone());
                    }
                    OperationKind::Delete => {
                        state.remove(&op.quad);
                    }
                }
            }
        }
        Ok(state)
    }
    pub async fn patch(&self, id: &PatchId) -> Result<Option<Patch>, LedgerError> {
        match self.store.get(&id.0).await? {
            Some(b) => Ok(Some(Patch::from_canonical_bytes(&b).map_err(storage)?)),
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
            .put(&id, b"x")
            .await
            .unwrap();
        assert_eq!(
            FileStore::open(t.path()).unwrap().get(&id).await.unwrap(),
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
                store.put(&id, b"same").await
            }));
        }
        for handle in handles {
            handle.await.unwrap().unwrap();
        }
        assert_eq!(store.get(&id).await.unwrap(), Some(b"same".to_vec()));
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
            store.put_commit(&commit).await,
            Err(LedgerError::MissingPatch(_))
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
        let stored = ledger.store.get_commit(&c1).await.unwrap().unwrap();
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
