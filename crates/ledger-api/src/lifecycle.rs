//! Request lifecycle at the HTTP edge (ADR-0026 §2, §4, §8): the validated timeout
//! hierarchy, the class of a request for timeout guidance, and the server-owned tracker of
//! detached database-bearing operations that graceful shutdown waits for.

use ledger_store::DbSessionLimits;
use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{sync::Notify, task::JoinHandle};

use crate::ApiLimits;

/// What a route's timeout guidance may claim (ADR-0026 §4): decided from the route and the
/// presence of an `Idempotency-Key`, never from how far the handler got.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationClass {
    /// A read: a timeout wrote nothing; there is no idempotency key to retry with.
    Read,
    /// An idempotent write: a timeout leaves the outcome unknown; the same key replays a
    /// committed result or executes afresh.
    IdempotentWrite,
}

/// The instant the request's time budget ends (`request_timeout` after the edge received
/// it). Carried in the request extensions by the edge middleware.
#[derive(Clone, Copy, Debug)]
pub struct RequestDeadline(pub Instant);

/// The longest a detached operation may run after the edge stopped waiting for it
/// (ADR-0026 §3, §8): its transaction deadline is capped by the request deadline, so at most
/// the request budget plus one statement tail.
pub fn max_detached_operation(db: &DbSessionLimits, api: &ApiLimits) -> Duration {
    api.request_timeout + db.statement_timeout
}

/// Validate the ADR-0026 §2 hierarchy between the database session limits, the API limits
/// and the drain deadline. Fails closed with the first violated relation, named by its
/// configuration variables. `validator_configured` enables the validator headroom relation.
pub fn validate_lifecycle(
    db: &DbSessionLimits,
    api: &ApiLimits,
    drain_deadline: Duration,
    validator_configured: bool,
) -> Result<(), String> {
    let ms = |d: Duration| d.as_millis();
    if db.lock_timeout >= db.statement_timeout {
        return Err(format!(
            "LEDGER_DB_LOCK_TIMEOUT_MS ({}) must be below LEDGER_DB_STATEMENT_TIMEOUT_MS ({})",
            ms(db.lock_timeout),
            ms(db.statement_timeout)
        ));
    }
    if db.statement_timeout >= db.transaction_bound {
        return Err(format!(
            "LEDGER_DB_STATEMENT_TIMEOUT_MS ({}) must be below LEDGER_DB_TRANSACTION_TIMEOUT_MS ({})",
            ms(db.statement_timeout),
            ms(db.transaction_bound)
        ));
    }
    if db.transaction_bound >= api.request_timeout {
        return Err(format!(
            "LEDGER_DB_TRANSACTION_TIMEOUT_MS ({}) must be below the request timeout \
             LEDGER_LIMIT_REQUEST_SECONDS ({} ms)",
            ms(db.transaction_bound),
            ms(api.request_timeout)
        ));
    }
    if db.transaction_bound + db.statement_timeout > api.request_timeout {
        return Err(format!(
            "LEDGER_DB_TRANSACTION_TIMEOUT_MS + LEDGER_DB_STATEMENT_TIMEOUT_MS ({} + {} ms) must \
             not exceed LEDGER_LIMIT_REQUEST_SECONDS ({} ms): the declared transaction bound is \
             the bound plus one statement tail",
            ms(db.transaction_bound),
            ms(db.statement_timeout),
            ms(api.request_timeout)
        ));
    }
    if db.acquire_timeout > api.request_timeout {
        return Err(format!(
            "LEDGER_DB_ACQUIRE_TIMEOUT_MS ({}) must not exceed LEDGER_LIMIT_REQUEST_SECONDS ({} ms)",
            ms(db.acquire_timeout),
            ms(api.request_timeout)
        ));
    }
    if db.idle_in_transaction_timeout < db.statement_timeout {
        return Err(format!(
            "LEDGER_DB_IDLE_IN_TRANSACTION_TIMEOUT_MS ({}) must be at least \
             LEDGER_DB_STATEMENT_TIMEOUT_MS ({}): the client-side work between two statements \
             of one transaction must not be cut shorter than a statement",
            ms(db.idle_in_transaction_timeout),
            ms(db.statement_timeout)
        ));
    }
    if db.idle_in_transaction_timeout > api.request_timeout {
        return Err(format!(
            "LEDGER_DB_IDLE_IN_TRANSACTION_TIMEOUT_MS ({}) must not exceed \
             LEDGER_LIMIT_REQUEST_SECONDS ({} ms): an idle transaction cannot outlive its request",
            ms(db.idle_in_transaction_timeout),
            ms(api.request_timeout)
        ));
    }
    if validator_configured && api.validator_timeout + db.statement_timeout > api.request_timeout {
        return Err(format!(
            "LEDGER_LIMIT_VALIDATOR_SECONDS + LEDGER_DB_STATEMENT_TIMEOUT_MS ({} + {} ms) must not \
             exceed LEDGER_LIMIT_REQUEST_SECONDS ({} ms): a validator answering at its timeout \
             must leave one statement of budget for the record transaction",
            ms(api.validator_timeout),
            ms(db.statement_timeout),
            ms(api.request_timeout)
        ));
    }
    if api.request_timeout > drain_deadline {
        return Err(format!(
            "LEDGER_LIMIT_REQUEST_SECONDS ({} ms) must not exceed LEDGER_DRAIN_TIMEOUT_MS ({})",
            ms(api.request_timeout),
            ms(drain_deadline)
        ));
    }
    if max_detached_operation(db, api) > drain_deadline {
        return Err(format!(
            "LEDGER_DRAIN_TIMEOUT_MS ({}) must cover the longest detached database operation: \
             LEDGER_LIMIT_REQUEST_SECONDS + LEDGER_DB_STATEMENT_TIMEOUT_MS ({} + {} ms)",
            ms(drain_deadline),
            ms(api.request_timeout),
            ms(db.statement_timeout)
        ));
    }
    Ok(())
}

