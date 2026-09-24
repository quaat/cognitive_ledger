use ledger_core::{Commit, ContentId};
#[test]
fn commit_v1_vector_is_stable() {
    // Keep protocol fixtures reviewable as text: the PR transport rejects binary
    // files, and the committed hexadecimal form is the inspectable canonical bytes.
    let bytes = hex::decode(include_str!("../../../fixtures/golden/commits/basic-v1.hex").trim())
        .expect("golden commit hex is valid");
    let expected = include_str!("../../../fixtures/golden/commits/basic-v1.sha256").trim();
    let commit = Commit::from_canonical_bytes(&bytes).unwrap();
    assert_eq!(commit.canonical_bytes().unwrap(), bytes);
    assert_eq!(commit.id().unwrap().to_string(), expected);
    assert_eq!(ContentId::for_bytes(&bytes).to_string(), expected);
}
