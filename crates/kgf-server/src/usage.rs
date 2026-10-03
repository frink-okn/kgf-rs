//! What a blocking worker spent on one piece of a request's work.
//!
//! Wall time alone cannot say whether the worker was computing or waiting: for a
//! mapped page to arrive from storage, for another request's bundle open to finish,
//! or for a core. The kernel keeps per-thread counters that can. CPU time is the part
//! of the wall time the thread actually ran, so the remainder is waiting, whatever
//! its cause. Page faults say how much of that waiting storage could explain.
//! Together they say whether admitting more concurrent work would keep storage busier
//! or only queue more computation.
//!
//! [`measure`] is the only way to read the counters, and it reads them on the thread
//! that runs the work, before and after it. A closure cannot await, so the work cannot
//! move to another thread partway, and the difference is that work's own. No reading
//! escapes to be compared against another thread's counters.
//!
//! Only Linux, where this server is deployed and tested, is read. FreeBSD and OpenBSD
//! count per thread as well but are untested here; macOS keeps no per-thread fault
//! counters. Elsewhere the usage is `None`, never a process-wide figure that every
//! concurrent request would share.

use std::ops::Add;
use std::time::{Duration, Instant};

/// What one piece of work spent.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Spent {
    /// Wall time from the work's start to its end.
    pub(crate) wall: Duration,
    /// The thread's counters across the work, where the platform keeps them.
    pub(crate) usage: Option<Usage>,
}

/// Per-thread counters across some work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Usage {
    /// Faults that waited for storage. Usually the page was not in the page cache;
    /// a fault that waited on another thread's read of the same page counts too, so
    /// concurrent requests can each count one read. A count of faults, not pages: a
    /// fault on a mapped file reads ahead around the page, so one major fault can
    /// bring in many pages.
    pub(crate) major_faults: u64,
    /// Faults resolved without storage: a mapped page already in the page cache,
    /// including one an earlier fault read ahead, or the first touch of freshly
    /// allocated memory, such as a growing response buffer.
    pub(crate) minor_faults: u64,
    /// Time the thread ran, in user and kernel mode together.
    pub(crate) cpu: Duration,
}

impl Add for Usage {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            major_faults: self.major_faults.saturating_add(other.major_faults),
            minor_faults: self.minor_faults.saturating_add(other.minor_faults),
            cpu: self.cpu.saturating_add(other.cpu),
        }
    }
}

/// Run `work` on this thread, and say what it spent.
pub(crate) fn measure<T>(work: impl FnOnce() -> T) -> (T, Spent) {
    let before = platform::read();
    let started = Instant::now();
    let output = work();
    let wall = started.elapsed();
    let usage = before
        .zip(platform::read())
        .map(|(before, after)| after.since(before));
    (output, Spent { wall, usage })
}

/// The calling thread's counters at one moment. Never leaves [`measure`].
#[derive(Debug, Clone, Copy)]
struct Reading {
    major_faults: u64,
    minor_faults: u64,
    cpu: Duration,
}

