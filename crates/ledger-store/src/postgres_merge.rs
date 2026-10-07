//! Merge preview, propose and apply (ADR-0023 integration commits, ADR-0024).
//!
//! - **preview** is a read: it reads both heads without locks, classifies the merge with
//!   `ledger-dag`, reconstructs base/target/source, runs the `ledger-merge` three-way merge
//!   and returns the classification, conflicts, merged-state digest and preview token. It
//!   writes nothing.
//! - **propose** replays a completed result first, recomputes the preview before any
//!   transaction, refuses unless the recomputed token equals the client's, and then in one
//!   transaction (idempotency lock, target branch `FOR SHARE`, heads and statuses re-read)
//!   persists the integration commit `[target, source]`, the proposal on the target and the
//!   write-once merge row.
//! - **apply** replays first, then locks both refs in branch-name order (target `FOR
//!   UPDATE`, source `FOR SHARE`) and both branch rows, compares the stored heads and token
//!   (nothing is reconstructed under the locks), enforces the **target** policy and the
//!   ADR-0019 validation binding, and installs the candidate with a `merge` ref event, the
//!   decision, the outbox row and the idempotency result.

use crate::db_error;
use crate::postgres_branches::GraphParents;
#[cfg(feature = "test-hooks")]
use crate::postgres_workflow::FailPoint;
use crate::postgres_workflow::{
    Operation, RequestScope, StoredResult, ValidationPolicy, WorkflowRepository, validate_reason,
    validate_scope_fn,
};
use ledger_core::{AnyCommit, CommitId, CommitV2, ContentId, GraphId, LedgerError, TenantId};
use ledger_dag::{MergeBase, Relation, TraversalLimits};
use ledger_merge::{
    Classification, Conflict, MERGE_ALGORITHM_V1, PreviewIdentity, ReportLimits, Strategy,
    three_way_reported,
};
use ledger_rdf::{Quad, diff};
use sqlx::{PgConnection, Row};
use std::collections::BTreeSet;
use tokio::sync::Mutex;

/// The activity of every integration commit (ADR-0023 envelope).
pub const MERGE_ACTIVITY: &str = "merge";

/// What a merge request names.
#[derive(Clone, Debug)]
pub struct MergeSpec {
    pub source: String,
    pub target: String,
    pub strategy: Strategy,
    /// An explicit merge base; must be one of the best common ancestors.
    pub base: Option<CommitId>,
}

/// The outcome class of a preview (ADR-0023/0024).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MergeClass {
    AlreadyEqual,
    AlreadyContained,
    /// The merged state equals the target state: nothing would be created.
    NoChange,
    FastForward,
    Divergent,
    /// Several best common ancestors and no (valid) explicit base: candidates ascending.
    AmbiguousMergeBase(Vec<CommitId>),
    UnrelatedHistories,
    /// `abort` with structural conflicts.
    Conflicted,
}

impl MergeClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AlreadyEqual => "already_equal",
            Self::AlreadyContained => "already_contained",
            Self::NoChange => "no_change",
            Self::FastForward => "fast_forward",
            Self::Divergent => "divergent",
            Self::AmbiguousMergeBase(_) => "ambiguous_merge_base",
            Self::UnrelatedHistories => "unrelated_histories",
            Self::Conflicted => "conflicted",
        }
    }
}

/// Summary of one side's change from the base.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeltaSummary {
    pub adds: usize,
    pub deletes: usize,
    pub affected_keys: usize,
}

/// A side-effect-free merge preview.
#[derive(Clone, Debug)]
pub struct MergePreview {
    pub class: MergeClass,
    pub source_head: CommitId,
    pub target_head: CommitId,
    pub merge_base: Option<CommitId>,
    pub base_explicit: bool,
    /// Commits the source has that the target lacks, and the reverse.
    pub ahead: usize,
    pub behind: usize,
    pub target_delta: DeltaSummary,
    pub source_delta: DeltaSummary,
    /// The detailed conflict report, bounded by the [`ReportLimits`] of the preview.
    pub conflicts: Vec<Conflict>,
    /// Exact, whatever the report limits.
    pub conflict_count: usize,
    /// `conflicts` is incomplete (a count, per-side or byte limit left something out).
    pub conflicts_truncated: bool,
    pub strategy: Strategy,
    pub merged_state_digest: Option<ContentId>,
    /// Present exactly when a candidate would result (`FastForward` / `Divergent`).
    pub preview_token: Option<String>,
    /// The exact patch from the target state to the merged state (internal: what propose
    /// persists, so it never reconstructs again under its locks).
    patch: Option<ledger_rdf::Patch>,
    /// Commits the source has and the target lacks (internal: their proposers become the
    /// merge's `source_parties`).
    source_only: Vec<CommitId>,
}

