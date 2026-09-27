//! Restricted deterministic RDF patch protocol for the walking skeleton.

mod effective_delta;
pub use effective_delta::{DeltaError, DeltaPolicy, apply_patch, effective_delta};

use ledger_core::{ContentId, PatchId};
use oxttl::NQuadsParser;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use std::{collections::BTreeSet, fmt, str::FromStr};
use thiserror::Error;

const PATCH_HEADER: &str = "sculpin-rdf-patch-v1\n";
/// Header of the canonical dataset serialization whose digest identifies a reconstructed
/// state (`sculpin-rdf-state/v1`, ADR-0018).
pub const STATE_V1_HEADER: &str = "sculpin-rdf-state/v1\n";

/// Canonical bytes of a dataset: the header, then one canonical N-Quads line per quad in
/// bytewise ascending order (the `BTreeSet<Quad>` order), each LF-terminated. An empty
/// dataset is the header alone. Named-graph quads carry their graph term in the line.
pub fn canonical_state_bytes(state: &BTreeSet<Quad>) -> Vec<u8> {
    let mut out = STATE_V1_HEADER.to_owned();
    for quad in state {
        out.push_str(&quad.0);
        out.push('\n');
    }
    out.into_bytes()
}

/// The candidate state digest (ADR-0018): `sha256:` over [`canonical_state_bytes`]. Derived
/// metadata — never part of `CommitId`; recomputable from immutable history.
pub fn state_digest(state: &BTreeSet<Quad>) -> ContentId {
    ContentId::for_bytes(&canonical_state_bytes(state))
}
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
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Quad(String);
impl fmt::Display for Quad {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl FromStr for Quad {
    type Err = RdfError;
    fn from_str(v: &str) -> Result<Self, Self::Err> {
        let mut parser = NQuadsParser::new().for_slice(v.as_bytes());
        let quad = parser
            .next()
            .ok_or_else(|| RdfError::InvalidQuad(v.into()))?
            .map_err(|error| RdfError::InvalidQuad(error.to_string()))?;
        if parser.next().is_some() {
            return Err(RdfError::InvalidQuad("expected exactly one quad".into()));
        }
        if quad.subject.is_blank_node()
            || quad.object.is_blank_node()
            || quad.graph_name.is_blank_node()
        {
            return Err(RdfError::BlankNode);
        }
        Ok(Self(format!("{quad} .")))
    }
}
impl Quad {
    /// The quad as an N-Triples statement (`<s> <p> <o> .`) when it is in the default graph,
    /// `None` for a named-graph quad. The terms are the canonical N-Triples forms (valid
    /// SPARQL term syntax), so the statement can be embedded in a SPARQL template verbatim.
    pub fn default_graph_triple(&self) -> Option<String> {
        let quad = NQuadsParser::new()
            .for_slice(self.0.as_bytes())
            .next()?
            .expect("a Quad holds one canonical N-Quad");
        quad.graph_name
            .is_default_graph()
            .then(|| format!("{} {} {} .", quad.subject, quad.predicate, quad.object))
    }
}
impl Serialize for Quad {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for Quad {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
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
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Patch {
    operations: Vec<Operation>,
}
impl<'de> Deserialize<'de> for Patch {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct WirePatch {
            operations: Vec<Operation>,
        }
        Self::new(WirePatch::deserialize(deserializer)?.operations).map_err(D::Error::custom)
    }
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
    fn default_graph_triples_are_exposed_and_named_graph_quads_are_not() {
        let default: Quad = "<urn:s> <urn:p> \"x\\ny\"@en .".parse().unwrap();
        assert_eq!(
            default.default_graph_triple().as_deref(),
            Some("<urn:s> <urn:p> \"x\\ny\"@en .")
        );
        let named: Quad = "<urn:s> <urn:p> <urn:o> <urn:g> .".parse().unwrap();
        assert_eq!(named.default_graph_triple(), None);
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
        assert!("<relative> <urn:p> <urn:o> .".parse::<Quad>().is_err());
        assert!("<urn:s bad> <urn:p> <urn:o> .".parse::<Quad>().is_err());
    }
    #[test]
    fn serde_cannot_bypass_quad_validation() {
        assert!(serde_json::from_str::<Quad>(r#""_:hidden <urn:p> <urn:o> .""#).is_err());
        assert!(serde_json::from_str::<Quad>(r#""<relative> <urn:p> <urn:o> .""#).is_err());
    }
    #[test]
    fn serde_cannot_bypass_patch_normalization() {
        let duplicate = r#"{"operations":[{"kind":"Add","quad":"<urn:s> <urn:p> <urn:o> ."},{"kind":"Add","quad":"<urn:s> <urn:p> <urn:o> ."}]}"#;
        let patch: Patch = serde_json::from_str(duplicate).unwrap();
        assert_eq!(patch.operations().len(), 1);

        let conflict = r#"{"operations":[{"kind":"Add","quad":"<urn:s> <urn:p> <urn:o> ."},{"kind":"Delete","quad":"<urn:s> <urn:p> <urn:o> ."}]}"#;
        assert!(serde_json::from_str::<Patch>(conflict).is_err());
    }
    #[test]
    fn state_digest_is_order_independent_and_distinguishes_graphs() {
        let a: BTreeSet<Quad> = [q("<urn:s> <urn:p> \"a\" ."), q("<urn:s> <urn:p> \"b\" .")]
            .into_iter()
            .collect();
        let b: BTreeSet<Quad> = [q("<urn:s> <urn:p> \"b\" ."), q("<urn:s> <urn:p> \"a\" .")]
            .into_iter()
            .collect();
        assert_eq!(state_digest(&a), state_digest(&b));
        let named: BTreeSet<Quad> = [q("<urn:s> <urn:p> \"a\" <urn:g> .")].into_iter().collect();
        let default: BTreeSet<Quad> = [q("<urn:s> <urn:p> \"a\" .")].into_iter().collect();
        assert_ne!(state_digest(&named), state_digest(&default));
        assert_eq!(
            canonical_state_bytes(&BTreeSet::new()),
            STATE_V1_HEADER.as_bytes()
        );
        assert_ne!(
            state_digest(&BTreeSet::new()),
            ContentId::for_bytes(b""),
            "the empty state is the header, not empty bytes"
        );
    }
    #[test]
    fn standards_parser_canonicalizes_supported_nquads_terms() {
        let language: Quad = "<urn:s> <urn:p> \"bonjour\"@fr <urn:g> .".parse().unwrap();
        assert_eq!(
            language.to_string(),
            "<urn:s> <urn:p> \"bonjour\"@fr <urn:g> ."
        );

        let typed: Quad = "<urn:s> <urn:p> \"7\"^^<http://www.w3.org/2001/XMLSchema#integer> ."
            .parse()
            .unwrap();
        assert_eq!(
            typed.to_string(),
            "<urn:s> <urn:p> \"7\"^^<http://www.w3.org/2001/XMLSchema#integer> ."
        );

        let escaped: Quad = "<urn:s> <urn:p> \"\\u0061\" .".parse().unwrap();
        assert_eq!(escaped.to_string(), "<urn:s> <urn:p> \"a\" .");
    }
}
