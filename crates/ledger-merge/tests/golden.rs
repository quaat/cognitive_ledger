//! `sculpin-ledger-merge-preview/v1` golden vectors (ADR-0024): every
//! `fixtures/golden/merge-preview/*.input` encodes, in Rust, to its `.hex` bytes and `.token`
//! digest; `scripts/golden/merge_preview_v1_reference.py` checks the same files.

use ledger_core::{CommitId, ContentId, GraphId};
use ledger_merge::{Classification, PreviewIdentity, Strategy};
use std::{fs, path::PathBuf, str::FromStr};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/merge-preview")
}

fn field<'a>(json: &'a str, name: &str) -> &'a str {
    let key = format!("\"{name}\": \"");
    let start = json.find(&key).unwrap_or_else(|| panic!("{name}")) + key.len();
    let end = start + json[start..].find('"').unwrap();
    &json[start..end]
}

#[test]
fn preview_token_vectors_are_stable() {
    let mut n = 0;
    for entry in fs::read_dir(fixtures()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("input") {
            continue;
        }
        n += 1;
        let json = fs::read_to_string(&path).unwrap();
        let commit = |name: &str| CommitId::from_str(field(&json, name)).unwrap();
        let id = PreviewIdentity {
            graph: GraphId::new(field(&json, "graph_id")).unwrap(),
            source_branch: field(&json, "source_branch").into(),
            source_head: commit("source_head"),
            target_branch: field(&json, "target_branch").into(),
            target_head: commit("target_head"),
            merge_base: commit("merge_base"),
            classification: match field(&json, "classification") {
                "fast_forward" => Classification::FastForward,
                "divergent" => Classification::Divergent,
                other => panic!("unknown classification {other}"),
            },
            strategy: Strategy::parse(field(&json, "strategy")).unwrap(),
            merged_state_digest: ContentId::from_str(field(&json, "merged_state_digest")).unwrap(),
        };
        let hex = fs::read_to_string(path.with_extension("hex")).unwrap();
        let token = fs::read_to_string(path.with_extension("token")).unwrap();
        let bytes: String = id
            .canonical_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(bytes, hex.trim(), "{}", path.display());
        assert_eq!(id.token(), token.trim(), "{}", path.display());
    }
    assert_eq!(n, 6, "expected exactly 6 preview-token vectors");
}
