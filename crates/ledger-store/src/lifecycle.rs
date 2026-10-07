//! Request and transaction lifecycle (ADR-0026 §2, §8): the request deadline a detached
//! database-bearing operation runs under, connection acquisition bounded by that budget,
//! and the bounded transaction whose deadline is checked before its statement phases and
//! before `COMMIT`.
//!
//! The request deadline travels as a tokio task-local set once by the API's operation runner
//! (`with_request_deadline`) and read by every pooled acquisition in this crate; nothing else
//! consults it, so the lifetime stays auditable: one writer, one reader. Without a scoped
//! deadline (tooling, tests, the projector) acquisitions use the pool's own timeout.
//!
//! The transaction bound starts when the transaction's connection was obtained (never
//! including the wait for one) and is capped by the request deadline when one is scoped, so
//! no `COMMIT` is ever sent after the request's budget ended. Checking the deadline before
//! each statement (and between reconstruction windows) and before sending `COMMIT` is not a
//! hard bound by itself — a statement started just before the deadline runs to its own
//! `statement_timeout` — which is why the declared contract is `transaction deadline + one
//! statement tail`, and PostgreSQL 17's `transaction_timeout` is set only as a
//! session-terminating backstop at `transaction_bound + statement_timeout` (`DbSessionLimits`).

use crate::{DbSessionLimits, db_error};
use ledger_core::LedgerError;
use sqlx::{PgConnection, PgPool, Postgres, Transaction, pool::PoolConnection};
use std::{
    future::Future,
    ops::{Deref, DerefMut},
    time::{Duration, Instant},
};

tokio::task_local! {
    static REQUEST_DEADLINE: Instant;
}

/// Run `operation` under a request deadline: every pooled acquisition inside waits at most
/// `min(pool_acquire_timeout, remaining budget)` and fails with `DependencyUnavailable` once
/// the budget is spent. Set once per detached operation by the API; never nested.
pub async fn with_request_deadline<F: Future>(deadline: Instant, operation: F) -> F::Output {
    REQUEST_DEADLINE.scope(deadline, operation).await
}

/// The request deadline of the current task, if one was scoped.
pub fn request_deadline() -> Option<Instant> {
    REQUEST_DEADLINE.try_with(|d| *d).ok()
}

fn acquire_budget(acquire_timeout: Duration) -> Option<Duration> {
    match request_deadline() {
        Some(deadline) => {
            let remaining = deadline.saturating_duration_since(Instant::now());
            (!remaining.is_zero()).then(|| remaining.min(acquire_timeout))
        }
        None => Some(acquire_timeout),
    }
}

/// A pooled connection for request-path work, waited for within the request budget.
pub(crate) async fn acquire(
    pool: &PgPool,
    acquire_timeout: Duration,
) -> Result<PoolConnection<Postgres>, LedgerError> {
    let Some(budget) = acquire_budget(acquire_timeout) else {
        return Err(LedgerError::DependencyUnavailable(
            "the request's time budget was spent before a database connection was obtained".into(),
        ));
    };
    match tokio::time::timeout(budget, pool.acquire()).await {
        Ok(result) => result.map_err(db_error),
        Err(_) => Err(LedgerError::DependencyUnavailable(
            "no database connection became available within the request's remaining time budget"
                .into(),
        )),
    }
}

/// Begin a bounded transaction on a pooled connection obtained within the request budget.
/// The transaction bound starts now, after the acquisition, and never extends past the
/// request deadline (ADR-0026 §8: a transaction whose client has given up ends at its next
/// deadline check instead of committing late).
pub(crate) async fn begin(
    pool: &PgPool,
    session: &DbSessionLimits,
) -> Result<BoundedTx, LedgerError> {
    let Some(budget) = acquire_budget(session.acquire_timeout) else {
        return Err(LedgerError::DependencyUnavailable(
            "the request's time budget was spent before a database connection was obtained".into(),
        ));
    };
    let tx = match tokio::time::timeout(budget, pool.begin()).await {
        Ok(result) => result.map_err(db_error)?,
        Err(_) => {
            return Err(LedgerError::DependencyUnavailable(
                "no database connection became available within the request's remaining time budget"
                    .into(),
            ));
        }
    };
    let started = Instant::now();
    let mut deadline = started + session.transaction_bound;
    if let Some(request) = request_deadline() {
        deadline = deadline.min(request);
    }
    Ok(BoundedTx {
        tx,
        started,
        deadline,
        bound: session.transaction_bound,
    })
}

/// How an error returned by the `COMMIT` statement itself is reported (ADR-0026 §4): when
/// the server's answer or the connection was lost (unavailable) or the statement was cancelled
/// or the session terminated (timeout classes), the transaction may be durable and the outcome
/// is unknown; any other error is an ordinary ERROR response from the server — an integrity
/// trigger firing at COMMIT, say — after which the transaction is definitely rolled back, so it
/// keeps its own class (a `Storage` fault, 500) and is never reported as retryable.
pub(crate) fn commit_error(e: sqlx::Error) -> LedgerError {
    match db_error(e) {
        inner @ (LedgerError::DependencyUnavailable(_) | LedgerError::DependencyTimeout(_)) => {
            LedgerError::CommitOutcomeUnknown(Box::new(inner))
        }
        definite => definite,
    }
}

/// A transaction that knows its application deadline (ADR-0026 §2). Dereferences to the
/// connection, so statements run exactly as before; `check_deadline` is called before each
/// statement phase of a workflow, and `commit` checks it once more before sending `COMMIT`.
/// An error returned by `COMMIT` itself is `CommitOutcomeUnknown`: the transaction may be
/// durable, and only the retry by key can tell.
pub(crate) struct BoundedTx {
    tx: Transaction<'static, Postgres>,
    started: Instant,
    /// `min(started + bound, request deadline)`.
    deadline: Instant,
    bound: Duration,
}

