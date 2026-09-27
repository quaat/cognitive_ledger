//! The accepted-state projector (ADR-0020, ADR-0021; Plan 0007).
//!
//! One step: lease a stream ([`ProjectionRepository::claim`], committed), observe the
//! target, decide with [`ledger_projection::plan`], reconstruct the accepted state and write it
//! with one **guarded** target transaction when needed, read the marker back, and acknowledge
//! under the lease — no database transaction is ever held across a target request. Every
//! crash window is a [`FailPoint`] the tests drive; each has a defined recovery (lease expiry,
//! then the marker tells the next worker what the target holds). Idle streams are
//! re-observed periodically (reconciliation), so a target that lost its data is repaired
//! without waiting for a new acceptance.

pub mod metrics;

use ledger_core::LedgerError;
use ledger_projection::{
    CognitiveGraph, ErrorClass, LedgerView, MarkerRead, Observation, Plan, ProjectedState,
    ProjectionClient, ProjectionError, ProjectionErrorCode as Code, ProjectionMarker, WriteMode,
    plan, write_mode,
};
use ledger_store::{
    Claim, FailureDisposition, LeaseOutcome, ProjectionRepository, ReconstructionLimits, StreamKey,
    WorkItem, WorkMode,
};
use metrics::Metrics;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct ProjectorConfig {
    /// The target this projector serves (`projection_state.target_id`).
    pub target_id: String,
    /// This worker instance (lease owner; unique per process).
    pub owner: String,
    pub lease_ttl: Duration,
    pub reconstruction: ReconstructionLimits,
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    pub poll_interval: Duration,
    pub concurrency: usize,
    /// Re-observe an idle stream's target after this long without a successful check.
    pub reconcile_interval: Duration,
    /// Re-run the transactional probe this often (claiming pauses while it fails).
    pub probe_interval: Duration,
}

/// Deterministic crash windows (tests only): the step returns as if the process died, leaving
/// the lease to expire.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailPoint {
    AfterClaim,
    BeforeTargetRequest,
    AfterTargetSuccess,
    BeforeMarkerVerification,
    AfterMarkerVerification,
    BeforeAcknowledge,
}

/// What one step did (for tests, logs and metrics).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StepOutcome {
    /// Nothing claimable.
    Idle,
    /// Claimed, but no event to project (lease released).
    NothingToDo,
    /// The target already represented the event (or a later one); acknowledged.
    Acknowledged { version: i64, wrote: bool },
    /// Wrote the state, verified the marker and acknowledged.
    Projected { version: i64, rebuilt: bool },
    /// A newer projection of this stream landed first; the lease was released and the next
    /// claim acknowledges it (not a failure).
    Superseded,
    /// A `disabling` stream fenced the target and is now `disabled`.
    Fenced,
    /// Failed and recorded (retry scheduled, blocked or rebuild required).
    Failed { code: Code, class: ErrorClass },
    /// Failed, and the failure could not be recorded (database unavailable); the lease
    /// expires and the next worker re-evaluates.
    Unrecorded { code: Code },
    /// The lease was lost (or about to expire) before writing or acknowledging; another
    /// worker owns the stream now.
    LeaseLost,
    /// A failpoint simulated a crash; the lease is still held until it expires.
    Crashed(FailPoint),
}

pub struct Projector<C: ProjectionClient + ?Sized> {
    repo: ProjectionRepository,
    client: Arc<C>,
    config: ProjectorConfig,
    failpoint: Option<FailPoint>,
    metrics: Arc<Metrics>,
    /// Set while the periodic transactional probe fails: no stream is claimed.
    paused: Arc<AtomicBool>,
    /// Distinguishes write ids minted by this process.
    writes: std::sync::atomic::AtomicU64,
}

/// Map a ledger-side failure to the projection error taxonomy: transient database conditions
/// (unavailable, timeouts, lock waits, serialization failures) are retryable.
pub fn ledger_failure(e: LedgerError) -> ProjectionError {
    match e {
        LedgerError::ResourceLimit(m) => ProjectionError::permanent(Code::StateTooLarge, m),
        LedgerError::DependencyUnavailable(m) | LedgerError::DependencyTimeout(m) => {
            ProjectionError::retryable(Code::LedgerUnavailable, m)
        }
        LedgerError::Storage(m) => ProjectionError::retryable(Code::LedgerUnavailable, m),
        other => ProjectionError::permanent(Code::LedgerState, other.to_string()),
    }
}

