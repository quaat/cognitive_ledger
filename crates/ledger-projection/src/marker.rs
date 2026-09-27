//! The projection marker (ADR-0020): which exact ledger state a cognitive graph represents.

use crate::{CognitiveGraph, LP_NAMESPACE, PROJECTION_PROTOCOL, XSD_INTEGER, XSD_STRING};
use ledger_core::{CommitId, ContentId, GraphId, validate_token};
use std::str::FromStr;

/// Maximum branch bytes (the ledger's ref-name bound).
const MAX_BRANCH_BYTES: usize = 128;

/// The marker's predicates, in the order they are written.
pub fn marker_predicates() -> [String; 7] {
    [
        "protocol",
        "graphId",
        "branch",
        "commitId",
        "refVersion",
        "stateDigest",
        "tripleCount",
    ]
    .map(|local| format!("{LP_NAMESPACE}{local}"))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectionMarker {
    pub graph_id: GraphId,
    pub branch: String,
    pub commit: CommitId,
    /// Ledger ref version (>= 1) the target represents.
    pub ref_version: i64,
    /// `sculpin-rdf-state/v1` digest of the accepted state (ledger-computed).
    pub state_digest: ContentId,
    /// Triples the target holds for this projection, **counted by the target inside the
    /// write transaction** (the target may merge literals it canonicalizes to one value, so
    /// the ledger cannot predict it). Ignored when writing.
    pub triple_count: u64,
}

/// One object term of a marker triple as a SPARQL result binding reports it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MarkerTerm {
    pub value: String,
    /// Datatype IRI of a literal (`None` for a plain literal).
    pub datatype: Option<String>,
    pub is_literal: bool,
    /// A blank node (never a valid marker value, and not addressable in a precondition).
    pub is_blank: bool,
    /// Language tag of a literal (a tagged literal is never a valid marker value).
    pub language: Option<String>,
}

/// The marker as observed in a target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MarkerRead {
    Absent,
    /// Present but not a well-formed v1 marker (the reason is for logs).
    Malformed(String),
    Present(ProjectionMarker),
}

fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

impl ProjectionMarker {
    /// The ledger-written marker statements about `graph` (valid SPARQL template syntax).
    /// `lp:tripleCount` is not among them: the write adds it from the target's own count.
    pub fn triples(&self, graph: &CognitiveGraph) -> Vec<String> {
        let [protocol, graph_id, branch, commit, version, digest, _count] = marker_predicates();
        let s = format!("<{}>", graph.as_iri());
        let text = |v: &str| format!("\"{}\"", escape(v));
        let int = |v: String| format!("\"{v}\"^^<{XSD_INTEGER}>");
        vec![
            format!("{s} <{protocol}> {} .", text(PROJECTION_PROTOCOL)),
            format!("{s} <{graph_id}> {} .", text(self.graph_id.as_str())),
            format!("{s} <{branch}> {} .", text(&self.branch)),
            format!("{s} <{commit}> {} .", text(&self.commit.to_string())),
            format!("{s} <{version}> {} .", int(self.ref_version.to_string())),
            format!("{s} <{digest}> {} .", text(&self.state_digest.to_string())),
        ]
    }

    /// Strictly parse the (predicate, object) pairs found for a cognitive graph's subject in
    /// the marker graph: every v1 predicate exactly once with the right literal type, no
    /// other predicate, values valid under the ledger's own rules.
    pub fn from_terms(pairs: &[(String, MarkerTerm)]) -> MarkerRead {
        if pairs.is_empty() {
            return MarkerRead::Absent;
        }
        match Self::parse_terms(pairs) {
            Ok(marker) => MarkerRead::Present(marker),
            Err(reason) => MarkerRead::Malformed(reason),
        }
    }

