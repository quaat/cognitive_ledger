//! Deterministic pause points and injected slow statements for forced-interleaving and
//! request-lifecycle tests (Plan 0009 merge races, Plan 0013 M1).
//!
//! Compiled only with the non-default `test-hooks` cargo feature. Only this crate's own
//! test targets (and the API crate's, through their dev-dependencies) enable it; the server,
//! admin and projector binaries never do, so a release build contains neither this module
//! nor any call site (`scripts/check-architecture.py` proves the feature graph, and a
//! compile probe proves the symbols are absent). There is no runtime switch, environment
//! variable or endpoint: a repository pauses only when a test explicitly gave it a
//! [`PauseHook`].
//!
//! A hook is one-shot per arrival: the paused request signals that it reached the point and
//! waits until the test resumes it. `Notify` stores a permit when nobody waits yet, so neither
//! signal can be lost; no sleep or timing assumption is involved. A *slow-statement* hook
//! signals arrival and then runs `count` statements of `SELECT pg_sleep(each)` on the
//! operation's own transaction connection instead of pausing: that is the deterministic
//! "long statement inside a workflow transaction" the abandonment, `statement_timeout` and
//! transaction-bound tests need, without a huge reconstruction.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

/// Where a request can be paused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookPoint {
    /// `merge_propose`: after the first stored-result lookup found nothing, before the
    /// preview is recomputed.
    ProposeAfterReplayCheck,
    /// `merge_propose`: everything written, idempotency lock held, just before `COMMIT`
    /// (kept for the Plan 0009 races; [`HookPoint::BeforeCommit`] fires there too).
    ProposeBeforeCommit,
    /// Every workflow transaction (prepare, accept, reject, merge propose, merge apply,
    /// branch create / delete / restore, validation record): the idempotency lock is held,
    /// the stored-result lookup found nothing, and no other statement has run yet (Plan 0013
    /// M2: pausing here past the transaction bound exercises the first mid-transaction
    /// deadline check).
    AfterReplayCheck,
    /// Every workflow transaction (prepare, accept, reject, merge propose, merge apply,
    /// branch create / delete / restore, validation record): every row written, the
    /// idempotency result inserted, the idempotency and graph-status locks held, just before
    /// `COMMIT` is sent.
    BeforeCommit,
    /// The same transactions: `COMMIT` has returned successfully and the connection is being
    /// returned to the pool (asynchronously), but the result has not been built or returned yet. A request dropped
    /// here has a durable, replayable outcome and no response.
    AfterCommit,
}

#[derive(Clone, Copy, Debug)]
enum Action {
    Pause,
    SlowStatement { each: Duration, count: u32 },
}

/// A pause (or injected slow statement) at one [`HookPoint`].
#[derive(Clone, Debug)]
pub struct PauseHook {
    point: HookPoint,
    action: Action,
    reached: Arc<Notify>,
    resume: Arc<Notify>,
}

impl PauseHook {
    pub fn new(point: HookPoint) -> Self {
        Self {
            point,
            action: Action::Pause,
            reached: Arc::new(Notify::new()),
            resume: Arc::new(Notify::new()),
        }
    }

    /// At `point`, signal arrival and then run `count` × `SELECT pg_sleep(each)` on the
    /// operation's transaction connection (no pause; `resume` is not needed). Only
    /// in-transaction points carry a connection; at [`HookPoint::AfterCommit`] the hook
    /// signals arrival and continues.
    pub fn slow_statement(point: HookPoint, each: Duration, count: u32) -> Self {
        Self {
            point,
            action: Action::SlowStatement { each, count },
            reached: Arc::new(Notify::new()),
            resume: Arc::new(Notify::new()),
        }
    }

    /// Wait until a request has arrived at the point (and is now paused there, or has
    /// started its first injected slow statement).
    pub async fn reached(&self) {
        self.reached.notified().await;
    }

    /// Let the paused request continue.
    pub fn resume(&self) {
        self.resume.notify_one();
    }

    /// Called by the repository at `point` with the transaction connection when there is one.
    pub(crate) async fn at(
        &self,
        point: HookPoint,
        conn: Option<&mut sqlx::PgConnection>,
    ) -> Result<(), ledger_core::LedgerError> {
        if point != self.point {
            return Ok(());
        }
        self.reached.notify_one();
        match (self.action, conn) {
            (Action::Pause, _) => {
                self.resume.notified().await;
                Ok(())
            }
            (Action::SlowStatement { each, count }, Some(conn)) => {
                for _ in 0..count {
                    sqlx::query("SELECT pg_sleep($1)")
                        .bind(each.as_secs_f64())
                        .execute(&mut *conn)
                        .await
                        .map_err(crate::db_error)?;
                }
                Ok(())
            }
            (Action::SlowStatement { .. }, None) => Ok(()),
        }
    }
}