/// The marker a write produced, compared field by field except the target-computed count.
fn same_projection(observed: &ProjectionMarker, written: &ProjectionMarker) -> bool {
    observed.write_id == written.write_id
        && observed.graph_id == written.graph_id
        && observed.branch == written.branch
        && observed.commit == written.commit
        && observed.ref_version == written.ref_version
        && observed.state_digest == written.state_digest
}

/// The target-side count is plausible for the state written: never more than the accepted
/// triples (the target may merge literals it canonicalizes, never invent triples), and not
/// zero for a non-empty state.
fn plausible_count(count: u64, projected: &ProjectedState) -> bool {
    count <= projected.triple_count() && (count > 0 || projected.triple_count() == 0)
}

impl<C: ProjectionClient + ?Sized> Projector<C> {
    pub fn new(
        repo: ProjectionRepository,
        client: Arc<C>,
        config: ProjectorConfig,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            repo,
            client,
            config,
            failpoint: None,
            metrics,
            paused: Arc::new(AtomicBool::new(false)),
            writes: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// A write id never used before (ADR-0020: marker terms never repeat, so no ABA): the
    /// digest of this lease, this process's counter and the clock. Unique, not secret.
    fn next_write_id(&self, claim: &Claim) -> String {
        let n = self.writes.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        ledger_core::ContentId::for_bytes(
            format!(
                "sculpin-ledger-projection/v1 write|{}|{}|{}|{}|{}|{n}|{nanos}",
                claim.key.graph_id, claim.key.branch, claim.key.target_id, claim.owner, claim.epoch
            )
            .as_bytes(),
        )
        .to_string()
    }

    /// Tests only: simulate a crash at `point` on every step.
    pub fn with_failpoint(mut self, point: FailPoint) -> Self {
        self.failpoint = Some(point);
        self
    }

    pub fn repository(&self) -> &ProjectionRepository {
        &self.repo
    }

    pub fn config(&self) -> &ProjectorConfig {
        &self.config
    }

    /// Whether claiming is paused because the target failed its transactional probe.
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    fn crash(&self, point: FailPoint) -> bool {
        self.failpoint == Some(point)
    }

    /// Claim one due stream of this projector's target and process it.
    pub async fn step(&self) -> Result<StepOutcome, LedgerError> {
        if self.is_paused() {
            return Ok(StepOutcome::Idle);
        }
        let Some(claim) = self
            .repo
            .claim(
                &self.config.target_id,
                &self.config.owner,
                self.config.lease_ttl,
            )
            .await?
        else {
            return Ok(StepOutcome::Idle);
        };
        Ok(self.process(claim, WorkMode::Pending, false).await)
    }

    /// Re-observe one idle stream whose last check is older than the reconcile interval; a
    /// target that lost or diverged from its projection is rebuilt from the ledger.
    pub async fn reconcile_step(&self) -> Result<StepOutcome, LedgerError> {
        if self.is_paused() {
            return Ok(StepOutcome::Idle);
        }
        let Some(claim) = self
            .repo
            .claim_reconcile(
                &self.config.target_id,
                &self.config.owner,
                self.config.lease_ttl,
                self.config.reconcile_interval,
            )
            .await?
        else {
            return Ok(StepOutcome::Idle);
        };
        Ok(self.process(claim, WorkMode::Recorded, false).await)
    }

    /// Operator rebuild of one stream: replace the cognitive graph with the accepted state at
    /// the ref's head (guarded by the highest version observed), verify marker and complete
    /// content, and record progress. Idempotent. `Ok(None)` if leased or disabled.
    pub async fn rebuild(&self, key: &StreamKey) -> Result<Option<StepOutcome>, LedgerError> {
        let Some(claim) = self
            .repo
            .claim_stream(key, &self.config.owner, self.config.lease_ttl)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(self.process(claim, WorkMode::Head, true).await))
    }

