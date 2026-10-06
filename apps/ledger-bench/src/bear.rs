//! `bear-b-ci`: BEAR-B (DBpedia Live, day granularity) as a ledger workload (Plan 0011, M3).
//!
//! **Lifecycle.**
//! - `fetch` is the only networked step. It downloads the pinned source files and checks
//!   each one's size and SHA-256 against the committed manifest.
//! - `prepare` is offline. It extracts and cross-checks, then writes the prepared artifact.
//! - The benchmark (`validate`/`run`) is offline. It loads only the hash-verified artifact.
//!
//! A missing or mismatching cache fails; nothing downloads implicitly.
//!
//! **Source facts, verified on the first reviewed download (DATASETS.md).** BEAR-B day
//! publishes three representations of 89 versions:
//! - IC: one N-Triples file per version, named `000001.nt.gz` … `000089.nt.gz`;
//! - CB: added and deleted files per version pair;
//! - TB: one N-Quads file whose graph `<http://example.org/v0_1_…>` lists the 0-based
//!   versions containing each triple, plus `owl:versionInfo` metadata about those graphs.
//!
//! IC version 1 plus the cumulative CB changes equals TB at every step: one *changeset
//! lineage*. The IC files after version 1 are a separately materialized lineage. It drops
//! stale values the changesets never delete, and disagrees with both. This dataset follows
//! the changeset lineage, where two independent representations agree exactly:
//! - the **oracle** is TB's per-version membership, the full versions. TB and CB are two
//!   encodings of one lineage (BEAR likely derived TB from the changesets): their agreement
//!   proves the extraction reads both consistently, not that the lineage is "true";
//! - the **hard cross-check**, at every step, is that CB's net change equals TB's difference
//!   and that no-op churn (in both CB files) is present in both versions;
//! - the **anchor** is that TB version 0 equals IC file 1.
//!
//! The IC divergence over the selected window is recorded in the manifest counts.
//!
//! **Normalization.** Every triple passes through the ledger's canonical N-Quads rules
//! (`ledger_rdf::Quad`, the frozen canonical form the ledger returns). Blank nodes are counted.
//! This source has none; any blank node fails extraction, because the documented
//! skolemization would have to be implemented and reviewed first. Parse failures fail too.
//!
//! **Selection.** The 12 consecutive versions with the most changes (adds + deletes; lowest
//! start on ties). All triples of every selected version are kept; nothing is sampled.

use crate::{
    archive::{gunzip_capped, tar_regular_files},
    dataset::{Context, Dataset},
    manifest::{Extraction, Manifest, Output},
    workload::{
        CommitStep, Expected, HistoryFact, Label, Provenance, Step, Workload, oracle_digest,
        statement_ref, workload_checksum,
    },
};
use ledger_rdf::{Quad, RdfError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub const EXTRACTION_ALGORITHM: &str = "BEAR-B day: TB/CB changeset lineage, anchored at IC version 1, canonical N-Quads, 12-version max-change window";
pub const EXTRACTION_VERSION: &str = "bear-b-day-extract/1";
const ARTIFACT_FORMAT: &str = "sculpin-ledger-bench-prepared/v1";
const WINDOW: usize = 12;
/// Ingestion batching (the API caps a request at 10,000 operations and 2 MiB; limits are
/// never raised): at most this many operations and quad bytes per commit.
const MAX_OPS_PER_COMMIT: usize = 5_000;
const MAX_BYTES_PER_COMMIT: usize = 1_400_000;
/// Decompression and selection caps (archive safety).
const GZIP_CAP: u64 = 512 * 1024 * 1024;
const ENTRY_CAP: u64 = 16 * 1024 * 1024;
const NT_CAP: u64 = 64 * 1024 * 1024;
const TAR_TOTAL_CAP: u64 = 256 * 1024 * 1024;
/// Prepared artifact cap.
const ARTIFACT_CAP: u64 = 256 * 1024 * 1024;
const TB_GRAPH: &str = "<http://example.org/v";
const MAX_HISTORY_FACTS: usize = 5;

pub struct BearB {
    pub id: &'static str,
}

fn source_dir(ctx: &Context, id: &str) -> PathBuf {
    ctx.cache.join(id).join("source")
}

fn artifact_path(ctx: &Context, id: &str) -> PathBuf {
    ctx.cache.join(id).join("prepared").join("artifact.txt")
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn file_name(url: &str) -> Result<String, String> {
    let name = url.rsplit('/').next().unwrap_or("");
    // Never `.`/`..` or a hidden name: the result becomes a cache path component.
    if name.is_empty()
        || name.starts_with('.')
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return Err(format!("unexpected source file name in {url}"));
    }
    Ok(name.to_owned())
}

/// Local cache name of a source file (role-prefixed: the publisher reuses file names).
fn cached_name(role: &str, url: &str) -> Result<String, String> {
    if role.is_empty() || !role.bytes().all(|b| b.is_ascii_lowercase() || b == b'-') {
        return Err(format!(
            "source role {role:?} must be lowercase letters and dashes"
        ));
    }
    Ok(format!("{role}--{}", file_name(url)?))
}

/// A sibling temporary name (`<name>.part`), renamed into place only after verification.
fn part_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    path.with_file_name(name)
}