/// `propose`: persist the integration candidate of a still-current preview.
#[derive(Clone, Debug)]
pub struct ProposeMergeRequest {
    pub scope: RequestScope,
    pub spec: MergeSpec,
    /// The preview token the client saw; must equal the recomputed one.
    pub preview_token: String,
    pub message: String,
    pub evidence_refs: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergeProposed {
    pub proposal_id: i64,
    pub candidate: CommitId,
    pub target_head: CommitId,
    pub source_head: CommitId,
    pub merge_base: CommitId,
    pub classification: String,
    pub strategy: String,
    pub conflict_count: i32,
    pub merged_state_digest: ContentId,
    pub preview_token: String,
    pub replayed: bool,
}

/// `apply`: accept a merge proposal onto its target.
#[derive(Clone, Debug)]
pub struct ApplyMergeRequest {
    pub scope: RequestScope,
    pub proposal_id: i64,
    pub preview_token: String,
    pub reason: Option<String>,
    pub validation: ValidationPolicy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergeApplied {
    pub decision_id: i64,
    pub ref_event_id: i64,
    pub outbox_id: i64,
    pub ref_version: i64,
    pub head: CommitId,
    pub replayed: bool,
}

/// The persisted merge row.
struct MergeRow {
    proposal_id: i64,
    target_branch: String,
    candidate: CommitId,
    target_head: CommitId,
    source_branch: String,
    source_head: CommitId,
    merge_base: CommitId,
    classification: String,
    strategy: String,
    conflict_count: i32,
    merged_state_digest: ContentId,
    preview_token: String,
    source_parties: Vec<String>,
}

const MERGE_ROW_COLUMNS: &str = "proposal_id, target_branch, candidate_commit, target_head, \
     source_branch, source_head, merge_base, classification, strategy, conflict_count, \
     merged_state_digest, preview_token, source_parties";

fn merge_row(row: &sqlx::postgres::PgRow) -> Result<MergeRow, LedgerError> {
    let get = |c: &str| row.try_get::<String, _>(c).map_err(db_error);
    Ok(MergeRow {
        proposal_id: row.try_get("proposal_id").map_err(db_error)?,
        target_branch: get("target_branch")?,
        candidate: get("candidate_commit")?.parse()?,
        target_head: get("target_head")?.parse()?,
        source_branch: get("source_branch")?,
        source_head: get("source_head")?.parse()?,
        merge_base: get("merge_base")?.parse()?,
        classification: get("classification")?,
        strategy: get("strategy")?,
        conflict_count: row.try_get("conflict_count").map_err(db_error)?,
        merged_state_digest: get("merged_state_digest")?.parse()?,
        preview_token: get("preview_token")?,
        source_parties: row.try_get("source_parties").map_err(db_error)?,
    })
}

fn summary(base: &BTreeSet<Quad>, side: &BTreeSet<Quad>) -> DeltaSummary {
    let d = diff(base, side);
    DeltaSummary {
        adds: d.adds.len(),
        deletes: d.deletes.len(),
        affected_keys: d.affected_keys().len(),
    }
}

fn dag_error(e: ledger_dag::DagError<LedgerError>) -> LedgerError {
    use ledger_dag::DagError;
    match e {
        DagError::VisitLimit { visited } => {
            LedgerError::ResourceLimit(format!("merge ancestry search exceeded {visited} commits"))
        }
        DagError::Deadline => {
            LedgerError::ResourceLimit("merge ancestry search exceeded its time limit".into())
        }
        DagError::UnknownCommit(c) => LedgerError::CorruptObject {
            id: c.0,
            reason: "parent commit missing from the graph's index".into(),
        },
        DagError::Cycle(c) => LedgerError::CorruptObject {
            id: c.0,
            reason: "commit cycle".into(),
        },
        DagError::Provider(e) => e,
    }
}

impl WorkflowRepository {
    /// Read a branch's head and status without locks; `BranchNotFound` if absent.
    async fn branch_head(
        conn: &mut PgConnection,
        graph: &GraphId,
        branch: &str,
    ) -> Result<(CommitId, String), LedgerError> {
        let row = sqlx::query(
            "SELECT r.head, b.status FROM refs r JOIN branches b USING (graph_id, branch) \
             WHERE r.graph_id = $1 AND r.branch = $2",
        )
        .bind(graph.as_str())
        .bind(branch)
        .fetch_optional(&mut *conn)
        .await
        .map_err(db_error)?;
        let Some(row) = row else {
            return Err(LedgerError::BranchNotFound(branch.to_owned()));
        };
        let head: String = row.try_get("head").map_err(db_error)?;
        Ok((head.parse()?, row.try_get("status").map_err(db_error)?))
    }

    /// The side-effect-free merge preview (ADR-0024): no transaction writes, no locks. The
    /// conflict report has the default limits.
    pub async fn merge_preview(
        &self,
        tenant: &TenantId,
        graph: &GraphId,
        spec: &MergeSpec,
        limits: TraversalLimits,
    ) -> Result<MergePreview, LedgerError> {
        self.merge_preview_reported(tenant, graph, spec, limits, ReportLimits::DEFAULT)
            .await
    }

