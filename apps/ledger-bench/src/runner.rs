//! Executes a [`Workload`] against a running ledger and checks every expectation.
//!
//! Every result labels the path it used:
//! - `api`: the public HTTP API of the production-shaped stack (prepare, accept, state read,
//!   ref read, branch create and log, merge preview/propose/apply). All writes go through it.
//! - `persisted`: owner-identity reads of the production store.
//!   - Graph provisioning uses the operator path that `ledger-admin graph create` uses
//!     (`PgGraphs`).
//!   - After the run, commit envelopes are read through `ImmutableStore::get_commit`, which
//!     re-verifies each commit's hash, to check parents and provenance. The public API has no
//!     commit endpoint.
//! - `algorithm`: an infrastructure-free ledger crate (`ledger_rdf::diff`) run on states the
//!   ledger materialized. The public API has no diff endpoint, so this applies the
//!   production diff to ledger-reconstructed states and compares it with the oracle's set
//!   difference.
//!
//! The oracle side uses a ledger crate exactly once, and labels it: preview
//! `merged_state_digest` values are compared with `ledger_rdf::state_digest` of the
//! oracle's expected merged state. That is the frozen `sculpin-rdf-state/v1` protocol
//! function, golden-pinned and independently re-implemented in
//! `scripts/golden/state_v1_reference.py`.
//!
//! The checks themselves are pure functions ([`state_mismatch`], [`preview_checks`]), so
//! unit tests can feed them wrong ledger replies. Correctness failures accumulate, with
//! bounded detail. A failure that makes later steps meaningless (a write the ledger refused)
//! ends the run.

use crate::{
    result::{Correctness, Failure, SeriesPoint},
    workload::{
        CommitStep, Expected, Label, MergeStep, PreviewExpect, State, Step, Workload,
        first_parent_chain, oracle_digest,
    },
};
use ledger_core::{AnyCommit, CommitId, GraphId, ImmutableStore, TenantId};
use ledger_rdf::Quad;
use ledger_store::{GraphStatus, NewGraph, PgGraphs, PostgresImmutableStore, V1Binding};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
    time::{Duration, Instant},
};

pub const TENANT: &str = "tenant-bench";
const ROLES: [&str; 3] = ["ledger.read", "ledger.propose", "ledger.review"];
const MAX_FAILURES: usize = 50;

pub struct Config {
    pub replica: String,
    pub owner_database_url: String,
    pub issuer: String,
    pub audience: String,
    pub secret: String,
    /// Distinguishes graphs and idempotency keys of repeated runs on one database.
    pub run_id: String,
}

/// One timed operation.
pub struct Sample {
    pub category: &'static str,
    pub op: &'static str,
    pub kind: &'static str,
    pub depth: Option<u32>,
    pub quads: Option<usize>,
    pub fold_ops: Option<u64>,
    /// Response body bytes (API samples).
    pub bytes: Option<usize>,
    pub micros: u64,
}

pub struct RunData {
    pub correctness: Correctness,
    pub samples: Vec<Sample>,
    pub phases_ms: BTreeMap<String, u128>,
    pub db_bytes_before: Option<i64>,
    pub db_bytes_after: Option<i64>,
    pub immutable_objects_bytes_after: Option<i64>,
    pub immutable_objects_logical_bytes_after: Option<i64>,
    pub postgres_version: String,
    /// The run could not complete (harness or environment error, or an aborted step).
    pub aborted: Option<String>,
}

// ---- pure checks ------------------------------------------------------------------------

/// `ledger_materialized_state(C) == expected_state(C)` on the raw API list: strings only,
/// strictly ascending (canonical order, no duplicates), then count and the oracle digest.
/// `None` when it holds; otherwise a diagnostic.
pub fn state_mismatch(e: &Expected, raw: &[Value]) -> Option<String> {
    let mut lines = Vec::with_capacity(raw.len());
    for q in raw {
        match q.as_str() {
            Some(s) => lines.push(s),
            None => return Some(format!("non-string entry in the state: {q}")),
        }
    }
    if let Some(w) = lines.windows(2).find(|w| w[0] >= w[1]) {
        return Some(format!(
            "state not strictly ascending (duplicate or out of order): {:?} then {:?}",
            w[0], w[1]
        ));
    }
    let digest = oracle_digest(lines.iter().copied());
    if lines.len() == e.quads && digest == e.digest {
        return None;
    }
    let mut detail = format!(
        "ledger {} quads ({}), oracle {} quads ({})",
        lines.len(),
        &digest[..12],
        e.quads,
        &e.digest[..12]
    );
    if let Some(full) = &e.state {
        let actual: BTreeSet<&str> = lines.iter().copied().collect();
        let missing: Vec<&String> = full
            .iter()
            .filter(|q| !actual.contains(q.as_str()))
            .take(3)
            .collect();
        let extra: Vec<&&str> = actual
            .iter()
            .filter(|q| !full.contains(**q))
            .take(3)
            .collect();
        detail.push_str(&format!("; missing {missing:?}; unexpected {extra:?}"));
    }
    Some(detail)
}

