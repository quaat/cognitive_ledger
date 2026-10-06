//! Deterministic pause points for forced-interleaving tests.
//!
//! Compiled only with the non-default `test-hooks` cargo feature. Only this crate's own
//! test targets enable it (through its dev-dependency on itself); the server, admin and
//! projector binaries never do, so a release build contains neither this module nor any call
//! site. There is no runtime switch, environment variable or endpoint: a repository pauses
//! only when a test explicitly gave it a [`PauseHook`].
//!
//! A hook is one-shot per arrival: the paused request signals that it reached the point and
//! waits until the test resumes it. `Notify` stores a permit when nobody waits yet, so neither
//! signal can be lost; no sleep or timing assumption is involved.

use std::sync::Arc;
use tokio::sync::Notify;

/// Where a request can be paused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookPoint {
    /// `merge_propose`: after the first stored-result lookup found nothing, before the
    /// preview is recomputed.
    ProposeAfterReplayCheck,
    /// `merge_propose`: everything written, idempotency lock held, just before `COMMIT`.
    ProposeBeforeCommit,
}

/// A pause at one [`HookPoint`].
#[derive(Clone, Debug)]
pub struct PauseHook {
    point: HookPoint,
    reached: Arc<Notify>,
    resume: Arc<Notify>,
}

impl PauseHook {
    pub fn new(point: HookPoint) -> Self {
        Self {
            point,
            reached: Arc::new(Notify::new()),
            resume: Arc::new(Notify::new()),
        }
    }

    /// Wait until a request has arrived at the point (and is now paused there).
    pub async fn reached(&self) {
        self.reached.notified().await;
    }

    /// Let the paused request continue.
    pub fn resume(&self) {
        self.resume.notify_one();
    }

    /// Called by the repository at `point`: signal arrival, then wait to be resumed.
    pub(crate) async fn at(&self, point: HookPoint) {
        if point == self.point {
            self.reached.notify_one();
            self.resume.notified().await;
        }
    }
}