    async fn process(&self, claim: Claim, mode: WorkMode, force: bool) -> StepOutcome {
        let started = Instant::now();
        let outcome = self.process_inner(&claim, mode, force, started).await;
        self.metrics.record(&outcome, started.elapsed());
        match &outcome {
            StepOutcome::Failed { code, class } => tracing::warn!(
                graph = %claim.key.graph_id, branch = %claim.key.branch, code = code.as_str(),
                class = ?class, "projection attempt failed"
            ),
            StepOutcome::Unrecorded { code } => tracing::error!(
                graph = %claim.key.graph_id, branch = %claim.key.branch, code = code.as_str(),
                "projection attempt failed and the failure could not be recorded"
            ),
            StepOutcome::Projected { version, rebuilt } => tracing::info!(
                graph = %claim.key.graph_id, branch = %claim.key.branch, version, rebuilt,
                "projected accepted state"
            ),
            _ => {}
        }
        outcome
    }

    async fn process_inner(
        &self,
        claim: &Claim,
        mode: WorkMode,
        force: bool,
        started: Instant,
    ) -> StepOutcome {
        if self.crash(FailPoint::AfterClaim) {
            return StepOutcome::Crashed(FailPoint::AfterClaim);
        }
        if claim.disabling {
            return match self.fence_out(claim).await {
                Ok(outcome) => outcome,
                Err(e) => self.failed(claim, None, e).await,
            };
        }
        let work = match self.repo.work_for(claim, mode).await {
            Ok(Some(work)) => work,
            Ok(None) => {
                let _ = self.repo.release(claim).await;
                return StepOutcome::NothingToDo;
            }
            Err(e) => return self.failed(claim, None, ledger_failure(e)).await,
        };
        match self.project(claim, &work, force, started).await {
            Ok(outcome) => outcome,
            Err(e) => self.failed(claim, Some(work.outbox_id), e).await,
        }
    }

    fn view(
        &self,
        claim: &Claim,
        work: &WorkItem,
        at_marker: Option<ledger_core::CommitId>,
    ) -> LedgerView {
        LedgerView {
            graph_id: claim.key.graph_id.clone(),
            branch: claim.key.branch.clone(),
            target_commit: work.commit.clone(),
            target_version: work.ref_version,
            head_version: work.head_version,
            commit_at_marker_version: at_marker,
            recorded_version: claim.projected.as_ref().map(|(_, v)| *v),
        }
    }