/// `sculpin-rdf-state/v1` of an expected state (the labelled oracle-side protocol use).
fn expected_protocol_digest(state: &BTreeSet<String>) -> Result<String, String> {
    let quads: BTreeSet<Quad> = state
        .iter()
        .map(|q| q.parse::<Quad>().map_err(|e| format!("{q}: {e}")))
        .collect::<Result<_, _>>()?;
    Ok(ledger_rdf::state_digest(&quads).to_string())
}

/// Every check of one preview reply: `(family, ok, detail)`. `ids` maps labels to the commit
/// ids the ledger created.
pub fn preview_checks(
    e: &PreviewExpect,
    v: &Value,
    ids: &BTreeMap<Label, CommitId>,
) -> Vec<(&'static str, bool, String)> {
    let id = |l: &Label| ids.get(l).map(ToString::to_string);
    let mut out = Vec::new();
    let class = v["classification"].as_str().unwrap_or("?");
    out.push((
        "merge classification",
        class == e.classification,
        format!("ledger {class}, oracle {}", e.classification),
    ));
    let base = v["merge_base"].as_str().map(str::to_owned);
    let expected_base = e.merge_base.as_ref().and_then(id);
    out.push((
        "merge base",
        base == expected_base,
        format!(
            "ledger {base:?}, oracle {:?} = {expected_base:?}",
            e.merge_base
        ),
    ));
    let mut candidates: Vec<String> = v["merge_base_candidates"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| c.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let mut expected_candidates: Vec<String> = e.base_candidates.iter().filter_map(id).collect();
    expected_candidates.sort();
    let ascending = candidates.windows(2).all(|w| w[0] < w[1]);
    candidates.sort();
    let count = v["merge_base_candidate_count"].as_u64();
    let expected_count = (!e.base_candidates.is_empty()).then_some(e.base_candidates.len() as u64);
    out.push((
        "merge base candidates",
        candidates == expected_candidates && ascending && count == expected_count,
        format!(
            "ledger {candidates:?} (ascending {ascending}, count {count:?}), oracle {:?}",
            e.base_candidates
        ),
    ));
    let ahead_behind = (v["ahead"].as_u64(), v["behind"].as_u64());
    out.push((
        "merge ahead/behind",
        ahead_behind == (Some(e.ahead as u64), Some(e.behind as u64)),
        format!(
            "ledger {ahead_behind:?}, oracle ({}, {})",
            e.ahead, e.behind
        ),
    ));
    let count = v["conflict_count"].as_u64();
    out.push((
        "merge conflict count",
        count == Some(e.conflict_count as u64),
        format!("ledger {count:?}, oracle {}", e.conflict_count),
    ));
    let keys: Vec<(Option<String>, String, String)> = v["conflicts"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|c| {
                    (
                        c["graph"].as_str().map(str::to_owned),
                        c["subject"].as_str().unwrap_or("").to_owned(),
                        c["predicate"].as_str().unwrap_or("").to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let expected_keys = if matches!(
        e.classification,
        "already_equal" | "already_contained" | "ambiguous_merge_base"
    ) {
        Vec::new()
    } else {
        e.conflict_keys.clone()
    };
    out.push((
        "merge conflict keys",
        keys == expected_keys,
        format!("ledger {keys:?}, oracle {expected_keys:?}"),
    ));
    for (side, expected) in [
        ("target_delta", e.target_delta),
        ("source_delta", e.source_delta),
    ] {
        let Some((adds, deletes, keys)) = expected else {
            continue;
        };
        let d = &v[side];
        let got = (
            d["adds"].as_u64(),
            d["deletes"].as_u64(),
            d["affected_keys"].as_u64(),
        );
        out.push((
            "merge delta summary",
            got == (Some(adds as u64), Some(deletes as u64), Some(keys as u64)),
            format!("{side}: ledger {got:?}, oracle ({adds}, {deletes}, {keys})"),
        ));
    }
    let has_token = v["preview_token"].is_string();
    let candidate = matches!(e.classification, "fast_forward" | "divergent");
    out.push((
        "merge preview token presence",
        has_token == candidate,
        format!("token present {has_token}, candidate class {candidate}"),
    ));
    if let Some(merged) = &e.merged {
        let got = v["merged_state_digest"].as_str().map(str::to_owned);
        let (ok, detail) = match expected_protocol_digest(merged) {
            Ok(expected) => (
                got.as_deref() == Some(expected.as_str()),
                format!("ledger {got:?}, oracle {expected}"),
            ),
            Err(e) => (false, format!("expected merged state does not parse: {e}")),
        };
        out.push(("merged state digest", ok, detail));
    }
    out
}

// ---- the runner -------------------------------------------------------------------------

struct Runner<'a> {
    w: &'a Workload,
    cfg: &'a Config,
    http: reqwest::Client,
    token: String,
    graph: String,
    ids: BTreeMap<Label, CommitId>,
    /// Current head label of every branch (for attributing samples).
    heads: BTreeMap<String, Label>,
    correctness: Correctness,
    samples: Vec<Sample>,
    keys: u64,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs()
}

fn token(cfg: &Config) -> String {
    let claims = json!({
        "iss": cfg.issuer, "aud": cfg.audience, "exp": now_secs() + 6 * 3600, "nbf": now_secs() - 30,
        "tid": TENANT, "oid": "ledger-bench", "sculpin_principal_type": "agent", "roles": ROLES,
    });
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(cfg.secret.as_bytes()),
    )
    .expect("HS256 encoding of static claims")
}

/// Status, decoded body, elapsed (request sent → body decoded), body bytes.
type Reply = (u16, Value, Duration, usize);

impl Runner<'_> {
    fn check(&mut self, family: &str, subject: &str, ok: bool, detail: impl FnOnce() -> String) {
        self.correctness.assertions += 1;
        *self
            .correctness
            .checks
            .entry(family.to_owned())
            .or_default() += 1;
        if !ok {
            self.correctness.failed += 1;
            if self.correctness.failures.len() < MAX_FAILURES {
                self.correctness.failures.push(Failure {
                    check: family.to_owned(),
                    subject: subject.to_owned(),
                    detail: detail(),
                });
            }
        }
    }

    fn key(&mut self) -> String {
        self.keys += 1;
        format!("bench-{}-{}", self.cfg.run_id, self.keys)
    }

    async fn call(
        &mut self,
        method: reqwest::Method,
        path: &str,
        key: Option<String>,
        body: Option<&Value>,
    ) -> Result<Reply, String> {
        let url = format!("{}{path}", self.cfg.replica);
        let mut request = self
            .http
            .request(method, url)
            .header("authorization", format!("Bearer {}", self.token));
        if let Some(key) = key {
            request = request.header("idempotency-key", key);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let started = Instant::now();
        let response = request.send().await.map_err(|e| format!("{path}: {e}"))?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await.map_err(|e| format!("{path}: {e}"))?;
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Ok((status, value, started.elapsed(), bytes.len()))
    }

    fn sample(
        &mut self,
        category: &'static str,
        op: &'static str,
        label: Option<&Label>,
        elapsed: Duration,
        bytes: Option<usize>,
    ) {
        let e = label.and_then(|l| self.w.expected.get(l));
        self.samples.push(Sample {
            category,
            op,
            kind: e.map_or("-", |e| e.kind),
            depth: e.map(|e| e.depth),
            quads: e.map(|e| e.quads),
            fold_ops: e.map(|e| e.fold_ops),
            bytes,
            micros: u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
        });
    }

    fn id(&self, label: &Label) -> Result<&CommitId, String> {
        self.ids
            .get(label)
            .ok_or_else(|| format!("no commit id for {label}"))
    }

    /// Read a state through the API and check it against the oracle; returns the raw list.
    async fn read_and_check(
        &mut self,
        family: &'static str,
        label: &Label,
        op: &'static str,
    ) -> Result<Vec<Value>, String> {
        let id = self.id(label)?.clone();
        let path = format!("/v1/graphs/{}/commits/{id}/state", self.graph);
        let (status, v, t, bytes) = self.call(reqwest::Method::GET, &path, None, None).await?;
        if status != 200 {
            return Err(format!("state of {label}: HTTP {status} {v}"));
        }
        self.sample("api", op, Some(label), t, Some(bytes));
        let raw = v["quads"].as_array().cloned().unwrap_or_default();
        let mismatch = state_mismatch(&self.w.expected[label], &raw);
        self.check(family, label, mismatch.is_none(), || {
            mismatch.unwrap_or_default()
        });
        Ok(raw)
    }

    async fn commit(&mut self, c: &CommitStep) -> Result<(), String> {
        let expected_head = match &c.parent {
            Some(p) => Value::String(self.id(p)?.to_string()),
            None => Value::Null,
        };
        let operations: Vec<Value> = c
            .deletes
            .iter()
            .map(|q| json!({"op": "delete", "quad": q}))
            .chain(c.adds.iter().map(|q| json!({"op": "add", "quad": q})))
            .collect();
        let body = json!({
            "ref": c.branch, "expected_head": expected_head, "operations": operations,
            "activity": c.provenance.activity, "message": c.provenance.message,
            "evidence_refs": c.provenance.evidence_refs, "source_system": c.provenance.source_system,
        });
        let key = self.key();
        let path = format!("/v1/graphs/{}/proposals", self.graph);
        let (status, v, t, bytes) = self
            .call(reqwest::Method::POST, &path, Some(key), Some(&body))
            .await?;
        if status != 201 {
            return Err(format!("prepare {}: HTTP {status} {v}", c.label));
        }
        self.sample("api", "prepare", Some(&c.label), t, Some(bytes));
        let candidate = v["candidate"]
            .as_str()
            .ok_or("prepare without candidate")?
            .to_owned();
        let body = json!({"ref": c.branch, "expected_head": expected_head, "reason": "benchmark"});
        let key = self.key();
        let path = format!("/v1/graphs/{}/proposals/{candidate}/accept", self.graph);
        let (status, v, t, bytes) = self
            .call(reqwest::Method::POST, &path, Some(key), Some(&body))
            .await?;
        if status != 200 {
            return Err(format!("accept {}: HTTP {status} {v}", c.label));
        }
        self.sample("api", "accept", Some(&c.label), t, Some(bytes));
        let id = CommitId::from_str(&candidate).map_err(|e| e.to_string())?;
        self.ids.insert(c.label.clone(), id);
        self.heads.insert(c.branch.clone(), c.label.clone());
        self.read_and_check("state after commit", &c.label, "state_read_head")
            .await?;
        Ok(())
    }

    async fn branch(&mut self, name: &str, source: &str, from: &Label) -> Result<(), String> {
        let body =
            json!({"name": name, "source": source, "from_commit": self.id(from)?.to_string()});
        let key = self.key();
        let path = format!("/v1/graphs/{}/branches", self.graph);
        let (status, v, t, bytes) = self
            .call(reqwest::Method::POST, &path, Some(key), Some(&body))
            .await?;
        if status != 201 {
            return Err(format!("create branch {name} at {from}: HTTP {status} {v}"));
        }
        self.sample("api", "branch_create", Some(from), t, Some(bytes));
        self.heads.insert(name.to_owned(), from.clone());
        Ok(())
    }

    fn merge_body(
        &self,
        m: &MergeStep,
        strategy: &str,
        base: Option<&Label>,
    ) -> Result<Value, String> {
        let mut body = json!({"source": m.source, "target": m.target, "strategy": strategy});
        if let Some(b) = base {
            body["base"] = Value::String(self.id(b)?.to_string());
        }
        Ok(body)
    }

    async fn merge(&mut self, m: &MergeStep) -> Result<(), String> {
        let path = format!("/v1/graphs/{}/merges/preview", self.graph);
        let mut tokens = BTreeMap::new();
        for e in &m.previews {
            let body = self.merge_body(m, e.strategy, e.explicit_base.as_ref())?;
            let (status, v, t, bytes) = self
                .call(reqwest::Method::POST, &path, None, Some(&body))
                .await?;
            if status != 200 {
                return Err(format!("preview {}: HTTP {status} {v}", m.id));
            }
            let target_head = self.heads.get(&m.target).cloned();
            self.sample("api", "merge_preview", target_head.as_ref(), t, Some(bytes));
            let subject = format!(
                "{} ({} → {}, {}, base {:?})",
                m.id, m.source, m.target, e.strategy, e.explicit_base
            );
            for (family, ok, detail) in preview_checks(e, &v, &self.ids) {
                self.check(family, &subject, ok, || detail);
            }
            if let Some(token) = v["preview_token"].as_str() {
                tokens.insert((e.strategy, e.explicit_base.clone()), token.to_owned());
            }
        }
        let Some(apply) = &m.apply else {
            return Ok(());
        };
        let token = tokens
            .get(&(apply.strategy, apply.explicit_base.clone()))
            .ok_or_else(|| format!("{}: no preview token for {}", m.id, apply.strategy))?
            .clone();
        let mut body = self.merge_body(m, apply.strategy, apply.explicit_base.as_ref())?;
        body["preview_token"] = Value::String(token.clone());
        body["message"] = Value::String(apply.provenance.message.clone());
        body["evidence_refs"] = json!(apply.provenance.evidence_refs);
        let key = self.key();
        let path = format!("/v1/graphs/{}/merges/propose", self.graph);
        let (status, v, t, bytes) = self
            .call(reqwest::Method::POST, &path, Some(key), Some(&body))
            .await?;
        if status != 201 {
            return Err(format!("propose {}: HTTP {status} {v}", m.id));
        }
        self.sample("api", "merge_propose", Some(&apply.label), t, Some(bytes));
        let candidate = v["candidate"]
            .as_str()
            .ok_or("propose without candidate")?
            .to_owned();
        let proposal = v["proposal_id"]
            .as_i64()
            .ok_or("propose without proposal")?;
        let body = json!({"proposal_id": proposal, "preview_token": token, "reason": "benchmark"});
        let key = self.key();
        let path = format!("/v1/graphs/{}/merges/apply", self.graph);
        let (status, v, t, bytes) = self
            .call(reqwest::Method::POST, &path, Some(key), Some(&body))
            .await?;
        if status != 200 {
            return Err(format!("apply {}: HTTP {status} {v}", m.id));
        }
        self.sample("api", "merge_apply", Some(&apply.label), t, Some(bytes));
        self.check(
            "merge apply moves the target to the candidate",
            &m.id,
            v["head"].as_str() == Some(candidate.as_str()),
            || format!("apply head {}, candidate {candidate}", v["head"]),
        );
        let id = CommitId::from_str(&candidate).map_err(|e| e.to_string())?;
        self.ids.insert(apply.label.clone(), id);
        self.heads.insert(m.target.clone(), apply.label.clone());
        self.read_and_check(
            "state after merge (reconstruct(I) == merged)",
            &apply.label,
            "state_read_head",
        )
        .await?;
        Ok(())
    }

    async fn verify_heads_and_logs(&mut self) -> Result<(), String> {
        for (branch, label) in self.w.final_heads.clone() {
            let path = format!("/v1/graphs/{}/refs?name={branch}", self.graph);
            let (status, v, t, bytes) = self.call(reqwest::Method::GET, &path, None, None).await?;
            self.sample("api", "ref_read", Some(&label), t, Some(bytes));
            let expected = self.id(&label)?.to_string();
            self.check(
                "branch head",
                &branch,
                status == 200 && v["head"].as_str() == Some(expected.as_str()),
                || {
                    format!(
                        "HTTP {status}, ledger {}, oracle {label} = {expected}",
                        v["head"]
                    )
                },
            );
            let chain = first_parent_chain(self.w, &label);
            let limit = chain.len().min(1_000);
            let path = format!(
                "/v1/graphs/{}/branches/log?name={branch}&limit={limit}",
                self.graph
            );
            let (status, v, t, bytes) = self.call(reqwest::Method::GET, &path, None, None).await?;
            self.sample("api", "branch_log", Some(&label), t, Some(bytes));
            let got: Vec<String> = v["commits"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|c| c.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let expected: Vec<String> = chain
                .iter()
                .take(limit)
                .map(|l| self.ids.get(l).map(ToString::to_string).unwrap_or_default())
                .collect();
            self.check(
                "first-parent history (branch log)",
                &branch,
                status == 200 && got == expected,
                || {
                    format!(
                        "HTTP {status}; ledger {} entries, oracle {}",
                        got.len(),
                        expected.len()
                    )
                },
            );
        }
        Ok(())
    }

    async fn verify_history(&mut self) -> Result<(), String> {
        let labels: Vec<Label> = self
            .w
            .expected
            .iter()
            .filter(|(_, e)| self.w.verify_all_history || e.state.is_some())
            .map(|(l, _)| l.clone())
            .collect();
        for label in labels {
            self.read_and_check("historical reconstruction", &label, "state_read_historical")
                .await?;
        }
        Ok(())
    }

    async fn verify_diffs(&mut self) -> Result<(), String> {
        for (a, b) in self.w.diff_pairs.clone() {
            let ra = self
                .read_and_check("historical reconstruction", &a, "state_read_historical")
                .await?;
            let rb = self
                .read_and_check("historical reconstruction", &b, "state_read_historical")
                .await?;
            let parse = |raw: &[Value]| -> Result<BTreeSet<Quad>, String> {
                raw.iter()
                    .map(|q| {
                        q.as_str()
                            .ok_or("non-string quad")?
                            .parse::<Quad>()
                            .map_err(|e| e.to_string())
                    })
                    .collect()
            };
            let (qa, qb) = (parse(&ra)?, parse(&rb)?);
            let started = Instant::now();
            let d = ledger_rdf::diff(&qa, &qb);
            self.sample("algorithm", "rdf_diff", Some(&b), started.elapsed(), None);
            let adds: BTreeSet<String> = d.adds.iter().map(|q| q.as_str().to_owned()).collect();
            let deletes: BTreeSet<String> =
                d.deletes.iter().map(|q| q.as_str().to_owned()).collect();
            let (ea, eb): (State, State) = (
                self.w.expected[&a]
                    .state
                    .clone()
                    .ok_or("diff pair not retained")?,
                self.w.expected[&b]
                    .state
                    .clone()
                    .ok_or("diff pair not retained")?,
            );
            let expected_adds: BTreeSet<String> = eb.difference(&ea).cloned().collect();
            let expected_deletes: BTreeSet<String> = ea.difference(&eb).cloned().collect();
            let subject = format!("{a} → {b}");
            self.check(
                "diff(A, B) == expected",
                &subject,
                adds == expected_adds && deletes == expected_deletes,
                || {
                    format!(
                        "ledger +{} −{}, oracle +{} −{}",
                        adds.len(),
                        deletes.len(),
                        expected_adds.len(),
                        expected_deletes.len()
                    )
                },
            );
        }
        Ok(())
    }

    /// Parents and provenance of every commit, from the persisted envelopes.
    async fn verify_persisted(&mut self, store: &PostgresImmutableStore) -> Result<(), String> {
        let graph = GraphId::new(self.graph.clone()).map_err(|e| e.to_string())?;
        for (label, e) in self.w.expected.clone() {
            let id = self.id(&label)?.clone();
            let started = Instant::now();
            let commit = store.get_commit(&id).await.map_err(|e| e.to_string())?;
            self.sample(
                "persisted",
                "commit_read",
                Some(&label),
                started.elapsed(),
                None,
            );
            let Some(AnyCommit::V2(c)) = commit else {
                self.check("persisted commit", &label, false, || {
                    "missing or not v2".into()
                });
                continue;
            };
            let parents: Vec<String> = c.parents.iter().map(ToString::to_string).collect();
            let expected: Vec<String> = e
                .parents
                .iter()
                .map(|p| self.ids.get(p).map(ToString::to_string).unwrap_or_default())
                .collect();
            self.check("commit parents", &label, parents == expected, || {
                format!("ledger {parents:?}, oracle {:?}", e.parents)
            });
            let mut refs = e.provenance.evidence_refs.clone();
            refs.sort();
            refs.dedup();
            let ok = c.graph_id == graph
                && c.activity == e.provenance.activity
                && c.message == e.provenance.message
                && c.evidence_refs == refs
                && c.source_system == e.provenance.source_system;
            self.check("commit provenance", &label, ok, || {
                format!(
                    "ledger ({}, {}, {:?}, {:?}), oracle ({}, {}, {:?}, {:?})",
                    c.activity,
                    c.message,
                    c.evidence_refs,
                    c.source_system,
                    e.provenance.activity,
                    e.provenance.message,
                    refs,
                    e.provenance.source_system
                )
            });
        }
        Ok(())
    }
}

async fn scalar(pool: &PgPool, sql: &str) -> Option<i64> {
    sqlx::query_scalar::<_, i64>(sql).fetch_one(pool).await.ok()
}

/// Run one dataset workload on a fresh graph.
pub async fn run(w: &Workload, dataset: &str, cfg: &Config) -> RunData {
    let mut phases = BTreeMap::new();
    let mut data = RunData {
        correctness: Correctness::default(),
        samples: Vec::new(),
        phases_ms: BTreeMap::new(),
        db_bytes_before: None,
        db_bytes_after: None,
        immutable_objects_bytes_after: None,
        immutable_objects_logical_bytes_after: None,
        postgres_version: "unknown".into(),
        aborted: None,
    };
    let pool = match PgPool::connect(&cfg.owner_database_url).await {
        Ok(p) => p,
        Err(e) => {
            data.aborted = Some(format!("owner database: {e}"));
            return data;
        }
    };
    data.postgres_version = sqlx::query_scalar::<_, String>("SHOW server_version")
        .fetch_one(&pool)
        .await
        .unwrap_or_else(|_| "unknown".into());
    data.db_bytes_before = scalar(&pool, "SELECT pg_database_size(current_database())").await;
    let store =
        match PostgresImmutableStore::connect(&cfg.owner_database_url, V1Binding::Reject).await {
            Ok(s) => s,
            Err(e) => {
                data.aborted = Some(format!("immutable store: {e}"));
                return data;
            }
        };
    let graph = format!("bench-{dataset}-{}", cfg.run_id);
    let created = PgGraphs::new(pool.clone())
        .create(&NewGraph {
            graph_id: GraphId::new(graph.clone()).expect("valid graph id"),
            tenant_id: TenantId::new(TENANT).expect("valid tenant"),
            knowledge_base_id: None,
            purpose: Some(format!("ledger-bench {dataset}")),
            status: GraphStatus::Active,
        })
        .await;
    if let Err(e) = created {
        data.aborted = Some(format!("graph provisioning: {e}"));
        return data;
    }
    let mut r = Runner {
        w,
        cfg,
        // Loopback only: never route the benchmark through an environment proxy.
        http: reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(120))
            .build()
            .expect("http client"),
        token: token(cfg),
        graph,
        ids: BTreeMap::new(),
        heads: BTreeMap::new(),
        correctness: Correctness::default(),
        samples: Vec::new(),
        keys: 0,
    };
    let outcome: Result<(), String> = async {
        let started = Instant::now();
        for step in &w.steps {
            match step {
                Step::Commit(c) => r.commit(c).await?,
                Step::Branch { name, source, from } => r.branch(name, source, from).await?,
                Step::Merge(m) => r.merge(m).await?,
            }
        }
        phases.insert(
            "ingest and per-step checks".to_owned(),
            started.elapsed().as_millis(),
        );
        let started = Instant::now();
        r.verify_heads_and_logs().await?;
        r.verify_history().await?;
        phases.insert(
            "historical reconstruction checks".to_owned(),
            started.elapsed().as_millis(),
        );
        let started = Instant::now();
        r.verify_diffs().await?;
        phases.insert("diff checks".to_owned(), started.elapsed().as_millis());
        let started = Instant::now();
        r.verify_persisted(&store).await?;
        phases.insert(
            "persisted parents and provenance".to_owned(),
            started.elapsed().as_millis(),
        );
        Ok(())
    }
    .await;
    data.aborted = outcome.err();
    data.db_bytes_after = scalar(&pool, "SELECT pg_database_size(current_database())").await;
    data.immutable_objects_bytes_after =
        scalar(&pool, "SELECT pg_total_relation_size('immutable_objects')").await;
    data.immutable_objects_logical_bytes_after = scalar(
        &pool,
        "SELECT COALESCE(sum(octet_length(bytes)), 0)::bigint FROM immutable_objects",
    )
    .await;
    data.correctness = r.correctness;
    data.samples = r.samples;
    data.phases_ms = phases;
    data
}

/// Raw series of the depth-sensitive operations.
pub fn series(samples: &[Sample]) -> Vec<SeriesPoint> {
    samples
        .iter()
        .filter(|s| {
            matches!(
                s.op,
                "prepare" | "state_read_head" | "state_read_historical" | "merge_preview"
            )
        })
        .filter_map(|s| {
            Some(SeriesPoint {
                op: s.op.into(),
                kind: s.kind.into(),
                depth: s.depth?,
                quads: s.quads?,
                fold_ops: s.fold_ops?,
                bytes: s.bytes,
                ms: s.micros as f64 / 1000.0,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workload::Provenance;
    use std::sync::Arc;

    fn expected(lines: &[&str]) -> Expected {
        let set: BTreeSet<String> = lines.iter().map(|s| (*s).to_owned()).collect();
        Expected {
            parents: vec![],
            depth: 0,
            fold_ops: 0,
            quads: set.len(),
            digest: oracle_digest(set.iter().map(String::as_str)),
            state: Some(Arc::new(set)),
            kind: "main",
            provenance: Provenance {
                activity: String::new(),
                message: String::new(),
                evidence_refs: vec![],
                source_system: None,
            },
        }
    }

    const A: &str = "<urn:a> <urn:p> \"1\" .";
    const B: &str = "<urn:b> <urn:p> \"1\" .";
    const C: &str = "<urn:c> <urn:p> \"1\" .";

    #[test]
    fn a_wrong_state_reply_is_a_failure() {
        let e = expected(&[A, B]);
        assert!(state_mismatch(&e, &[json!(A), json!(B)]).is_none());
        for (reply, why) in [
            (vec![json!(A)], "missing"),
            (vec![json!(A), json!(B), json!(C)], "unexpected"),
            (vec![json!(A), json!(C)], "missing"),
            (vec![json!(B), json!(A)], "ascending"),
            (vec![json!(A), json!(A), json!(B)], "ascending"),
            (vec![json!(A), json!(1)], "non-string"),
        ] {
            let m = state_mismatch(&e, &reply).expect("must fail");
            assert!(m.contains(why), "{why}: {m}");
        }
    }

    fn preview(classification: &'static str) -> PreviewExpect {
        PreviewExpect {
            strategy: "abort",
            explicit_base: None,
            classification,
            base_candidates: vec![],
            merge_base: None,
            ahead: 1,
            behind: 2,
            conflict_count: 0,
            conflict_keys: vec![],
            target_delta: None,
            source_delta: None,
            merged: None,
        }
    }

    fn failed(checks: &[(&'static str, bool, String)]) -> Vec<&'static str> {
        checks.iter().filter(|c| !c.1).map(|c| c.0).collect()
    }

    #[test]
    fn a_wrong_preview_reply_is_a_failure() {
        let ids = BTreeMap::new();
        let e = preview("already_contained");
        let good = json!({"classification": "already_contained", "ahead": 1, "behind": 2,
            "conflict_count": 0, "conflicts": []});
        assert!(failed(&preview_checks(&e, &good, &ids)).is_empty());
        let mut wrong = good.clone();
        wrong["classification"] = json!("fast_forward");
        wrong["preview_token"] = json!("sha256:00");
        assert_eq!(
            failed(&preview_checks(&e, &wrong, &ids)),
            ["merge classification", "merge preview token presence"]
        );
        let mut wrong = good.clone();
        wrong["behind"] = json!(3);
        assert_eq!(
            failed(&preview_checks(&e, &wrong, &ids)),
            ["merge ahead/behind"]
        );
        // A merged state is expected: a missing or different digest fails.
        let mut e = preview("divergent");
        e.merged = Some(Arc::new(BTreeSet::from([A.to_owned()])));
        let reply = json!({"classification": "divergent", "ahead": 1, "behind": 2,
            "conflict_count": 0, "conflicts": [], "preview_token": "sha256:00"});
        assert_eq!(
            failed(&preview_checks(&e, &reply, &ids)),
            ["merged state digest"]
        );
        let mut reply = reply;
        reply["merged_state_digest"] =
            json!(expected_protocol_digest(&BTreeSet::from([A.to_owned()])).unwrap());
        assert!(failed(&preview_checks(&e, &reply, &ids)).is_empty());
    }
}
