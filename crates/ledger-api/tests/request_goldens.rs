//! Golden vectors for the canonical request identity `sculpin-ledger-request/v1`
//! (ADR-0015; fixtures/golden/requests). Every `.input` is turned into the exact request
//! body the API accepts and pushed through the handlers' own `canonical_*` builders, so the
//! vectors pin the real request path, not a test re-implementation. Both the canonical
//! bytes (`.hex`) and the digest (`.sha256`) must match the frozen vectors, which are
//! independently produced by `scripts/golden/request_v1_reference.py`. Changing any vector
//! is a protocol change and needs an ADR plus golden review.

use ledger_api::{
    AcceptBody, ApiLimits, PrepareBody, RejectBody, canonical_accept, canonical_prepare,
    canonical_reject, request_identity::CanonicalRequest,
};
use ledger_core::{CommitId, GraphId};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, str::FromStr};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/requests")
}

/// Map a fixture (which names the operation, graph and candidate the way the URL does) to
/// the JSON body a client would send, then run the handler's builder.
fn through_the_api(input: &Value) -> CanonicalRequest {
    let graph = GraphId::new(input["graph_id"].as_str().unwrap()).unwrap();
    let mut body = input.clone();
    let obj = body.as_object_mut().unwrap();
    let operation = obj.remove("operation").unwrap();
    obj.remove("graph_id");
    let branch = obj.remove("branch").unwrap();
    obj.insert("ref".into(), branch);
    let candidate = obj
        .remove("candidate")
        .map(|c| CommitId::from_str(c.as_str().unwrap()).unwrap());
    match operation.as_str().unwrap() {
        "prepare" => {
            let body: PrepareBody = serde_json::from_value(body).expect("valid prepare body");
            canonical_prepare(&graph, body, &ApiLimits::default(), "golden")
                .expect("fixture is a valid request")
                .canonical
        }
        "accept" => {
            // The policy field is server-side; the body has no such field (ADR-0015).
            obj.remove("validation_policy");
            let body: AcceptBody = serde_json::from_value(body).expect("valid accept body");
            canonical_accept(&graph, &candidate.unwrap(), &body, "golden").unwrap()
        }
        "reject" => {
            let body: RejectBody = serde_json::from_value(body).expect("valid reject body");
            canonical_reject(&graph, &candidate.unwrap(), &body, "golden").unwrap()
        }
        other => panic!("unknown operation {other}"),
    }
}

#[test]
fn every_request_identity_vector_matches_bytes_and_digest_through_the_handlers() {
    let mut count = 0;
    for entry in fs::read_dir(fixtures()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("input") {
            continue;
        }
        count += 1;
        let name = path.file_stem().unwrap().to_str().unwrap().to_owned();
        let input: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let request = through_the_api(&input);
        let expected_hex = fs::read_to_string(path.with_extension("hex")).unwrap();
        let expected_digest = fs::read_to_string(path.with_extension("sha256")).unwrap();
        assert_eq!(
            hex::encode(request.canonical_bytes()),
            expected_hex.trim(),
            "{name}: canonical bytes drifted from the golden vector"
        );
        assert_eq!(
            request.digest().to_string(),
            expected_digest.trim(),
            "{name}: digest drifted from the golden vector"
        );
    }
    assert!(
        count >= 6,
        "expected at least 6 request vectors, found {count}"
    );
}

#[test]
fn json_order_and_evidence_order_do_not_change_identity() {
    let a: Value = serde_json::from_str(
        &fs::read_to_string(fixtures().join("request-prepare-advance.input")).unwrap(),
    )
    .unwrap();
    let b: Value = serde_json::from_str(
        &fs::read_to_string(fixtures().join("request-prepare-advance-reordered.input")).unwrap(),
    )
    .unwrap();
    assert_ne!(
        a.to_string(),
        b.to_string(),
        "fixtures must differ textually"
    );
    assert_eq!(through_the_api(&a).digest(), through_the_api(&b).digest());
}

#[test]
fn empty_optional_fields_are_absent_and_absent_differs_from_present() {
    let genesis: Value = serde_json::from_str(
        &fs::read_to_string(fixtures().join("request-prepare-genesis.input")).unwrap(),
    )
    .unwrap();
    let mut with_empty = genesis.clone();
    with_empty["source_system"] = json!("");
    assert_eq!(
        through_the_api(&genesis).digest(),
        through_the_api(&with_empty).digest(),
        "an empty source_system is the same request as none"
    );
    let mut with_source = genesis.clone();
    with_source["source_system"] = json!("sculpin-agent-api");
    assert_ne!(
        through_the_api(&genesis).digest(),
        through_the_api(&with_source).digest()
    );
    // An empty reason on accept is refused, not silently normalized, by the handler.
    let accept: Value =
        serde_json::from_str(&fs::read_to_string(fixtures().join("request-accept.input")).unwrap())
            .unwrap();
    let mut body =
        json!({"ref": accept["branch"], "expected_head": accept["expected_head"], "reason": ""});
    let graph = GraphId::new(accept["graph_id"].as_str().unwrap()).unwrap();
    let candidate = CommitId::from_str(accept["candidate"].as_str().unwrap()).unwrap();
    let parsed: AcceptBody = serde_json::from_value(body.take()).unwrap();
    assert!(canonical_accept(&graph, &candidate, &parsed, "golden").is_err());
}