    async fn project(
        &self,
        claim: &Claim,
        work: &WorkItem,
        force: bool,
        started: Instant,
    ) -> Result<StepOutcome, ProjectionError> {
        let graph = CognitiveGraph::parse(&claim.cognitive_graph)?;
        let observation = self.client.observe(&graph).await?;
        let (mode, rebuilt) = if force {
            // An operator rebuild replaces whatever it observes — except a newer genuine
            // projection of this stream: if the target holds a later accepted state of this
            // ref (its marker names the ledger's own commit at that version), this rebuild's
            // work item is stale (e.g. its claim reply was delayed while another worker
            // projected on) and must not regress it.
            if let MarkerRead::Present(m) = &observation.marker
                && m.graph_id == claim.key.graph_id
                && m.branch == claim.key.branch
                && m.ref_version > work.ref_version
                && self
                    .repo
                    .commit_at(&claim.key.graph_id, &claim.key.branch, m.ref_version)
                    .await
                    .map_err(ledger_failure)?
                    .as_ref()
                    == Some(&m.commit)
            {
                let _ = self.repo.release(claim).await;
                return Ok(StepOutcome::Superseded);
            }
            (WriteMode::Replace, true)
        } else {
            let at_marker = self.commit_at_marker(claim, work, &observation).await?;
            match plan(&self.view(claim, work, at_marker), &observation) {
                Plan::AlreadyProjected => {
                    return self
                        .acknowledge(claim, work, &work.commit, work.ref_version, false, false)
                        .await;
                }
                Plan::AcknowledgeBeyond { commit, version } => {
                    return self
                        .acknowledge(claim, work, &commit, version, false, false)
                        .await;
                }
                Plan::RecoveryRequired(e) => return Err(e),
                Plan::Write { rebuild } => {
                    if let Some(reason) = rebuild {
                        tracing::warn!(
                            graph = %claim.key.graph_id, branch = %claim.key.branch,
                            reason = reason.as_str(), "projection target is ambiguous; rebuilding"
                        );
                    }
                    (write_mode(rebuild), rebuild.is_some())
                }
            }
        };
        let state = self
            .repo
            .state_at(
                &claim.key.graph_id,
                &work.commit,
                &self.config.reconstruction,
            )
            .await
            .map_err(ledger_failure)?;
        let projected = ProjectedState::from_state(&state)?;
        let marker = ProjectionMarker {
            graph_id: claim.key.graph_id.clone(),
            branch: claim.key.branch.clone(),
            commit: work.commit.clone(),
            ref_version: work.ref_version,
            state_digest: projected.digest().clone(),
            triple_count: 0, // computed by the target in the write transaction
            write_id: self.next_write_id(claim),
        };
        // Never start a target write with less than a quarter of the lease left: a write
        // that outlives its lease is still harmless (guarded), but it is wasted work.
        if started.elapsed() > self.config.lease_ttl.mul_f64(0.75) {
            let _ = self.repo.release(claim).await;
            return Ok(StepOutcome::LeaseLost);
        }
        if self.crash(FailPoint::BeforeTargetRequest) {
            return Ok(StepOutcome::Crashed(FailPoint::BeforeTargetRequest));
        }
        // Compare-and-swap on exactly the observed marker (ADR-0020): if anything changed it
        // since — another version, another stream, another feed — the write is a no-op.
        self.client
            .write(&graph, &projected, &marker, mode, &observation.terms)
            .await?;
        if self.crash(FailPoint::AfterTargetSuccess) {
            return Ok(StepOutcome::Crashed(FailPoint::AfterTargetSuccess));
        }
        if self.crash(FailPoint::BeforeMarkerVerification) {
            return Ok(StepOutcome::Crashed(FailPoint::BeforeMarkerVerification));
        }
        let after = self.client.observe(&graph).await?;
        match &after.marker {
            MarkerRead::Present(m)
                if same_projection(m, &marker)
                    && after.triple_count == m.triple_count
                    && plausible_count(m.triple_count, &projected) => {}
            // The guarded write found a newer projection of this stream: it won; the next
            // claim acknowledges it.
            MarkerRead::Present(m)
                if m.graph_id == marker.graph_id
                    && m.branch == marker.branch
                    && m.ref_version > marker.ref_version =>
            {
                let _ = self.repo.release(claim).await;
                return Ok(StepOutcome::Superseded);
            }
            _ => {
                return Err(ProjectionError::retryable(
                    Code::VerificationFailed,
                    "the target does not read back the marker just written",
                ));
            }
        }
        if rebuilt && !self.client.contains_all(&graph, &projected).await? {
            return Err(ProjectionError::permanent(
                Code::VerificationFailed,
                "the rebuilt graph does not contain the accepted state",
            ));
        }
        if self.crash(FailPoint::AfterMarkerVerification) {
            return Ok(StepOutcome::Crashed(FailPoint::AfterMarkerVerification));
        }
        self.acknowledge(claim, work, &work.commit, work.ref_version, true, rebuilt)
            .await
    }

    /// The ref's commit at a well-formed marker's version, when the marker is this stream's
    /// and within the ledger's history.
    async fn commit_at_marker(
        &self,
        claim: &Claim,
        work: &WorkItem,
        observation: &Observation,
    ) -> Result<Option<ledger_core::CommitId>, ProjectionError> {
        match &observation.marker {
            MarkerRead::Present(m)
                if m.graph_id == claim.key.graph_id
                    && m.branch == claim.key.branch
                    && m.ref_version <= work.head_version =>
            {
                self.repo
                    .commit_at(&claim.key.graph_id, &claim.key.branch, m.ref_version)
                    .await
                    .map_err(ledger_failure)
            }
            _ => Ok(None),
        }
    }

    async fn acknowledge(
        &self,
        claim: &Claim,
        work: &WorkItem,
        commit: &ledger_core::CommitId,
        version: i64,
        wrote: bool,
        rebuilt: bool,
    ) -> Result<StepOutcome, ProjectionError> {
        if self.crash(FailPoint::BeforeAcknowledge) {
            return Ok(StepOutcome::Crashed(FailPoint::BeforeAcknowledge));
        }
        match self
            .repo
            .acknowledge(claim, Some(work.outbox_id), commit, version, rebuilt)
            .await
            .map_err(ledger_failure)?
        {
            LeaseOutcome::LeaseLost => Ok(StepOutcome::LeaseLost),
            LeaseOutcome::Committed if wrote => Ok(StepOutcome::Projected { version, rebuilt }),
            LeaseOutcome::Committed => Ok(StepOutcome::Acknowledged { version, wrote }),
        }
    }