/// The committed manifest's pinned source, without requiring the extraction and output
/// sections that only `prepare` can produce (bootstrap and re-preparation).
fn source_manifest(ctx: &Context, id: &str) -> Result<Manifest, String> {
    let path = ctx.manifests.join(format!("{id}.json"));
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("manifest {}: {e}", path.display()))?;
    let m: Manifest =
        serde_json::from_str(&text).map_err(|e| format!("manifest {}: {e}", path.display()))?;
    if m.dataset != id {
        return Err(format!(
            "manifest {} names dataset {:?}, not {id:?}",
            path.display(),
            m.dataset
        ));
    }
    let mut probe = m.clone();
    // Check the source section with placeholders for the derived ones.
    probe.extraction.get_or_insert(Extraction {
        algorithm: "-".into(),
        version: "-".into(),
        parameters: BTreeMap::new(),
        range: "-".into(),
        counts: BTreeMap::new(),
        blank_nodes_skolemized: 0,
    });
    probe.output.artifact_sha256.get_or_insert("0".repeat(64));
    probe.output.artifact_bytes.get_or_insert(1);
    if !probe.output.workload_checksum.starts_with("sha256:") {
        probe.output.workload_checksum = "sha256:-".into();
    }
    probe.check_complete()?;
    Ok(m)
}

/// Download every pinned source file that is not already cached and verified.
pub async fn fetch(ctx: &Context, id: &str) -> Result<Vec<String>, String> {
    let manifest = source_manifest(ctx, id)?;
    let source = manifest.source.as_ref().ok_or("not an extracted dataset")?;
    let dir = source_dir(ctx, id);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // Redirects only to https on the pinned URL's own host (integrity is pinned anyway; this
    // keeps the download from wandering to other hosts or to plain http).
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        // A stalled transfer fails instead of holding a CI runner until the job timeout.
        .read_timeout(Duration::from_secs(120))
        .timeout(Duration::from_secs(1800))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let same_host = attempt
                .previous()
                .first()
                .is_some_and(|first| first.host_str() == attempt.url().host_str());
            if attempt.url().scheme() == "https" && same_host && attempt.previous().len() < 5 {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
        .map_err(|e| e.to_string())?;
    let mut log = Vec::new();
    for f in &source.files {
        let path = dir.join(cached_name(&f.role, &f.url)?);
        if let Ok(bytes) = std::fs::read(&path)
            && bytes.len() as u64 == f.bytes
            && sha256_hex(&bytes) == f.sha256
        {
            log.push(format!("{}: cached and verified", f.role));
            continue;
        }
        let body = download(&http, &f.url, f.bytes).await?;
        let (size, digest) = (body.len() as u64, sha256_hex(&body));
        if size != f.bytes || digest != f.sha256 {
            return Err(format!(
                "{}: got {size} bytes sha256 {digest}, the manifest pins {} bytes sha256 {}",
                f.url, f.bytes, f.sha256
            ));
        }
        let part = part_path(&path);
        let mut file = std::fs::File::create(&part).map_err(|e| e.to_string())?;
        file.write_all(&body).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        std::fs::rename(&part, &path).map_err(|e| e.to_string())?;
        log.push(format!("{}: downloaded {size} bytes, verified", f.role));
    }
    Ok(log)
}

/// Download attempts per file; transient failures back off for 5 s, then 20 s.
const FETCH_ATTEMPTS: u32 = 3;

/// Download one pinned file, retrying only transient failures: transport errors (connect,
/// timeout, reset) and HTTP 429/5xx. Any other status, and a body larger than the pin, fail at
/// once. Integrity is checked by the caller and is never retried.
async fn download(http: &reqwest::Client, url: &str, cap: u64) -> Result<Vec<u8>, String> {
    let mut attempt = 1;
    loop {
        let failure = match http.get(url).send().await {
            Ok(r) if r.status().is_success() => match read_capped(r, url, cap).await {
                Ok(body) => return Ok(body),
                Err(Transfer::Fatal(e)) => return Err(e),
                Err(Transfer::Transient(e)) => e,
            },
            Ok(r) if r.status().as_u16() == 429 || r.status().is_server_error() => {
                format!("{url}: HTTP {}", r.status())
            }
            Ok(r) => return Err(format!("{url}: HTTP {}", r.status())),
            Err(e) => format!("{url}: {e}"),
        };
        if attempt == FETCH_ATTEMPTS {
            return Err(format!("{failure} (after {FETCH_ATTEMPTS} attempts)"));
        }
        let wait = Duration::from_secs(5 * 4u64.pow(attempt - 1));
        eprintln!("fetch: {failure}; retrying in {} s", wait.as_secs());
        tokio::time::sleep(wait).await;
        attempt += 1;
    }
}

enum Transfer {
    Fatal(String),
    Transient(String),
}

async fn read_capped(mut r: reqwest::Response, url: &str, cap: u64) -> Result<Vec<u8>, Transfer> {
    let mut body = Vec::new();
    loop {
        match r.chunk().await {
            Ok(Some(chunk)) => {
                body.extend_from_slice(&chunk);
                if body.len() as u64 > cap {
                    return Err(Transfer::Fatal(format!(
                        "{url}: larger than the pinned {cap} bytes"
                    )));
                }
            }
            Ok(None) => return Ok(body),
            Err(e) => return Err(Transfer::Transient(format!("{url}: {e}"))),
        }
    }
}

/// Read every pinned source file from the cache, verifying size and SHA-256.
pub fn verified_sources(
    ctx: &Context,
    manifest: &Manifest,
) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let source = manifest.source.as_ref().ok_or("not an extracted dataset")?;
    let dir = source_dir(ctx, &manifest.dataset);
    let mut out = BTreeMap::new();
    for f in &source.files {
        let path = dir.join(cached_name(&f.role, &f.url)?);
        let bytes = std::fs::read(&path).map_err(|_| {
            format!(
                "source {} is not cached at {}: run `ledger-bench fetch {}` first",
                f.role,
                path.display(),
                manifest.dataset
            )
        })?;
        let (size, digest) = (bytes.len() as u64, sha256_hex(&bytes));
        if size != f.bytes || digest != f.sha256 {
            return Err(format!(
                "cached source {} is {size} bytes sha256 {digest}; the manifest pins {} bytes sha256 {}",
                f.role, f.bytes, f.sha256
            ));
        }
        out.insert(f.role.clone(), bytes);
    }
    Ok(out)
}

