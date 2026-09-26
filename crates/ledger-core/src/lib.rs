//! Infrastructure-free protocol types and storage boundaries.

mod commit_v2;
mod identity;
mod temporal;

pub use commit_v2::{AnyCommit, COMMIT_V2_HEADER, CommitV2};
pub use identity::{
    Actor, AuthenticatedPrincipal, GraphId, MAX_EVIDENCE_REFS, MAX_GRAPH_ID_BYTES,
    MAX_IDENTIFIER_BYTES, MAX_MESSAGE_BYTES, PrincipalId, PrincipalType, TenantId,
};
pub use temporal::LedgerTimestamp;

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use sha2::{Digest, Sha256};
use std::{fmt, str::FromStr};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum LedgerError {
    #[error("invalid content ID: {0}")]
    InvalidContentId(String),
    #[error("content {0} was not found")]
    NotFound(ContentId),
    #[error("parent commit {0} does not exist")]
    MissingParent(CommitId),
    #[error("patch {0} does not exist")]
    MissingPatch(PatchId),
    #[error("ref target commit {0} does not exist")]
    MissingTarget(CommitId),
    #[error("HEAD_CHANGED: expected {expected:?}, actual {actual:?}")]
    HeadChanged {
        expected: Option<CommitId>,
        actual: Option<CommitId>,
    },
    #[error("immutable object collision for {0}")]
    ObjectCollision(ContentId),
    #[error("corrupt object {id}: {reason}")]
    CorruptObject { id: ContentId, reason: String },
    #[error("invalid commit encoding: {0}")]
    InvalidCommit(String),
    #[error("unknown commit version (header {0:?})")]
    UnknownCommitVersion(String),
    #[error("invalid {field}: {reason}")]
    InvalidIdentifier { field: &'static str, reason: String },
    #[error("invalid timestamp: {0}")]
    InvalidTimestamp(String),
    #[error(
        "GRAPH_BINDING_CONFLICT: commit {commit} is indexed under graph {indexed}, not {requested}"
    )]
    GraphBindingConflict {
        commit: CommitId,
        indexed: String,
        requested: String,
    },
    #[error(
        "CROSS_GRAPH_PARENT: parent {parent} belongs to graph {parent_graph}, child belongs to {graph}"
    )]
    CrossGraphParent {
        parent: CommitId,
        parent_graph: String,
        graph: String,
    },
    #[error("INVALID_PATCH: patch {id} is not a valid canonical ledger patch: {reason}")]
    InvalidPatch { id: PatchId, reason: String },
    #[error(
        "MIGRATION_SOURCE_MOVED: source HEAD changed during cutover (before {before:?}, after {after:?}); quiesce source writers and re-run"
    )]
    MigrationSourceMoved {
        before: Option<CommitId>,
        after: Option<CommitId>,
    },
    #[error("graph {0} already exists")]
    GraphAlreadyExists(String),
    #[error("graph {0} does not exist")]
    UnknownGraph(String),
    #[error("storage error: {0}")]
    Storage(String),
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ContentId(String);

impl ContentId {
    pub fn for_bytes(bytes: &[u8]) -> Self {
        Self(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
    }
    pub fn digest_hex(&self) -> &str {
        &self.0[7..]
    }
}
impl fmt::Display for ContentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl FromStr for ContentId {
    type Err = LedgerError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some(hex_part) = value.strip_prefix("sha256:") else {
            return Err(LedgerError::InvalidContentId(value.into()));
        };
        if hex_part.len() != 64
            || !hex_part
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(LedgerError::InvalidContentId(value.into()));
        }
        Ok(Self(value.into()))
    }
}
impl TryFrom<String> for ContentId {
    type Error = LedgerError;
    fn try_from(v: String) -> Result<Self, Self::Error> {
        v.parse()
    }
}
impl From<ContentId> for String {
    fn from(v: ContentId) -> Self {
        v.0
    }
}

macro_rules! typed_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub ContentId);
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
        impl FromStr for $name {
            type Err = LedgerError;
            fn from_str(v: &str) -> Result<Self, Self::Err> {
                Ok(Self(v.parse()?))
            }
        }
    };
}
typed_id!(PatchId);
typed_id!(CommitId);

/// The bootstrap `sculpin-commit-v1` envelope. Readable forever (dual read, ADR-0009);
/// new persistent history uses [`CommitV2`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Commit {
    /// Ordered parents. Parent zero is the reconstruction/mainline parent.
    /// Protocol v1 permits zero (genesis), one (linear), or two (merge).
    pub parents: Vec<CommitId>,
    pub patch: PatchId,
    pub author: String,
    pub message: String,
    pub event_time: String,
    pub recorded_time: String,
}

