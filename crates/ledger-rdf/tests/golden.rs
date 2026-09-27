use ledger_rdf::Patch;
#[test]
fn patch_v1_vector_is_stable() {
    let bytes = include_bytes!("../../../fixtures/golden/patches/basic-v1.patch");
    let expected = include_str!("../../../fixtures/golden/patches/basic-v1.sha256").trim();
    let patch = Patch::from_canonical_bytes(bytes).unwrap();
    assert_eq!(patch.canonical_bytes(), bytes);
    assert_eq!(patch.id().to_string(), expected);
}

/// `sculpin-rdf-state/v1` vectors (ADR-0018): `.nq` holds canonical lines in any order with
/// duplicates; the digest is over the sorted, unique, header-prefixed bytes. Produced by the
/// independent `scripts/golden/state_v1_reference.py`.
#[test]
fn state_digest_vectors_are_stable() {
    use ledger_rdf::{Quad, state_digest};
    use std::collections::BTreeSet;
    let dir = format!(
        "{}/../../fixtures/golden/states",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("nq") {
            continue;
        }
        let expected = std::fs::read_to_string(path.with_extension("sha256"))
            .unwrap()
            .trim()
            .to_owned();
        let state: BTreeSet<Quad> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| {
                let quad = l.parse::<Quad>().unwrap();
                // The Python reference hashes the lines as written: fixtures must already be
                // canonical, so both sides hash identical bytes.
                assert_eq!(
                    quad.to_string(),
                    l,
                    "{}: fixture line is not canonical",
                    path.display()
                );
                quad
            })
            .collect();
        assert_eq!(
            state_digest(&state).to_string(),
            expected,
            "{}",
            path.display()
        );
        checked += 1;
    }
    assert_eq!(checked, 3, "every state vector is checked");
}
