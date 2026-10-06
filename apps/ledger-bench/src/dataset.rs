//! Dataset lifecycle and profiles.
//!
//! A dataset adapter turns its source into a [`Workload`]: commits, branches, merges and
//! the oracle's expectations. It does this deterministically and offline. A generated dataset
//! (synthetic) needs nothing else. An extracted dataset (BEAR-B, TGB; later milestones)
//! reads a cached, checksum-verified extraction prepared before the run, never the network.
//!
//! Every dataset has a committed manifest under `benchmark/datasets/<id>.json`. Before
//! anything touches the ledger, the manifest the adapter computes must equal the committed
//! one field by field (source, version, license, generator version, seed, parameters and
//! output checksum). A difference makes the dataset invalid and fails the run. Updating a
//! manifest is a deliberate, reviewed change, like a golden vector.

use crate::{
    synthetic::{self, GENERATOR_VERSION, Params},
    workload::{Workload, workload_checksum},
};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub dataset: String,
    /// `generated` (owned by this project) or `extracted` (third-party source).
    pub kind: String,
    pub source: String,
    pub source_version: String,
    pub license: String,
    pub generator: String,
    pub generator_version: String,
    pub seed: String,
    pub params: serde_json::Value,
    pub commits: usize,
    pub output_checksum: String,
}

pub trait Dataset {
    fn id(&self) -> &'static str;
    /// Deterministic and offline.
    fn prepare(&self) -> Workload;
    fn manifest(&self, workload: &Workload) -> Manifest;
}

pub struct Synthetic {
    pub id: &'static str,
    pub params: Params,
}

impl Dataset for Synthetic {
    fn id(&self) -> &'static str {
        self.id
    }

    fn prepare(&self) -> Workload {
        synthetic::generate(&self.params)
    }

    fn manifest(&self, w: &Workload) -> Manifest {
        Manifest {
            dataset: self.id.into(),
            kind: "generated".into(),
            source: "apps/ledger-bench/src/synthetic.rs (generated; no external data)".into(),
            source_version: GENERATOR_VERSION.into(),
            license: "Apache-2.0 (project-owned generated data)".into(),
            generator: "ledger-bench synthetic".into(),
            generator_version: GENERATOR_VERSION.into(),
            seed: format!("{:#018x}", self.params.seed),
            params: serde_json::to_value(&self.params).expect("serializable params"),
            commits: w.commit_count(),
            output_checksum: workload_checksum(w),
        }
    }
}

/// The datasets of a profile. `ci` runs on every pull request; `local` is the deeper
/// baseline profile for a workstation.
pub fn profile(name: &str) -> Option<Vec<Box<dyn Dataset>>> {
    match name {
        "ci" => Some(vec![Box::new(Synthetic {
            id: "synthetic-ledger-ci",
            params: Params::ci(),
        })]),
        "local" => Some(vec![Box::new(Synthetic {
            id: "synthetic-ledger-local",
            params: Params::local(),
        })]),
        _ => None,
    }
}

pub const PROFILES: [&str; 2] = ["ci", "local"];

/// Compare the computed manifest with the committed one.
pub fn verify_manifest(dir: &Path, computed: &Manifest) -> Result<(), String> {
    let path = dir.join(format!("{}.json", computed.dataset));
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("manifest {}: {e}", path.display()))?;
    let committed: Manifest =
        serde_json::from_str(&text).map_err(|e| format!("manifest {}: {e}", path.display()))?;
    if &committed == computed {
        return Ok(());
    }
    let a = serde_json::to_value(&committed).expect("serializable");
    let b = serde_json::to_value(computed).expect("serializable");
    let differing: Vec<String> = a
        .as_object()
        .expect("object")
        .iter()
        .filter(|(k, v)| b.get(k.as_str()) != Some(v))
        .map(|(k, v)| format!("{k}: committed {v}, computed {}", b[k.as_str()]))
        .collect();
    Err(format!(
        "dataset {} does not match its manifest {}: {}",
        computed.dataset,
        path.display(),
        differing.join("; ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifests() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../benchmark/datasets")
    }

    #[test]
    fn the_committed_ci_manifest_matches_the_generator() {
        for d in profile("ci").unwrap() {
            let w = d.prepare();
            verify_manifest(&manifests(), &d.manifest(&w)).unwrap();
        }
    }

    #[test]
    fn a_changed_dataset_is_refused() {
        let d = Synthetic {
            id: "synthetic-ledger-ci",
            params: Params {
                seed: 1,
                ..Params::ci()
            },
        };
        let w = d.prepare();
        let err = verify_manifest(&manifests(), &d.manifest(&w)).unwrap_err();
        assert!(
            err.contains("seed") && err.contains("output_checksum"),
            "{err}"
        );
    }
}