    /// [`Self::merge_preview`] with explicit conflict report limits (an operational bound on
    /// the detailed report only: the classification, merged state, conflict count and token
    /// are the same under any limits).
    pub async fn merge_preview_reported(
        &self,
        tenant: &TenantId,
        graph: &GraphId,
        spec: &MergeSpec,
        limits: TraversalLimits,
        report: ReportLimits,
    ) -> Result<MergePreview, LedgerError> {
        crate::postgres_workflow::validate_branch(&spec.source)?;
        crate::postgres_workflow::validate_branch(&spec.target)?;
        if spec.source == spec.target {
            return Err(LedgerError::InvalidIdentifier {
                field: "source",
                reason: "source and target must be different branches".into(),
            });
        }
        self.readable_graph(tenant, graph).await?;
        let mut conn = crate::lifecycle::acquire(&self.pool, self.session.acquire_timeout).await?;
        let (target_head, target_status) =
            Self::branch_head(&mut conn, graph, &spec.target).await?;
        let (source_head, source_status) =
            Self::branch_head(&mut conn, graph, &spec.source).await?;
        if target_status != "active" {
            return Err(LedgerError::BranchDeleted(spec.target.clone()));
        }
        if source_status != "active" {
            return Err(LedgerError::BranchDeleted(spec.source.clone()));
        }
        let analysis = {
            let provider = GraphParents {
                conn: Mutex::new(&mut *conn),
                graph: graph.clone(),
                window: self.windows.ancestry,
            };
            ledger_dag::analyze_with_ancestries(&provider, &target_head, &source_head, limits)
                .await
                .map_err(dag_error)?
        };
        let (analysis, target_ancestry, source_ancestry) = analysis;
        let mut preview = MergePreview {
            class: MergeClass::AlreadyEqual,
            source_head: source_head.clone(),
            target_head: target_head.clone(),
            merge_base: None,
            base_explicit: spec.base.is_some(),
            ahead: analysis.ahead,
            behind: analysis.behind,
            target_delta: DeltaSummary::default(),
            source_delta: DeltaSummary::default(),
            conflicts: Vec::new(),
            conflict_count: 0,
            conflicts_truncated: false,
            strategy: spec.strategy,
            merged_state_digest: None,
            preview_token: None,
            patch: None,
            source_only: source_ancestry.difference(&target_ancestry),
        };
        // An explicit base only means something for a divergent merge.
        let base_given = spec.base.is_some();
        let (classification, base) = match analysis.relation {
            Relation::Equal | Relation::SourceContained if base_given => {
                return Err(LedgerError::InvalidMergeBase(
                    "nothing to merge: a base cannot be chosen".into(),
                ));
            }
            Relation::Equal => return Ok(preview),
            Relation::SourceContained => {
                preview.class = MergeClass::AlreadyContained;
                return Ok(preview);
            }
            Relation::FastForward => {
                if spec.base.as_ref().is_some_and(|b| b != &target_head) {
                    return Err(LedgerError::InvalidMergeBase(
                        "a fast-forward's base is the target head".into(),
                    ));
                }
                (Classification::FastForward, target_head.clone())
            }
            Relation::Divergent(MergeBase::Unique(base)) => {
                if spec.base.as_ref().is_some_and(|b| b != &base) {
                    return Err(LedgerError::InvalidMergeBase(format!(
                        "the unique merge base is {base}"
                    )));
                }
                (Classification::Divergent, base)
            }
            Relation::Divergent(MergeBase::Ambiguous(candidates)) => match &spec.base {
                Some(b) if candidates.contains(b) => (Classification::Divergent, b.clone()),
                Some(b) => {
                    return Err(LedgerError::InvalidMergeBase(format!(
                        "{b} is not one of the best common ancestors"
                    )));
                }
                None => {
                    preview.class = MergeClass::AmbiguousMergeBase(candidates);
                    return Ok(preview);
                }
            },
            Relation::Divergent(MergeBase::Unrelated) if base_given => {
                return Err(LedgerError::InvalidMergeBase(
                    "the branches share no history".into(),
                ));
            }
            Relation::Divergent(MergeBase::Unrelated) => {
                preview.class = MergeClass::UnrelatedHistories;
                return Ok(preview);
            }
        };
        preview.merge_base = Some(base.clone());
        let w = self.windows;
        let base_state = Self::state_at_on_windowed(&mut conn, &base, &self.limits, w).await?;
        let target = Self::state_at_on_windowed(&mut conn, &target_head, &self.limits, w).await?;
        let source = Self::state_at_on_windowed(&mut conn, &source_head, &self.limits, w).await?;
        drop(conn);
        preview.target_delta = summary(&base_state.state, &target.state);
        preview.source_delta = summary(&base_state.state, &source.state);
        let strategy = match classification {
            Classification::FastForward => Strategy::Abort,
            Classification::Divergent => spec.strategy,
        };
        preview.strategy = strategy;
        let merged = three_way_reported(
            &base_state.state,
            &target.state,
            &source.state,
            strategy,
            report,
        );
        preview.conflicts = merged.conflicts;
        preview.conflict_count = merged.conflict_count;
        preview.conflicts_truncated = merged.conflicts_truncated;
        let Some(merged) = merged.merged else {
            preview.class = MergeClass::Conflicted;
            return Ok(preview);
        };
        // Nothing to record only when the source contributed no net change from the base:
        // then the merged state is the target state and later merges cannot lose anything.
        // If the source did change something but the merged state still equals the target
        // (a conflict resolved by `take-target`, a convergent change, a change already
        // present), an empty integration commit is recorded, so the resolution is part of
        // history and a later merge never reapplies what was set aside (ADR-0023; the
        // explicit-workflow exception of ADR-0008).
        if ledger_merge::creates_nothing(&base_state.state, &target.state, &source.state, &merged) {
            preview.class = MergeClass::NoChange;
            return Ok(preview);
        }
        // The integration commit must stay readable: prepare's limits on depth and size.
        self.limits.check_depth(target.depth + 1)?;
        let bytes = merged.iter().map(crate::quad_line_len).sum();
        self.limits.check_state(merged.len(), bytes)?;
        let digest = ledger_merge::merged_state_digest(&merged);
        preview.class = match classification {
            Classification::FastForward => MergeClass::FastForward,
            Classification::Divergent => MergeClass::Divergent,
        };
        preview.preview_token = Some(
            PreviewIdentity {
                graph: graph.clone(),
                source_branch: spec.source.clone(),
                source_head,
                target_branch: spec.target.clone(),
                target_head,
                merge_base: base,
                classification,
                strategy,
                merged_state_digest: digest.clone(),
            }
            .token(),
        );
        preview.merged_state_digest = Some(digest);
        preview.patch = Some(diff(&target.state, &merged).to_patch());
        Ok(preview)
    }

