//! SPARQL 1.1 request bodies of the projection protocol (ADR-0020) and parsing of the
//! SPARQL JSON results the observation query returns. Pure functions; no I/O.

use ledger_projection::{
    CognitiveGraph, LP_NAMESPACE, MARKER_GRAPH, MarkerTerm, Observation, PROBE_GRAPH,
    ProjectedState, ProjectionError, ProjectionErrorCode, ProjectionMarker, TARGET_SUBJECT,
    WriteMode,
};
use serde::Deserialize;

/// A deliberately unloadable IRI: `LOAD` of it fails without any network access.
const UNLOADABLE: &str = "urn:sculpin:ledger-projection:v1:unloadable";

/// Conditional guard: no `refVersion` of this graph's marker is `>= version` (a non-numeric
/// value counts as blocking).
fn conditional_guard(graph: &CognitiveGraph, version: i64) -> String {
    format!(
        "FILTER NOT EXISTS {{ GRAPH <{MARKER_GRAPH}> {{ <{g}> <{LP_NAMESPACE}refVersion> ?v }} \
         FILTER (COALESCE(?v >= {version}, true)) }}",
        g = graph.as_iri()
    )
}

/// Replacement guard: no `refVersion` of this graph's marker is `> ceiling` (non-numeric
/// garbage does not block a recovery).
fn replace_guard(graph: &CognitiveGraph, ceiling: i64) -> String {
    format!(
        "FILTER NOT EXISTS {{ GRAPH <{MARKER_GRAPH}> {{ <{g}> <{LP_NAMESPACE}refVersion> ?v }} \
         FILTER (COALESCE(?v > {ceiling}, false)) }}",
        g = graph.as_iri()
    )
}

/// One SPARQL Update request writing `state` and `marker` into `graph` (one transaction on a
/// transactional dataset). Every operation carries the same guard, so either all apply or
/// none does; the last operation adds `lp:tripleCount` from the target's own count of the
/// graph it just wrote.
pub fn write_update(
    graph: &CognitiveGraph,
    state: &ProjectedState,
    marker: &ProjectionMarker,
    mode: WriteMode,
) -> String {
    let g = graph.as_iri();
    let guard = match mode {
        WriteMode::Conditional => conditional_guard(graph, marker.ref_version),
        WriteMode::Replace { ceiling } => replace_guard(graph, ceiling),
    };
    let body = state.triples().join("\n");
    let marker_triples = marker.triples(graph).join("\n");
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
    // Only our own, count-less marker (just inserted) receives the count.
    ops.push(format!(
        "INSERT {{ GRAPH <{MARKER_GRAPH}> {{ <{g}> <{LP_NAMESPACE}tripleCount> ?n }} }} WHERE {{ \
         GRAPH <{MARKER_GRAPH}> {{ <{g}> <{LP_NAMESPACE}refVersion> {version} ; \
         <{LP_NAMESPACE}commitId> \"{commit}\" }} \
         FILTER NOT EXISTS {{ GRAPH <{MARKER_GRAPH}> {{ <{g}> <{LP_NAMESPACE}tripleCount> ?x }} }} \
         {{ SELECT (COUNT(*) AS ?n) WHERE {{ GRAPH <{g}> {{ ?s ?p ?o }} }} }} }}",
        version = marker.ref_version,
        commit = marker.commit
    ));
    ops.join(" ;\n")
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

    #[test]
    fn conditional_writes_guard_every_operation_on_the_marker_version() {
        let (graph, state, marker) = fixture();
        let update = write_update(&graph, &state, &marker, WriteMode::Conditional);
        let ops: Vec<&str> = update.split(" ;\n").collect();
        assert_eq!(ops.len(), 5);
        for op in &ops[..4] {
            assert!(op.contains("FILTER (COALESCE(?v >= 3, true))"), "{op}");
        }
        assert!(ops[1].contains("<urn:a> <urn:p> \"1\" ."));
        assert!(
            ops[4].contains("COUNT(*)") && ops[4].contains("refVersion> 3"),
            "{}",
            ops[4]
        );
        assert!(!update.contains("DROP") && !update.contains("INSERT DATA"));
        assert!(
            !update.contains("tripleCount> \""),
            "the ledger never writes the count"
        );
        // an empty state writes no INSERT for the graph, still guards the rest
        let empty = ProjectedState::from_state(&BTreeSet::new()).unwrap();
        let update = write_update(&graph, &empty, &marker, WriteMode::Conditional);
        assert_eq!(update.split(" ;\n").count(), 4);
    }

    #[test]
    fn replacements_are_guarded_by_their_ceiling_and_touch_only_the_two_graphs() {
        let (graph, state, marker) = fixture();
        let update = write_update(&graph, &state, &marker, WriteMode::Replace { ceiling: 9 });
        let ops: Vec<&str> = update.split(" ;\n").collect();
        assert_eq!(ops.len(), 5);
        for op in &ops[..4] {
            assert!(op.contains("FILTER (COALESCE(?v > 9, false))"), "{op}");
        }
        for iri in [
            "<urn:sculpin:kb:kb:cognitive>",
            &format!("<{MARKER_GRAPH}>"),
        ] {
            assert!(update.contains(iri));
        }
        assert!(!update.contains("DEFAULT") && !update.contains("DROP"));
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