/// Normalization outcome counters.
#[derive(Default)]
struct Normalizer {
    blank_nodes: u64,
    invalid: Vec<String>,
    /// Source lines whose canonical form differs from the source spelling (bounds what the
    /// shared canonicalizer could mask).
    rewritten: u64,
    /// Repeated lines inside one IC or CB file.
    duplicates: u64,
}

impl Normalizer {
    /// The canonical N-Quads line of one source triple (the form the ledger returns).
    fn canonical(&mut self, line: &str) -> Option<String> {
        match line.parse::<Quad>() {
            Ok(q) => {
                if q.as_str() != line.trim() {
                    self.rewritten += 1;
                }
                Some(q.as_str().to_owned())
            }
            Err(RdfError::BlankNode) => {
                self.blank_nodes += 1;
                None
            }
            Err(e) => {
                if self.invalid.len() < 3 {
                    // Third-party statements are named by hash, never printed (CI logs).
                    self.invalid
                        .push(format!("{e}: {}", statement_ref(line, true)));
                }
                self.invalid.push(String::new());
                None
            }
        }
    }

    fn set(&mut self, text: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for l in text
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        {
            if let Some(c) = self.canonical(l)
                && !out.insert(c)
            {
                self.duplicates += 1;
            }
        }
        out
    }

    fn finish(&self) -> Result<(), String> {
        if self.blank_nodes > 0 {
            return Err(format!(
                "{} source statements contain blank nodes: deterministic skolemization must be \
                 implemented and reviewed before this source can be used",
                self.blank_nodes
            ));
        }
        if self.duplicates > 0 {
            return Err(format!(
                "{} repeated statements inside one source file",
                self.duplicates
            ));
        }
        if !self.invalid.is_empty() {
            let samples: Vec<&String> = self.invalid.iter().filter(|s| !s.is_empty()).collect();
            return Err(format!(
                "{} source statements do not parse as RDF: {samples:?}",
                self.invalid.len()
            ));
        }
        Ok(())
    }
}

/// What `prepare` writes as the artifact's first line.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactHeader {
    format: String,
    dataset: String,
    extraction: Extraction,
}

/// The extracted window: the first version's state and each step's changes.
#[derive(Debug)]
struct Prepared {
    start: usize,
    base: BTreeSet<String>,
    /// `(deletes, adds)` from version `start + i` to `start + i + 1`.
    steps: Vec<(BTreeSet<String>, BTreeSet<String>)>,
}

