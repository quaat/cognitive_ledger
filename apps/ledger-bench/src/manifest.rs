//! Dataset manifests, schema `sculpin-ledger-bench-manifest/v2` (Plan 0011).
//!
//! v2 makes provenance explicit and typed:
//! - a `generator` section for project-generated datasets;
//! - `source` and `extraction` sections for third-party ones;
//! - an `output` section binding the workload checksum and, for extracted datasets, the
//!   prepared artifact's SHA-256 and size.
//!
//! Every struct refuses unknown fields. [`Manifest::check_complete`] refuses a manifest whose
//! sections do not match its kind or whose provenance is incomplete. The schema id is
//! explicit, so a v1 manifest (no `manifest_schema`) is refused rather than reinterpreted.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MANIFEST_SCHEMA: &str = "sculpin-ledger-bench-manifest/v2";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub manifest_schema: String,
    pub dataset: String,
    /// `generated` (project-owned) or `extracted` (third-party source).
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generator: Option<Generator>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extraction: Option<Extraction>,
    pub output: Output,
}

/// A project-generated dataset.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Generator {
    pub name: String,
    pub version: String,
    /// Hex, `0x…` (a JSON number could not hold every u64 exactly).
    pub seed: String,
    pub params: serde_json::Value,
    pub license: String,
}

/// A third-party source, pinned on its first reviewed download.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub publisher: String,
    pub title: String,
    pub source_version: String,
    pub landing_url: String,
    pub license: String,
    pub attribution: String,
    /// Whether, and on what basis, derived data may be committed or redistributed.
    pub redistribution: String,
    /// UTC date of the reviewed download that pinned the hashes below.
    pub retrieved: String,
    pub files: Vec<SourceFile>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFile {
    /// What the extractor uses the file for (e.g. `full-versions`, `changesets`).
    pub role: String,
    pub url: String,
    pub sha256: String,
    pub bytes: u64,
}

