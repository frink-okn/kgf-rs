//! Bounded admission to the blocking read pool.
//!
//! Every individual KGF operation has bounded work, but an unbounded number of
//! individually bounded operations is not a bounded deployment. This module
//! puts one fair, weighted gate in front of [`tokio::task::spawn_blocking`]:
//! ordinary reads consume one permit and operations with candidate-sized or
//! random-I/O work consume a configurable larger share.
//!
//! Waiting is bounded separately from execution. A request first tries the
//! active gate without waiting, then claims one of a fixed number of queue
//! slots and waits for at most the configured interval. A full queue or an
//! expired wait returns a `rate_limited` problem with
//! `Retry-After`; no query work has begun, so this is an error response rather
//! than an incomplete result with a cursor.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};

use crate::envelope::{ErrorCode, Problem};

/// Deployment-wide limits on active and waiting bundle work.
///
/// `max_concurrent_work` is measured in ordinary requests. A heavy request
/// consumes `heavy_request_weight` of those units, so the defaults admit 32
/// ordinary operations or eight heavy ones, with mixed traffic sharing the
/// same capacity. This is intentionally generous enough for normal parallel
/// clients while still putting a finite bound on candidate heaps, response
/// buffers, blocking threads, and concurrent page faults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Admission {
    /// Active-work units; an ordinary bundle operation consumes one.
    pub max_concurrent_work: u32,
    /// Active-work units consumed by candidate-heavy or random-I/O work.
    pub heavy_request_weight: u32,
    /// Requests allowed to wait after the active-work gate fills.
    pub max_queued_requests: u32,
    /// Maximum wait for active capacity before returning `rate_limited`.
    pub queue_timeout_ms: u64,
}

impl Admission {
    /// Defaults chosen from the initial 41-KG local load pass.
    pub const fn new() -> Self {
        Self {
            max_concurrent_work: 32,
            heavy_request_weight: 4,
            max_queued_requests: 128,
            queue_timeout_ms: 500,
        }
    }

    /// Refuse a configuration that cannot admit every work class.
    pub(crate) fn validate(self) -> Result<(), String> {
        if self.max_concurrent_work == 0 {
            return Err(
                "admission.max_concurrent_work must be at least 1; no bundle request could run"
                    .to_owned(),
            );
        }
        if self.heavy_request_weight == 0 {
            return Err(
                "admission.heavy_request_weight must be at least 1; a heavy request must consume capacity"
                    .to_owned(),
            );
        }
        if self.heavy_request_weight > self.max_concurrent_work {
            return Err(format!(
                "admission.heavy_request_weight is {}, over max_concurrent_work of {}; no heavy request could run",
                self.heavy_request_weight, self.max_concurrent_work
            ));
        }
        Ok(())
    }

    fn retry_after_seconds(self) -> u64 {
        self.queue_timeout_ms.div_ceil(1_000).max(1)
    }
}

impl Default for Admission {
    fn default() -> Self {
        Self::new()
    }
}

/// The two cost classes the first admission policy distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkClass {
    /// Bounded index descent and row materialization.
    Ordinary,
    /// Candidate-sized ranking/scanning, random sampling, or bulk body work.
    Heavy,
}

impl WorkClass {
    fn weight(self, limits: Admission) -> u32 {
        match self {
            Self::Ordinary => 1,
            Self::Heavy => limits.heavy_request_weight,
        }
    }
}

/// The process-wide gate shared by every version and operation.
#[derive(Debug, Clone)]
pub(crate) struct AdmissionController {
    limits: Admission,
    active: Arc<Semaphore>,
    queued: Arc<Semaphore>,
}

impl AdmissionController {
    pub(crate) fn new(limits: Admission) -> Self {
        debug_assert!(limits.validate().is_ok());
        Self {
            limits,
            active: Arc::new(Semaphore::new(limits.max_concurrent_work as usize)),
            queued: Arc::new(Semaphore::new(limits.max_queued_requests as usize)),
        }
    }

