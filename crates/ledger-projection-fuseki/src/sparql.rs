//! SPARQL 1.1 request bodies of the projection protocol (ADR-0020) and parsing of the
//! SPARQL JSON results the observation query returns. Pure functions; no I/O.

use ledger_projection::{
    CognitiveGraph, LP_NAMESPACE, MARKER_GRAPH, MarkerTerm, Observation, PROBE_GRAPH,
    ProjectedState, ProjectionError, ProjectionErrorCode, ProjectionMarker, TARGET_SUBJECT,
    WRITE_SUBJECT, WriteMode, XSD_STRING,
};
use serde::Deserialize;

/// A deliberately unloadable IRI: `LOAD` of it fails without any network access.
const UNLOADABLE: &str = "urn:sculpin:ledger-projection:v1:unloadable";

/// Marker terms a write precondition may name (a v1 marker has seven; more means garbage an
/// operator must clear by hand).
pub const MAX_EXPECTED_TERMS: usize = 64;

fn unrepresentable(why: &str) -> ProjectionError {
    ProjectionError::permanent(
        ProjectionErrorCode::TargetProtocol,
        format!(
            "the target's marker cannot be named exactly in a write precondition ({why}); \
             clear the marker subject by hand, then rebuild (ADR-0020)"
        ),
    )
}

/// An IRI as a SPARQL constant, or `None` if it holds a character IRIREF forbids.
fn iri_constant(iri: &str) -> Option<String> {
    (!iri.is_empty()
        && iri
            .chars()
            .all(|c| c > ' ' && !matches!(c, '<' | '>' | '"' | '{' | '}' | '|' | '^' | '`' | '\\')))
    .then(|| format!("<{iri}>"))
}

