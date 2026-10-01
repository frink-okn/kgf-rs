//! Page faults a blocking worker took while doing one request's work.
//!
//! A request's work time cannot say by itself whether the worker was computing or
//! waiting for a mapped page to arrive from storage: both are wall time on the same
//! thread. The kernel counts faults per thread, and a blocking task runs on one thread
//! from start to finish, so the change in that thread's counters across the work is
//! the request's own. A major fault read a page from storage; a minor fault mapped a
//! page that was already in memory. Together with `work_ms` they say whether more
//! concurrency would keep storage busier or only queue more computation.
//!
//! Only Linux keeps the counters per thread. Elsewhere nothing is recorded, rather
//! than a process-wide figure that every concurrent request would share.

/// Faults taken between two readings on one thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PageFaults {
    /// Pages read from storage.
    pub(crate) major: u64,
    /// Pages already in memory, mapped into this process.
    pub(crate) minor: u64,
}

/// The calling thread's fault counters at one moment.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ThreadFaults {
    major: u64,
    minor: u64,
}

impl ThreadFaults {
    /// Reads the calling thread's counters, or `None` where the platform does not
    /// keep them per thread or the reading failed.
    pub(crate) fn now() -> Option<Self> {
        platform::now()
    }

    /// Faults the calling thread took since `self` was read. Only meaningful on the
    /// thread that read `self`, which the blocking pool guarantees for one task.
    pub(crate) fn since(self) -> Option<PageFaults> {
        let now = Self::now()?;
        Some(PageFaults {
            major: now.major.saturating_sub(self.major),
            minor: now.minor.saturating_sub(self.minor),
        })
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use nix::sys::resource::{UsageWho, getrusage};

    pub(super) fn now() -> Option<super::ThreadFaults> {
        let usage = getrusage(UsageWho::RUSAGE_THREAD).ok()?;
        Some(super::ThreadFaults {
            major: u64::try_from(usage.major_page_faults()).ok()?,
            minor: u64::try_from(usage.minor_page_faults()).ok()?,
        })
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    pub(super) fn now() -> Option<super::ThreadFaults> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "linux")]
    fn touching_fresh_memory_counts_minor_faults_on_this_thread() {
        let before = ThreadFaults::now().expect("Linux counts faults per thread");
        // Fresh anonymous pages fault in on first write, and none of them comes from storage.
        // 64 MiB is above the largest size glibc's allocator ever serves from memory it has
        // already touched (its mmap threshold rises with frees, to at most 32 MiB), so the
        // allocation is always newly mapped, whatever other tests in the process freed first.
        let mut pages = vec![0_u8; 64 << 20];
        for page in pages.chunks_mut(4096) {
            page[0] = 1;
        }
        std::hint::black_box(&pages);
        let faults = before.since().expect("a second reading");
        assert!(faults.minor >= 1, "{faults:?}");
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn other_platforms_record_nothing() {
        assert!(ThreadFaults::now().is_none());
    }
}