    async fn replay_merge_proposed(
        conn: &mut PgConnection,
        stored: StoredResult,
        scope: &RequestScope,
    ) -> Result<MergeProposed, LedgerError> {
        Self::check_digest(&stored, scope)?;
        if stored.result_kind != "merge_proposed" {
            return Err(LedgerError::Storage(
                "idempotency row is not a merge proposal".into(),
            ));
        }
        let proposal_id = stored
            .result_proposal_id
            .ok_or_else(|| LedgerError::Storage("merge result without proposal".into()))?;
        let row = Self::load_merge_row(conn, &scope.graph, proposal_id)
            .await?
            .ok_or_else(|| LedgerError::Storage("merge result without merge row".into()))?;
        Ok(Self::proposed_from_row(row, true))
    }

    fn proposed_from_row(row: MergeRow, replayed: bool) -> MergeProposed {
        MergeProposed {
            proposal_id: row.proposal_id,
            candidate: row.candidate,
            target_head: row.target_head,
            source_head: row.source_head,
            merge_base: row.merge_base,
            classification: row.classification,
            strategy: row.strategy,
            conflict_count: row.conflict_count,
            merged_state_digest: row.merged_state_digest,
            preview_token: row.preview_token,
            replayed,
        }
    }

    async fn load_merge_row(
        conn: &mut PgConnection,
        graph: &GraphId,
        proposal_id: i64,
    ) -> Result<Option<MergeRow>, LedgerError> {
        let row = sqlx::query(&format!(
            "SELECT {MERGE_ROW_COLUMNS} FROM merge_proposals WHERE proposal_id = $1 AND graph_id = $2"
        ))
        .bind(proposal_id)
        .bind(graph.as_str())
        .fetch_optional(&mut *conn)
        .await
        .map_err(db_error)?;
        row.as_ref().map(merge_row).transpose()
    }

    /// The stored result of a completed propose for this scope, if any (a cheap replay the
    /// API answers before taking admission permits; `IDEMPOTENCY_CONFLICT` if the key was used
    /// with another request).
    pub async fn stored_merge_proposal(
        &self,
        scope: &RequestScope,
    ) -> Result<Option<MergeProposed>, LedgerError> {
        validate_scope_fn(scope)?;
        let mut conn = crate::lifecycle::acquire(&self.pool, self.session.acquire_timeout).await?;
        match Self::stored_result(&mut conn, scope, Operation::MergePropose).await? {
            Some(stored) => Ok(Some(
                Self::replay_merge_proposed(&mut conn, stored, scope).await?,
            )),
            None => Ok(None),
        }
    }