    fn parse_terms(pairs: &[(String, MarkerTerm)]) -> Result<Self, String> {
        let predicates = marker_predicates();
        let mut values: [Option<&MarkerTerm>; 7] = Default::default();
        for (predicate, term) in pairs {
            let index = predicates
                .iter()
                .position(|p| p == predicate)
                .ok_or_else(|| "unexpected predicate on the marker subject".to_owned())?;
            if values[index].replace(term).is_some() {
                return Err(format!("predicate {predicate} appears more than once"));
            }
        }
        let get = |i: usize| values[i].ok_or_else(|| format!("missing {}", predicates[i]));
        let string = |i: usize| -> Result<&str, String> {
            let term = get(i)?;
            let plain =
                term.datatype.as_deref().is_none_or(|d| d == XSD_STRING) && term.language.is_none();
            if term.is_literal && plain {
                Ok(term.value.as_str())
            } else {
                Err(format!("{} is not a string literal", predicates[i]))
            }
        };
        let integer = |i: usize| -> Result<&str, String> {
            let term = get(i)?;
            let canonical = !term.value.is_empty()
                && term.value.bytes().all(|b| b.is_ascii_digit())
                && (term.value == "0" || !term.value.starts_with('0'));
            if term.is_literal && term.datatype.as_deref() == Some(XSD_INTEGER) && canonical {
                Ok(term.value.as_str())
            } else {
                Err(format!(
                    "{} is not a canonical non-negative xsd:integer",
                    predicates[i]
                ))
            }
        };
        if string(0)? != PROJECTION_PROTOCOL {
            return Err("unknown projection protocol".into());
        }
        let graph_id = GraphId::new(string(1)?).map_err(|e| e.to_string())?;
        let branch = string(2)?.to_owned();
        validate_token("branch", &branch, MAX_BRANCH_BYTES).map_err(|e| e.to_string())?;
        let commit = CommitId::from_str(string(3)?).map_err(|e| e.to_string())?;
        let ref_version: i64 = integer(4)?.parse().map_err(|_| "refVersion out of range")?;
        if ref_version < 1 {
            return Err("refVersion must be >= 1".into());
        }
        let state_digest = ContentId::from_str(string(5)?).map_err(|e| e.to_string())?;
        let triple_count: u64 = integer(6)?
            .parse()
            .map_err(|_| "tripleCount out of range")?;
        Ok(Self {
            graph_id,
            branch,
            commit,
            ref_version,
            state_digest,
            triple_count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker() -> ProjectionMarker {
        ProjectionMarker {
            graph_id: GraphId::new("0b0a2a1c-2e3d-4f50-8a61-72b384c5d6e7").unwrap(),
            branch: "main".into(),
            commit: CommitId(ContentId::for_bytes(b"c7")),
            ref_version: 7,
            state_digest: ContentId::for_bytes(b"state"),
            triple_count: 42,
        }
    }

    /// Turn written statements back into observed pairs (what a SPARQL read reports), plus
    /// the target-computed count.
    fn observed(m: &ProjectionMarker) -> Vec<(String, MarkerTerm)> {
        let graph = CognitiveGraph::for_knowledge_base("kb").unwrap();
        let mut pairs: Vec<(String, MarkerTerm)> = m
            .triples(&graph)
            .into_iter()
            .map(|t| {
                let rest = t.split_once("> <").unwrap().1;
                let (p, o) = rest.split_once("> ").unwrap();
                let o = o.trim_end_matches(" .");
                let (value, datatype) = match o.split_once("\"^^<") {
                    Some((v, d)) => (v[1..].to_owned(), Some(d.trim_end_matches('>').to_owned())),
                    None => (o.trim_matches('"').to_owned(), None),
                };
                (
                    p.to_owned(),
                    MarkerTerm {
                        value,
                        datatype,
                        is_literal: true,
                        is_blank: false,
                        language: None,
                    },
                )
            })
            .collect();
        pairs.push((
            marker_predicates()[6].clone(),
            MarkerTerm {
                value: m.triple_count.to_string(),
                datatype: Some(XSD_INTEGER.into()),
                is_literal: true,
                is_blank: false,
                language: None,
            },
        ));
        pairs
    }

    #[test]
    fn marker_statements_are_frozen() {
        let graph = CognitiveGraph::for_knowledge_base("kb").unwrap();
        let triples = marker().triples(&graph);
        assert_eq!(
            triples[0],
            "<urn:sculpin:kb:kb:cognitive> <urn:sculpin:ledger-projection:v1#protocol> \"sculpin-ledger-projection/v1\" ."
        );
        assert_eq!(
            triples[4],
            "<urn:sculpin:kb:kb:cognitive> <urn:sculpin:ledger-projection:v1#refVersion> \"7\"^^<http://www.w3.org/2001/XMLSchema#integer> ."
        );
        assert_eq!(triples.len(), 6, "tripleCount is added by the target");
    }

    #[test]
    fn a_written_marker_reads_back_exactly() {
        assert_eq!(
            ProjectionMarker::from_terms(&observed(&marker())),
            MarkerRead::Present(marker())
        );
        assert_eq!(ProjectionMarker::from_terms(&[]), MarkerRead::Absent);
    }

    type Pairs = Vec<(String, MarkerTerm)>;
    type Change = Box<dyn Fn(&mut Pairs)>;

    #[test]
    fn malformed_markers_are_never_guessed() {
        let good = observed(&marker());
        let mutate = |f: &dyn Fn(&mut Pairs)| {
            let mut pairs = good.clone();
            f(&mut pairs);
            ProjectionMarker::from_terms(&pairs)
        };
        let cases: Vec<(&str, Change)> = vec![
            (
                "missing field",
                Box::new(|p| {
                    p.remove(3);
                }),
            ),
            (
                "duplicate field",
                Box::new(|p| {
                    let d = p[4].clone();
                    p.push(d);
                }),
            ),
            (
                "foreign predicate",
                Box::new(|p| p.push(("urn:x".into(), p[0].1.clone()))),
            ),
            (
                "other protocol",
                Box::new(|p| p[0].1.value = "sculpin-ledger-projection/v2".into()),
            ),
            ("untyped version", Box::new(|p| p[4].1.datatype = None)),
            ("zero version", Box::new(|p| p[4].1.value = "0".into())),
            ("leading zero", Box::new(|p| p[4].1.value = "07".into())),
            ("negative count", Box::new(|p| p[6].1.value = "-1".into())),
            (
                "bad commit",
                Box::new(|p| p[3].1.value = "sha256:XYZ".into()),
            ),
            (
                "iri instead of literal",
                Box::new(|p| p[2].1.is_literal = false),
            ),
            (
                "typed string",
                Box::new(|p| p[2].1.datatype = Some(XSD_INTEGER.into())),
            ),
        ];
        for (why, change) in cases {
            assert!(
                matches!(mutate(&*change), MarkerRead::Malformed(_)),
                "{why}"
            );
        }
    }
}
