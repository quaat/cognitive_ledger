//! The accepted-state projector (ADR-0020, ADR-0021; Plan 0007).
//!
//! One step: lease a stream ([`ProjectionRepository::claim`], committed), observe the
//! target, decide with [`ledger_projection::plan`], reconstruct the accepted state and write it
//! with one target transaction when needed, read the marker back, and acknowledge under the
//! lease — no database transaction is ever held across a target request. Every crash window
//! is a [`FailPoint`] the tests drive; each has a defined recovery (lease expiry, then the
//! marker tells the next worker what the target holds).

pub mod metrics;

use ledger_core::LedgerError;
use ledger_projection::{
    CognitiveGraph, ErrorClass, LedgerView, MarkerRead, Plan, ProjectedState, ProjectionClient,
    ProjectionError, ProjectionErrorCode as Code, ProjectionMarker, WriteMode, plan,
};
use ledger_store::{
    Claim, FailureDisposition, LeaseOutcome, ProjectionRepository, ReconstructionLimits, StreamKey,
    WorkItem,
};
use metrics::Metrics;
use std::{sync::Arc, time::Duration};

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
    /// Claimed, but no event beyond the recorded progress (lease released).
    NothingToDo,
    /// The target already represented the event (or a later one); acknowledged.
    Acknowledged { version: i64, wrote: bool },
    /// Wrote the state, verified the marker and acknowledged.
    Projected { version: i64, rebuilt: bool },
    /// Failed and recorded (retry scheduled, blocked or rebuild required).
    Failed { code: Code, class: ErrorClass },
    /// The lease was lost before acknowledging; another worker owns the stream now.
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
}

enum Force {
    /// Normal processing by the decision table.
    No,
    /// Operator rebuild: unconditional replacement at the ref's accepted head.
    Rebuild,
}

