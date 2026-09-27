#![no_main]
//! Structured fuzzing of the prepare normalization + request-identity encoder: arbitrary
//! field values (including control characters, huge strings, duplicate/unsorted evidence
//! and operations) must never panic, and semantically equal inputs (operation order,
//! duplicated evidence) must produce the same digest.
use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Input {
    ref_name: String,
    expected_head: Option<[u8; 32]>,
    operations: Vec<(bool, String)>,
    activity: String,
    event_time: Option<String>,
    evidence_refs: Vec<String>,
    source_system: Option<String>,
    message: String,
}

fn body(i: &Input, reversed: bool) -> ledger_api::PrepareBody {
    let mut operations: Vec<ledger_api::OperationBody> = i
        .operations
        .iter()
        .map(|(add, quad)| ledger_api::OperationBody {
            op: if *add { "add".into() } else { "delete".into() },
            quad: quad.clone(),
        })
        .collect();
    let mut evidence = i.evidence_refs.clone();
    if reversed {
        operations.reverse();
        evidence.reverse();
        evidence.extend(i.evidence_refs.iter().take(1).cloned());
    }
    ledger_api::PrepareBody {
        ref_name: i.ref_name.clone(),
        expected_head: i.expected_head.map(|h| {
            let hex: String = h.iter().map(|b| format!("{b:02x}")).collect();
            format!("sha256:{hex}")
                .parse()
                .expect("well-formed commit id")
        }),
        operations,
        activity: i.activity.clone(),
        event_time: i.event_time.clone(),
        evidence_refs: evidence,
        source_system: i.source_system.clone(),
        message: i.message.clone(),
    }
}

fuzz_target!(|input: Input| {
    let graph = ledger_core::GraphId::new("fuzz-graph").unwrap();
    let limits = ledger_api::ApiLimits::default();
    let a = ledger_api::canonical_prepare(&graph, body(&input, false), &limits, "fuzz");
    let b = ledger_api::canonical_prepare(&graph, body(&input, true), &limits, "fuzz");
    match (a, b) {
        (Ok(a), Ok(b)) => {
            assert_eq!(
                a.canonical.digest(),
                b.canonical.digest(),
                "operation order and duplicated evidence must not change request identity"
            );
            assert_eq!(a.requested.id(), b.requested.id());
        }
        (Err(_), Err(_)) => {}
        (Ok(_), Err(_)) | (Err(_), Ok(_)) => {
            // The reversed body duplicates the first evidence ref, which can only change the
            // outcome by pushing the metadata byte budget (activity + message + evidence +
            // source system, `ApiLimits::max_metadata_bytes`) over its limit. Any other
            // asymmetry is a bug.
            let limits = ledger_api::ApiLimits::default();
            let meta: usize = input.activity.len()
                + input.message.len()
                + input.evidence_refs.iter().map(String::len).sum::<usize>()
                + input.source_system.as_deref().map_or(0, str::len);
            let extra = input.evidence_refs.first().map_or(0, String::len);
            assert!(
                meta <= limits.max_metadata_bytes && meta + extra > limits.max_metadata_bytes,
                "acceptance differed without crossing the metadata budget (meta {meta}, extra {extra})"
            );
        }
    }
});