impl Reading {
    /// The change from `earlier`, a reading taken on the same thread.
    fn since(self, earlier: Self) -> Usage {
        Usage {
            major_faults: self.major_faults.saturating_sub(earlier.major_faults),
            minor_faults: self.minor_faults.saturating_sub(earlier.minor_faults),
            cpu: self.cpu.saturating_sub(earlier.cpu),
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::fmt;
    use std::sync::Once;
    use std::time::Duration;

    use nix::errno::Errno;
    use nix::sys::resource::{UsageWho, getrusage};
    use nix::sys::time::TimeVal;

    use super::Reading;

    /// The calling thread's counters, or `None` if they cannot be read.
    ///
    /// A failure here is not per request: a sandbox that refuses the call refuses
    /// it every time, and every record loses its usage. So it is reported, once,
    /// rather than leaving `null` fields indistinguishable from an unsupported
    /// platform.
    pub(super) fn read() -> Option<Reading> {
        static REPORTED: Once = Once::new();
        reading()
            .inspect_err(|error| {
                REPORTED.call_once(|| {
                    tracing::warn!(
                        %error,
                        "per-thread resource usage cannot be read; access records will carry \
                         no cpu_ms or page-fault counts",
                    );
                });
            })
            .ok()
    }

    fn reading() -> Result<Reading, Unreadable> {
        let usage = getrusage(UsageWho::RUSAGE_THREAD).map_err(Unreadable::Refused)?;
        let count = |value| u64::try_from(value).map_err(|_| Unreadable::OutOfRange);
        Ok(Reading {
            major_faults: count(usage.major_page_faults())?,
            minor_faults: count(usage.minor_page_faults())?,
            cpu: duration(usage.user_time())?
                .checked_add(duration(usage.system_time())?)
                .ok_or(Unreadable::OutOfRange)?,
        })
    }

    fn duration(time: TimeVal) -> Result<Duration, Unreadable> {
        let seconds = u64::try_from(time.tv_sec()).map_err(|_| Unreadable::OutOfRange)?;
        let micros = u64::try_from(time.tv_usec()).map_err(|_| Unreadable::OutOfRange)?;
        Duration::from_secs(seconds)
            .checked_add(Duration::from_micros(micros))
            .ok_or(Unreadable::OutOfRange)
    }

    /// Why the counters could not be read.
    #[derive(Debug)]
    enum Unreadable {
        /// The kernel refused `getrusage`.
        Refused(Errno),
        /// The kernel reported a negative or unrepresentable value.
        OutOfRange,
    }

    impl fmt::Display for Unreadable {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::Refused(errno) => {
                    write!(formatter, "getrusage(RUSAGE_THREAD) failed: {errno}")
                }
                Self::OutOfRange => {
                    formatter.write_str("getrusage(RUSAGE_THREAD) reported a value out of range")
                }
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    use super::Reading;

    pub(super) fn read() -> Option<Reading> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "linux")]
    fn touching_fresh_memory_counts_minor_faults_on_this_thread() {
        let ((), spent) = measure(|| {
            // Fresh anonymous pages fault in on first write, and none of them comes
            // from storage. 64 MiB is above the largest size glibc's allocator ever
            // serves from memory it has already touched (its mmap threshold rises with
            // frees, to at most 32 MiB), so the allocation is always newly mapped,
            // whatever other tests in the process freed first.
            let mut pages = vec![0_u8; 64 << 20];
            for page in pages.chunks_mut(4096) {
                page[0] = 1;
            }
            std::hint::black_box(&pages);
        });
        let usage = spent.usage.expect("Linux counts per thread");
        assert!(usage.minor_faults >= 1, "{usage:?}");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn waiting_takes_wall_time_but_not_cpu_time() {
        let ((), spent) = measure(|| std::thread::sleep(Duration::from_millis(50)));
        let usage = spent.usage.expect("Linux counts per thread");
        assert!(spent.wall >= Duration::from_millis(50), "{spent:?}");
        assert!(usage.cpu < spent.wall / 2, "{spent:?}");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn computing_takes_cpu_time() {
        let ((), spent) = measure(|| {
            let started = Instant::now();
            while started.elapsed() < Duration::from_millis(20) {
                std::hint::black_box(started);
            }
        });
        let usage = spent.usage.expect("Linux counts per thread");
        assert!(usage.cpu > Duration::ZERO, "{spent:?}");
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn other_platforms_time_the_work_and_record_no_usage() {
        let ((), spent) = measure(|| std::thread::sleep(Duration::from_millis(5)));
        assert!(spent.wall >= Duration::from_millis(5));
        assert!(spent.usage.is_none());
    }

    #[test]
    fn usage_adds_field_by_field() {
        let one = Usage {
            major_faults: 1,
            minor_faults: 10,
            cpu: Duration::from_millis(3),
        };
        assert_eq!(
            one + one,
            Usage {
                major_faults: 2,
                minor_faults: 20,
                cpu: Duration::from_millis(6),
            }
        );
    }
}
