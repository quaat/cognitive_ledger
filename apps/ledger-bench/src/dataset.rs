//! Dataset lifecycle and profiles.
//!
//! A dataset adapter turns its source into a [`Workload`]: commits, branches, merges and
//! the oracle's expectations. It does this deterministically and **offline**:
//! - a generated dataset (synthetic) regenerates it from its seed;
//! - an extracted dataset (BEAR-B) loads its prepared artifact from the cache, after
//!   `fetch` (network: download and verify the pinned source) and `prepare` (offline:
//!   extract, cross-check and write the artifact) have run.
//!
//! A missing or unverified cache is an error, never a download.
//!
//! Every dataset has a committed manifest under `benchmark/datasets/<id>.json`
//! ([`crate::manifest`], schema v2). Before anything touches the ledger, the manifest the
//! adapter computes must equal the committed one field by field. A difference makes the
//! dataset invalid and fails the run (exit 3). Updating a manifest is a deliberate, reviewed
//! change, like a golden vector.

use crate::{
    bear,
    manifest::{Generator, MANIFEST_SCHEMA, Manifest, Output},
    synthetic::{self, GENERATOR_VERSION, Params},
    workload::{Workload, workload_checksum},
};
use std::path::{Path, PathBuf};

/// Where manifests and the dataset cache live.
#[derive(Clone, Debug)]
pub struct Context {
    pub manifests: PathBuf,
    pub cache: PathBuf,
}

pub trait Dataset {
    fn id(&self) -> &'static str;
    /// Deterministic and offline (see the module docs).
    fn prepare(&self, ctx: &Context) -> Result<Workload, String>;
    /// The manifest this preparation corresponds to.
    fn manifest(&self, ctx: &Context, workload: &Workload) -> Result<Manifest, String>;
}

pub struct Synthetic {
    pub id: &'static str,
    pub params: Params,
}

impl Dataset for Synthetic {
    fn id(&self) -> &'static str {
        self.id
    }

    fn prepare(&self, _ctx: &Context) -> Result<Workload, String> {
        Ok(synthetic::generate(&self.params))
    }

    fn manifest(&self, _ctx: &Context, w: &Workload) -> Result<Manifest, String> {
        Ok(synthetic_manifest(self.id, &self.params, w))
    }
}

pub fn synthetic_manifest(id: &str, params: &Params, w: &Workload) -> Manifest {
    Manifest {
        manifest_schema: MANIFEST_SCHEMA.into(),
        dataset: id.into(),
        kind: "generated".into(),
        generator: Some(Generator {
            name: "ledger-bench synthetic (apps/ledger-bench/src/synthetic.rs)".into(),
            version: GENERATOR_VERSION.into(),
            seed: format!("{:#018x}", params.seed),
            params: serde_json::to_value(params).expect("serializable params"),
            license: "Apache-2.0 (project-owned generated data)".into(),
        }),
        source: None,
        extraction: None,
        output: Output {
            commits: w.commit_count(),
            workload_checksum: workload_checksum(w),
            artifact_sha256: None,
            artifact_bytes: None,
        },
    }
}

/// The datasets of a profile.
/// - `ci` runs on every pull request.
/// - `local` is the deeper synthetic baseline.
/// - `bear` is the BEAR-B dataset alone.
pub fn profile(name: &str) -> Option<Vec<Box<dyn Dataset>>> {
    let synthetic_ci = || -> Box<dyn Dataset> {
        Box::new(Synthetic {
            id: "synthetic-ledger-ci",
            params: Params::ci(),
        })
    };
    let bear_ci = || -> Box<dyn Dataset> { Box::new(bear::BearB { id: "bear-b-ci" }) };
    match name {
        "ci" => Some(vec![synthetic_ci(), bear_ci()]),
        "local" => Some(vec![Box::new(Synthetic {
            id: "synthetic-ledger-local",
            params: Params::local(),
        })]),
        "bear" => Some(vec![bear_ci()]),
        _ => None,
    }
}

pub const PROFILES: [&str; 3] = ["ci", "local", "bear"];

/// Read and check a committed manifest.
pub fn load_manifest(dir: &Path, id: &str) -> Result<Manifest, String> {
    let path = dir.join(format!("{id}.json"));
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("manifest {}: {e}", path.display()))?;
    let m: Manifest =
        serde_json::from_str(&text).map_err(|e| format!("manifest {}: {e}", path.display()))?;
    m.check_complete()?;
    Ok(m)
}

/// Compare the computed manifest with the committed one.
pub fn verify_manifest(dir: &Path, computed: &Manifest) -> Result<(), String> {
    let committed = load_manifest(dir, &computed.dataset)?;
    computed.check_complete()?;
    if &committed == computed {
        return Ok(());
    }
    let a = serde_json::to_value(&committed).expect("serializable");
    let b = serde_json::to_value(computed).expect("serializable");
    let mut differing = Vec::new();
    diff_values("", &a, &b, &mut differing);
    Err(format!(
        "dataset {} does not match its committed manifest: {}",
        computed.dataset,
        differing.join("; ")
    ))
}

fn diff_values(at: &str, a: &serde_json::Value, b: &serde_json::Value, out: &mut Vec<String>) {
    match (a, b) {
        (serde_json::Value::Object(x), serde_json::Value::Object(y)) => {
            let keys: std::collections::BTreeSet<&String> = x.keys().chain(y.keys()).collect();
            for k in keys {
                let null = serde_json::Value::Null;
                diff_values(
                    &format!("{at}{}{k}", if at.is_empty() { "" } else { "." }),
                    x.get(k).unwrap_or(&null),
                    y.get(k).unwrap_or(&null),
                    out,
                );
            }
        }
        _ if a != b => out.push(format!("{at}: committed {a}, computed {b}")),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Context {
        Context {
            manifests: Path::new(env!("CARGO_MANIFEST_DIR")).join("../../benchmark/datasets"),
            cache: PathBuf::from("/nonexistent-cache"),
        }
    }

    #[test]
    fn the_committed_synthetic_manifests_match_the_generator() {
        let d = Synthetic {
            id: "synthetic-ledger-ci",
            params: Params::ci(),
        };
        let w = d.prepare(&ctx()).unwrap();
        verify_manifest(&ctx().manifests, &d.manifest(&ctx(), &w).unwrap()).unwrap();
    }

    #[test]
    fn a_changed_expectation_changes_the_checksum() {
        let w = synthetic::generate(&Params::ci());
        let mut changed = w.clone();
        changed
            .expected
            .values_mut()
            .next()
            .unwrap()
            .provenance
            .message
            .push('!');
        assert_ne!(workload_checksum(&w), workload_checksum(&changed));
        let mut changed = w.clone();
        changed.expected.values_mut().next().unwrap().fold_ops += 1;
        assert_ne!(workload_checksum(&w), workload_checksum(&changed));
    }

    #[test]
    fn a_changed_dataset_is_refused_with_the_differing_fields() {
        let d = Synthetic {
            id: "synthetic-ledger-ci",
            params: Params {
                seed: 1,
                ..Params::ci()
            },
        };
        let w = d.prepare(&ctx()).unwrap();
        let err = verify_manifest(&ctx().manifests, &d.manifest(&ctx(), &w).unwrap()).unwrap_err();
        assert!(
            err.contains("generator.seed") && err.contains("output.workload_checksum"),
            "{err}"
        );
    }

    #[test]
    fn an_extracted_dataset_without_its_prepared_cache_fails_without_downloading() {
        let err = bear::BearB { id: "bear-b-ci" }.prepare(&ctx()).unwrap_err();
        assert!(err.contains("prepare"), "{err}");
    }
}
