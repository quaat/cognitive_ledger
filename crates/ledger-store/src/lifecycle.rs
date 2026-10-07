//! Request and transaction lifecycle (ADR-0026 §2, §8): the request deadline a detached
//! database-bearing operation runs under, connection acquisition bounded by that budget,
//! and the bounded transaction whose deadline is checked before **every** statement and
//! before `COMMIT`.
//!
//! The request deadline travels as a tokio task-local set once by the API's operation runner
//! (`with_request_deadline`) and read by every pooled acquisition in this crate; nothing else
//! consults it, so the lifetime stays auditable: one writer, one reader. Without a scoped
//! deadline (tooling, tests, the projector) acquisitions use the pool's own timeout.
//!
//! **Statement-level enforcement is structural.** The only way to run SQL on a
//! [`BoundedTx`] is [`Statements::stmt`], which checks the transaction deadline and hands
//! out the connection for exactly one statement; the transaction does not dereference to a
//! connection, so a helper cannot run a statement without naming the phase it guards.
//! Helpers shared with unbounded contexts (tooling, the projector, migrations) are generic
//! over [`Statements`]: a plain `PgConnection` passes through unchecked, a pooled
//! connection on a request path refuses to start a statement once the request deadline has
//! passed, a bounded transaction refuses once its deadline has passed. Only `test-hooks`
//! builds have an unchecked accessor ([`BoundedTx::raw`]), for the injected statements that
//! simulate a check-skipping bug in the PostgreSQL 17 backstop test.
//!
//! The transaction bound starts when the transaction's connection was obtained (never
//! including the wait for one) and is capped by the request deadline when one is scoped, so
//! no `COMMIT` is ever sent after the request's budget ended. Checking the deadline before
//! each statement and before sending `COMMIT` is not a hard bound by itself — a statement
//! started just before the deadline runs to its own `statement_timeout` — which is why the
//! declared contract is `transaction deadline + one statement tail`, and PostgreSQL 17's
//! `transaction_timeout` is set only as a session-terminating backstop at
//! `transaction_bound + statement_timeout` (`DbSessionLimits`).
//!
//! **Transaction start-up is cancellation-safe.** Only the pool acquisition is raced
//! against the client-side budget; `BEGIN` is never. Between the two, a budget that is
//! already spent returns the clean connection to the pool without beginning anything. The
//! begin itself runs in a task of its own that is awaited, never dropped: in sqlx 0.8.6 the
//! client-side transaction depth is incremented only after `BEGIN`'s `ReadyForQuery`, so a
//! begin future dropped in that window would return a connection to the pool that
//! PostgreSQL still considers inside a transaction (PR #17 review, P1). If the awaiting
//! future is ever dropped, the spawned begin still runs to completion and its transaction
//! is dropped there — which queues `ROLLBACK` — so the connection is never returned reusable
//! while a transaction is open on it.

use crate::{DbSessionLimits, db_error};
use ledger_core::LedgerError;
use sqlx::{PgConnection, PgPool, Postgres, Transaction, pool::PoolConnection};
use std::{
    future::Future,
    time::{Duration, Instant},
};

tokio::task_local! {
    static REQUEST_DEADLINE: Instant;
}

/// Run `operation` under a request deadline: every pooled acquisition inside waits at most
/// `min(pool_acquire_timeout, remaining budget)` and fails with `DependencyUnavailable` once
/// the budget is spent; every statement on a pooled connection or bounded transaction
/// refuses to start once the deadline has passed. Set once per detached operation by the
/// API; never nested.
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

fn budget_spent() -> LedgerError {
    LedgerError::DependencyUnavailable(
        "the request's time budget was spent before a database connection was obtained".into(),
    )
}

