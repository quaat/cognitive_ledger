//! SPARQL 1.1 request bodies of the projection protocol (ADR-0020) and parsing of the
//! SPARQL JSON results the observation query returns. Pure functions; no I/O.

use ledger_projection::{
    CognitiveGraph, LP_NAMESPACE, MARKER_GRAPH, MarkerTerm, Observation, PROBE_GRAPH,
    ProjectedState, ProjectionError, ProjectionErrorCode, ProjectionMarker, WriteMode,
};
use serde::Deserialize;

/// A deliberately unloadable IRI: `LOAD` of it fails without any network access.
const UNLOADABLE: &str = "urn:sculpin:ledger-projection:v1:unloadable";

fn guard(graph: &CognitiveGraph, version: i64) -> String {
    format!(
        "OPTIONAL {{ GRAPH <{MARKER_GRAPH}> {{ <{g}> <{LP_NAMESPACE}refVersion> ?v }} }} \
         FILTER (!BOUND(?v) || ?v < {version})",
        g = graph.as_iri()
    )
}

/// One SPARQL Update request writing `state` and `marker` into `graph` (one transaction on a
/// transactional dataset). `Conditional` applies only while the target's marker is absent or
/// older than the marker being written.
pub fn write_update(
    graph: &CognitiveGraph,
    state: &ProjectedState,
    marker: &ProjectionMarker,
    mode: WriteMode,
) -> String {
    let g = graph.as_iri();
    let body = state.triples().join("\n");
    let marker_triples = marker.triples(graph).join("\n");
    match mode {
        WriteMode::Conditional => {
            let guard = guard(graph, marker.ref_version);
            let mut ops = vec![format!(
                "DELETE {{ GRAPH <{g}> {{ ?s ?p ?o }} }} WHERE {{ {guard} GRAPH <{g}> {{ ?s ?p ?o }} }}"
            )];
            if !state.triples().is_empty() {
                ops.push(format!(
                    "INSERT {{ GRAPH <{g}> {{\n{body}\n}} }} WHERE {{ {guard} }}"
                ));
            }
            ops.push(format!(
                "DELETE {{ GRAPH <{MARKER_GRAPH}> {{ <{g}> ?mp ?mo }} }} WHERE {{ {guard} \
                 GRAPH <{MARKER_GRAPH}> {{ <{g}> ?mp ?mo }} }}"
            ));
            ops.push(format!(
                "INSERT {{ GRAPH <{MARKER_GRAPH}> {{\n{marker_triples}\n}} }} WHERE {{ {guard} }}"
            ));
            ops.join(" ;\n")
        }
        WriteMode::Replace => {
            let mut ops = vec![format!("DROP SILENT GRAPH <{g}>")];
            if !state.triples().is_empty() {
                ops.push(format!("INSERT DATA {{ GRAPH <{g}> {{\n{body}\n}} }}"));
            }
            ops.push(format!(
                "DELETE WHERE {{ GRAPH <{MARKER_GRAPH}> {{ <{g}> ?mp ?mo }} }}"
            ));
            ops.push(format!(
                "INSERT DATA {{ GRAPH <{MARKER_GRAPH}> {{\n{marker_triples}\n}} }}"
            ));
            ops.join(" ;\n")
        }
    }
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
                },
            )),
            _ => return Err(protocol("unexpected binding in the observation result")),
        }
    }
    Ok(Observation {
        marker: ProjectionMarker::from_terms(&pairs),
        triple_count: count.ok_or_else(|| protocol("the triple count is missing"))?,
    })
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

    #[test]
    fn conditional_writes_guard_every_operation_on_the_marker_version() {
        let (graph, state, marker) = fixture();
        let update = write_update(&graph, &state, &marker, WriteMode::Conditional);
        let ops: Vec<&str> = update.split(" ;\n").collect();
        assert_eq!(ops.len(), 4);
        for op in &ops {
            assert!(op.contains("FILTER (!BOUND(?v) || ?v < 3)"), "{op}");
        }
        assert!(ops[1].contains("<urn:a> <urn:p> \"1\" ."));
        assert!(!update.contains("DROP"));
        // an empty state writes no INSERT for the graph, still guards the rest
        let empty = ProjectedState::from_state(&BTreeSet::new()).unwrap();
        let update = write_update(&graph, &empty, &marker, WriteMode::Conditional);
        assert_eq!(update.split(" ;\n").count(), 3);
    }

    #[test]
    fn replacement_is_unconditional_and_touches_only_the_two_graphs() {
        let (graph, state, marker) = fixture();
        let update = write_update(&graph, &state, &marker, WriteMode::Replace);
        assert!(update.starts_with("DROP SILENT GRAPH <urn:sculpin:kb:kb:cognitive>"));
        assert!(!update.contains("FILTER"));
        for iri in [
            "<urn:sculpin:kb:kb:cognitive>",
            &format!("<{MARKER_GRAPH}>"),
        ] {
            assert!(update.contains(iri));
        }
        assert!(!update.contains("DEFAULT") && !update.contains("DROP ALL"));
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