/// Header bytes of the bootstrap v1 envelope (see [`COMMIT_V2_HEADER`] for v2).
pub const COMMIT_V1_HEADER: &[u8] = b"sculpin-commit-v1\0";
pub(crate) const COMMIT_HEADER: &[u8] = COMMIT_V1_HEADER;
/// Alias naming the bootstrap envelope explicitly alongside [`CommitV2`].
pub type CommitV1 = Commit;
impl Commit {
    fn validate_shape(&self) -> Result<(), LedgerError> {
        if self.parents.len() > 2 {
            return Err(LedgerError::InvalidCommit(
                "protocol v1 permits at most two parents".into(),
            ));
        }
        if self.parents.len() == 2 && self.parents[0] == self.parents[1] {
            return Err(LedgerError::InvalidCommit(
                "merge parents must be distinct".into(),
            ));
        }
        Ok(())
    }
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, LedgerError> {
        self.validate_shape()?;
        let fields = [
            self.patch.to_string(),
            self.author.clone(),
            self.message.clone(),
            self.event_time.clone(),
            self.recorded_time.clone(),
        ];
        let mut out = COMMIT_HEADER.to_vec();
        out.extend_from_slice(
            &u32::try_from(self.parents.len())
                .expect("parent count is bounded by two")
                .to_be_bytes(),
        );
        for parent in &self.parents {
            encode_field(&mut out, &parent.to_string())?;
        }
        for field in fields {
            encode_field(&mut out, &field)?;
        }
        Ok(out)
    }
    pub fn id(&self) -> Result<CommitId, LedgerError> {
        Ok(CommitId(ContentId::for_bytes(&self.canonical_bytes()?)))
    }
    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, LedgerError> {
        let mut rest = bytes
            .strip_prefix(COMMIT_HEADER)
            .ok_or_else(|| LedgerError::InvalidCommit("unknown header".into()))?;
        if rest.len() < 4 {
            return Err(LedgerError::InvalidCommit("truncated parent count".into()));
        }
        let parent_count = u32::from_be_bytes(rest[..4].try_into().expect("four bytes")) as usize;
        rest = &rest[4..];
        if parent_count > 2 {
            return Err(LedgerError::InvalidCommit(
                "protocol v1 permits at most two parents".into(),
            ));
        }
        let mut parents = Vec::with_capacity(parent_count);
        for _ in 0..parent_count {
            parents.push(read_field(&mut rest)?.parse()?);
        }
        if parents.len() == 2 && parents[0] == parents[1] {
            return Err(LedgerError::InvalidCommit(
                "merge parents must be distinct".into(),
            ));
        }
        let mut values = Vec::with_capacity(5);
        for _ in 0..5 {
            values.push(read_field(&mut rest)?);
        }
        if !rest.is_empty() {
            return Err(LedgerError::InvalidCommit("trailing bytes".into()));
        }
        Ok(Self {
            parents,
            patch: values[0].parse()?,
            author: values[1].clone(),
            message: values[2].clone(),
            event_time: values[3].clone(),
            recorded_time: values[4].clone(),
        })
    }
}

impl<'de> Deserialize<'de> for Commit {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct WireCommit {
            parents: Vec<CommitId>,
            patch: PatchId,
            author: String,
            message: String,
            event_time: String,
            recorded_time: String,
        }
        let wire = WireCommit::deserialize(deserializer)?;
        let commit = Self {
            parents: wire.parents,
            patch: wire.patch,
            author: wire.author,
            message: wire.message,
            event_time: wire.event_time,
            recorded_time: wire.recorded_time,
        };
        commit.validate_shape().map_err(D::Error::custom)?;
        Ok(commit)
    }
}

pub(crate) fn encode_field(out: &mut Vec<u8>, field: &str) -> Result<(), LedgerError> {
    let len = u32::try_from(field.len())
        .map_err(|_| LedgerError::InvalidCommit("field exceeds u32".into()))?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(field.as_bytes());
    Ok(())
}

pub(crate) fn read_field(rest: &mut &[u8]) -> Result<String, LedgerError> {
    if rest.len() < 4 {
        return Err(LedgerError::InvalidCommit("truncated length".into()));
    }
    let len = u32::from_be_bytes(rest[..4].try_into().expect("four bytes")) as usize;
    *rest = &rest[4..];
    if rest.len() < len {
        return Err(LedgerError::InvalidCommit("truncated field".into()));
    }
    let value = std::str::from_utf8(&rest[..len])
        .map_err(|_| LedgerError::InvalidCommit("non-UTF-8 field".into()))?
        .to_owned();
    *rest = &rest[len..];
    Ok(value)
}