    /// Bounded exponential backoff with deterministic jitter (0–25% extra, derived from the
    /// stream and attempt so replicas do not synchronize).
    fn backoff(&self, claim: &Claim) -> Duration {
        let exp = claim.consecutive_failures.clamp(0, 20) as u32;
        let base = self.config.backoff_base.saturating_mul(1u32 << exp.min(16));
        let capped = base.min(self.config.backoff_max);
        let seed = claim
            .key
            .graph_id
            .as_str()
            .bytes()
            .chain(claim.key.branch.bytes())
            .fold(u64::from(exp) + 1, |h, b| {
                h.wrapping_mul(1_099_511_628_211).wrapping_add(u64::from(b))
            });
        capped + capped.mul_f64((seed % 256) as f64 / 1024.0)
    }

    /// A `disabling` stream (ADR-0020/0021): rotate the marker's write id under the
    /// compare-and-swap on what is observed now, so no write planned under this stream's
    /// authority before this point can land; then the stream is `disabled` and its cognitive
    /// graph free for another stream.
    async fn fence_out(&self, claim: &Claim) -> Result<StepOutcome, ProjectionError> {
        let graph = CognitiveGraph::parse(&claim.cognitive_graph)?;
        let observed = self.client.observe(&graph).await?;
        let write_id = self.next_write_id(claim);
        self.client
            .fence(&graph, &observed.terms, &write_id)
            .await?;
        let after = self.client.observe(&graph).await?;
        let write_id_predicate = format!("{}writeId", ledger_projection::LP_NAMESPACE);
        if !after
            .terms
            .iter()
            .any(|(p, o)| *p == write_id_predicate && o.value == write_id)
        {
            // The marker changed between observation and fence (a write landed): retry.
            return Err(ProjectionError::retryable(
                Code::VerificationFailed,
                "the target does not read back the fence just written",
            ));
        }
        match self
            .repo
            .finish_disable(claim)
            .await
            .map_err(ledger_failure)?
        {
            LeaseOutcome::Committed => Ok(StepOutcome::Fenced),
            LeaseOutcome::LeaseLost => Ok(StepOutcome::LeaseLost),
        }
    }

    async fn failed(
        &self,
        claim: &Claim,
        outbox_id: Option<i64>,
        error: ProjectionError,
    ) -> StepOutcome {
        let disposition = match (error.class(), error.code()) {
            // Disabling only ever needs its fence: retry it (the stream stays `disabling`).
            _ if claim.disabling => FailureDisposition::Retry(self.backoff(claim)),
            (_, Code::MarkerAhead | Code::TargetConflict) => FailureDisposition::RebuildRequired,
            (ErrorClass::Permanent, _) => FailureDisposition::Block,
            (ErrorClass::Retryable, _) => FailureDisposition::Retry(self.backoff(claim)),
        };
        match self
            .repo
            .fail(claim, outbox_id, error.code().as_str(), disposition)
            .await
        {
            Ok(LeaseOutcome::LeaseLost) => StepOutcome::LeaseLost,
            Ok(LeaseOutcome::Committed) => StepOutcome::Failed {
                code: error.code(),
                class: error.class(),
            },
            // Could not even record the failure: the lease expires (crash semantics).
            Err(_) => StepOutcome::Unrecorded { code: error.code() },
        }
    }

    /// Re-run the transactional probe; claiming pauses while it fails.
    pub async fn probe(&self) -> Result<(), ProjectionError> {
        let result = self.client.probe_transactional().await;
        let was = self.paused.swap(result.is_err(), Ordering::SeqCst);
        match (&result, was) {
            (Err(e), false) => {
                tracing::error!(error = %e, "target failed the transactional probe; claiming paused")
            }
            (Ok(()), true) => {
                tracing::info!("target passed the transactional probe; claiming resumed")
            }
            _ => {}
        }
        result
    }