fn language_tag_ok(tag: &str) -> bool {
    let mut parts = tag.split('-');
    parts
        .next()
        .is_some_and(|p| (1..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphabetic()))
        && parts.all(|p| (1..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// A string literal body: only the four characters SPARQL forbids raw are escaped, with
/// `ECHAR`s (never `\u` escapes, which the target decodes before parsing).
fn escape_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
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

/// A marker object exactly as observed, as a SPARQL constant (`None`: not expressible).
fn term_constant(term: &MarkerTerm) -> Option<String> {
    if term.is_blank {
        return None;
    }
    if !term.is_literal {
        return iri_constant(&term.value);
    }
    let lexical = format!("\"{}\"", escape_literal(&term.value));
    match (&term.language, &term.datatype) {
        (Some(tag), _) => language_tag_ok(tag).then(|| format!("{lexical}@{tag}")),
        (None, None) => Some(lexical),
        (None, Some(datatype)) if datatype == XSD_STRING => Some(lexical),
        (None, Some(datatype)) => iri_constant(datatype).map(|d| format!("{lexical}^^{d}")),
    }
}

/// Compare-and-swap precondition: the marker subject holds exactly `expected` (every pair
/// present, no other pair), compared by the target's own term identity.
fn precondition(
    graph: &CognitiveGraph,
    expected: &[(String, MarkerTerm)],
) -> Result<String, ProjectionError> {
    let g = graph.as_iri();
    if expected.is_empty() {
        return Ok(format!(
            "FILTER NOT EXISTS {{ GRAPH <{MARKER_GRAPH}> {{ <{g}> ?cp ?co }} }}"
        ));
    }
    if expected.len() > MAX_EXPECTED_TERMS {
        return Err(unrepresentable("too many values"));
    }
    let mut present = String::new();
    let mut allowed = Vec::with_capacity(expected.len());
    for (predicate, object) in expected {
        let p = iri_constant(predicate).ok_or_else(|| unrepresentable("predicate IRI"))?;
        let o = term_constant(object).ok_or_else(|| unrepresentable("object term"))?;
        present.push_str(&format!("<{g}> {p} {o} . "));
        allowed.push(format!("(?cp = {p} && sameTerm(?co, {o}))"));
    }
    Ok(format!(
        "GRAPH <{MARKER_GRAPH}> {{ {present}}} FILTER NOT EXISTS {{ GRAPH <{MARKER_GRAPH}> \
         {{ <{g}> ?cp ?co }} FILTER (!({})) }}",
        allowed.join(" || ")
    ))
}

/// One SPARQL Update request writing `state` and `marker` into `graph` (one transaction on a
/// transactional dataset), applied only if the marker is still exactly `expected` — and, for
/// a conditional write, names no ref version `>=` the one written. The first operations
/// clear any stray token and insert the transaction-local write token under that
/// precondition; every data operation is gated on the token; the count operation adds
/// `lp:tripleCount` from the target's own count of the graph it just wrote; the last
/// operation deletes the token. Either every data operation applies or none does.
pub fn write_update(
    graph: &CognitiveGraph,
    state: &ProjectedState,
    marker: &ProjectionMarker,
    mode: WriteMode,
    expected: &[(String, MarkerTerm)],
) -> Result<String, ProjectionError> {
    let g = graph.as_iri();
    let mut guard = precondition(graph, expected)?;
    if mode == WriteMode::Conditional {
        guard.push_str(&format!(
            " FILTER NOT EXISTS {{ GRAPH <{MARKER_GRAPH}> {{ <{g}> <{LP_NAMESPACE}refVersion> ?v }} \
             FILTER (COALESCE(?v >= {}, true)) }}",
            marker.ref_version
        ));
    }
    let token =
        format!("GRAPH <{MARKER_GRAPH}> {{ <{WRITE_SUBJECT}> <{LP_NAMESPACE}writeFor> <{g}> }}");
    let clear = format!(
        "DELETE WHERE {{ GRAPH <{MARKER_GRAPH}> {{ <{WRITE_SUBJECT}> <{LP_NAMESPACE}writeFor> ?any }} }}"
    );
    let body = state.triples().join("\n");
    let marker_triples = marker.triples(graph).join("\n");
    let mut ops = vec![
        clear.clone(),
        format!("INSERT {{ {token} }} WHERE {{ {guard} }}"),
        format!(
            "DELETE {{ GRAPH <{g}> {{ ?s ?p ?o }} }} WHERE {{ {token} GRAPH <{g}> {{ ?s ?p ?o }} }}"
        ),
    ];
    if !state.triples().is_empty() {
        ops.push(format!(
            "INSERT {{ GRAPH <{g}> {{\n{body}\n}} }} WHERE {{ {token} }}"
        ));
    }
    ops.push(format!(
        "DELETE {{ GRAPH <{MARKER_GRAPH}> {{ <{g}> ?mp ?mo }} }} WHERE {{ {token} \
         GRAPH <{MARKER_GRAPH}> {{ <{g}> ?mp ?mo }} }}"
    ));
    ops.push(format!(
        "INSERT {{ GRAPH <{MARKER_GRAPH}> {{\n{marker_triples}\n}} }} WHERE {{ {token} }}"
    ));
    ops.push(format!(
        "INSERT {{ GRAPH <{MARKER_GRAPH}> {{ <{g}> <{LP_NAMESPACE}tripleCount> ?n }} }} WHERE {{ \
         {token} {{ SELECT (COUNT(*) AS ?n) WHERE {{ GRAPH <{g}> {{ ?s ?p ?o }} }} }} }}"
    ));
    ops.push(clear);
    Ok(ops.join(" ;\n"))
}

/// Whether the dataset's default graph shows the target binding: true only for a union
/// default graph (or a binding written into the default graph), which the projector refuses.
pub fn union_default_graph_ask() -> String {
    format!("ASK {{ <{TARGET_SUBJECT}> <{LP_NAMESPACE}targetId> ?t }}")
}

/// Whether every triple of `state` is in `graph`, by the target's own term equality.
pub fn contains_all_query(graph: &CognitiveGraph, state: &ProjectedState) -> String {
    format!(
        "ASK {{ GRAPH <{}> {{\n{}\n}} }}",
        graph.as_iri(),
        state.triples().join("\n")
    )
}

/// Bind the dataset to a target id if it is not bound yet.
pub fn bind_target_update(target_id: &str) -> String {
    format!(
        "INSERT {{ GRAPH <{MARKER_GRAPH}> {{ <{TARGET_SUBJECT}> <{LP_NAMESPACE}targetId> \"{target_id}\" }} }} \
         WHERE {{ FILTER NOT EXISTS {{ GRAPH <{MARKER_GRAPH}> {{ <{TARGET_SUBJECT}> <{LP_NAMESPACE}targetId> ?any }} }} }}"
    )
}

pub fn bound_target_query() -> String {
    format!(
        "SELECT ?p ?o ?n WHERE {{ {{ GRAPH <{MARKER_GRAPH}> {{ <{TARGET_SUBJECT}> ?p ?o }} }} UNION \
         {{ SELECT (COUNT(*) AS ?n) WHERE {{ GRAPH <{MARKER_GRAPH}> {{ <{TARGET_SUBJECT}> ?pp ?oo }} }} }} }}"
    )
}

/// One query returning the marker's predicate/object pairs and the graph's triple count, so
/// both come from one read transaction.
pub fn observe_query(graph: &CognitiveGraph) -> String {
    let g = graph.as_iri();
    format!(
        "SELECT ?p ?o ?n WHERE {{ {{ GRAPH <{MARKER_GRAPH}> {{ <{g}> ?p ?o }} }} UNION \
         {{ SELECT (COUNT(*) AS ?n) WHERE {{ GRAPH <{g}> {{ ?s ?pp ?oo }} }} }} }}"
    )
}

pub fn read_graph_query(graph: &CognitiveGraph) -> String {
    format!(
        "CONSTRUCT {{ ?s ?p ?o }} WHERE {{ GRAPH <{}> {{ ?s ?p ?o }} }}",
        graph.as_iri()
    )
}

/// The probe: an insert into the probe graph followed by a failing `LOAD`. A transactional
/// target rejects the whole request and keeps nothing.
pub fn probe_update() -> String {
    format!(
        "INSERT DATA {{ GRAPH <{PROBE_GRAPH}> {{ <{PROBE_GRAPH}> <{LP_NAMESPACE}probe> \"probe\" }} }} ;\n\
         LOAD <{UNLOADABLE}>"
    )
}

pub fn probe_ask() -> String {
    format!("ASK {{ GRAPH <{PROBE_GRAPH}> {{ ?s ?p ?o }} }}")
}

pub fn probe_cleanup() -> String {
    format!("DROP SILENT GRAPH <{PROBE_GRAPH}>")
}

// ---- SPARQL 1.1 Query Results JSON ----------------------------------------------------

#[derive(Deserialize)]
struct Results {
    results: Option<Bindings>,
    boolean: Option<bool>,
}

#[derive(Deserialize)]
struct Bindings {
    bindings: Vec<std::collections::HashMap<String, Term>>,
}

#[derive(Deserialize)]
struct Term {
    #[serde(rename = "type")]
    kind: String,
    value: String,
    datatype: Option<String>,
    #[serde(rename = "xml:lang")]
    lang: Option<String>,
}

fn protocol(message: &str) -> ProjectionError {
    ProjectionError::permanent(ProjectionErrorCode::TargetProtocol, message)
}

/// Parse the observation query's results into the marker and the triple count.
pub fn parse_observation(body: &[u8]) -> Result<Observation, ProjectionError> {
    let results: Results = serde_json::from_slice(body)
        .map_err(|_| protocol("the target's query result is not SPARQL JSON results"))?;
    let bindings = results
        .results
        .ok_or_else(|| protocol("the target's query result has no bindings"))?
        .bindings;
    let mut pairs = Vec::new();
    let mut count: Option<u64> = None;
    for row in bindings {
        if let Some(n) = row.get("n") {
            let valid = n.kind == "literal" || n.kind == "typed-literal";
            let parsed = n.value.parse::<u64>().ok().filter(|_| valid);
            if count
                .replace(parsed.ok_or_else(|| protocol("invalid triple count"))?)
                .is_some()
            {
                return Err(protocol("the triple count appears more than once"));
            }
            continue;
        }
        match (row.get("p"), row.get("o")) {
            (Some(p), Some(o)) if p.kind == "uri" => pairs.push((
                p.value.clone(),
                MarkerTerm {
                    value: o.value.clone(),
                    datatype: o.datatype.clone(),
                    is_literal: o.kind == "literal" || o.kind == "typed-literal",
                    is_blank: o.kind == "bnode",
                    language: o.lang.clone(),
                },
            )),
            _ => return Err(protocol("unexpected binding in the observation result")),
        }
    }
    let version_predicate = format!("{LP_NAMESPACE}refVersion");
    let max_ref_version = pairs
        .iter()
        .filter(|(p, _)| *p == version_predicate)
        .filter_map(|(_, o)| o.value.parse::<i64>().ok())
        .max();
    Ok(Observation {
        marker: ProjectionMarker::from_terms(&pairs),
        triple_count: count.ok_or_else(|| protocol("the triple count is missing"))?,
        max_ref_version,
        terms: pairs,
    })
}

/// The target ids a dataset is bound to (the bind query's pairs).
pub fn parse_bound_target(body: &[u8]) -> Result<Vec<String>, ProjectionError> {
    let results: Results = serde_json::from_slice(body)
        .map_err(|_| protocol("the target's query result is not SPARQL JSON results"))?;
    let id_predicate = format!("{LP_NAMESPACE}targetId");
    Ok(results
        .results
        .ok_or_else(|| protocol("the target's query result has no bindings"))?
        .bindings
        .into_iter()
        .filter(|row| row.get("p").is_some_and(|p| p.value == id_predicate))
        .filter_map(|row| row.get("o").map(|o| o.value.clone()))
        .collect())
}

pub fn parse_ask(body: &[u8]) -> Result<bool, ProjectionError> {
    let results: Results = serde_json::from_slice(body)
        .map_err(|_| protocol("the target's ASK result is not SPARQL JSON results"))?;
    results
        .boolean
        .ok_or_else(|| protocol("the target's ASK result has no boolean"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ledger_core::{CommitId, ContentId, GraphId};
    use ledger_projection::MarkerRead;
    use std::collections::BTreeSet;

    fn fixture() -> (CognitiveGraph, ProjectedState, ProjectionMarker) {
        let graph = CognitiveGraph::for_knowledge_base("kb").unwrap();
        let state: BTreeSet<ledger_rdf::Quad> = ["<urn:a> <urn:p> \"1\" ."]
            .iter()
            .map(|q| q.parse().unwrap())
            .collect();
        let projected = ProjectedState::from_state(&state).unwrap();
        let marker = ProjectionMarker {
            graph_id: GraphId::new("g").unwrap(),
            branch: "main".into(),
            commit: CommitId(ContentId::for_bytes(b"c")),
            ref_version: 3,
            state_digest: projected.digest().clone(),
            triple_count: 1,
        };
        (graph, projected, marker)
    }

    fn term(value: &str, datatype: Option<&str>) -> MarkerTerm {
        MarkerTerm {
            value: value.into(),
            datatype: datatype.map(str::to_owned),
            is_literal: true,
            is_blank: false,
            language: None,
        }
    }

    #[test]
    fn writes_are_gated_on_a_token_inserted_only_under_the_exact_observed_marker() {
        let (graph, state, marker) = fixture();
        let observed = vec![
            (
                format!("{LP_NAMESPACE}refVersion"),
                term("2", Some("http://www.w3.org/2001/XMLSchema#integer")),
            ),
            (format!("{LP_NAMESPACE}graphId"), term("g\"x", None)),
        ];
        let update =
            write_update(&graph, &state, &marker, WriteMode::Conditional, &observed).unwrap();
        let ops: Vec<&str> = update.split(" ;\n").collect();
        assert_eq!(ops.len(), 8);
        let token =
            format!("<{WRITE_SUBJECT}> <{LP_NAMESPACE}writeFor> <urn:sculpin:kb:kb:cognitive>");
        assert!(
            ops[0].starts_with("DELETE WHERE") && ops[7] == ops[0],
            "stray tokens cleared first and last"
        );
        // the precondition: every observed pair present, nothing else, and the version rule
        assert!(
            ops[1].starts_with(&format!(
                "INSERT {{ GRAPH <{MARKER_GRAPH}> {{ {token} }} }} WHERE"
            )),
            "{}",
            ops[1]
        );
        assert!(ops[1].contains("<urn:sculpin:ledger-projection:v1#refVersion> \"2\"^^<http://www.w3.org/2001/XMLSchema#integer> ."));
        assert!(ops[1].contains("sameTerm(?co, \"g\\\"x\")"), "{}", ops[1]);
        assert!(ops[1].contains("FILTER (COALESCE(?v >= 3, true))"));
        for op in &ops[2..7] {
            assert!(
                op.contains(&token),
                "every data operation is gated on the token: {op}"
            );
            assert!(!op.contains("sameTerm"), "{op}");
        }
        assert!(ops[3].contains("<urn:a> <urn:p> \"1\" ."));
        assert!(ops[6].contains("COUNT(*)"));
        assert!(
            !update.contains("tripleCount> \""),
            "the ledger never writes the count"
        );
        assert!(
            !update.contains("DROP") && !update.contains("INSERT DATA") && !update.contains("\\u")
        );
        // replacements carry the precondition but no version rule; an empty observation
        // requires an absent marker; an empty state writes no graph INSERT
        let empty = ProjectedState::from_state(&BTreeSet::new()).unwrap();
        let update = write_update(&graph, &empty, &marker, WriteMode::Replace, &[]).unwrap();
        let ops: Vec<&str> = update.split(" ;\n").collect();
        assert_eq!(ops.len(), 7);
        assert!(ops[1].contains("FILTER NOT EXISTS { GRAPH <urn:sculpin:ledger-projection:v1:markers> { <urn:sculpin:kb:kb:cognitive> ?cp ?co } }"));
        assert!(!update.contains("COALESCE"));
    }

    #[test]
    fn markers_that_cannot_be_named_exactly_are_refused_not_overwritten() {
        let (graph, state, marker) = fixture();
        let p = format!("{LP_NAMESPACE}graphId");
        let blank = MarkerTerm {
            is_literal: false,
            is_blank: true,
            ..term("b0", None)
        };
        let bad_iri = MarkerTerm {
            is_literal: false,
            ..term("urn:x> } ; DROP ALL ; #", None)
        };
        let bad_lang = MarkerTerm {
            language: Some("en\"@".into()),
            ..term("x", None)
        };
        let bad_datatype = term("1", Some("urn:t>"));
        for object in [blank, bad_iri, bad_lang, bad_datatype] {
            let e = write_update(
                &graph,
                &state,
                &marker,
                WriteMode::Replace,
                &[(p.clone(), object)],
            )
            .unwrap_err();
            assert_eq!(e.code(), ProjectionErrorCode::TargetProtocol);
        }
        let e = write_update(
            &graph,
            &state,
            &marker,
            WriteMode::Replace,
            &[("urn:p\\".into(), term("x", None))],
        )
        .unwrap_err();
        assert_eq!(e.code(), ProjectionErrorCode::TargetProtocol);
        let many: Vec<_> = (0..=MAX_EXPECTED_TERMS)
            .map(|i| (p.clone(), term(&i.to_string(), None)))
            .collect();
        assert!(write_update(&graph, &state, &marker, WriteMode::Replace, &many).is_err());
        // language-tagged and xsd:string literals are named exactly
        let tagged = MarkerTerm {
            language: Some("en-GB".into()),
            ..term("x", None)
        };
        let update = write_update(
            &graph,
            &state,
            &marker,
            WriteMode::Replace,
            &[(p.clone(), tagged), (p, term("y", Some(XSD_STRING)))],
        )
        .unwrap();
        assert!(update.contains("\"x\"@en-GB") && update.contains("sameTerm(?co, \"y\")"));
    }

    #[test]
    fn containment_and_binding_queries_are_well_formed() {
        let (graph, state, _) = fixture();
        let ask = contains_all_query(&graph, &state);
        assert!(ask.starts_with("ASK { GRAPH <urn:sculpin:kb:kb:cognitive>"));
        assert!(ask.contains("<urn:a> <urn:p> \"1\" ."));
        let bind = bind_target_update("fuseki-main");
        assert!(bind.contains(TARGET_SUBJECT) && bind.contains("\"fuseki-main\""));
        assert!(bind.contains("FILTER NOT EXISTS"));
        let union = union_default_graph_ask();
        assert!(
            union.starts_with("ASK {")
                && union.contains(TARGET_SUBJECT)
                && !union.contains("GRAPH")
        );
    }

    #[test]
    fn observation_results_parse_strictly() {
        let body = serde_json::json!({
            "head": {"vars": ["p", "o", "n"]},
            "results": {"bindings": [
                {"n": {"type": "literal", "value": "0",
                       "datatype": "http://www.w3.org/2001/XMLSchema#integer"}}
            ]}
        });
        let obs = parse_observation(body.to_string().as_bytes()).unwrap();
        assert_eq!(obs.triple_count, 0);
        assert_eq!(obs.marker, MarkerRead::Absent);
        for bad in [
            serde_json::json!({"results": {"bindings": []}}),
            serde_json::json!({"results": {"bindings": [{"x": {"type": "uri", "value": "u"}}]}}),
            serde_json::json!({"results": {"bindings": [
                {"n": {"type": "literal", "value": "1"}},
                {"n": {"type": "literal", "value": "2"}}]}}),
            serde_json::json!({"boolean": true}),
        ] {
            assert!(
                parse_observation(bad.to_string().as_bytes()).is_err(),
                "{bad}"
            );
        }
        assert!(parse_observation(b"<html>").is_err());
    }
}
