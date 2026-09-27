//! Target graph identity (ADR-0020): the accepted cognitive graph of a knowledge base and the
//! reserved protocol graphs.

use crate::{ProjectionError, ProjectionErrorCode};
use ledger_core::{MAX_IDENTIFIER_BYTES, validate_token};
use std::fmt;

/// Reserved graph holding one marker per cognitive graph in a target dataset.
pub const MARKER_GRAPH: &str = "urn:sculpin:ledger-projection:v1:markers";
/// Reserved graph used only by the start-up transactional probe (always left empty).
pub const PROBE_GRAPH: &str = "urn:sculpin:ledger-projection:v1:probe";

const PREFIX: &str = "urn:sculpin:kb:";
const SUFFIX: &str = ":cognitive";

/// `urn:sculpin:kb:<pct(kb_id)>:cognitive`: the dedicated named graph holding the accepted
/// cognitive state of one knowledge base. Injective and round-trippable (ADR-0020).
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CognitiveGraph(String);

impl CognitiveGraph {
    /// The cognitive graph of `knowledge_base_id` (a bounded opaque token; never invented).
    pub fn for_knowledge_base(knowledge_base_id: &str) -> Result<Self, ProjectionError> {
        validate_token("knowledge_base_id", knowledge_base_id, MAX_IDENTIFIER_BYTES).map_err(
            |e| ProjectionError::permanent(ProjectionErrorCode::InvalidTargetGraph, e.to_string()),
        )?;
        Ok(Self(format!(
            "{PREFIX}{}{SUFFIX}",
            pct_encode(knowledge_base_id)
        )))
    }

    /// Parse a stored IRI; it must be exactly the mapping of some valid KB id.
    pub fn parse(iri: &str) -> Result<Self, ProjectionError> {
        let invalid = || {
            ProjectionError::permanent(
                ProjectionErrorCode::InvalidTargetGraph,
                "not a sculpin-ledger-projection/v1 cognitive graph IRI",
            )
        };
        let encoded = iri
            .strip_prefix(PREFIX)
            .and_then(|rest| rest.strip_suffix(SUFFIX))
            .ok_or_else(invalid)?;
        let kb = pct_decode(encoded).ok_or_else(invalid)?;
        let graph = Self::for_knowledge_base(&kb)?;
        if graph.0 != iri {
            return Err(invalid());
        }
        Ok(graph)
    }

    pub fn as_iri(&self) -> &str {
        &self.0
    }

    /// The knowledge-base id this graph was derived from.
    pub fn knowledge_base_id(&self) -> String {
        let encoded = &self.0[PREFIX.len()..self.0.len() - SUFFIX.len()];
        pct_decode(encoded).expect("constructed from a valid id")
    }
}

impl fmt::Display for CognitiveGraph {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

/// Percent-encode every UTF-8 byte outside `A–Z a–z 0–9 - . _ ~` as uppercase `%XX`.
pub fn pct_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if unreserved(b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Inverse of [`pct_encode`]; `None` unless the input is exactly its canonical form
/// (uppercase hex, no encoded unreserved bytes, valid UTF-8).
pub fn pct_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = bytes.get(i + 1..i + 3)?;
                if !hex
                    .iter()
                    .all(|h| h.is_ascii_digit() || (b'A'..=b'F').contains(h))
                {
                    return None;
                }
                let byte = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
                if unreserved(byte) {
                    return None;
                }
                out.push(byte);
                i += 3;
            }
            b if unreserved(b) => {
                out.push(b);
                i += 1;
            }
            _ => return None,
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frozen vectors (ADR-0020): changing any of these is a protocol change.
    const VECTORS: [(&str, &str); 5] = [
        (
            "urn:exodus:kb:material-science",
            "urn:sculpin:kb:urn%3Aexodus%3Akb%3Amaterial-science:cognitive",
        ),
        ("materials", "urn:sculpin:kb:materials:cognitive"),
        ("A.b_c-d~e", "urn:sculpin:kb:A.b_c-d~e:cognitive"),
        (
            "kb with space/and#hash",
            "urn:sculpin:kb:kb%20with%20space%2Fand%23hash:cognitive",
        ),
        ("æ検", "urn:sculpin:kb:%C3%A6%E6%A4%9C:cognitive"),
    ];

    #[test]
    fn cognitive_graph_iris_are_frozen_and_round_trip() {
        for (kb, iri) in VECTORS {
            let graph = CognitiveGraph::for_knowledge_base(kb).unwrap();
            assert_eq!(graph.as_iri(), iri, "{kb}");
            assert_eq!(graph.knowledge_base_id(), kb);
            assert_eq!(CognitiveGraph::parse(iri).unwrap(), graph);
        }
        assert_eq!(
            super::MARKER_GRAPH,
            "urn:sculpin:ledger-projection:v1:markers"
        );
        assert_eq!(super::PROBE_GRAPH, "urn:sculpin:ledger-projection:v1:probe");
    }

    #[test]
    fn distinct_ids_never_share_a_graph_and_non_canonical_iris_are_refused() {
        let a = CognitiveGraph::for_knowledge_base("a:b").unwrap();
        let b = CognitiveGraph::for_knowledge_base("a%3Ab").unwrap();
        assert_ne!(a, b);
        for bad in [
            "urn:sculpin:kb:a%3ab:cognitive", // lowercase hex
            "urn:sculpin:kb:%61:cognitive",   // encoded unreserved byte
            "urn:sculpin:kb:a:b:cognitive",   // raw reserved byte
            "urn:sculpin:kb::cognitive",      // empty id
            "urn:sculpin:kb:%FF:cognitive",   // not UTF-8
            "urn:sculpin:kb:a%3:cognitive",   // truncated escape
            "urn:other:kb:a:cognitive",
        ] {
            assert!(CognitiveGraph::parse(bad).is_err(), "{bad}");
        }
        assert!(CognitiveGraph::for_knowledge_base("").is_err());
        assert!(CognitiveGraph::for_knowledge_base("a\nb").is_err());
    }
}