    /// Run `concurrency` workers until `shutdown` resolves; each finishes its current step.
    /// Idle workers reconcile; one task re-runs the transactional probe periodically.
    pub async fn run(self: Arc<Self>, shutdown: tokio::sync::watch::Receiver<bool>)
    where
        C: 'static,
    {
        let mut workers = Vec::new();
        {
            let me = self.clone();
            let mut stop = shutdown.clone();
            workers.push(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(me.config.probe_interval) => {}
                        _ = stop.changed() => {}
                    }
                    if *stop.borrow() {
                        break;
                    }
                    let _ = me.probe().await;
                }
            }));
        }
        for _ in 0..self.config.concurrency.max(1) {
            let me = self.clone();
            let mut stop = shutdown.clone();
            workers.push(tokio::spawn(async move {
                loop {
                    if *stop.borrow() {
                        break;
                    }
                    let mut busy = match me.step().await {
                        Ok(outcome) => outcome != StepOutcome::Idle,
                        Err(e) => {
                            tracing::error!(error = %e, "claiming a projection stream failed");
                            false
                        }
                    };
                    if !busy {
                        busy = match me.reconcile_step().await {
                            Ok(outcome) => outcome != StepOutcome::Idle,
                            Err(e) => {
                                tracing::error!(error = %e, "claiming a stream for reconciliation failed");
                                false
                            }
                        };
                    }
                    if !busy {
                        tokio::select! {
                            _ = tokio::time::sleep(me.config.poll_interval) => {}
                            _ = stop.changed() => {}
                        }
                    }
                }
            }));
        }
        for worker in workers {
            let _ = worker.await;
        }
    }

    /// Compare the target's cognitive graph and marker with the accepted state at the
    /// recorded projection (read-only; never writes the target or the ledger). Content is
    /// compared by the target's own term equality plus its write-time count.
    pub async fn verify(&self, key: &StreamKey) -> Result<VerifyReport, LedgerError> {
        let streams = self.repo.status(Some(&key.target_id)).await?;
        let stream = streams
            .into_iter()
            .find(|s| s.key == *key)
            .ok_or_else(|| LedgerError::Storage("no such projection stream".into()))?;
        let (Some(commit), Some(version)) =
            (&stream.projected_commit, stream.projected_ref_version)
        else {
            return Ok(VerifyReport {
                consistent: false,
                detail: "the stream has not projected anything yet".into(),
            });
        };
        let graph = CognitiveGraph::parse(&stream.cognitive_graph)
            .map_err(|e| LedgerError::Storage(e.to_string()))?;
        let commit: ledger_core::CommitId = commit.parse()?;
        let state = self
            .repo
            .state_at(&key.graph_id, &commit, &self.config.reconstruction)
            .await?;
        let projected =
            ProjectedState::from_state(&state).map_err(|e| LedgerError::Storage(e.to_string()))?;
        let unreadable = |e: ProjectionError| VerifyReport {
            consistent: false,
            detail: format!("target unreadable: {e}"),
        };
        let observed = match self.client.observe(&graph).await {
            Ok(o) => o,
            Err(e) => return Ok(unreadable(e)),
        };
        let contains = match self.client.contains_all(&graph, &projected).await {
            Ok(c) => c,
            Err(e) => return Ok(unreadable(e)),
        };
        let marker_ok = matches!(&observed.marker, MarkerRead::Present(m)
            if m.graph_id == key.graph_id && m.branch == key.branch && m.commit == commit
                && m.ref_version == version && m.state_digest == *projected.digest()
                && m.triple_count == observed.triple_count
                && plausible_count(m.triple_count, &projected));
        Ok(VerifyReport {
            consistent: marker_ok && contains,
            detail: format!(
                "recorded v{version}; marker {}; content {} ({} triples in target, {} accepted)",
                if marker_ok { "matches" } else { "DIFFERS" },
                if contains {
                    "contains the accepted state"
                } else {
                    "is MISSING accepted triples"
                },
                observed.triple_count,
                state.len()
            ),
        })
    }
}

#[derive(Clone, Debug)]
pub struct VerifyReport {
    pub consistent: bool,
    pub detail: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_database_conditions_are_retryable() {
        for e in [
            LedgerError::DependencyTimeout("lock_timeout".into()),
            LedgerError::DependencyUnavailable("down".into()),
            LedgerError::Storage("io".into()),
        ] {
            let mapped = ledger_failure(e);
            assert!(mapped.is_retryable(), "{mapped}");
            assert_eq!(mapped.code(), Code::LedgerUnavailable);
        }
        assert!(!ledger_failure(LedgerError::ResourceLimit("big".into())).is_retryable());
    }
}
