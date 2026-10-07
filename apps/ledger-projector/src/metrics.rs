//! Prometheus text metrics. Counters live in the process; backlog gauges are read from the
//! database at scrape time (so they are correct across replicas). Correctness never depends
//! on metrics.

use crate::StepOutcome;
use ledger_projection::ProjectionErrorCode;
use ledger_store::StreamStatus;
use std::{
    fmt::Write as _,
    sync::atomic::{AtomicU64, Ordering::Relaxed},
    time::Duration,
};

/// A Prometheus label value (escapes `\\`, `"` and newlines; the database already bounds
/// these identifiers, this keeps the exposition format intact regardless).
fn label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

const BUCKETS: [f64; 10] = [0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0];

#[derive(Default)]
pub struct Metrics {
    attempts: AtomicU64,
    failures_retryable: AtomicU64,
    failures_permanent: AtomicU64,
    failures_by_code: [AtomicU64; ProjectionErrorCode::ALL.len()],
    rebuilds: AtomicU64,
    lease_lost: AtomicU64,
    superseded: AtomicU64,
    /// Simulated crashes (test builds only; never exported).
    #[cfg(feature = "test-hooks")]
    crashed: AtomicU64,
    duration_count: AtomicU64,
    duration_micros: AtomicU64,
    duration_buckets: [AtomicU64; BUCKETS.len()],
}

impl Metrics {
    pub fn record(&self, outcome: &StepOutcome, elapsed: Duration) {
        if matches!(outcome, StepOutcome::Idle | StepOutcome::NothingToDo) {
            return;
        }
        self.attempts.fetch_add(1, Relaxed);
        match outcome {
            StepOutcome::Failed { code, class } => {
                match class {
                    ledger_projection::ErrorClass::Retryable => &self.failures_retryable,
                    ledger_projection::ErrorClass::Permanent => &self.failures_permanent,
                }
                .fetch_add(1, Relaxed);
                if let Some(i) = ProjectionErrorCode::ALL.iter().position(|c| c == code) {
                    self.failures_by_code[i].fetch_add(1, Relaxed);
                }
            }
            StepOutcome::Projected { rebuilt: true, .. } => {
                self.rebuilds.fetch_add(1, Relaxed);
            }
            StepOutcome::LeaseLost => {
                self.lease_lost.fetch_add(1, Relaxed);
            }
            #[cfg(feature = "test-hooks")]
            StepOutcome::Crashed(_) => {
                self.crashed.fetch_add(1, Relaxed);
            }
            StepOutcome::Unrecorded { .. } => {
                self.failures_retryable.fetch_add(1, Relaxed);
            }
            StepOutcome::Superseded => {
                self.superseded.fetch_add(1, Relaxed);
            }
            _ => {}
        }
        let secs = elapsed.as_secs_f64();
        self.duration_count.fetch_add(1, Relaxed);
        self.duration_micros.fetch_add(
            elapsed.as_micros().min(u128::from(u64::MAX)) as u64,
            Relaxed,
        );
        for (i, bound) in BUCKETS.iter().enumerate() {
            if secs <= *bound {
                self.duration_buckets[i].fetch_add(1, Relaxed);
            }
        }
    }

    pub fn rebuilds(&self) -> u64 {
        self.rebuilds.load(Relaxed)
    }

    /// Render counters plus the database-derived gauges for `streams`.
    pub fn render(&self, streams: &[StreamStatus], unconfigured_pending: i64) -> String {
        let mut out = String::new();
        let counter = |out: &mut String, name: &str, help: &str, value: u64| {
            let _ = writeln!(
                out,
                "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}"
            );
        };
        counter(
            &mut out,
            "projection_attempts_total",
            "Projection steps that did work.",
            self.attempts.load(Relaxed),
        );
        let _ = writeln!(
            out,
            "# HELP projection_failures_total Failed projection attempts.\n\
             # TYPE projection_failures_total counter\n\
             projection_failures_total{{class=\"retryable\"}} {}\n\
             projection_failures_total{{class=\"permanent\"}} {}",
            self.failures_retryable.load(Relaxed),
            self.failures_permanent.load(Relaxed)
        );
        let _ = writeln!(
            out,
            "# HELP projection_failures_by_code_total Failed projection attempts by stable code.\n\
             # TYPE projection_failures_by_code_total counter"
        );
        for (i, code) in ProjectionErrorCode::ALL.iter().enumerate() {
            let _ = writeln!(
                out,
                "projection_failures_by_code_total{{code=\"{}\"}} {}",
                code.as_str(),
                self.failures_by_code[i].load(Relaxed)
            );
        }
        counter(
            &mut out,
            "projection_rebuilds_total",
            "Full rebuilds of a cognitive graph.",
            self.rebuilds.load(Relaxed),
        );
        counter(
            &mut out,
            "projection_lease_lost_total",
            "Acknowledgements refused because the lease was lost.",
            self.lease_lost.load(Relaxed),
        );
        counter(
            &mut out,
            "projection_superseded_total",
            "Writes that found a newer projection already in the target.",
            self.superseded.load(Relaxed),
        );
        let _ = writeln!(
            out,
            "# HELP projection_duration_seconds Duration of projection steps.\n\
             # TYPE projection_duration_seconds histogram"
        );
        for (i, bound) in BUCKETS.iter().enumerate() {
            let _ = writeln!(
                out,
                "projection_duration_seconds_bucket{{le=\"{bound}\"}} {}",
                self.duration_buckets[i].load(Relaxed)
            );
        }
        let count = self.duration_count.load(Relaxed);
        let _ = writeln!(
            out,
            "projection_duration_seconds_bucket{{le=\"+Inf\"}} {count}\n\
             projection_duration_seconds_sum {}\nprojection_duration_seconds_count {count}",
            self.duration_micros.load(Relaxed) as f64 / 1e6
        );
        let _ = writeln!(
            out,
            "# HELP projection_pending Accepted outbox events not yet represented in the target.\n\
             # TYPE projection_pending gauge\n\
             # HELP projection_lag_versions Ledger head version minus projected version.\n\
             # TYPE projection_lag_versions gauge\n\
             # HELP projection_oldest_pending_seconds Age of the oldest unprojected event.\n\
             # TYPE projection_oldest_pending_seconds gauge\n\
             # HELP projection_stream_up 1 when the stream is active and not failing.\n\
             # TYPE projection_stream_up gauge"
        );
        for s in streams {
            let labels = format!(
                "graph=\"{}\",ref=\"{}\",target=\"{}\",status=\"{}\"",
                label(s.key.graph_id.as_str()),
                label(&s.key.branch),
                label(&s.key.target_id),
                label(&s.status)
            );
            let _ = writeln!(
                out,
                "projection_pending{{{labels}}} {}\nprojection_lag_versions{{{labels}}} {}\n\
                 projection_oldest_pending_seconds{{{labels}}} {}\nprojection_stream_up{{{labels}}} {}",
                s.pending_events,
                s.lag_versions(),
                s.oldest_pending_seconds.unwrap_or(0.0),
                u8::from(s.status == "active" && s.consecutive_failures == 0)
            );
        }
        let _ = writeln!(
            out,
            "# HELP projection_unconfigured_pending Outbox events of projection-eligible refs (main) without an enabled stream.\n\
             # TYPE projection_unconfigured_pending gauge\nprojection_unconfigured_pending {unconfigured_pending}"
        );
        out
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn label_values_cannot_break_the_exposition_format() {
        assert_eq!(super::label("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
        assert_eq!(super::label("main"), "main");
    }
}