/// How the prepared dataset was derived from the source.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Extraction {
    pub algorithm: String,
    pub version: String,
    pub parameters: BTreeMap<String, String>,
    /// The selected temporal/version range, e.g. `day versions v22..=v33 of v0..=v88`.
    pub range: String,
    /// Source and extracted counts (versions, triples, adds, deletes, reappearances, …).
    pub counts: BTreeMap<String, u64>,
    pub blank_nodes_skolemized: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Output {
    pub commits: usize,
    /// `workload_checksum` of the workload the runner executes.
    pub workload_checksum: String,
    /// The prepared artifact (extracted datasets).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_bytes: Option<u64>,
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

impl Manifest {
    /// Refuse a manifest whose sections do not match its kind or whose provenance is
    /// incomplete.
    pub fn check_complete(&self) -> Result<(), String> {
        let fail = |why: &str| Err(format!("manifest {}: {why}", self.dataset));
        if self.manifest_schema != MANIFEST_SCHEMA {
            return fail(&format!("manifest_schema must be {MANIFEST_SCHEMA}"));
        }
        if !self
            .output
            .workload_checksum
            .strip_prefix("sha256:")
            .is_some_and(is_sha256)
        {
            return fail("output.workload_checksum must be sha256:<64 lowercase hex digits>");
        }
        match self.kind.as_str() {
            "generated" => {
                let Some(g) = &self.generator else {
                    return fail("a generated dataset needs a generator section");
                };
                if self.source.is_some() || self.extraction.is_some() {
                    return fail("a generated dataset has no source or extraction section");
                }
                if g.version.is_empty() || !g.seed.starts_with("0x") || g.license.is_empty() {
                    return fail("generator version, seed and license are required");
                }
            }
            "extracted" => {
                let (Some(s), Some(x)) = (&self.source, &self.extraction) else {
                    return fail("an extracted dataset needs source and extraction sections");
                };
                if self.generator.is_some() {
                    return fail("an extracted dataset has no generator section");
                }
                for (field, value) in [
                    ("publisher", &s.publisher),
                    ("title", &s.title),
                    ("source_version", &s.source_version),
                    ("landing_url", &s.landing_url),
                    ("license", &s.license),
                    ("attribution", &s.attribution),
                    ("redistribution", &s.redistribution),
                    ("retrieved", &s.retrieved),
                    ("extraction.algorithm", &x.algorithm),
                    ("extraction.version", &x.version),
                    ("extraction.range", &x.range),
                ] {
                    if value.trim().is_empty() {
                        return fail(&format!("{field} is required"));
                    }
                }
                if s.files.is_empty() {
                    return fail("at least one pinned source file is required");
                }
                for f in &s.files {
                    if !f.url.starts_with("https://") || !is_sha256(&f.sha256) || f.bytes == 0 {
                        return fail(&format!(
                            "source file {}: an https url, a lowercase SHA-256 and a non-zero size are required",
                            f.role
                        ));
                    }
                }
                match (&self.output.artifact_sha256, self.output.artifact_bytes) {
                    (Some(h), Some(n)) if is_sha256(h) && n > 0 => {}
                    _ => return fail("output.artifact_sha256 and artifact_bytes are required"),
                }
            }
            other => return fail(&format!("unknown kind {other:?}")),
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn extracted() -> Manifest {
        Manifest {
            manifest_schema: MANIFEST_SCHEMA.into(),
            dataset: "bear-b-ci".into(),
            kind: "extracted".into(),
            generator: None,
            source: Some(Source {
                publisher: "p".into(),
                title: "t".into(),
                source_version: "v".into(),
                landing_url: "https://example.org/".into(),
                license: "l".into(),
                attribution: "a".into(),
                redistribution: "r".into(),
                retrieved: "2026-10-06".into(),
                files: vec![SourceFile {
                    role: "full-versions".into(),
                    url: "https://example.org/a.tar.gz".into(),
                    sha256: "a".repeat(64),
                    bytes: 1,
                }],
            }),
            extraction: Some(Extraction {
                algorithm: "x".into(),
                version: "x/1".into(),
                parameters: BTreeMap::new(),
                range: "v0..=v1".into(),
                counts: BTreeMap::new(),
                blank_nodes_skolemized: 0,
            }),
            output: Output {
                commits: 1,
                workload_checksum: format!("sha256:{}", "c".repeat(64)),
                artifact_sha256: Some("b".repeat(64)),
                artifact_bytes: Some(1),
            },
        }
    }

    #[test]
    fn complete_extracted_provenance_is_required() {
        assert!(extracted().check_complete().is_ok());
        let mut m = extracted();
        m.source.as_mut().unwrap().license.clear();
        assert!(m.check_complete().unwrap_err().contains("license"));
        let mut m = extracted();
        m.source.as_mut().unwrap().files[0].sha256 = "short".into();
        assert!(m.check_complete().unwrap_err().contains("SHA-256"));
        let mut m = extracted();
        m.source.as_mut().unwrap().files[0].url = "http://example.org/a".into();
        assert!(m.check_complete().is_err());
        let mut m = extracted();
        m.output.artifact_sha256 = None;
        assert!(m.check_complete().unwrap_err().contains("artifact"));
        let mut m = extracted();
        m.extraction = None;
        assert!(m.check_complete().unwrap_err().contains("extraction"));
        for bad in [
            "sha256:00".to_owned(),
            "c".repeat(64),
            format!("sha256:{}", "C".repeat(64)),
            format!("sha256:{}", "c".repeat(65)),
            format!("sha256:{}g", "c".repeat(63)),
            format!("SHA256:{}", "c".repeat(64)),
        ] {
            let mut m = extracted();
            m.output.workload_checksum = bad.clone();
            assert!(
                m.check_complete()
                    .unwrap_err()
                    .contains("workload_checksum"),
                "{bad}"
            );
        }
        let mut m = extracted();
        m.output.artifact_sha256 = Some("B".repeat(64));
        assert!(m.check_complete().unwrap_err().contains("artifact"));
        let mut m = extracted();
        m.manifest_schema = "sculpin-ledger-bench-manifest/v1".into();
        assert!(m.check_complete().unwrap_err().contains("manifest_schema"));
    }

    #[test]
    fn unknown_fields_and_v1_manifests_are_refused() {
        let mut v = serde_json::to_value(extracted()).unwrap();
        v["source"]["mirror"] = serde_json::json!("x");
        assert!(serde_json::from_value::<Manifest>(v).is_err());
        let v1 = serde_json::json!({"dataset": "x", "kind": "generated", "source": "s",
            "source_version": "v", "license": "l", "generator": "g", "generator_version": "1",
            "seed": "0x1", "params": {}, "commits": 1, "output_checksum": "sha256:00"});
        assert!(serde_json::from_value::<Manifest>(v1).is_err());
    }
}