    /// Persist the integration candidate of a still-current preview (ADR-0024 propose).
    pub async fn merge_propose(
        &self,
        request: &ProposeMergeRequest,
        limits: TraversalLimits,
    ) -> Result<MergeProposed, LedgerError> {
        let scope = &request.scope;
        validate_scope_fn(scope)?;
        validate_reason(Some(&request.message))?;
        // A completed request replays before anything is recomputed (Phase-4 lesson).
        {
            let mut conn =
                crate::lifecycle::acquire(&self.pool, self.session.acquire_timeout).await?;
            if let Some(stored) =
                Self::stored_result(&mut conn, scope, Operation::MergePropose).await?
            {
                return Self::replay_merge_proposed(&mut conn, stored, scope).await;
            }
        }
        #[cfg(feature = "test-hooks")]
        self.pause_at(crate::test_hooks::HookPoint::ProposeAfterReplayCheck)
            .await;
        // Propose never returns the detailed conflict report: the smallest report budget.
        let preview = self
            .merge_preview_reported(
                &scope.principal.tenant_id,
                &scope.graph,
                &request.spec,
                limits,
                ReportLimits::SMALLEST,
            )
            .await;
        let current = matches!(&preview, Ok(p)
            if p.preview_token.as_deref() == Some(request.preview_token.as_str()));
        if !current {
            // Before any refusal (a recomputation error, another class, another token): a
            // lost response retried while the original commits (and is perhaps applied, so
            // the recomputation is now contained, moved, or an explicit base no longer
            // applies) replays the original result. Completed durable replay wins over
            // mutable recomputed state. The lookup runs under the request's idempotency lock,
            // so a duplicate that arrives while the original is still committing waits for it
            // and replays rather than refusing.
            let mut tx = self.begin_merge(scope, Operation::MergePropose).await?;
            if let Some(stored) =
                Self::stored_result(&mut tx, scope, Operation::MergePropose).await?
            {
                return Self::replay_merge_proposed(&mut tx, stored, scope).await;
            }
        }
        let preview = preview?;
        // Classes without a token are reported as such (more actionable than "stale").
        match &preview.class {
            MergeClass::FastForward | MergeClass::Divergent => {}
            MergeClass::Conflicted => {
                return Err(LedgerError::MergeConflict(preview.conflict_count));
            }
            MergeClass::AmbiguousMergeBase(c) => {
                return Err(LedgerError::InvalidMergeBase(format!(
                    "ambiguous merge base ({} best common ancestors); name one as `base`",
                    c.len()
                )));
            }
            MergeClass::UnrelatedHistories => {
                return Err(LedgerError::InvalidMergeBase(
                    "the branches share no history".into(),
                ));
            }
            other => {
                return Err(LedgerError::MergeNothingToDo(other.as_str().into()));
            }
        }
        if preview.preview_token.as_deref() != Some(request.preview_token.as_str()) {
            return Err(LedgerError::MergeStale(
                "the merge no longer matches the previewed token; preview again".into(),
            ));
        }
        let patch = preview
            .patch
            .clone()
            .expect("a candidate class has a patch");
        let base = preview
            .merge_base
            .clone()
            .expect("a candidate class has a base");

        let mut tx = self.begin_merge(scope, Operation::MergePropose).await?;
        if let Some(stored) = Self::stored_result(&mut tx, scope, Operation::MergePropose).await? {
            return Self::replay_merge_proposed(&mut tx, stored, scope).await;
        }
        #[cfg(feature = "test-hooks")]
        self.hook_at(crate::test_hooks::HookPoint::AfterReplayCheck, &mut tx)
            .await?;
        tx.check_deadline("the graph and branch locks")?;
        Self::graph_must_be_active(&mut tx, scope).await?;
        match Self::lock_branch(&mut tx, &scope.graph, &request.spec.target, false).await? {
            None => return Err(LedgerError::BranchNotFound(request.spec.target.clone())),
            Some(b) if b.status != "active" => {
                return Err(LedgerError::BranchDeleted(request.spec.target.clone()));
            }
            Some(_) => {}
        }
        let (target_now, _) =
            Self::branch_head(&mut tx, &scope.graph, &request.spec.target).await?;
        let (source_now, source_status) =
            Self::branch_head(&mut tx, &scope.graph, &request.spec.source).await?;
        if target_now != preview.target_head
            || source_now != preview.source_head
            || source_status != "active"
        {
            return Err(LedgerError::MergeStale(
                "a branch moved or changed status since the preview".into(),
            ));
        }

        // The proposers of the commits the source has and the target lacks: with the merge
        // proposer they are the parties `require_distinct_reviewer` keeps from applying.
        let source_only: Vec<String> = preview
            .source_only
            .iter()
            .map(ToString::to_string)
            .collect();
        let source_parties = source_parties_on(&mut tx, &scope.graph, &source_only).await?;

        // The integration commit (ADR-0023 envelope): parents [target, source], the exact
        // patch from the target state to the merged state (computed by the preview).
        tx.check_deadline("the candidate publication")?;
        let patch_id = patch.id();
        crate::postgres_immutable::publish_object(&mut tx, &patch_id.0, &patch.canonical_bytes())
            .await?;
        let candidate = AnyCommit::V2(CommitV2 {
            graph_id: scope.graph.clone(),
            parents: vec![preview.target_head.clone(), preview.source_head.clone()],
            patch: patch_id.clone(),
            actor: scope.principal.actor(),
            activity: MERGE_ACTIVITY.into(),
            event_time: None,
            recorded_at: crate::postgres_workflow::now()?,
            evidence_refs: request.evidence_refs.clone(),
            source_system: None,
            message: request.message.clone(),
        });
        tx.check_deadline("the candidate commit publication")?;
        let candidate_id = self
            .immutable
            .publish_commit_in(&mut tx, &candidate)
            .await?;
        let actor = scope.principal.actor();
        tx.check_deadline("the proposal row")?;
        let proposal_id: i64 = sqlx::query_scalar(
            "INSERT INTO proposals (graph_id, branch, tenant_id, principal_id, principal_type, \
             on_behalf_of, expected_head, requested_patch_id, effective_patch_id, candidate_commit, \
             correlation_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8, $9, $10) RETURNING proposal_id",
        )
        .bind(scope.graph.as_str())
        .bind(&request.spec.target)
        .bind(scope.principal.tenant_id.as_str())
        .bind(actor.principal_id.as_str())
        .bind(actor.principal_type.as_str())
        .bind(actor.on_behalf_of.as_ref().map(|p| p.as_str().to_owned()))
        .bind(preview.target_head.to_string())
        .bind(patch_id.to_string())
        .bind(candidate_id.to_string())
        .bind(scope.correlation_id.as_deref())
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(d) if d.is_unique_violation() => LedgerError::MergeStale(
                "an identical integration candidate was proposed concurrently; preview again"
                    .into(),
            ),
            _ => db_error(e),
        })?;
        #[cfg(feature = "test-hooks")]
        self.fail_at(FailPoint::AfterDecision)?;
        let digest = preview
            .merged_state_digest
            .clone()
            .expect("a candidate class has a digest");
        tx.check_deadline("the merge proposal row")?;
        sqlx::query(
            "INSERT INTO merge_proposals (proposal_id, graph_id, target_branch, candidate_commit, \
             target_head, source_branch, source_head, merge_base, base_explicit, classification, \
             strategy, merge_algorithm, conflict_count, merged_state_digest, preview_token, \
             source_parties) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)",
        )
        .bind(proposal_id)
        .bind(scope.graph.as_str())
        .bind(&request.spec.target)
        .bind(candidate_id.to_string())
        .bind(preview.target_head.to_string())
        .bind(&request.spec.source)
        .bind(preview.source_head.to_string())
        .bind(base.to_string())
        .bind(preview.base_explicit)
        .bind(preview.class.as_str())
        .bind(preview.strategy.as_str())
        .bind(MERGE_ALGORITHM_V1)
        .bind(i32::try_from(preview.conflict_count).unwrap_or(i32::MAX))
        .bind(digest.to_string())
        .bind(&request.preview_token)
        .bind(&source_parties)
        .execute(&mut *tx)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(d) if d.is_unique_violation() => LedgerError::MergeStale(
                "an identical integration candidate was proposed concurrently; preview again"
                    .into(),
            ),
            _ => db_error(e),
        })?;
        tx.check_deadline("the idempotency result")?;
        Self::record_result(
            &mut tx,
            scope,
            Operation::MergePropose,
            "merge_proposed",
            Some(&candidate_id),
            None,
            None,
            Some(proposal_id),
            None,
        )
        .await?;
        #[cfg(feature = "test-hooks")]
        self.fail_at(FailPoint::BeforeCommit)?;
        let row = Self::load_merge_row(&mut tx, &scope.graph, proposal_id)
            .await?
            .ok_or_else(|| LedgerError::Storage("merge row vanished".into()))?;
        #[cfg(feature = "test-hooks")]
        self.pause_at(crate::test_hooks::HookPoint::ProposeBeforeCommit)
            .await;
        #[cfg(feature = "test-hooks")]
        self.hook_at(crate::test_hooks::HookPoint::BeforeCommit, &mut tx)
            .await?;
        tx.commit().await?;
        #[cfg(feature = "test-hooks")]
        self.pause_at(crate::test_hooks::HookPoint::AfterCommit)
            .await;
        Ok(Self::proposed_from_row(row, false))
    }

    async fn begin_merge(
        &self,
        scope: &RequestScope,
        operation: Operation,
    ) -> Result<crate::lifecycle::BoundedTx, LedgerError> {
        Self::begin_scoped(&self.pool, &self.session, scope, operation).await
    }

    async fn replay_merge_applied(
        conn: &mut PgConnection,
        stored: StoredResult,
        scope: &RequestScope,
    ) -> Result<MergeApplied, LedgerError> {
        Self::check_digest(&stored, scope)?;
        if stored.result_kind != "merge_applied" {
            return Err(LedgerError::Storage(
                "idempotency row is not a merge apply".into(),
            ));
        }
        let decision_id = stored
            .result_decision_id
            .ok_or_else(|| LedgerError::Storage("merge apply result without decision".into()))?;
        let row = sqlx::query(
            "SELECT d.ref_event_id, o.outbox_id FROM decisions d \
             JOIN projection_outbox o ON o.ref_event_id = d.ref_event_id WHERE d.decision_id = $1",
        )
        .bind(decision_id)
        .fetch_one(&mut *conn)
        .await
        .map_err(db_error)?;
        Ok(MergeApplied {
            decision_id,
            ref_event_id: row.try_get("ref_event_id").map_err(db_error)?,
            outbox_id: row.try_get("outbox_id").map_err(db_error)?,
            ref_version: stored
                .result_ref_version
                .ok_or_else(|| LedgerError::Storage("merge apply result without version".into()))?,
            head: stored
                .result_commit
                .as_deref()
                .ok_or_else(|| LedgerError::Storage("merge apply result without head".into()))?
                .parse()?,
            replayed: true,
        })
    }

    /// Lock a ref row `FOR UPDATE` (target) or `FOR SHARE` (source); head and version.
    async fn lock_ref(
        conn: &mut PgConnection,
        graph: &GraphId,
        branch: &str,
        exclusive: bool,
    ) -> Result<Option<(CommitId, i64)>, LedgerError> {
        let sql = if exclusive {
            "SELECT head, version FROM refs WHERE graph_id = $1 AND branch = $2 FOR UPDATE"
        } else {
            "SELECT head, version FROM refs WHERE graph_id = $1 AND branch = $2 FOR SHARE"
        };
        let row = sqlx::query(sql)
            .bind(graph.as_str())
            .bind(branch)
            .fetch_optional(&mut *conn)
            .await
            .map_err(db_error)?;
        row.map(|r| {
            let head: String = r.try_get("head").map_err(db_error)?;
            Ok((head.parse()?, r.try_get("version").map_err(db_error)?))
        })
        .transpose()
    }

    /// Accept a merge proposal onto its target (ADR-0024 apply).
    pub async fn merge_apply(
        &self,
        request: &ApplyMergeRequest,
    ) -> Result<MergeApplied, LedgerError> {
        let scope = &request.scope;
        validate_scope_fn(scope)?;
        validate_reason(request.reason.as_deref())?;
        let mut tx = self.begin_merge(scope, Operation::MergeApply).await?;
        if let Some(stored) = Self::stored_result(&mut tx, scope, Operation::MergeApply).await? {
            return Self::replay_merge_applied(&mut tx, stored, scope).await;
        }
        if request.validation == ValidationPolicy::Required {
            return Err(LedgerError::ValidationRequired);
        }
        #[cfg(feature = "test-hooks")]
        self.hook_at(crate::test_hooks::HookPoint::AfterReplayCheck, &mut tx)
            .await?;
        tx.check_deadline("the graph, ref and branch locks")?;
        Self::graph_must_be_active(&mut tx, scope).await?;
        let Some(row) = Self::load_merge_row(&mut tx, &scope.graph, request.proposal_id).await?
        else {
            return Err(LedgerError::LineageMismatch(format!(
                "proposal {} is not a merge proposal of this graph",
                request.proposal_id
            )));
        };
        if row.preview_token != request.preview_token {
            return Err(LedgerError::MergeStale(
                "the preview token does not match this merge proposal".into(),
            ));
        }
        // Lock protocol (ADR-0024): both refs in branch-name order, target FOR UPDATE and
        // source FOR SHARE; then both branch rows FOR SHARE in the same order. Every
        // multi-ref lock precedes every branch lock, so no wait cycle with accept (ref then
        // its branch), branch create (ref then its branch) or delete/restore (branch only);
        // two opposite merges serialize and the second sees a moved source.
        let mut names = [
            (row.target_branch.clone(), true),
            (row.source_branch.clone(), false),
        ];
        names.sort();
        let (mut target_ref, mut source_ref) = (None, None);
        for (name, is_target) in &names {
            let locked = Self::lock_ref(&mut tx, &scope.graph, name, *is_target).await?;
            if *is_target {
                target_ref = locked;
            } else {
                source_ref = locked;
            }
        }
        let (mut target_branch, mut source_branch) = (None, None);
        for (name, is_target) in &names {
            let locked = Self::lock_branch(&mut tx, &scope.graph, name, false).await?;
            if *is_target {
                target_branch = locked;
            } else {
                source_branch = locked;
            }
        }
        let (Some((target_head, target_version)), Some(target_branch)) =
            (target_ref, target_branch)
        else {
            return Err(LedgerError::BranchNotFound(row.target_branch.clone()));
        };
        if target_branch.status != "active" {
            return Err(LedgerError::BranchDeleted(row.target_branch.clone()));
        }
        // An already-decided proposal is reported as decided, not as stale (after an applied
        // merge the target head is the candidate itself, which would otherwise look stale).
        let decided: Option<(i64, String)> = sqlx::query_as(
            "SELECT decision_id, decision FROM decisions WHERE candidate_commit = $1",
        )
        .bind(row.candidate.to_string())
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_error)?;
        if let Some((decision_id, decision)) = decided {
            return Err(LedgerError::LineageMismatch(format!(
                "merge proposal {} already has a terminal decision ({decision}, decision {decision_id})",
                row.proposal_id
            )));
        }
        let source_current = source_ref.map(|(h, _)| h);
        if target_head != row.target_head
            || source_current.as_ref() != Some(&row.source_head)
            || source_branch.as_ref().is_none_or(|b| b.status != "active")
        {
            return Err(LedgerError::MergeStale(
                "a branch moved or changed status since the merge was proposed".into(),
            ));
        }
        // The target's policy is authoritative; the source's never applies.
        if target_branch.require_validation && request.validation == ValidationPolicy::NoValidation
        {
            return Err(LedgerError::ValidationRequired);
        }
        let proposal = Self::bound_undecided_proposal(
            &mut tx,
            scope,
            &row.target_branch,
            Some(&row.target_head),
            &row.candidate,
        )
        .await?;
        if target_branch.require_distinct_reviewer {
            // Distinct from the merge proposer, and from everyone who proposed the source
            // content being integrated: a merge never launders self-reviewed work into a
            // four-eyes target (ADR-0024).
            Self::require_distinct_parties(&mut tx, proposal.proposal_id, scope).await?;
            let actor = scope.principal.actor();
            let applier = [
                Some(actor.principal_id.as_str().to_owned()),
                actor.on_behalf_of.as_ref().map(|p| p.as_str().to_owned()),
            ];
            if applier
                .iter()
                .flatten()
                .any(|a| row.source_parties.contains(a))
            {
                return Err(LedgerError::BranchPolicyViolation(
                    "this branch requires a reviewer distinct from the authors of the merged \
                     source changes"
                        .into(),
                ));
            }
        }
        let cited = self
            .cite_validation(&mut tx, scope, &row.candidate, &request.validation)
            .await?;
        if let Some(cited) = &cited
            && cited.candidate_state_digest != row.merged_state_digest
        {
            return Err(LedgerError::LineageMismatch(
                "the cited validation's state digest is not the merged state".into(),
            ));
        }
        let validation_ids: Vec<String> =
            cited.iter().map(|c| c.validation_id.to_string()).collect();

        tx.check_deadline("the ref movement")?;
        let new_version = target_version + 1;
        let updated = sqlx::query(
            "UPDATE refs SET head = $3, version = version + 1, updated_at = now() \
             WHERE graph_id = $1 AND branch = $2 AND head = $4",
        )
        .bind(scope.graph.as_str())
        .bind(&row.target_branch)
        .bind(row.candidate.to_string())
        .bind(target_head.to_string())
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
        if updated.rows_affected() != 1 {
            return Err(LedgerError::Storage(
                "locked target ref disappeared during merge".into(),
            ));
        }
        #[cfg(feature = "test-hooks")]
        self.fail_at(FailPoint::AfterRefUpdate)?;
        let actor = scope.principal.actor();
        tx.check_deadline("the ref event")?;
        let ref_event_id: i64 = sqlx::query_scalar(
            "INSERT INTO ref_events (graph_id, branch, old_head, new_head, old_version, new_version, \
             operation, tenant_id, principal_id, principal_type, on_behalf_of, reason, correlation_id) \
             VALUES ($1, $2, $3, $4, $5, $6, 'merge', $7, $8, $9, $10, $11, $12) RETURNING event_id",
        )
        .bind(scope.graph.as_str())
        .bind(&row.target_branch)
        .bind(target_head.to_string())
        .bind(row.candidate.to_string())
        .bind(target_version)
        .bind(new_version)
        .bind(scope.principal.tenant_id.as_str())
        .bind(actor.principal_id.as_str())
        .bind(actor.principal_type.as_str())
        .bind(actor.on_behalf_of.as_ref().map(|p| p.as_str().to_owned()))
        .bind(request.reason.as_deref())
        .bind(scope.correlation_id.as_deref())
        .fetch_one(&mut *tx)
        .await
        .map_err(db_error)?;
        #[cfg(feature = "test-hooks")]
        self.fail_at(FailPoint::AfterRefEvent)?;
        tx.check_deadline("the decision")?;
        let decision_id: i64 = sqlx::query_scalar(
            "INSERT INTO decisions (proposal_id, graph_id, branch, candidate_commit, decision, \
             tenant_id, principal_id, principal_type, on_behalf_of, reason, validation_ids, ref_event_id, \
             correlation_id) \
             VALUES ($1, $2, $3, $4, 'accepted', $5, $6, $7, $8, $9, $10, $11, $12) RETURNING decision_id",
        )
        .bind(proposal.proposal_id)
        .bind(scope.graph.as_str())
        .bind(&row.target_branch)
        .bind(row.candidate.to_string())
        .bind(scope.principal.tenant_id.as_str())
        .bind(actor.principal_id.as_str())
        .bind(actor.principal_type.as_str())
        .bind(actor.on_behalf_of.as_ref().map(|p| p.as_str().to_owned()))
        .bind(request.reason.as_deref())
        .bind(&validation_ids)
        .bind(ref_event_id)
        .bind(scope.correlation_id.as_deref())
        .fetch_one(&mut *tx)
        .await
        .map_err(db_error)?;
        if let Some(cited) = &cited {
            Self::link_decision_validation(
                &mut tx,
                decision_id,
                scope,
                &row.candidate,
                &cited.validation_id,
            )
            .await?;
        }
        #[cfg(feature = "test-hooks")]
        self.fail_at(FailPoint::AfterDecision)?;
        tx.check_deadline("the outbox row")?;
        let outbox_id: i64 = sqlx::query_scalar(
            "INSERT INTO projection_outbox (graph_id, branch, commit_id, ref_version, event_kind, ref_event_id) \
             VALUES ($1, $2, $3, $4, 'ref_advanced', $5) RETURNING outbox_id",
        )
        .bind(scope.graph.as_str())
        .bind(&row.target_branch)
        .bind(row.candidate.to_string())
        .bind(new_version)
        .bind(ref_event_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_error)?;
        #[cfg(feature = "test-hooks")]
        self.fail_at(FailPoint::AfterOutbox)?;
        tx.check_deadline("the idempotency result")?;
        Self::record_result(
            &mut tx,
            scope,
            Operation::MergeApply,
            "merge_applied",
            Some(&row.candidate),
            Some(new_version),
            Some(decision_id),
            Some(proposal.proposal_id),
            None,
        )
        .await?;
        #[cfg(feature = "test-hooks")]
        self.fail_at(FailPoint::BeforeCommit)?;
        #[cfg(feature = "test-hooks")]
        self.hook_at(crate::test_hooks::HookPoint::BeforeCommit, &mut tx)
            .await?;
        tx.commit().await?;
        #[cfg(feature = "test-hooks")]
        self.pause_at(crate::test_hooks::HookPoint::AfterCommit)
            .await;
        Ok(MergeApplied {
            decision_id,
            ref_event_id,
            outbox_id,
            ref_version: new_version,
            head: row.candidate,
            replayed: false,
        })
    }
}

/// The proposers (and principals acted for) of the given commits, sorted and distinct: the
/// `source_parties` of a merge row. `verify` recomputes it with the same query.
pub(crate) async fn source_parties_on(
    conn: &mut sqlx::PgConnection,
    graph: &GraphId,
    commits: &[String],
) -> Result<Vec<String>, LedgerError> {
    sqlx::query_scalar(
        "SELECT DISTINCT party FROM ( \
             SELECT principal_id AS party FROM proposals WHERE graph_id = $1 AND candidate_commit = ANY($2) \
             UNION SELECT on_behalf_of FROM proposals \
              WHERE graph_id = $1 AND candidate_commit = ANY($2) AND on_behalf_of IS NOT NULL) p \
         ORDER BY party",
    )
    .bind(graph.as_str())
    .bind(commits)
    .fetch_all(conn)
    .await
    .map_err(db_error)
}