/// Extract, cross-check and write the prepared artifact (offline). Returns the artifact's
/// SHA-256 and size.
pub fn prepare(ctx: &Context, id: &str) -> Result<(String, u64, Extraction), String> {
    let manifest = source_manifest(ctx, id)?;
    let sources = verified_sources(ctx, &manifest)?;
    let get = |role: &str| {
        sources
            .get(role)
            .ok_or_else(|| format!("the manifest pins no `{role}` source"))
    };
    let mut norm = Normalizer::default();
    let mut counts = BTreeMap::new();

    // IC: full versions as N-Triples, one gzipped entry per version.
    let ic_tar = gunzip_capped(get("full-versions")?, GZIP_CAP)?;
    let ic_name = |n: &str| {
        n.len() == 12 && n.ends_with(".nt.gz") && n[..6].bytes().all(|b| b.is_ascii_digit())
    };
    let (ic_files, ic_report) = tar_regular_files(&ic_tar, ic_name, ENTRY_CAP, TAR_TOTAL_CAP)?;
    let versions = ic_files.len();
    if versions < WINDOW {
        return Err(format!(
            "IC holds {versions} versions; the window needs {WINDOW}"
        ));
    }
    for (i, name) in ic_files.keys().enumerate() {
        if *name != format!("{:06}.nt.gz", i + 1) {
            return Err(format!(
                "IC entries are not 000001..{versions:06} (found {name})"
            ));
        }
    }
    counts.insert("source_versions".into(), versions as u64);
    counts.insert(
        "ic_tar_entries_ignored".into(),
        (ic_report.regular_ignored + ic_report.non_regular_skipped) as u64,
    );
    let ic = |k: usize, norm: &mut Normalizer| -> Result<BTreeSet<String>, String> {
        let gz = &ic_files[&format!("{k:06}.nt.gz")];
        let text =
            String::from_utf8(gunzip_capped(gz, NT_CAP)?).map_err(|_| "IC entry is not UTF-8")?;
        Ok(norm.set(&text))
    };

    // CB: added/deleted per version pair a -> a+1 (1-based, IC numbering).
    let cb_tar = gunzip_capped(get("changesets")?, GZIP_CAP)?;
    let cb_name = |n: &str| {
        let Some(rest) = n
            .strip_prefix("data-added_")
            .or_else(|| n.strip_prefix("data-deleted_"))
        else {
            return false;
        };
        let Some(pair) = rest.strip_suffix(".nt.gz") else {
            return false;
        };
        let Some((a, b)) = pair.split_once('-') else {
            return false;
        };
        matches!((a.parse::<usize>(), b.parse::<usize>()), (Ok(a), Ok(b)) if b == a + 1 && a >= 1)
    };
    let (cb_files, _) = tar_regular_files(&cb_tar, cb_name, ENTRY_CAP, TAR_TOTAL_CAP)?;
    if cb_files.len() != 2 * (versions - 1) {
        return Err(format!(
            "expected {} CB entries, found {}",
            2 * (versions - 1),
            cb_files.len()
        ));
    }
    let cb = |kind: &str, a: usize, norm: &mut Normalizer| -> Result<BTreeSet<String>, String> {
        let gz = cb_files
            .get(&format!("data-{kind}_{a}-{}.nt.gz", a + 1))
            .ok_or_else(|| format!("missing CB {kind} {a}-{}", a + 1))?;
        let text =
            String::from_utf8(gunzip_capped(gz, NT_CAP)?).map_err(|_| "CB entry is not UTF-8")?;
        Ok(norm.set(&text))
    };

    // TB: triple -> the 0-based versions containing it.
    let tb_text = String::from_utf8(gunzip_capped(get("time-annotated")?, GZIP_CAP)?)
        .map_err(|_| "TB is not UTF-8")?;
    // TB may split one triple's membership over several lines; their version lists must be
    // disjoint (overlap would be ambiguous). Membership is the union.
    let mut raw: BTreeMap<&str, BTreeSet<usize>> = BTreeMap::new();
    let (mut metadata, mut tb_lines, mut split_lines) = (0u64, 0u64, 0u64);
    for line in tb_text.lines().filter(|l| !l.trim().is_empty()) {
        if line.starts_with(TB_GRAPH) {
            if !line.contains("> <http://www.w3.org/2002/07/owl#versionInfo> ") {
                return Err(format!(
                    "unexpected TB metadata statement {}",
                    statement_ref(line, true)
                ));
            }
            metadata += 1;
            continue;
        }
        tb_lines += 1;
        let (triple, graph) = line
            .strip_suffix(" .")
            .and_then(|l| l.rsplit_once(' '))
            .ok_or_else(|| format!("unexpected TB statement {}", statement_ref(line, true)))?;
        let list = graph
            .strip_prefix(TB_GRAPH)
            .and_then(|g| g.strip_suffix('>'))
            .ok_or_else(|| format!("unexpected TB graph {graph}"))?;
        let mut vs = BTreeSet::new();
        let mut last = None;
        for v in list.split('_') {
            let v: usize = v
                .parse()
                .map_err(|_| format!("bad TB version list {graph}"))?;
            if v >= versions || last.is_some_and(|l| v <= l) {
                return Err(format!(
                    "TB version list not strictly increasing within 0..{versions}: {graph}"
                ));
            }
            last = Some(v);
            vs.insert(v);
        }
        let entry = raw.entry(triple).or_default();
        if !entry.is_empty() {
            split_lines += 1;
            if !entry.is_disjoint(&vs) {
                return Err(format!(
                    "TB lists overlapping versions for one triple {}",
                    statement_ref(triple, true)
                ));
            }
        }
        entry.extend(vs);
    }
    // Normalize; distinct source spellings with one canonical form merge (counted).
    let mut membership: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    let mut collisions = 0u64;
    for (triple, vs) in raw {
        let Some(canonical) = norm.canonical(&format!("{triple} .")) else {
            continue;
        };
        if membership.contains_key(&canonical) {
            collisions += 1;
        }
        membership.entry(canonical).or_default().extend(vs);
    }
    // Two source spellings with one canonical form would merge silently: refused.
    if collisions > 0 {
        return Err(format!(
            "{collisions} distinct TB spellings normalize to one canonical statement"
        ));
    }
    counts.insert("tb_statements".into(), tb_lines);
    counts.insert("tb_version_metadata_statements".into(), metadata);
    counts.insert("tb_unique_triples".into(), membership.len() as u64);
    counts.insert("tb_split_annotation_lines".into(), split_lines);
    counts.insert("normalization_collisions".into(), collisions);
    let state = |k: usize| -> BTreeSet<String> {
        membership
            .iter()
            .filter(|(_, v)| v.contains(&k))
            .map(|(t, _)| t.clone())
            .collect()
    };

    // Anchor: TB version 0 == IC version 1.
    if state(0) != ic(1, &mut norm)? {
        return Err(
            "TB version 0 differs from IC version 1: the lineages do not share a start".into(),
        );
    }
    // Cross-check every step: CB's net change equals TB's difference; no-op churn is present
    // in both versions.
    let mut churn = 0u64;
    for k in 1..versions {
        let (added, deleted) = (cb("added", k, &mut norm)?, cb("deleted", k, &mut norm)?);
        let in_both: BTreeSet<&String> = added.intersection(&deleted).collect();
        churn += in_both.len() as u64;
        let tb_adds: BTreeSet<&String> = membership
            .iter()
            .filter(|(_, v)| v.contains(&k) && !v.contains(&(k - 1)))
            .map(|(t, _)| t)
            .collect();
        let tb_dels: BTreeSet<&String> = membership
            .iter()
            .filter(|(_, v)| v.contains(&(k - 1)) && !v.contains(&k))
            .map(|(t, _)| t)
            .collect();
        let net_adds: BTreeSet<&String> = added.difference(&deleted).collect();
        let net_dels: BTreeSet<&String> = deleted.difference(&added).collect();
        let churn_ok = in_both.iter().all(|t| {
            membership
                .get(*t)
                .is_some_and(|v| v.contains(&(k - 1)) && v.contains(&k))
        });
        if net_adds != tb_adds || net_dels != tb_dels || !churn_ok {
            return Err(format!(
                "CB {k}-{} disagrees with TB v{}→v{k}: CB net +{} −{}, TB +{} −{}, churn in both versions: {churn_ok}",
                k + 1,
                k - 1,
                net_adds.len(),
                net_dels.len(),
                tb_adds.len(),
                tb_dels.len()
            ));
        }
    }
    counts.insert("cb_noop_churn_statements".into(), churn);
    counts.insert("cb_steps_cross_checked".into(), (versions - 1) as u64);
    norm.finish()?;

    // Window: the 12 consecutive versions with the most changes (lowest start on ties).
    let changes = |k: usize| -> (usize, usize) {
        let adds = membership
            .values()
            .filter(|v| v.contains(&k) && !v.contains(&(k - 1)))
            .count();
        let dels = membership
            .values()
            .filter(|v| v.contains(&(k - 1)) && !v.contains(&k))
            .count();
        (adds, dels)
    };
    let per_step: Vec<(usize, usize)> = (1..versions).map(changes).collect();
    let start = (0..=versions - WINDOW)
        .max_by_key(|s| {
            let total: usize = per_step[*s..*s + WINDOW - 1]
                .iter()
                .map(|(a, d)| a + d)
                .sum();
            (total, std::cmp::Reverse(*s))
        })
        .ok_or("fewer versions than the window")?;
    let base = state(start);
    let mut steps = Vec::new();
    let mut prev = base.clone();
    let mut divergence = 0u64;
    for k in start + 1..start + WINDOW {
        let next = state(k);
        steps.push((
            prev.difference(&next).cloned().collect::<BTreeSet<_>>(),
            next.difference(&prev).cloned().collect::<BTreeSet<_>>(),
        ));
        prev = next;
    }
    for k in start..start + WINDOW {
        let (tb, icv) = (state(k), ic(k + 1, &mut norm)?);
        // The IC lineage only drops statements the changesets keep: IC ⊆ TB at every
        // selected version (verified on the source). A violation means the documented
        // relation between the two lineages no longer holds.
        if !icv.is_subset(&tb) {
            return Err(format!(
                "IC file {:06} holds {} statements outside TB v{k}: the documented lineage relation does not hold",
                k + 1,
                icv.difference(&tb).count()
            ));
        }
        divergence += tb.difference(&icv).count() as u64;
    }
    norm.finish()?;
    let prepared = Prepared { start, base, steps };
    let (adds, deletes) = prepared
        .steps
        .iter()
        .fold((0, 0), |(a, d), (sd, sa)| (a + sa.len(), d + sd.len()));
    counts.insert("selected_versions".into(), WINDOW as u64);
    counts.insert("window_start".into(), start as u64);
    counts.insert("window_adds".into(), adds as u64);
    counts.insert("window_deletes".into(), deletes as u64);
    counts.insert(
        "window_reappearances".into(),
        reappearances(&prepared) as u64,
    );
    counts.insert("triples_first_version".into(), prepared.base.len() as u64);
    counts.insert("triples_last_version".into(), prev.len() as u64);
    counts.insert("ic_lineage_divergence_in_window".into(), divergence);
    counts.insert("canonicalization_rewrites".into(), norm.rewritten);

    let extraction = Extraction {
        algorithm: EXTRACTION_ALGORITHM.into(),
        version: EXTRACTION_VERSION.into(),
        parameters: BTreeMap::from([
            ("granularity".into(), "day".into()),
            ("window".into(), WINDOW.to_string()),
            (
                "window_rule".into(),
                "max adds+deletes over consecutive versions; lowest start on ties".into(),
            ),
            ("max_ops_per_commit".into(), MAX_OPS_PER_COMMIT.to_string()),
            (
                "max_quad_bytes_per_commit".into(),
                MAX_BYTES_PER_COMMIT.to_string(),
            ),
            (
                "normalization".into(),
                "ledger_rdf::Quad canonical N-Quads (default graph)".into(),
            ),
        ]),
        range: format!(
            "day versions v{start}..=v{} (0-based TB numbering; IC files {:06}..{:06}) of v0..=v{}",
            start + WINDOW - 1,
            start + 1,
            start + WINDOW,
            versions - 1
        ),
        counts,
        blank_nodes_skolemized: 0,
    };
    let bytes = render_artifact(id, &extraction, &prepared);
    if bytes.len() as u64 > ARTIFACT_CAP {
        return Err(format!("prepared artifact exceeds {ARTIFACT_CAP} bytes"));
    }
    let path = artifact_path(ctx, id);
    std::fs::create_dir_all(path.parent().expect("has parent")).map_err(|e| e.to_string())?;
    let part = part_path(&path);
    std::fs::write(&part, &bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&part, &path).map_err(|e| e.to_string())?;
    Ok((sha256_hex(&bytes), bytes.len() as u64, extraction))
}