fn ledger_failure(e: LedgerError) -> ProjectionError {
    match e {
        LedgerError::ResourceLimit(m) => ProjectionError::permanent(Code::StateTooLarge, m),
        LedgerError::DependencyUnavailable(m) => {
            ProjectionError::retryable(Code::LedgerUnavailable, m)
        }
        LedgerError::Storage(m) => ProjectionError::retryable(Code::LedgerUnavailable, m),
        other => ProjectionError::permanent(Code::LedgerState, other.to_string()),
    }
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
        }
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

    fn crash(&self, point: FailPoint) -> bool {
        self.failpoint == Some(point)
    }

    /// Claim one due stream of this projector's target and process it.
    pub async fn step(&self) -> Result<StepOutcome, LedgerError> {
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
        Ok(self.process(claim, Force::No).await)
    }

    /// Operator rebuild of one stream: replace the cognitive graph with the accepted state
    /// at the ref's head, write the exact marker, verify it and the complete graph content,
    /// and record progress. Idempotent. `Ok(None)` if the stream is leased or disabled.
    pub async fn rebuild(&self, key: &StreamKey) -> Result<Option<StepOutcome>, LedgerError> {
        let Some(claim) = self
            .repo
            .claim_stream(key, &self.config.owner, self.config.lease_ttl)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(self.process(claim, Force::Rebuild).await))
    }

    async fn process(&self, claim: Claim, force: Force) -> StepOutcome {
        let started = std::time::Instant::now();
        let outcome = self.process_inner(&claim, &force).await;
        self.metrics.record(&outcome, started.elapsed());
        match &outcome {
            StepOutcome::Failed { code, class } => tracing::warn!(
                graph = %claim.key.graph_id, branch = %claim.key.branch, code = code.as_str(),
                class = ?class, "projection attempt failed"
            ),
            StepOutcome::Projected { version, rebuilt } => tracing::info!(
                graph = %claim.key.graph_id, branch = %claim.key.branch, version, rebuilt,
                "projected accepted state"
            ),
            _ => {}
        }
        outcome
    }

    async fn process_inner(&self, claim: &Claim, force: &Force) -> StepOutcome {
        if self.crash(FailPoint::AfterClaim) {
            return StepOutcome::Crashed(FailPoint::AfterClaim);
        }
        let rebuild = matches!(force, Force::Rebuild);
        let work = match self.repo.work_for(claim, rebuild).await {
            Ok(Some(work)) => work,
            Ok(None) => {
                let _ = self.repo.release(claim).await;
                return StepOutcome::NothingToDo;
            }
            Err(e) => return self.failed(claim, None, ledger_failure(e)).await,
        };
        match self.project(claim, &work, rebuild).await {
            Ok(outcome) => outcome,
            Err(e) => self.failed(claim, Some(work.outbox_id), e).await,
        }
    }

    async fn project(
        &self,
        claim: &Claim,
        work: &WorkItem,
        rebuild: bool,
    ) -> Result<StepOutcome, ProjectionError> {
        let graph = CognitiveGraph::parse(&claim.cognitive_graph)?;
        let observation = self.client.observe(&graph).await?;
        let (mode, rebuilt) = if rebuild {
            (WriteMode::Replace, true)
        } else {
            let commit_at_marker_version = match &observation.marker {
                MarkerRead::Present(m)
                    if m.graph_id == claim.key.graph_id
                        && m.branch == claim.key.branch
                        && m.ref_version <= work.head_version =>
                {
                    self.repo
                        .commit_at(&claim.key.graph_id, &claim.key.branch, m.ref_version)
                        .await
                        .map_err(ledger_failure)?
                }
                _ => None,
            };
            let view = LedgerView {
                graph_id: claim.key.graph_id.clone(),
                branch: claim.key.branch.clone(),
                target_commit: work.commit.clone(),
                target_version: work.ref_version,
                head_version: work.head_version,
                commit_at_marker_version,
            };
            match plan(&view, &observation) {
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
                Plan::Write { mode, rebuild } => {
                    if let Some(reason) = rebuild {
                        tracing::warn!(
                            graph = %claim.key.graph_id, branch = %claim.key.branch,
                            reason = reason.as_str(), "projection target is ambiguous; rebuilding"
                        );
                    }
                    (mode, rebuild.is_some())
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
            triple_count: projected.triple_count(),
        };
        if self.crash(FailPoint::BeforeTargetRequest) {
            return Ok(StepOutcome::Crashed(FailPoint::BeforeTargetRequest));
        }
        self.client.write(&graph, &projected, &marker, mode).await?;
        if self.crash(FailPoint::AfterTargetSuccess) {
            return Ok(StepOutcome::Crashed(FailPoint::AfterTargetSuccess));
        }
        if self.crash(FailPoint::BeforeMarkerVerification) {
            return Ok(StepOutcome::Crashed(FailPoint::BeforeMarkerVerification));
        }
        let after = self.client.observe(&graph).await?;
        match &after.marker {
            MarkerRead::Present(m) if *m == marker && after.triple_count == marker.triple_count => {
            }
            // A conditional write that found a newer marker (another worker got further):
            // the target is ahead of this event; let the next claim acknowledge it.
            MarkerRead::Present(m)
                if mode == WriteMode::Conditional
                    && m.graph_id == marker.graph_id
                    && m.branch == marker.branch
                    && m.ref_version > marker.ref_version =>
            {
                return Err(ProjectionError::retryable(
                    Code::VerificationFailed,
                    "the target holds a newer projection than this event; re-evaluating",
                ));
            }
            _ => {
                return Err(ProjectionError::retryable(
                    Code::VerificationFailed,
                    "the target does not read back the marker and size just written",
                ));
            }
        }
        if rebuild {
            // Operator rebuild: the complete graph must be exactly the accepted state.
            let content = self.client.read_graph(&graph).await?;
            if content != state {
                return Err(ProjectionError::permanent(
                    Code::VerificationFailed,
                    "the rebuilt graph's content differs from the accepted state",
                ));
            }
        }
        if self.crash(FailPoint::AfterMarkerVerification) {
            return Ok(StepOutcome::Crashed(FailPoint::AfterMarkerVerification));
        }
        self.acknowledge(claim, work, &work.commit, work.ref_version, true, rebuilt)
            .await
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

    async fn failed(
        &self,
        claim: &Claim,
        outbox_id: Option<i64>,
        error: ProjectionError,
    ) -> StepOutcome {
        let disposition = match (error.class(), error.code()) {
            (_, Code::MarkerAhead) => FailureDisposition::RebuildRequired,
            (ErrorClass::Permanent, _) => FailureDisposition::Block,
            (ErrorClass::Retryable, _) => FailureDisposition::Retry(self.backoff(claim)),
        };
        match self
            .repo
            .fail(claim, outbox_id, error.code().as_str(), disposition)
            .await
        {
            Ok(LeaseOutcome::LeaseLost) => StepOutcome::LeaseLost,
            // Could not even record the failure: the lease expires (crash semantics).
            Ok(LeaseOutcome::Committed) | Err(_) => StepOutcome::Failed {
                code: error.code(),
                class: error.class(),
            },
        }
    }

    /// Run `concurrency` workers until `shutdown` resolves; each finishes its current step.
    pub async fn run(self: Arc<Self>, shutdown: tokio::sync::watch::Receiver<bool>)
    where
        C: 'static,
    {
        let mut workers = Vec::new();
        for _ in 0..self.config.concurrency.max(1) {
            let me = self.clone();
            let mut stop = shutdown.clone();
            workers.push(tokio::spawn(async move {
                loop {
                    if *stop.borrow() {
                        break;
                    }
                    let busy = match me.step().await {
                        Ok(StepOutcome::Idle) => false,
                        Ok(_) => true,
                        Err(e) => {
                            tracing::warn!(error = %e, "projector could not claim work");
                            false
                        }
                    };
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
    /// recorded projection (read-only; never writes the target or the ledger).
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
        let observed = self.client.observe(&graph).await;
        let content = self.client.read_graph(&graph).await;
        let (observed, content) = match (observed, content) {
            (Ok(o), Ok(c)) => (o, c),
            (Err(e), _) | (_, Err(e)) => {
                return Ok(VerifyReport {
                    consistent: false,
                    detail: format!("target unreadable: {e}"),
                });
            }
        };
        let marker_ok = matches!(&observed.marker, MarkerRead::Present(m)
            if m.graph_id == key.graph_id && m.branch == key.branch && m.commit == commit
                && m.ref_version == version && m.state_digest == ledger_rdf::state_digest(&state)
                && m.triple_count == state.len() as u64);
        let content_ok = content == state;
        Ok(VerifyReport {
            consistent: marker_ok && content_ok,
            detail: format!(
                "recorded v{version}; marker {}; content {} ({} triples in target, {} accepted)",
                if marker_ok { "matches" } else { "DIFFERS" },
                if content_ok { "matches" } else { "DIFFERS" },
                content.len(),
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
