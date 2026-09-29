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
//!
//! Whole-artifact downloads have a gate of their own. A download lasts as long
//! as the client takes to read it — minutes or hours, not milliseconds — so
//! charging it to the work gate would let a handful of transfers hold every
//! query out. Its slot is held by the response body and released when the body
//! is dropped, whether the transfer finished or the client went away.
//!
//! Downloads are also counted per client, because the deployment-wide limit
//! alone is first come, first served: one client opening a few connections —
//! a parallel downloader asking for sixteen ranges at once, or a client that
//! stops reading — would hold every slot there is.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
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
    /// Whole-artifact downloads streaming at once.
    ///
    /// Separate from the work units above: a download is admitted once and
    /// then holds its slot for the life of the transfer. A full gate refuses
    /// immediately rather than queueing, because the wait for a slot is the
    /// length of someone else's download.
    pub max_concurrent_downloads: u32,
    /// Downloads one client may stream at once, within
    /// `max_concurrent_downloads`.
    ///
    /// A client is an IPv4 address or an IPv6 /64, as reported by the trusted
    /// forwarding chain when there is one, so behind a proxy this is only as
    /// good as `trusted_proxies` is right: counted from the wrong hop, every
    /// client is the proxy and this becomes the deployment's limit.
    pub max_downloads_per_client: u32,
}