impl BoundedTx {
    /// Fail with `DependencyTimeout` once the transaction has run past its deadline; the
    /// caller's `?` drops the transaction, which rolls it back.
    pub(crate) fn check_deadline(&self, before: &'static str) -> Result<(), LedgerError> {
        check_deadline_at(self.deadline, self.started, self.bound, before)
    }

    /// The deadline, for statement loops that check between their statements.
    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }

    pub(crate) async fn commit(self) -> Result<(), LedgerError> {
        self.check_deadline("COMMIT")?;
        self.tx.commit().await.map_err(commit_error)
    }

    pub(crate) async fn rollback(self) -> Result<(), LedgerError> {
        self.tx.rollback().await.map_err(db_error)
    }
}

/// The deadline check itself (also used between reconstruction windows, which only have the
/// deadline instant).
pub(crate) fn check_deadline_at(
    deadline: Instant,
    started: Instant,
    bound: Duration,
    before: &'static str,
) -> Result<(), LedgerError> {
    let now = Instant::now();
    if now >= deadline {
        let allowed = deadline.saturating_duration_since(started);
        let capped = if allowed < bound {
            " (capped by the request deadline)"
        } else {
            ""
        };
        return Err(LedgerError::DependencyTimeout(format!(
            "transaction exceeded its bound of {} ms{capped} ({} ms elapsed) before {before}; rolled back",
            allowed.as_millis(),
            now.saturating_duration_since(started).as_millis()
        )));
    }
    Ok(())
}

/// A deadline check with only an instant (reconstruction windows): the message names the
/// window loop.
pub(crate) fn check_window_deadline(deadline: Option<Instant>) -> Result<(), LedgerError> {
    match deadline {
        Some(d) if Instant::now() >= d => Err(LedgerError::DependencyTimeout(
            "transaction exceeded its bound before the next reconstruction window; rolled back"
                .into(),
        )),
        _ => Ok(()),
    }
}

impl Deref for BoundedTx {
    type Target = PgConnection;
    fn deref(&self) -> &PgConnection {
        &self.tx
    }
}

impl DerefMut for BoundedTx {
    fn deref_mut(&mut self) -> &mut PgConnection {
        &mut self.tx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_request_deadline_is_scoped_to_the_operation() {
        assert!(request_deadline().is_none());
        let deadline = Instant::now() + Duration::from_secs(5);
        with_request_deadline(deadline, async move {
            assert_eq!(request_deadline(), Some(deadline));
            // The acquire budget is the smaller of the pool timeout and the remaining budget.
            let budget = acquire_budget(Duration::from_secs(60)).unwrap();
            assert!(budget <= Duration::from_secs(5) && budget > Duration::from_secs(4));
            assert_eq!(
                acquire_budget(Duration::from_millis(100)),
                Some(Duration::from_millis(100))
            );
        })
        .await;
        assert!(request_deadline().is_none());
        // A spent budget yields no wait at all.
        with_request_deadline(Instant::now(), async {
            assert_eq!(acquire_budget(Duration::from_secs(1)), None);
        })
        .await;
    }

    #[test]
    fn the_deadline_check_fails_closed_past_the_deadline_and_names_the_phase_and_the_cap() {
        let bound = Duration::from_secs(20);
        check_deadline_at(
            Instant::now() + bound,
            Instant::now(),
            bound,
            "the ref movement",
        )
        .unwrap();
        // A transaction that began a full bound ago is due; the message names the phase and
        // the bound, and is not "capped" (the deadline equals started + bound).
        let started = Instant::now() - bound - Duration::from_millis(1);
        let err =
            check_deadline_at(started + bound, started, bound, "the ref movement").unwrap_err();
        match err {
            LedgerError::DependencyTimeout(m) => {
                assert!(
                    m.contains("before the ref movement") && m.contains("20000 ms"),
                    "{m}"
                );
                assert!(!m.contains("capped"), "{m}");
            }
            other => panic!("{other:?}"),
        }
        // A deadline shorter than the bound was capped by the request budget and says so.
        let err = check_deadline_at(
            Instant::now() - Duration::from_millis(1),
            Instant::now() - Duration::from_secs(2),
            bound,
            "COMMIT",
        )
        .unwrap_err();
        assert!(
            matches!(&err, LedgerError::DependencyTimeout(m) if m.contains("capped by the request deadline") && m.contains("before COMMIT")),
            "{err:?}"
        );
        assert!(check_window_deadline(None).is_ok());
        assert!(check_window_deadline(Some(Instant::now() + Duration::from_secs(1))).is_ok());
        assert!(check_window_deadline(Some(Instant::now())).is_err());
    }

    #[test]
    fn only_lost_or_cancelled_commits_are_outcome_unknown() {
        for e in [
            sqlx::Error::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "eof",
            )),
            sqlx::Error::PoolClosed,
            sqlx::Error::Protocol("reading ReadyForQuery".into()),
        ] {
            assert!(matches!(
                commit_error(e),
                LedgerError::CommitOutcomeUnknown(inner) if matches!(*inner, LedgerError::DependencyUnavailable(_))
            ));
        }
        // An ordinary server ERROR (decode stands in for an integrity trigger raised at
        // COMMIT) is a definite rollback and keeps its own class.
        assert!(matches!(
            commit_error(sqlx::Error::Decode("bad".into())),
            LedgerError::Storage(_)
        ));
    }
}