fn reappearances(p: &Prepared) -> usize {
    let mut gone: BTreeSet<&String> = BTreeSet::new();
    let mut n = 0;
    for (dels, adds) in &p.steps {
        n += adds.iter().filter(|a| gone.contains(a)).count();
        gone.extend(dels.iter());
    }
    n
}

fn render_artifact(id: &str, extraction: &Extraction, p: &Prepared) -> Vec<u8> {
    let header = ArtifactHeader {
        format: ARTIFACT_FORMAT.into(),
        dataset: id.into(),
        extraction: extraction.clone(),
    };
    let mut out = serde_json::to_string(&header).expect("serializable");
    out.push('\n');
    out.push_str(&format!("base v{} {}\n", p.start, p.base.len()));
    for l in &p.base {
        out.push_str(l);
        out.push('\n');
    }
    for (i, (dels, adds)) in p.steps.iter().enumerate() {
        out.push_str(&format!(
            "step v{} {} {}\n",
            p.start + i + 1,
            dels.len(),
            adds.len()
        ));
        for l in dels.iter().chain(adds) {
            out.push_str(l);
            out.push('\n');
        }
    }
    out.into_bytes()
}

fn parse_artifact(bytes: &[u8]) -> Result<(ArtifactHeader, Prepared), String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "artifact is not UTF-8")?;
    let mut lines = text.lines();
    let header: ArtifactHeader = serde_json::from_str(lines.next().ok_or("empty artifact")?)
        .map_err(|e| format!("artifact header: {e}"))?;
    if header.format != ARTIFACT_FORMAT || header.extraction.version != EXTRACTION_VERSION {
        return Err(format!(
            "artifact is {} / {}; this build reads {ARTIFACT_FORMAT} / {EXTRACTION_VERSION}: run `ledger-bench prepare` again",
            header.format, header.extraction.version
        ));
    }
    fn take(lines: &mut std::str::Lines<'_>, n: &str) -> Result<BTreeSet<String>, String> {
        let n: usize = n.parse().map_err(|_| format!("bad count {n:?}"))?;
        let set: BTreeSet<String> = (0..n)
            .map(|_| lines.next().map(str::to_owned).ok_or("truncated artifact"))
            .collect::<Result<_, _>>()?;
        if set.len() != n {
            return Err("duplicate statements in the artifact".into());
        }
        Ok(set)
    }
    let mut prepared: Option<Prepared> = None;
    while let Some(head) = lines.next() {
        match head.split(' ').collect::<Vec<_>>().as_slice() {
            ["base", v, n] => {
                let start = v
                    .strip_prefix('v')
                    .and_then(|v| v.parse().ok())
                    .ok_or("bad base header")?;
                let base = take(&mut lines, n)?;
                prepared = Some(Prepared {
                    start,
                    base,
                    steps: Vec::new(),
                });
            }
            ["step", _v, d, a] => {
                let dels = take(&mut lines, d)?;
                let adds = take(&mut lines, a)?;
                prepared
                    .as_mut()
                    .ok_or("step before base")?
                    .steps
                    .push((dels, adds));
            }
            _ => return Err(format!("unexpected artifact line {head:?}")),
        }
    }
    let prepared = prepared.ok_or("artifact has no base")?;
    if prepared.steps.len() != WINDOW - 1 {
        return Err("artifact does not hold the full window".into());
    }
    Ok((header, prepared))
}