/// A pooled connection for request-path work, waited for within the request budget. The
/// acquisition is the only database step ever raced against a client-side timer: sqlx's
/// `acquire` is cancellation-safe (a connection checked out by a dropped acquire future is
/// returned to the pool with nothing sent on it).
pub(crate) async fn acquire(
    pool: &PgPool,
    acquire_timeout: Duration,
) -> Result<PoolConnection<Postgres>, LedgerError> {
    let Some(budget) = acquire_budget(acquire_timeout) else {
        return Err(budget_spent());
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
///
/// Cancellation safety (module docs): the acquisition is the only timed step; a budget
/// spent after it returns the clean connection without `BEGIN`; the `BEGIN` is awaited in a
/// task of its own and never raced or dropped half-way.
pub(crate) async fn begin(
    pool: &PgPool,
    session: &DbSessionLimits,
    hook: BeginHook<'_>,
) -> Result<BoundedTx, LedgerError> {
    let conn = acquire(pool, session.acquire_timeout).await?;
    #[cfg(feature = "test-hooks")]
    if let Some(hook) = hook {
        hook.at(crate::test_hooks::HookPoint::BeforeBegin, None)
            .await?;
    }
    #[cfg(not(feature = "test-hooks"))]
    let _ = hook;
    if request_deadline().is_some_and(|d| Instant::now() >= d) {
        // Nothing was sent on `conn`; dropping it returns it to the pool idle.
        drop(conn);
        return Err(LedgerError::DependencyUnavailable(
            "the request's time budget was spent before the transaction began; nothing was \
             started"
                .into(),
        ));
    }
    #[cfg(feature = "test-hooks")]
    let statement = hook.and_then(|h| h.slow_begin_statement());
    #[cfg(not(feature = "test-hooks"))]
    let statement = None;
    let tx = begin_on(conn, statement).await?;
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

/// `BEGIN` on an owned pooled connection, run to completion in its own task. Dropping the
/// returned future does not drop the begin: the task finishes it and drops the transaction
/// (queueing `ROLLBACK`), so the pool never receives a connection that PostgreSQL considers
/// inside a transaction the client does not know about.
async fn begin_on(
    conn: PoolConnection<Postgres>,
    statement: Option<std::borrow::Cow<'static, str>>,
) -> Result<Transaction<'static, Postgres>, LedgerError> {
    let handle = tokio::spawn(Transaction::<Postgres>::begin(conn, statement));
    match handle.await {
        Ok(result) => result.map_err(db_error),
        Err(join) => Err(LedgerError::DependencyUnavailable(format!(
            "the transaction could not be begun: the begin task {}",
            if join.is_panic() {
                "panicked"
            } else {
                "was cancelled"
            }
        ))),
    }
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

/// Something SQL statements run on, under the deadline that governs it: a bounded
/// transaction (its deadline), a pooled connection on a request path (the request deadline),
/// or a plain connection outside any request (unchecked). Every statement of this crate's
/// request-path helpers is obtained through [`Self::stmt`] immediately before it runs.
pub(crate) trait Statements: Send {
    /// The connection for exactly one statement, after the deadline check that guards it;
    /// `before` names the statement in the error ("… before the ref movement").
    fn stmt(&mut self, before: &'static str) -> Result<&mut PgConnection, LedgerError>;
}

impl Statements for PgConnection {
    /// Outside any request budget (tooling, the projector, migrations, verification).
    fn stmt(&mut self, _before: &'static str) -> Result<&mut PgConnection, LedgerError> {
        Ok(self)
    }
}

impl Statements for PoolConnection<Postgres> {
    /// A request-path read: never starts a statement after the request deadline (the client
    /// has its `REQUEST_TIMEOUT`; the detached operation stops at its next statement).
    fn stmt(&mut self, before: &'static str) -> Result<&mut PgConnection, LedgerError> {
        if let Some(deadline) = request_deadline()
            && Instant::now() >= deadline
        {
            return Err(LedgerError::DependencyTimeout(format!(
                "the request's time budget ended before {before}; nothing further was started"
            )));
        }
        Ok(&mut **self)
    }
}

impl Statements for BoundedTx {
    fn stmt(&mut self, before: &'static str) -> Result<&mut PgConnection, LedgerError> {
        self.check_deadline(before)?;
        Ok(&mut *self.tx)
    }
}

/// A transaction that knows its application deadline (ADR-0026 §2). It does **not**
/// dereference to a connection: SQL runs only through [`Statements::stmt`], which checks the
/// deadline before each statement; `commit` checks it once more before sending `COMMIT`.
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

    /// The unchecked connection, for test hooks only: the injected statements that simulate
    /// a check-skipping bug (the PostgreSQL 17 backstop test needs statements that keep
    /// starting past the deadline). Production code has no unchecked path.
    #[cfg(feature = "test-hooks")]
    pub(crate) fn raw(&mut self) -> &mut PgConnection {
        &mut self.tx
    }

    pub(crate) async fn commit(self) -> Result<(), LedgerError> {
        self.check_deadline("COMMIT")?;
        self.tx.commit().await.map_err(commit_error)
    }

    pub(crate) async fn rollback(self) -> Result<(), LedgerError> {
        self.tx.rollback().await.map_err(db_error)
    }
}

/// The deadline check itself.
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

/// The repository's pause hook, offered to `begin` (test-hooks builds): a `BeforeBegin`
/// pause between the acquisition and `BEGIN`, or a deliberately slow `BEGIN`. In production
/// builds the type is uninhabited and the value is always `None`.
#[cfg(feature = "test-hooks")]
pub(crate) type BeginHook<'a> = Option<&'a crate::test_hooks::PauseHook>;
#[cfg(not(feature = "test-hooks"))]
pub(crate) type BeginHook<'a> = Option<&'a std::convert::Infallible>;

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
