//! Infrastructure-free protocol types and storage boundaries.

use serde::{Deserialize, Serialize};
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Commit {
    pub parent: Option<CommitId>,
    pub patch: PatchId,
    pub author: String,
    pub message: String,
    pub event_time: String,
    pub recorded_time: String,
}

const COMMIT_HEADER: &[u8] = b"sculpin-commit-v1\0";
impl Commit {
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, LedgerError> {
        let fields = [
            self.parent
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            self.patch.to_string(),
            self.author.clone(),
            self.message.clone(),
            self.event_time.clone(),
            self.recorded_time.clone(),
        ];
        let mut out = COMMIT_HEADER.to_vec();
        for field in fields {
            let len = u32::try_from(field.len())
                .map_err(|_| LedgerError::InvalidCommit("field exceeds u32".into()))?;
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(field.as_bytes());
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
        let mut values = Vec::with_capacity(6);
        for _ in 0..6 {
            if rest.len() < 4 {
                return Err(LedgerError::InvalidCommit("truncated length".into()));
            }
            let len = u32::from_be_bytes(rest[..4].try_into().expect("four bytes")) as usize;
            rest = &rest[4..];
            if rest.len() < len {
                return Err(LedgerError::InvalidCommit("truncated field".into()));
            }
            values.push(
                std::str::from_utf8(&rest[..len])
                    .map_err(|_| LedgerError::InvalidCommit("non-UTF-8 field".into()))?
                    .to_owned(),
            );
            rest = &rest[len..];
        }
        if !rest.is_empty() {
            return Err(LedgerError::InvalidCommit("trailing bytes".into()));
        }
        let parent = if values[0].is_empty() {
            None
        } else {
            Some(values[0].parse()?)
        };
        Ok(Self {
            parent,
            patch: values[1].parse()?,
            author: values[2].clone(),
            message: values[3].clone(),
            event_time: values[4].clone(),
            recorded_time: values[5].clone(),
        })
    }
}

pub trait ObjectStore: Send + Sync {
    fn put(&self, id: &ContentId, bytes: &[u8]) -> Result<(), LedgerError>;
    fn get(&self, id: &ContentId) -> Result<Option<Vec<u8>>, LedgerError>;
}
pub trait CommitStore: Send + Sync {
    fn put_commit(&self, commit: &Commit) -> Result<CommitId, LedgerError>;
    fn get_commit(&self, id: &CommitId) -> Result<Option<Commit>, LedgerError>;
}
pub trait RefStore: Send + Sync {
    fn head(&self) -> Result<Option<CommitId>, LedgerError>;
    fn compare_and_set(
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
            parent: None,
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
}