impl Admission {
    /// Defaults chosen from the initial 41-KG local load pass.
    pub const fn new() -> Self {
        Self {
            max_concurrent_work: 32,
            heavy_request_weight: 4,
            max_queued_requests: 128,
            queue_timeout_ms: 500,
            max_concurrent_downloads: 16,
            max_downloads_per_client: 4,
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
        if self.max_concurrent_downloads == 0 {
            return Err(
                "admission.max_concurrent_downloads must be at least 1; no download could run"
                    .to_owned(),
            );
        }
        if self.max_downloads_per_client == 0 {
            return Err(
                "admission.max_downloads_per_client must be at least 1; no client could download"
                    .to_owned(),
            );
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
///
/// The dividing question is **what bounds the work**, not how expensive it
/// feels: heavy is for requests whose cost is bounded by something other than
/// the page they return — the candidate budget, a random draw, an index scan,
/// or a whole unpaged document. Everything proportional to `limit` is
/// ordinary, however many rows that is, because `limit` is already a published
/// cap and the gate would be pricing the same bound twice.
///
/// **A different bound is not automatically a large one**, and reading the rule
/// without that half misclassifies as surely as feeling expensive does. A
/// dictionary prefix count over every role is bounded by the bundle's predicate
/// count rather than by any page — and measures 0.36 ms against the widest
/// predicate set in the OKN corpus, where an *ordinary* page of the same
/// operation costs 18 ms. So the second question is whether that other bound can
/// grow: `candidate_budget` is deliberately large and a `values=` union is
/// quadratic in its input, while a published per-bundle count of a few hundred
/// is not a bound worth four permits. Measure before classifying; the
/// classification is a claim about contention, and contention is observable.
///
/// Serialization format is deliberately *not* an input. Measured over a page
/// of 10 000 rows, Turtle and N-Quads cost 1.6× the equivalent JSON page and
/// JSON-LD 2.1×, while the HTML page — which resolves a display label per
/// distinct term — costs more than any of them; at the default page size all
/// four are within a few hundred microseconds of each other. A rule keyed on
/// representation charged the cheapest bytes on the wire four times an
/// ordinary permit and the most expensive one a single permit.
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
    downloads: Arc<Semaphore>,
    /// Downloads in progress per client, holding only clients with at least
    /// one — so it never has more entries than there are download slots.
    ///
    /// A lock, but not on any read path: it is taken once when a download is
    /// admitted and once when it ends, never per chunk.
    clients: Arc<Mutex<HashMap<DownloadClient, u32>>>,
}

impl AdmissionController {
    pub(crate) fn new(limits: Admission) -> Self {
        debug_assert!(limits.validate().is_ok());
        Self {
            limits,
            active: Arc::new(Semaphore::new(limits.max_concurrent_work as usize)),
            queued: Arc::new(Semaphore::new(limits.max_queued_requests as usize)),
            downloads: Arc::new(Semaphore::new(limits.max_concurrent_downloads as usize)),
            clients: Arc::default(),
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

    /// Claim a download slot for `client`, or refuse at once.
    ///
    /// No waiting room: a slot frees when some other transfer ends, which is
    /// not an interval worth holding a request open for. The `Retry-After` is
    /// a suggestion of the right order rather than a promise.
    ///
    /// The client's own limit is checked first, because when both are reached
    /// it is the one the client can do something about. `None` — a request
    /// whose address the listener did not supply — is counted against the
    /// deployment's limit only: a shared bucket for every such request would
    /// turn the per-client limit into a global one.
    pub(crate) fn download(&self, client: Option<DownloadClient>) -> Result<DownloadSlot, Problem> {
        let mut clients = lock(&self.clients);
        let per_client = self.limits.max_downloads_per_client;
        if let Some(client) = client
            && clients.get(&client).is_some_and(|&held| held >= per_client)
        {
            return Err(Problem::new(
                ErrorCode::RateLimited,
                format!(
                    "this client already has {per_client} downloads streaming, the most one \
                     client may run at once; retry when one finishes"
                ),
            )
            .with_retry_after(DOWNLOAD_RETRY_AFTER_SECONDS));
        }
        let slot = match Arc::clone(&self.downloads).try_acquire_owned() {
            Ok(slot) => slot,
            Err(TryAcquireError::Closed) => return Err(closed_problem()),
            Err(TryAcquireError::NoPermits) => {
                return Err(Problem::new(
                    ErrorCode::RateLimited,
                    "the server is streaming as many downloads as it allows at once; \
                     retry after the interval in Retry-After",
                )
                .with_retry_after(DOWNLOAD_RETRY_AFTER_SECONDS));
            }
        };
        let claim = client.map(|client| {
            *clients.entry(client).or_insert(0) += 1;
            ClientClaim {
                clients: Arc::clone(&self.clients),
                client,
            }
        });
        Ok(DownloadSlot {
            _claim: claim,
            _slot: slot,
        })
    }

    /// Requests in the waiting room at this instant.
    pub(crate) fn waiting(&self) -> usize {
        (self.limits.max_queued_requests as usize).saturating_sub(self.queued.available_permits())
    }

    #[cfg(test)]
    fn queued_available(&self) -> usize {
        self.queued.available_permits()
    }
}

/// One active operation's capacity, released on every return or cancellation.
#[derive(Debug)]
pub(crate) struct AdmissionGuard {
    _active: OwnedSemaphorePermit,
}

/// One streaming download's claim on the download gate.
///
/// Owned by the response body, so the slot lasts exactly as long as the
/// transfer: completion, a client disconnect, and a server error all drop it.
#[derive(Debug)]
pub(crate) struct DownloadSlot {
    _claim: Option<ClientClaim>,
    _slot: OwnedSemaphorePermit,
}

/// Who a download is counted against: an IPv4 address, or an IPv6 /64.
///
/// A /64 because that is what one IPv6 subscriber is normally assigned, and
/// temporary addressing gives a single host many addresses inside it; keyed on
/// the full address, one client could open as many downloads as it has
/// addresses. An IPv4 address can equally be many people behind one NAT, which
/// is the price of counting by address at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum DownloadClient {
    V4(Ipv4Addr),
    V6Prefix(u64),
}

impl DownloadClient {
    /// The client `address` belongs to. An IPv4 address in IPv6's mapped form
    /// is the IPv4 client.
    pub(crate) fn of(address: IpAddr) -> Self {
        match address.to_canonical() {
            IpAddr::V4(address) => Self::V4(address),
            IpAddr::V6(address) => Self::V6Prefix((address.to_bits() >> 64) as u64),
        }
    }
}

/// One download's place in its client's count, given back when it ends.
#[derive(Debug)]
struct ClientClaim {
    clients: Arc<Mutex<HashMap<DownloadClient, u32>>>,
    client: DownloadClient,
}

impl Drop for ClientClaim {
    fn drop(&mut self) {
        let mut clients = lock(&self.clients);
        if let Some(held) = clients.get_mut(&self.client) {
            *held -= 1;
            if *held == 0 {
                clients.remove(&self.client);
            }
        }
    }
}

/// The per-client counts, whatever a panicking holder left behind.
///
/// Every critical section is an increment, a decrement, or a lookup, none of
/// which can panic part-way, so a poisoned map is still a consistent one.
fn lock(
    clients: &Mutex<HashMap<DownloadClient, u32>>,
) -> MutexGuard<'_, HashMap<DownloadClient, u32>> {
    clients.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How long a refused download is told to wait.
const DOWNLOAD_RETRY_AFTER_SECONDS: u64 = 30;

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
            max_concurrent_downloads: 1,
            max_downloads_per_client: 1,
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
        let no_downloads = Admission {
            max_concurrent_downloads: 0,
            ..limits(1, 1, 0, 0)
        };
        assert!(no_downloads.validate().is_err());
        let no_client_downloads = Admission {
            max_downloads_per_client: 0,
            ..limits(1, 1, 0, 0)
        };
        assert!(no_client_downloads.validate().is_err());
    }

    #[tokio::test]
    async fn a_download_slot_is_separate_from_work_and_released_on_drop() {
        let admission = AdmissionController::new(limits(1, 1, 0, 0));
        let slot = admission.download(None).unwrap();

        // The work gate is untouched by a transfer in progress.
        let work = admission.enter(WorkClass::Ordinary).await.unwrap();
        drop(work);

        let refused = admission.download(None).unwrap_err();
        assert_eq!(refused.code(), ErrorCode::RateLimited);
        assert_eq!(
            refused.retry_after_seconds(),
            Some(DOWNLOAD_RETRY_AFTER_SECONDS)
        );

        drop(slot);
        admission.download(None).unwrap();
    }

    fn client(address: &str) -> Option<DownloadClient> {
        Some(DownloadClient::of(address.parse().unwrap()))
    }

    #[test]
    fn one_client_cannot_hold_every_download_slot() {
        let admission = AdmissionController::new(Admission {
            max_concurrent_downloads: 4,
            max_downloads_per_client: 2,
            ..Admission::new()
        });
        let first = admission.download(client("192.0.2.1")).unwrap();
        let second = admission.download(client("192.0.2.1")).unwrap();
        let refused = admission.download(client("192.0.2.1")).unwrap_err();
        assert_eq!(refused.code(), ErrorCode::RateLimited);
        assert!(refused.to_string().contains("this client"), "{refused}");

        // Another client is unaffected, until the deployment's limit binds.
        let _third = admission.download(client("192.0.2.2")).unwrap();
        let _fourth = admission.download(None).unwrap();
        let full = admission.download(client("192.0.2.3")).unwrap_err();
        assert!(full.to_string().contains("the server"), "{full}");

        // A finished download is given back to its client and to the server.
        drop(first);
        let again = admission.download(client("192.0.2.1")).unwrap();
        drop((second, again));
        assert!(
            lock(&admission.clients)
                .get(&client("192.0.2.1").unwrap())
                .is_none()
        );
    }

    #[test]
    fn an_ipv6_client_is_its_64_and_a_mapped_ipv4_client_is_itself() {
        assert_eq!(client("2001:db8:1:2::1"), client("2001:db8:1:2:ffff::9"));
        assert_ne!(client("2001:db8:1:2::1"), client("2001:db8:1:3::1"));
        assert_eq!(client("::ffff:192.0.2.1"), client("192.0.2.1"));
        assert_ne!(client("192.0.2.1"), client("192.0.2.2"));
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