/// Load the prepared artifact, verifying it against the committed manifest.
///
/// The committed manifest is read leniently (its source section must be complete). The
/// artifact is checked against its pinned hash and size whenever the manifest pins them, which
/// it always must for `validate`/`run`: `verify_manifest` loads it strictly. Only the
/// bootstrap `manifest` command ever sees it unpinned.
fn load(ctx: &Context, id: &str) -> Result<(Manifest, ArtifactHeader, Prepared, Vec<u8>), String> {
    let manifest = source_manifest(ctx, id)?;
    let path = artifact_path(ctx, id);
    let bytes = std::fs::read(&path).map_err(|_| {
        format!(
            "{id} is not prepared ({} missing): run `ledger-bench fetch {id}` and `ledger-bench prepare {id}` first",
            path.display()
        )
    })?;
    let digest = sha256_hex(&bytes);
    let pinned =
        manifest.output.artifact_sha256.is_some() || manifest.output.artifact_bytes.is_some();
    if pinned
        && (Some(&digest) != manifest.output.artifact_sha256.as_ref()
            || Some(bytes.len() as u64) != manifest.output.artifact_bytes)
    {
        return Err(format!(
            "prepared artifact {} is {} bytes sha256 {digest}; the manifest pins {:?} bytes sha256 {:?}: run `ledger-bench prepare {id}` again",
            path.display(),
            bytes.len(),
            manifest.output.artifact_bytes,
            manifest.output.artifact_sha256
        ));
    }
    let (header, prepared) = parse_artifact(&bytes)?;
    Ok((manifest, header, prepared, bytes))
}

/// Split one transition into commits under the per-commit operation and byte bounds:
/// deletes first, then adds; deterministic (sorted inputs).
fn chunks(dels: &BTreeSet<String>, adds: &BTreeSet<String>) -> Vec<(Vec<String>, Vec<String>)> {
    let mut out = Vec::new();
    let (mut d, mut a, mut bytes) = (Vec::new(), Vec::new(), 0usize);
    let ops: Vec<(bool, &String)> = dels
        .iter()
        .map(|q| (true, q))
        .chain(adds.iter().map(|q| (false, q)))
        .collect();
    for (is_delete, q) in ops {
        let full =
            d.len() + a.len() == MAX_OPS_PER_COMMIT || bytes + q.len() > MAX_BYTES_PER_COMMIT;
        if full && (!d.is_empty() || !a.is_empty()) {
            out.push((std::mem::take(&mut d), std::mem::take(&mut a)));
            bytes = 0;
        }
        bytes += q.len();
        if is_delete {
            d.push(q.clone());
        } else {
            a.push(q.clone());
        }
    }
    if !d.is_empty() || !a.is_empty() {
        out.push((d, a));
    }
    out
}