    /// Enter the blocking pool, waiting only within the published host policy.
    pub(crate) async fn enter(&self, class: WorkClass) -> Result<AdmissionGuard, Problem> {
        let weight = class.weight(self.limits);
        match Arc::clone(&self.active).try_acquire_many_owned(weight) {
            Ok(active) => return Ok(AdmissionGuard { _active: active }),
            Err(TryAcquireError::Closed) => return Err(closed_problem()),
            Err(TryAcquireError::NoPermits) => {}
        }

        if self.limits.max_queued_requests == 0 || self.limits.queue_timeout_ms == 0 {
            return Err(self.rate_limited());
        }
        let queued = match Arc::clone(&self.queued).try_acquire_owned() {
            Ok(queued) => queued,
            Err(TryAcquireError::Closed) => return Err(closed_problem()),
            Err(TryAcquireError::NoPermits) => return Err(self.rate_limited()),
        };

        let waiting = Arc::clone(&self.active).acquire_many_owned(weight);
        let admitted =
            tokio::time::timeout(Duration::from_millis(self.limits.queue_timeout_ms), waiting)
                .await;
        drop(queued);

        match admitted {
            Ok(Ok(active)) => Ok(AdmissionGuard { _active: active }),
            Ok(Err(_)) => Err(closed_problem()),
            Err(_) => Err(self.rate_limited()),
        }
    }

    fn rate_limited(&self) -> Problem {
        Problem::new(
            ErrorCode::RateLimited,
            "the server is at its concurrent bundle-work limit; retry after the interval in Retry-After",
        )
        .with_retry_after(self.limits.retry_after_seconds())
    }

    /// Waiting-room slots available at this instant.
    pub(crate) fn queued_available(&self) -> usize {
        self.queued.available_permits()
    }
}

/// One active operation's capacity, released on every return or cancellation.
#[derive(Debug)]
pub(crate) struct AdmissionGuard {
    _active: OwnedSemaphorePermit,
}

fn closed_problem() -> Problem {
    tracing::error!("the bundle-work admission semaphore was closed");
    Problem::new(
        ErrorCode::InternalError,
        "the server could not admit bundle work",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(active: u32, heavy: u32, queued: u32, timeout_ms: u64) -> Admission {
        Admission {
            max_concurrent_work: active,
            heavy_request_weight: heavy,
            max_queued_requests: queued,
            queue_timeout_ms: timeout_ms,
        }
    }

    #[test]
    fn defaults_are_generous_but_finite() {
        let limits = Admission::new();
        assert_eq!(limits.max_concurrent_work, 32);
        assert_eq!(limits.max_concurrent_work / limits.heavy_request_weight, 8);
        assert_eq!(limits.max_queued_requests, 128);
        assert_eq!(limits.queue_timeout_ms, 500);
        assert_eq!(limits.validate(), Ok(()));
    }

    #[test]
    fn invalid_weights_are_refused_at_startup() {
        assert!(limits(0, 1, 0, 0).validate().is_err());
        assert!(limits(1, 0, 0, 0).validate().is_err());
        assert!(limits(3, 4, 0, 0).validate().is_err());
    }

    #[tokio::test]
    async fn heavy_work_consumes_its_configured_share() {
        let admission = AdmissionController::new(limits(4, 4, 0, 0));
        let heavy = admission.enter(WorkClass::Heavy).await.unwrap();
        let refused = admission.enter(WorkClass::Ordinary).await.unwrap_err();
        assert_eq!(refused.code(), ErrorCode::RateLimited);
        drop(heavy);
        admission.enter(WorkClass::Ordinary).await.unwrap();
    }

    #[tokio::test]
    async fn the_waiting_room_is_bounded_and_active_capacity_is_released() {
        let admission = AdmissionController::new(limits(1, 1, 1, 5_000));
        let active = admission.enter(WorkClass::Ordinary).await.unwrap();

        let waiting_admission = admission.clone();
        let waiting =
            tokio::spawn(async move { waiting_admission.enter(WorkClass::Ordinary).await });
        while admission.queued_available() != 0 {
            tokio::task::yield_now().await;
        }

        let full = admission.enter(WorkClass::Ordinary).await.unwrap_err();
        assert_eq!(full.code(), ErrorCode::RateLimited);
        assert_eq!(full.retry_after_seconds(), Some(5));

        drop(active);
        let admitted = waiting.await.unwrap().unwrap();
        drop(admitted);
        admission.enter(WorkClass::Ordinary).await.unwrap();
    }

    #[tokio::test]
    async fn a_queue_wait_has_a_deadline() {
        let admission = AdmissionController::new(limits(1, 1, 1, 5));
        let _active = admission.enter(WorkClass::Ordinary).await.unwrap();
        let refused = admission.enter(WorkClass::Ordinary).await.unwrap_err();
        assert_eq!(refused.code(), ErrorCode::RateLimited);
        assert_eq!(refused.retry_after_seconds(), Some(1));
        assert_eq!(admission.queued_available(), 1);
    }
}
