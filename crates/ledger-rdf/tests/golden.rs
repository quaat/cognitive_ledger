use ledger_rdf::Patch;
#[test]
fn patch_v1_vector_is_stable() {
    let bytes = include_bytes!("../../../fixtures/golden/patches/basic-v1.patch");
    let expected = include_str!("../../../fixtures/golden/patches/basic-v1.sha256").trim();
    let patch = Patch::from_canonical_bytes(bytes).unwrap();
    assert_eq!(patch.canonical_bytes(), bytes);
    assert_eq!(patch.id().to_string(), expected);
}