/// The workload: `main` ingests the first selected version (bulk commits), then one
/// version per transition. A version may span several commits; the last one carries the
/// version label `v<k>`, the version boundary whose state must equal the source version.
fn workload(p: &Prepared) -> Workload {
    let mut steps = Vec::new();
    let mut expected = BTreeMap::new();
    let mut state: BTreeSet<String> = BTreeSet::new();
    let (mut parent, mut depth, mut fold_ops): (Option<Label>, u32, u64) = (None, 0, 0);
    let mut versions = Vec::new();
    let empty = BTreeSet::new();
    let transitions = std::iter::once((p.start, &empty, &p.base)).chain(
        p.steps
            .iter()
            .enumerate()
            .map(|(i, (d, a))| (p.start + i + 1, d, a)),
    );
    for (version, dels, adds) in transitions {
        let parts = chunks(dels, adds);
        let n = parts.len();
        for (i, (d, a)) in parts.into_iter().enumerate() {
            let label: Label = if i + 1 == n {
                format!("v{version}")
            } else {
                format!("v{version}.{}", i + 1)
            };
            for q in &d {
                state.remove(q);
            }
            state.extend(a.iter().cloned());
            fold_ops += (d.len() + a.len()) as u64;
            let provenance = Provenance {
                activity: "benchmark-bear".into(),
                message: label.clone(),
                evidence_refs: vec![format!("urn:bench:bear-b:{label}")],
                source_system: Some("BEAR-B day (DBpedia Live)".into()),
            };
            let boundary = i + 1 == n;
            expected.insert(
                label.clone(),
                Expected {
                    parents: parent.iter().cloned().collect(),
                    depth,
                    fold_ops,
                    quads: state.len(),
                    digest: oracle_digest(state.iter().map(String::as_str)),
                    state: boundary.then(|| Arc::new(state.clone())),
                    kind: if version == p.start {
                        "bulk"
                    } else {
                        "version"
                    },
                    provenance: provenance.clone(),
                },
            );
            steps.push(Step::Commit(CommitStep {
                label: label.clone(),
                branch: "main".into(),
                parent: parent.clone(),
                adds: a,
                deletes: d,
                provenance,
            }));
            parent = Some(label.clone());
            depth += 1;
            if boundary {
                versions.push(label);
            }
        }
    }
    // Diffs: every adjacent pair and three wider gaps, each in both directions.
    let last = versions.len() - 1;
    let mut pairs: Vec<(Label, Label)> = versions
        .windows(2)
        .map(|w| (w[0].clone(), w[1].clone()))
        .collect();
    pairs.extend(
        [(0, last), (0, last / 2), (last / 2, last)]
            .map(|(a, b)| (versions[a].clone(), versions[b].clone())),
    );
    let diff_pairs = pairs
        .iter()
        .flat_map(|(a, b)| [(a.clone(), b.clone()), (b.clone(), a.clone())])
        .collect();
    Workload {
        steps,
        expected,
        final_heads: BTreeMap::from([("main".into(), versions[last].clone())]),
        diff_pairs,
        verify_all_history: true,
        history_facts: history_facts(p, &versions),
        redact_statements: true,
    }
}

/// Statements that appear, disappear and reappear in the window, as membership facts at
/// version boundaries (the oracle's set algebra over the source versions).
fn history_facts(p: &Prepared, versions: &[Label]) -> Vec<HistoryFact> {
    let mut states = vec![p.base.clone()];
    for (d, a) in &p.steps {
        let mut s = states.last().expect("base").clone();
        for q in d {
            s.remove(q);
        }
        s.extend(a.iter().cloned());
        states.push(s);
    }
    let mut out = Vec::new();
    // Reappearing: present at i, absent at j > i, present again at l > j.
    'outer: for (j, (dels, _)) in p.steps.iter().enumerate() {
        for q in dels {
            if let Some(l) = (j + 2..states.len()).find(|l| states[*l].contains(q)) {
                out.push(HistoryFact {
                    quad: q.clone(),
                    present: vec![versions[j].clone(), versions[l].clone()],
                    absent: (j + 1..l).map(|v| versions[v].clone()).collect(),
                });
                if out.len() == MAX_HISTORY_FACTS {
                    break 'outer;
                }
            }
        }
    }
    // Appearing (absent in the first version, present in the last) and disappearing.
    let first = &states[0];
    let last = states.last().expect("states");
    let n = versions.len() - 1;
    if let Some(q) = last.difference(first).next() {
        out.push(HistoryFact {
            quad: q.clone(),
            present: vec![versions[n].clone()],
            absent: vec![versions[0].clone()],
        });
    }
    if let Some(q) = first.difference(last).next() {
        out.push(HistoryFact {
            quad: q.clone(),
            present: vec![versions[0].clone()],
            absent: vec![versions[n].clone()],
        });
    }
    out
}

impl Dataset for BearB {
    fn id(&self) -> &'static str {
        self.id
    }

    fn prepare(&self, ctx: &Context) -> Result<Workload, String> {
        let (_, _, prepared, _) = load(ctx, self.id)?;
        Ok(workload(&prepared))
    }

    fn manifest(&self, ctx: &Context, w: &Workload) -> Result<Manifest, String> {
        let (committed, header, _, bytes) = load(ctx, self.id)?;
        Ok(Manifest {
            extraction: Some(header.extraction),
            output: Output {
                commits: w.commit_count(),
                workload_checksum: workload_checksum(w),
                artifact_sha256: Some(sha256_hex(&bytes)),
                artifact_bytes: Some(bytes.len() as u64),
            },
            ..committed
        })
    }
}