/// The immutable content boundary the ledger service depends on. It unifies content and
/// commit persistence behind one object-safe async trait so `Ledger` no longer holds a
/// concrete filesystem store, enabling shared (e.g. PostgreSQL/S3) backends without
/// touching the service (ADR-0012).
///
/// Contract: `put_content`/`put_commit` are idempotent and MUST enforce content-addressing
/// on the write path (bytes hashing to their id) and MUST NOT expose partially-written
/// objects. `get_content`/`get_commit` verify the digest on read. `exists` MAY be a cheap
/// key-presence probe that reads no bytes and does not verify the digest; therefore the
/// ref-target-existence invariant (a ref never points to missing content) relies on the
/// backend's write path being content-addressed and partial-write-free, not on `exists`
/// itself. A backend that cannot guarantee that (e.g. an object store where a truncated
/// upload is visible) MUST make `exists` verify integrity instead.
///
/// Commit operations are version-neutral: they take and return [`AnyCommit`], so a store
/// holds v1 and v2 envelopes side by side and a decoder change never touches the storage
/// boundary. `put_commit` MUST verify that every parent is an existing *commit* (not merely
/// an existing object) and that the patch exists; `get_commit` MUST return `None` for an
/// id that names a non-commit object rather than reinterpreting its bytes.
///
/// Idempotency is truthful, never assumed: publishing bytes under an id that already
/// holds *different* bytes is `ObjectCollision`, and publishing a commit whose
/// authoritative index row disagrees with the commit (another graph binding, other
/// parents) fails explicitly. `ON CONFLICT DO NOTHING` is never treated as success.
///
/// Same-graph ancestry: a graph is a self-contained version DAG. A production, indexed
/// store MUST reject a commit whose effective graph (the v2 envelope's `graph_id`, or the
/// configured v1 binding) differs from any parent's graph (`CrossGraphParent`). Cross-graph
/// relationships are expressed by merge coordination later, never by parent edges.
///
/// Patch validity: a commit's `patch` MUST name stored bytes that hash to the `PatchId`
/// **and** decode as a canonical Sculpin RDF patch (`InvalidPatch` otherwise). Existing
/// non-commit content is not enough: `put_content` stays generic (checkpoints and other
/// artifacts will use it), so the check is made when a commit references the patch, and
/// again by index verification.
#[async_trait::async_trait]
pub trait ImmutableStore: Send + Sync {
    async fn put_content(&self, id: &ContentId, bytes: &[u8]) -> Result<(), LedgerError>;
    async fn get_content(&self, id: &ContentId) -> Result<Option<Vec<u8>>, LedgerError>;
    async fn put_commit(&self, commit: &AnyCommit) -> Result<CommitId, LedgerError>;
    async fn get_commit(&self, id: &CommitId) -> Result<Option<AnyCommit>, LedgerError>;
    async fn exists(&self, id: &ContentId) -> Result<bool, LedgerError>;
}

/// A pure atomic ref primitive. Implementations swap the stored ref if and only if the
/// current value equals `expected`; they know nothing about commit contents. The
/// application service enforces that `new` targets an existing immutable commit.
#[async_trait::async_trait]
pub trait RefStore: Send + Sync {
    async fn head(&self) -> Result<Option<CommitId>, LedgerError>;
    async fn compare_and_set(
        &self,
        expected: Option<&CommitId>,
        new: &CommitId,
    ) -> Result<(), LedgerError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ids_are_strict() {
        let id = ContentId::for_bytes(b"x");
        assert_eq!(id.to_string().len(), 71);
        assert!(id.to_string().to_uppercase().parse::<ContentId>().is_err());
    }
    #[test]
    fn commit_round_trip() {
        let c = Commit {
            parents: vec![],
            patch: PatchId(ContentId::for_bytes(b"p")),
            author: "a".into(),
            message: "m".into(),
            event_time: "e".into(),
            recorded_time: "r".into(),
        };
        assert_eq!(
            Commit::from_canonical_bytes(&c.canonical_bytes().unwrap()).unwrap(),
            c
        );
    }
    #[test]
    fn ordered_merge_parents_round_trip_and_affect_identity() {
        let a = CommitId(ContentId::for_bytes(b"a"));
        let b = CommitId(ContentId::for_bytes(b"b"));
        let commit = Commit {
            parents: vec![a.clone(), b.clone()],
            patch: PatchId(ContentId::for_bytes(b"p")),
            author: "a".into(),
            message: "merge".into(),
            event_time: "e".into(),
            recorded_time: "r".into(),
        };
        let decoded = Commit::from_canonical_bytes(&commit.canonical_bytes().unwrap()).unwrap();
        assert_eq!(decoded, commit);
        let mut reversed = commit.clone();
        reversed.parents = vec![b, a];
        assert_ne!(commit.id().unwrap(), reversed.id().unwrap());
    }
    #[test]
    fn v1_rejects_duplicate_or_excess_parents() {
        let parent = CommitId(ContentId::for_bytes(b"p"));
        let base = Commit {
            parents: vec![parent.clone(), parent.clone()],
            patch: PatchId(ContentId::for_bytes(b"patch")),
            author: "a".into(),
            message: "m".into(),
            event_time: "e".into(),
            recorded_time: "r".into(),
        };
        assert!(base.canonical_bytes().is_err());
        let mut excessive = base;
        excessive.parents = vec![
            parent,
            CommitId(ContentId::for_bytes(b"q")),
            CommitId(ContentId::for_bytes(b"r")),
        ];
        assert!(excessive.canonical_bytes().is_err());
    }
    #[test]
    fn serde_cannot_bypass_parent_validation() {
        let id = ContentId::for_bytes(b"parent");
        let patch = ContentId::for_bytes(b"patch");
        let duplicate = format!(
            r#"{{"parents":["{id}","{id}"],"patch":"{patch}","author":"a","message":"m","event_time":"e","recorded_time":"r"}}"#
        );
        assert!(serde_json::from_str::<Commit>(&duplicate).is_err());
    }
}
