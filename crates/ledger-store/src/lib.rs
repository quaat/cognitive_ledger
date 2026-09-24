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
    sync::atomic::{AtomicU64, Ordering},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Debug)]
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
impl ObjectStore for FileStore {
    fn put(&self, id: &ContentId, bytes: &[u8]) -> Result<(), LedgerError> {
        if &ContentId::for_bytes(bytes) != id {
            return Err(LedgerError::ObjectCollision(id.clone()));
        }
        let path = self.object_path(id);
        if let Some(existing) = self.get(id)? {
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
    fn get(&self, id: &ContentId) -> Result<Option<Vec<u8>>, LedgerError> {
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
}
impl CommitStore for FileStore {
    fn put_commit(&self, commit: &Commit) -> Result<CommitId, LedgerError> {
        for parent in &commit.parents {
            if self.get_commit(parent)?.is_none() {
                return Err(LedgerError::MissingParent(parent.clone()));
            }
        }
        if self.get(&commit.patch.0)?.is_none() {
            return Err(LedgerError::MissingPatch(commit.patch.clone()));
        }
        let bytes = commit.canonical_bytes()?;
        let id = commit.id()?;
        self.put(&id.0, &bytes)?;
        Ok(id)
    }
    fn get_commit(&self, id: &CommitId) -> Result<Option<Commit>, LedgerError> {
        let Some(bytes) = self.get(&id.0)? else {
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
}
impl RefStore for FileStore {
    fn head(&self) -> Result<Option<CommitId>, LedgerError> {
        match fs::read_to_string(self.head_path()) {
            Ok(v) => Ok(Some(CommitId::from_str(v.trim())?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(storage(e)),
        }
    }
    fn compare_and_set(
        &self,
        expected: Option<&CommitId>,
        new: &CommitId,
    ) -> Result<(), LedgerError> {
        if self.get_commit(new)?.is_none() {
            return Err(LedgerError::MissingTarget(new.clone()));
        }
        let _guard = self.lock()?;
        let actual = self.head()?;
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

#[derive(Clone, Debug)]
pub struct CommitRequest {
    pub expected_head: Option<CommitId>,
    pub patch: Patch,
    pub author: String,
    pub message: String,
    pub event_time: String,
}
#[derive(Debug)]
pub struct Ledger {
    store: FileStore,
}
impl Ledger {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, LedgerError> {
        Ok(Self {
            store: FileStore::open(root)?,
        })
    }
    pub fn head(&self) -> Result<Option<CommitId>, LedgerError> {
        self.store.head()
    }
    pub fn commit(&self, request: CommitRequest) -> Result<CommitId, LedgerError> {
        let patch_bytes = request.patch.canonical_bytes();
        let patch_id = request.patch.id();
        self.store.put(&patch_id.0, &patch_bytes)?;
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
        let id = self.store.put_commit(&commit)?;
        self.store
            .compare_and_set(request.expected_head.as_ref(), &id)?;
        Ok(id)
    }
    pub fn state_at(&self, id: &CommitId) -> Result<BTreeSet<Quad>, LedgerError> {
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
                .get_commit(&current)?
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
                .get(&commit.patch.0)?
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
    pub fn patch(&self, id: &PatchId) -> Result<Option<Patch>, LedgerError> {
        self.store
            .get(&id.0)?
            .map(|b| Patch::from_canonical_bytes(&b).map_err(storage))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ledger_core::PatchId;
    use ledger_rdf::{Operation, OperationKind};
    use std::sync::{Arc, Barrier};
    #[test]
    fn missing_target_rejected() {
        let t = tempfile::tempdir().unwrap();
        let s = FileStore::open(t.path()).unwrap();
        let id = CommitId(ContentId::for_bytes(b"missing"));
        assert!(matches!(
            s.compare_and_set(None, &id),
            Err(LedgerError::MissingTarget(_))
        ));
    }
    #[test]
    fn object_survives_reopen() {
        let t = tempfile::tempdir().unwrap();
        let id = ContentId::for_bytes(b"x");
        FileStore::open(t.path()).unwrap().put(&id, b"x").unwrap();
        assert_eq!(
            FileStore::open(t.path()).unwrap().get(&id).unwrap(),
            Some(b"x".to_vec())
        );
    }
    #[test]
    fn concurrent_identical_object_writes_are_idempotent() {
        let t = tempfile::tempdir().unwrap();
        let store = Arc::new(FileStore::open(t.path()).unwrap());
        let barrier = Arc::new(Barrier::new(3));
        let id = ContentId::for_bytes(b"same");
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                let id = id.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store.put(&id, b"same")
                })
            })
            .collect();
        barrier.wait();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
        assert_eq!(store.get(&id).unwrap(), Some(b"same".to_vec()));
    }
    #[test]
    fn commit_with_missing_patch_is_rejected() {
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
            store.put_commit(&commit),
            Err(LedgerError::MissingPatch(_))
        ));
    }
    #[test]
    fn stale_cas_loses() {
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
        let c1 = ledger.commit(req(None)).unwrap();
        let stored = ledger.store.get_commit(&c1).unwrap().unwrap();
        assert!(OffsetDateTime::parse(&stored.recorded_time, &Rfc3339).is_ok());
        assert_ne!(stored.recorded_time, "2026-01-01T00:00:00Z");
        let c2 = ledger.commit(req(Some(c1.clone()))).unwrap();
        assert!(matches!(
            ledger.commit(req(Some(c1))),
            Err(LedgerError::HeadChanged { .. })
        ));
        assert_eq!(ledger.head().unwrap(), Some(c2));
    }
    #[test]
    fn simultaneous_writers_exactly_one_advances() {
        let t = tempfile::tempdir().unwrap();
        let ledger = Arc::new(Ledger::open(t.path()).unwrap());
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
            .unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let handles: Vec<_> = ["writer-a", "writer-b"]
            .into_iter()
            .map(|value| {
                let ledger = Arc::clone(&ledger);
                let barrier = Arc::clone(&barrier);
                let expected = genesis.clone();
                let patch = make_patch(value);
                std::thread::spawn(move || {
                    barrier.wait();
                    ledger.commit(CommitRequest {
                        expected_head: Some(expected),
                        patch,
                        author: value.into(),
                        message: value.into(),
                        event_time: "e".into(),
                    })
                })
            })
            .collect();
        barrier.wait();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
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