/// Remove cached data: the prepared artifact, and the downloaded sources too with `all`.
pub fn clean(ctx: &Context, id: &str, all: bool) -> Result<Vec<String>, String> {
    let mut removed = Vec::new();
    let mut remove = |p: &Path| -> Result<(), String> {
        if p.exists() {
            std::fs::remove_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
            removed.push(p.display().to_string());
        }
        Ok(())
    };
    remove(&ctx.cache.join(id).join("prepared"))?;
    if all {
        remove(&source_dir(ctx, id))?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn chunks_respect_operation_and_byte_bounds_deletes_first() {
        let dels: BTreeSet<String> = (0..3).map(|i| format!("d{i}")).collect();
        let adds: BTreeSet<String> = (0..MAX_OPS_PER_COMMIT + 2)
            .map(|i| format!("a{i:06}"))
            .collect();
        let c = chunks(&dels, &adds);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].0.len(), 3);
        assert_eq!(c[0].0.len() + c[0].1.len(), MAX_OPS_PER_COMMIT);
        assert_eq!(c[1].1.len(), 5);
        let big: BTreeSet<String> = (0..4)
            .map(|i| format!("{i}{}", "x".repeat(MAX_BYTES_PER_COMMIT / 3)))
            .collect();
        assert!(chunks(&BTreeSet::new(), &big).len() >= 2);
    }

    #[test]
    fn the_workload_reaches_each_source_version_at_its_boundary() {
        let p = Prepared {
            start: 3,
            base: set(&["<urn:a> <urn:p> \"1\" .", "<urn:b> <urn:p> \"1\" ."]),
            steps: vec![
                (
                    set(&["<urn:a> <urn:p> \"1\" ."]),
                    set(&["<urn:c> <urn:p> \"1\" ."]),
                ),
                (BTreeSet::new(), set(&["<urn:a> <urn:p> \"1\" ."])),
            ],
        };
        let w = workload(&p);
        assert_eq!(w.final_heads["main"], "v5");
        let v4 = &w.expected["v4"];
        assert_eq!(
            v4.state.as_deref(),
            Some(&set(&[
                "<urn:b> <urn:p> \"1\" .",
                "<urn:c> <urn:p> \"1\" ."
            ]))
        );
        // `<urn:a>` disappears at v4 and reappears at v5.
        assert!(
            w.history_facts
                .iter()
                .any(|f| f.quad == "<urn:a> <urn:p> \"1\" ."
                    && f.absent == ["v4"]
                    && f.present == ["v3", "v5"])
        );
        // Adjacent and wide pairs, both directions.
        assert!(w.diff_pairs.contains(&("v4".into(), "v3".into())));
        assert!(w.diff_pairs.contains(&("v3".into(), "v5".into())));
    }

    #[test]
    fn an_artifact_round_trips_and_a_tampered_one_is_refused() {
        let p = Prepared {
            start: 0,
            base: set(&["<urn:a> <urn:p> \"1\" ."]),
            steps: (0..WINDOW - 1)
                .map(|i| {
                    (
                        BTreeSet::new(),
                        set(&[&format!("<urn:s{i}> <urn:p> \"1\" .")]),
                    )
                })
                .collect(),
        };
        let x = Extraction {
            algorithm: EXTRACTION_ALGORITHM.into(),
            version: EXTRACTION_VERSION.into(),
            parameters: BTreeMap::new(),
            range: "r".into(),
            counts: BTreeMap::new(),
            blank_nodes_skolemized: 0,
        };
        let bytes = render_artifact("bear-b-ci", &x, &p);
        let (h, back) = parse_artifact(&bytes).unwrap();
        assert_eq!(h.extraction, x);
        assert_eq!((back.base, back.steps), (p.base.clone(), p.steps.clone()));
        let mut old = x.clone();
        old.version = "bear-b-day-extract/0".into();
        assert!(
            parse_artifact(&render_artifact("bear-b-ci", &old, &p))
                .unwrap_err()
                .contains("prepare")
        );
        // Structural truncation (a missing statement line) is refused by the parser; altered
        // content is refused by the artifact hash check in `load` (next test).
        let text = String::from_utf8(bytes.clone()).unwrap();
        let cut = text
            .trim_end_matches('\n')
            .rsplit_once('\n')
            .unwrap()
            .0
            .to_owned()
            + "\n";
        assert!(
            parse_artifact(cut.as_bytes())
                .unwrap_err()
                .contains("truncated")
        );
    }

    #[test]
    fn normalization_counts_blank_nodes_and_refuses_invalid_rdf() {
        let mut n = Normalizer::default();
        assert_eq!(
            n.canonical("<urn:a>   <urn:p> \"x\"@EN ."),
            Some("<urn:a> <urn:p> \"x\"@en .".into())
        );
        assert!(n.finish().is_ok());
        assert_eq!(n.canonical("_:b0 <urn:p> \"x\" ."), None);
        assert!(n.finish().unwrap_err().contains("skolemization"));
        let mut n = Normalizer::default();
        assert_eq!(n.canonical("<urn:a> <urn:p> ."), None);
        assert!(n.finish().unwrap_err().contains("do not parse"));
    }

    #[test]
    fn a_tampered_or_stale_prepared_artifact_is_refused_by_its_pinned_hash() {
        let dir = std::env::temp_dir().join(format!("ledger-bench-load-{}", std::process::id()));
        let manifests = dir.join("manifests");
        std::fs::create_dir_all(&manifests).unwrap();
        let mut m = crate::manifest::tests::extracted();
        m.dataset = "bear-b-ci".into();
        let ctx = Context {
            manifests: manifests.clone(),
            cache: dir.join("cache"),
        };
        let p = Prepared {
            start: 0,
            base: set(&["<urn:a> <urn:p> \"1\" ."]),
            steps: (0..WINDOW - 1)
                .map(|i| {
                    (
                        BTreeSet::new(),
                        set(&[&format!("<urn:s{i}> <urn:p> \"1\" .")]),
                    )
                })
                .collect(),
        };
        let x = m.extraction.clone().unwrap();
        let x = Extraction {
            version: EXTRACTION_VERSION.into(),
            ..x
        };
        let bytes = render_artifact("bear-b-ci", &x, &p);
        let path = artifact_path(&ctx, "bear-b-ci");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        m.output.artifact_sha256 = Some(sha256_hex(&bytes));
        m.output.artifact_bytes = Some(bytes.len() as u64);
        std::fs::write(
            manifests.join("bear-b-ci.json"),
            serde_json::to_vec(&m).unwrap(),
        )
        .unwrap();
        assert!(load(&ctx, "bear-b-ci").is_ok());
        let mut tampered = bytes.clone();
        let last = tampered.len() - 3;
        tampered[last] ^= 1;
        std::fs::write(&path, &tampered).unwrap();
        assert!(load(&ctx, "bear-b-ci").unwrap_err().contains("prepare"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cache_names_are_role_prefixed_and_reject_odd_urls() {
        assert_eq!(
            cached_name("changesets", "https://x/day/CB/alldata.CB.nt.tar.gz").unwrap(),
            "changesets--alldata.CB.nt.tar.gz"
        );
        assert!(cached_name("x", "https://x/a/../..").is_err());
        assert!(cached_name("x", "https://x/").is_err());
        assert!(cached_name("x", "https://x/.hidden").is_err());
    }
}
