//! Restricted deterministic RDF patch protocol for the walking skeleton.

use ledger_core::{ContentId, PatchId};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fmt, str::FromStr};
use thiserror::Error;

const PATCH_HEADER: &str = "sculpin-rdf-patch-v1\n";
#[derive(Debug, Error, Eq, PartialEq)]
pub enum RdfError {
    #[error("invalid canonical N-Quad: {0}")]
    InvalidQuad(String),
    #[error("blank nodes are forbidden in persistent ledger RDF")]
    BlankNode,
    #[error("a quad cannot be both added and deleted in one normalized patch: {0}")]
    ConflictingOperation(String),
    #[error("invalid patch encoding: {0}")]
    InvalidPatch(String),
}
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Quad(String);
impl fmt::Display for Quad {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl FromStr for Quad {
    type Err = RdfError;
    fn from_str(v: &str) -> Result<Self, Self::Err> {
        validate_quad(v)?;
        Ok(Self(v.into()))
    }
}

fn valid_iri(v: &str) -> bool {
    v.starts_with('<')
        && v.ends_with('>')
        && v[1..v.len() - 1].contains(':')
        && !v.contains([' ', '\n', '\r', '\t'])
}
fn valid_literal(value: &str) -> bool {
    let Some(body) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else {
        return false;
    };
    let mut escaped = false;
    for ch in body.chars() {
        if escaped {
            if !matches!(ch, '"' | '\\' | 'n' | 'r' | 't') {
                return false;
            }
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' || ch.is_control() {
            return false;
        }
    }
    !escaped
}
fn terms(input: &str) -> Result<Vec<String>, RdfError> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for ch in input.chars() {
        if quoted {
            current.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                quoted = false;
            }
        } else if ch == '"' {
            quoted = true;
            current.push(ch);
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                result.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if quoted || escaped {
        return Err(RdfError::InvalidQuad(input.into()));
    }
    if !current.is_empty() {
        result.push(current);
    }
    Ok(result)
}
fn validate_quad(value: &str) -> Result<(), RdfError> {
    if value.chars().any(|c| c == '\n' || c == '\r' || c == '\0') {
        return Err(RdfError::InvalidQuad(value.into()));
    }
    let body = value
        .strip_suffix(" .")
        .ok_or_else(|| RdfError::InvalidQuad(value.into()))?;
    let ts = terms(body)?;
    if ts.iter().any(|term| term.starts_with("_:")) {
        return Err(RdfError::BlankNode);
    }
    if !(ts.len() == 3 || ts.len() == 4)
        || !valid_iri(&ts[0])
        || !valid_iri(&ts[1])
        || (ts.len() == 4 && !valid_iri(&ts[3]))
    {
        return Err(RdfError::InvalidQuad(value.into()));
    }
    let object = &ts[2];
    if !(valid_iri(object) || valid_literal(object)) {
        return Err(RdfError::InvalidQuad(value.into()));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum OperationKind {
    Add,
    Delete,
}
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Operation {
    pub kind: OperationKind,
    pub quad: Quad,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Patch {
    operations: Vec<Operation>,
}
impl Patch {
    pub fn new(operations: impl IntoIterator<Item = Operation>) -> Result<Self, RdfError> {
        let sorted: BTreeSet<_> = operations.into_iter().collect();
        let mut seen = std::collections::BTreeMap::new();
        for op in &sorted {
            if let Some(previous) = seen.insert(&op.quad, op.kind)
                && previous != op.kind
            {
                return Err(RdfError::ConflictingOperation(op.quad.to_string()));
            }
        }
        Ok(Self {
            operations: sorted.into_iter().collect(),
        })
    }
    pub fn operations(&self) -> &[Operation] {
        &self.operations
    }
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = PATCH_HEADER.to_owned();
        for op in &self.operations {
            out.push(match op.kind {
                OperationKind::Add => 'A',
                OperationKind::Delete => 'D',
            });
            out.push(' ');
            out.push_str(&op.quad.0);
            out.push('\n');
        }
        out.into_bytes()
    }
    pub fn id(&self) -> PatchId {
        PatchId(ContentId::for_bytes(&self.canonical_bytes()))
    }
    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, RdfError> {
        let text =
            std::str::from_utf8(bytes).map_err(|_| RdfError::InvalidPatch("non-UTF-8".into()))?;
        let body = text
            .strip_prefix(PATCH_HEADER)
            .ok_or_else(|| RdfError::InvalidPatch("unknown header".into()))?;
        let mut ops = Vec::new();
        for line in body.lines() {
            let (tag, q) = line
                .split_once(' ')
                .ok_or_else(|| RdfError::InvalidPatch("operation missing separator".into()))?;
            let kind = match tag {
                "A" => OperationKind::Add,
                "D" => OperationKind::Delete,
                _ => return Err(RdfError::InvalidPatch("unknown operation".into())),
            };
            ops.push(Operation {
                kind,
                quad: q.parse()?,
            });
        }
        let patch = Self::new(ops)?;
        if patch.canonical_bytes() != bytes {
            return Err(RdfError::InvalidPatch("bytes are not canonical".into()));
        }
        Ok(patch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn q(v: &str) -> Quad {
        v.parse().unwrap()
    }
    #[test]
    fn sorts_and_deduplicates() {
        let a = Operation {
            kind: OperationKind::Add,
            quad: q("<urn:s> <urn:p> \"v\" ."),
        };
        let p = Patch::new([a.clone(), a]).unwrap();
        assert_eq!(p.operations().len(), 1);
        assert_eq!(
            p.canonical_bytes(),
            b"sculpin-rdf-patch-v1\nA <urn:s> <urn:p> \"v\" .\n"
        );
    }
    #[test]
    fn rejects_blank_nodes() {
        assert_eq!(
            "_:s <urn:p> \"v\" .".parse::<Quad>().unwrap_err(),
            RdfError::BlankNode
        );
    }
    #[test]
    fn rejects_conflict() {
        let quad = q("<urn:s> <urn:p> <urn:o> .");
        assert!(matches!(
            Patch::new([
                Operation {
                    kind: OperationKind::Add,
                    quad: quad.clone()
                },
                Operation {
                    kind: OperationKind::Delete,
                    quad
                }
            ]),
            Err(RdfError::ConflictingOperation(_))
        ));
    }
    #[test]
    fn rejects_non_canonical_literals() {
        assert!("<urn:s> <urn:p> \"a\"\"b\" .".parse::<Quad>().is_err());
        assert!(r#"<urn:s> <urn:p> "a\q" ."#.parse::<Quad>().is_err());
        assert!("<urn:s> <urn:p> \"line\nbreak\" .".parse::<Quad>().is_err());
    }
}