/// The server-owned registry of detached database-bearing operations (ADR-0026 §8). Every
/// operation the edge may stop waiting for is spawned through it, so graceful shutdown can
/// close it to new work and wait for the registered operations to reach their bounded end.
#[derive(Clone, Default)]
pub struct DetachedOperations(Arc<Inner>);

#[derive(Default)]
struct Inner {
    active: AtomicUsize,
    closed: AtomicBool,
    changed: Notify,
}

/// The tracker was closed by shutdown: no new database-bearing operation starts.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct ShuttingDown;

struct Registration(Arc<Inner>);

impl Drop for Registration {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
        self.0.changed.notify_waiters();
    }
}

impl DetachedOperations {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register and spawn one detached operation. The registration ends when the task
    /// completes (normally or by panic), never when a `JoinHandle` is dropped — dropping the
    /// handle is exactly how the edge stops waiting without stopping the work.
    pub fn spawn<T, F>(&self, operation: F) -> Result<JoinHandle<T>, ShuttingDown>
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        // Register before checking `closed`, so a close that races this spawn either sees
        // the registration (and waits for it) or refuses it here.
        self.0.active.fetch_add(1, Ordering::SeqCst);
        let registration = Registration(Arc::clone(&self.0));
        if self.0.closed.load(Ordering::SeqCst) {
            drop(registration);
            return Err(ShuttingDown);
        }
        Ok(tokio::spawn(async move {
            let _registration = registration;
            operation.await
        }))
    }

    /// Operations registered and not yet finished.
    pub fn active(&self) -> usize {
        self.0.active.load(Ordering::SeqCst)
    }

    pub fn is_closed(&self) -> bool {
        self.0.closed.load(Ordering::SeqCst)
    }

    /// Refuse every further operation (shutdown step 4). Running operations are unaffected.
    pub fn close(&self) {
        self.0.closed.store(true, Ordering::SeqCst);
        self.0.changed.notify_waiters();
    }

    /// Resolve once no operation is registered. Combine with a timeout for the drain
    /// deadline; the caller decides what to log.
    pub async fn wait_idle(&self) {
        loop {
            let notified = self.0.changed.notified();
            if self.active() == 0 {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(statement: u64, lock: u64, bound: u64, idle: u64, acquire: u64) -> DbSessionLimits {
        DbSessionLimits {
            statement_timeout: Duration::from_millis(statement),
            lock_timeout: Duration::from_millis(lock),
            idle_in_transaction_timeout: Duration::from_millis(idle),
            max_connections: 16,
            transaction_bound: Duration::from_millis(bound),
            acquire_timeout: Duration::from_millis(acquire),
        }
    }

    fn api(request_secs: u64, validator_secs: u64) -> ApiLimits {
        ApiLimits {
            request_timeout: Duration::from_secs(request_secs),
            validator_timeout: Duration::from_secs(validator_secs),
            ..ApiLimits::default()
        }
    }

    #[test]
    fn the_defaults_satisfy_every_relation_and_the_longest_detached_operation_fits_the_drain() {
        let d = DbSessionLimits::default();
        let a = ApiLimits::default();
        validate_lifecycle(&d, &a, Duration::from_secs(40), true).unwrap();
        assert_eq!(
            (
                d.lock_timeout,
                d.statement_timeout,
                d.transaction_bound,
                d.idle_in_transaction_timeout,
                d.acquire_timeout,
                a.request_timeout,
                a.validator_timeout
            ),
            (
                Duration::from_secs(5),
                Duration::from_secs(10),
                Duration::from_secs(20),
                Duration::from_secs(30),
                Duration::from_secs(5),
                Duration::from_secs(30),
                Duration::from_secs(15)
            )
        );
        assert_eq!(
            max_detached_operation(&d, &a),
            Duration::from_secs(40),
            "request budget + one statement tail"
        );
        assert_eq!(
            d.transaction_timeout_backstop(),
            Duration::from_secs(30),
            "PostgreSQL 17 backstop = bound + statement"
        );
    }

    #[test]
    fn every_violated_relation_fails_closed_with_the_variables_named() {
        let ok = db(10_000, 5_000, 20_000, 30_000, 5_000);
        let drain = Duration::from_secs(40);
        // (db, api, validator configured, drain, expected message fragment)
        let cases: Vec<(DbSessionLimits, ApiLimits, bool, Duration, &str)> = vec![
            (
                db(5_000, 5_000, 20_000, 30_000, 5_000),
                api(30, 15),
                false,
                drain,
                "LEDGER_DB_LOCK_TIMEOUT_MS (5000) must be below LEDGER_DB_STATEMENT_TIMEOUT_MS",
            ),
            (
                db(20_000, 5_000, 20_000, 30_000, 5_000),
                api(30, 15),
                false,
                drain,
                "LEDGER_DB_STATEMENT_TIMEOUT_MS (20000) must be below LEDGER_DB_TRANSACTION_TIMEOUT_MS",
            ),
            (
                db(10_000, 5_000, 30_000, 30_000, 5_000),
                api(30, 15),
                false,
                drain,
                "LEDGER_DB_TRANSACTION_TIMEOUT_MS (30000) must be below the request timeout",
            ),
            (
                db(10_000, 5_000, 25_000, 30_000, 5_000),
                api(30, 15),
                false,
                drain,
                "LEDGER_DB_TRANSACTION_TIMEOUT_MS + LEDGER_DB_STATEMENT_TIMEOUT_MS (25000 + 10000 ms)",
            ),
            (
                db(10_000, 5_000, 20_000, 30_000, 31_000),
                api(30, 15),
                false,
                drain,
                "LEDGER_DB_ACQUIRE_TIMEOUT_MS (31000) must not exceed",
            ),
            (
                db(10_000, 5_000, 20_000, 5_000, 5_000),
                api(30, 15),
                false,
                drain,
                "LEDGER_DB_IDLE_IN_TRANSACTION_TIMEOUT_MS (5000) must be at least LEDGER_DB_STATEMENT_TIMEOUT_MS (10000)",
            ),
            (
                db(10_000, 5_000, 20_000, 31_000, 5_000),
                api(30, 15),
                false,
                drain,
                "LEDGER_DB_IDLE_IN_TRANSACTION_TIMEOUT_MS (31000) must not exceed",
            ),
            (
                ok,
                api(30, 25),
                true,
                drain,
                "LEDGER_LIMIT_VALIDATOR_SECONDS + LEDGER_DB_STATEMENT_TIMEOUT_MS (25000 + 10000 ms)",
            ),
            (
                ok,
                api(45, 15),
                false,
                drain,
                "LEDGER_LIMIT_REQUEST_SECONDS (45000 ms) must not exceed LEDGER_DRAIN_TIMEOUT_MS (40000)",
            ),
            (
                ok,
                api(30, 15),
                false,
                Duration::from_secs(39),
                "LEDGER_DRAIN_TIMEOUT_MS (39000) must cover the longest detached database operation: LEDGER_LIMIT_REQUEST_SECONDS + LEDGER_DB_STATEMENT_TIMEOUT_MS (30000 + 10000 ms)",
            ),
        ];
        for (i, (d, a, validator, drain, expected)) in cases.iter().enumerate() {
            let error = validate_lifecycle(d, a, *drain, *validator).unwrap_err();
            assert!(error.contains(expected), "case {i}: {error}");
        }
        // The validator relation is skipped without a validator.
        validate_lifecycle(&ok, &api(30, 25), drain, false).unwrap();
        // Boundary values of the non-strict relations pass.
        validate_lifecycle(&ok, &api(30, 20), drain, true).unwrap();
        validate_lifecycle(&ok, &api(30, 15), Duration::from_secs(40), false).unwrap();
        validate_lifecycle(
            &db(10_000, 5_000, 20_000, 10_000, 5_000),
            &api(30, 15),
            drain,
            false,
        )
        .unwrap();
    }

    #[tokio::test]
    async fn detached_operations_outlive_dropped_handles_and_shutdown_waits_for_them() {
        let ops = DetachedOperations::new();
        let gate = Arc::new(Notify::new());
        let done = Arc::new(AtomicBool::new(false));
        let handle = ops
            .spawn({
                let (gate, done) = (Arc::clone(&gate), Arc::clone(&done));
                async move {
                    gate.notified().await;
                    done.store(true, Ordering::SeqCst);
                    7
                }
            })
            .unwrap();
        assert_eq!(ops.active(), 1);
        // The edge stops waiting: dropping the handle detaches, it does not cancel.
        drop(handle);
        tokio::task::yield_now().await;
        assert_eq!(ops.active(), 1);
        assert!(!done.load(Ordering::SeqCst));
        // Shutdown closes the tracker: new operations are refused, the running one is kept.
        ops.close();
        assert!(ops.spawn(async { 1 }).is_err());
        assert_eq!(ops.active(), 1);
        let idle = tokio::time::timeout(Duration::from_millis(50), ops.wait_idle()).await;
        assert!(
            idle.is_err(),
            "wait_idle must not resolve while an operation runs"
        );
        gate.notify_one();
        tokio::time::timeout(Duration::from_secs(2), ops.wait_idle())
            .await
            .expect("the detached operation completed and the tracker went idle");
        assert!(done.load(Ordering::SeqCst));
        assert_eq!(ops.active(), 0);
    }

    #[tokio::test]
    async fn a_panicking_operation_still_deregisters() {
        let ops = DetachedOperations::new();
        let handle = ops.spawn(async { panic!("boom") }).unwrap();
        assert!(handle.await.is_err());
        tokio::time::timeout(Duration::from_secs(2), ops.wait_idle())
            .await
            .unwrap();
        assert_eq!(ops.active(), 0);
    }
}
