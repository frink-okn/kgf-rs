//! Lazy multi-tenant bundle catalog.
//!
//! A server points at a directory of bundles and serves all of them. The
//! catalog scans at startup and **opens nothing**; a version opens on its first
//! request, and opening is cheap enough that this is invisible.
//!
//! # Why this is only a map
//!
//! Reads take an `Arc<Store>` and never a lock. The catalog map is the only
//! synchronized structure in the crate, and it is touched once per request to
//! clone an `Arc` — never during evaluation. Eviction is dropping the `Arc`:
//! in-flight requests finish on their clone and the maps unmap when the last
//! one goes.
//!
//! Because an idle open bundle costs address space rather than resident memory,
//! eviction policy is about file descriptors and hygiene, not memory pressure.
//! Historical versions are ordinary entries; nothing distinguishes them from
//! current ones.
//!
//! # Budgets to watch
//!
//! Roughly one file descriptor per artifact per open bundle (raise `ulimit -n`
//! in deployment), and Linux's `vm.max_map_count` at hundreds of bundles. Both
//! are deployment notes, not design constraints.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use crate::error::{Error, Result};
use crate::map::{PublishedBundle, PublishedRoot};
use crate::store::{OpenOptions, Store};

/// A dataset and version, the catalog's key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BundleId {
    /// Dataset identifier.
    pub dataset: String,
    /// Version identifier.
    pub version: String,
}

/// The set of known bundles, opened on demand.
#[derive(Debug)]
pub struct Catalog {
    root: PublishedRoot,
    opts: OpenOptions,
    entries: BTreeMap<BundleId, CatalogEntry>,
}

#[derive(Debug)]
struct CatalogEntry {
    bundle: PublishedBundle,
    state: Mutex<EntryState>,
    ready: Condvar,
}

#[derive(Debug)]
enum EntryState {
    Closed,
    Opening(Arc<()>),
    Open(Arc<Store>),
    Failed(Arc<Error>),
}

impl Catalog {
    /// Scan `root` for `{dataset}/{version}/` directories without opening any.
    ///
    /// The [`PublishedRoot`] capability records the external immutability
    /// invariant required by every file-backed mapping the catalog may open.
    pub fn scan(root: PublishedRoot, opts: OpenOptions) -> Result<Self> {
        let mut entries = BTreeMap::new();
        for dataset_entry in directory_entries(root.path())? {
            let dataset = directory_name(&dataset_entry)?;
            for version_entry in directory_entries(&dataset_entry.path())? {
                let version = directory_name(&version_entry)?;
                entries.insert(
                    BundleId {
                        dataset: dataset.clone(),
                        version,
                    },
                    CatalogEntry {
                        bundle: root.bundle(version_entry.path()),
                        state: Mutex::new(EntryState::Closed),
                        ready: Condvar::new(),
                    },
                );
            }
        }
        Ok(Self {
            root,
            opts,
            entries,
        })
    }

    /// The directory whose dataset/version children were scanned.
    pub fn root(&self) -> &Path {
        self.root.path()
    }

    /// Every known bundle, whether open or not.
    pub fn ids(&self) -> Vec<BundleId> {
        self.entries.keys().cloned().collect()
    }

    /// The directory a scanned version was found in.
    ///
    /// For the non-artifact files beside the artifacts — `manifest.json` above
    /// all, which a server reads to describe a bundle it has not opened. It
    /// hands out the path the scan recorded rather than one composed from a
    /// caller's strings, so a dataset or version name arriving from a URL can
    /// never reach the filesystem.
    pub fn bundle_dir(&self, id: &BundleId) -> Result<&Path> {
        Ok(self.entry(id)?.bundle.path())
    }

    /// Get a bundle, opening it if this is its first request.
    ///
    /// A singleflight guard means concurrent first requests for the same
    /// version open it once rather than N times. Deterministic failures are
    /// cached with their classified source until eviction: published versions
    /// are immutable, so retrying the same files cannot repair them. Failures
    /// that say something about this process rather than about the bundle —
    /// descriptor pressure and the like — are not cached
    /// ([`Error::is_transient`]).
    pub fn get(&self, id: &BundleId) -> Result<Arc<Store>> {
        self.get_with(id, Store::open)
    }

    /// Whether this catalog currently holds an open store for `id`.
    ///
    /// This is an observational probe for request telemetry, not a promise
    /// that the next [`get`](Self::get) will or will not open: another thread
    /// may change the state immediately after this call. Unknown bundles and
    /// cached failures are not open.
    pub fn is_open(&self, id: &BundleId) -> bool {
        self.entries
            .get(id)
            .is_some_and(|entry| matches!(&*lock(entry), EntryState::Open(_)))
    }

    fn get_with<F>(&self, id: &BundleId, open: F) -> Result<Arc<Store>>
    where
        F: Fn(&PublishedBundle, OpenOptions) -> Result<Store>,
    {
        let entry = self.entry(id)?;
        loop {
            let mut state = lock(entry);
            match &*state {
                EntryState::Open(store) => return Ok(Arc::clone(store)),
                EntryState::Failed(error) => {
                    return Err(bundle_open_error(entry, Arc::clone(error)));
                }
                EntryState::Opening(_) => {
                    state = entry
                        .ready
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner);
                    drop(state);
                }
                EntryState::Closed => {
                    let mut claim = OpenClaim::begin(entry, &mut state);
                    drop(state);

                    return match open(&entry.bundle, self.opts) {
                        Ok(store) => {
                            let store = Arc::new(store);
                            claim.settle(EntryState::Open(Arc::clone(&store)));
                            Ok(store)
                        }
                        Err(error) => {
                            let error = Arc::new(error);
                            claim.settle(if error.is_transient() {
                                EntryState::Closed
                            } else {
                                EntryState::Failed(Arc::clone(&error))
                            });
                            Err(bundle_open_error(entry, error))
                        }
                    };
                }
            }
        }
    }

    /// Drop the catalog's reference to a bundle.
    ///
    /// In-flight requests finish on their own clone; the mapping is released
    /// when the last `Arc` goes.
    pub fn evict(&self, id: &BundleId) {
        let Some(entry) = self.entries.get(id) else {
            return;
        };
        *lock(entry) = EntryState::Closed;
        entry.ready.notify_all();
    }

    fn entry(&self, id: &BundleId) -> Result<&CatalogEntry> {
        self.entries.get(id).ok_or_else(|| Error::UnknownBundle {
            dataset: id.dataset.clone(),
            version: id.version.clone(),
        })
    }
}

/// The right to open one catalog entry, and the obligation to release it.
///
/// A claim is held across the `open` call, which deliberately runs without the
/// entry lock so that one slow open does not block lookups of other bundles.
/// That leaves a window in which this thread could unwind — `Store::open`
/// reports failure by `Result`, but an unimplemented path in this crate panics
/// by convention, and a panic here would otherwise leave the entry `Opening`
/// with nothing left to publish a result or wake the condvar. Every waiter for
/// that bundle would then block forever. [`Drop`] closes that window by
/// returning the entry to `Closed` and notifying.
struct OpenClaim<'a> {
    entry: &'a CatalogEntry,
    token: Arc<()>,
    settled: bool,
}

impl<'a> OpenClaim<'a> {
    /// Claim an entry the caller has found `Closed`, holding its lock.
    fn begin(entry: &'a CatalogEntry, state: &mut EntryState) -> Self {
        let token = Arc::new(());
        *state = EntryState::Opening(Arc::clone(&token));
        Self {
            entry,
            token,
            settled: false,
        }
    }

    /// Publish `next` if this claim still owns the entry, and wake waiters.
    ///
    /// A concurrent [`Catalog::evict`] replaces the state, which revokes the
    /// claim: the opened store then belongs to this caller alone and nothing is
    /// published. Waiters are notified either way, since they re-examine the
    /// state they wake to.
    fn settle(&mut self, next: EntryState) {
        self.settled = true;
        let mut state = lock(self.entry);
        if matches!(&*state, EntryState::Opening(current) if Arc::ptr_eq(current, &self.token)) {
            *state = next;
        }
        drop(state);
        self.entry.ready.notify_all();
    }
}

impl Drop for OpenClaim<'_> {
    fn drop(&mut self) {
        if !self.settled {
            self.settle(EntryState::Closed);
        }
    }
}

/// The immediate subdirectories of `path`, in whatever order the OS lists them.
///
/// Unordered on purpose: every caller funnels these into the [`Catalog`]'s
/// `BTreeMap`, which imposes the (dataset, version) order that [`Catalog::ids`]
/// reports. Sorting here would allocate an `OsString` per comparison for an
/// ordering the map re-derives.
///
fn directory_entries(path: &Path) -> Result<Vec<std::fs::DirEntry>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if resolves_to_directory(&entry)? && !is_in_progress(&entry) {
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// Whether a directory entry is a build in progress rather than a publication.
///
/// A producer stages on the same filesystem as its output so that publication
/// is a `rename` — `kgf build` does — which puts a half-written directory
/// inside the tree this scan walks.
/// Without this rule a server started mid-build lists that directory as a
/// release, and a request for it fails at open instead of at the catalog.
///
/// A leading `.` is the rule, and it is a rule rather than a convention: it is
/// the other half of the one `kgf build` enforces on a dataset id and a
/// version label, which refuse a leading `.` for exactly this reason. Between
/// them a staging directory can never be mistaken for a published one, in
/// either direction.
fn is_in_progress(entry: &std::fs::DirEntry) -> bool {
    entry
        .file_name()
        .as_encoded_bytes()
        .first()
        .is_some_and(|byte| *byte == b'.')
}

/// Whether a directory entry resolves to a directory.
///
/// Follows symlinks, unlike [`std::fs::DirEntry::file_type`]: a dataset or
/// version directory published as a symlink is an ordinary deployment shape,
/// and skipping it silently would leave every request for that bundle answering
/// `UnknownBundle` with no diagnostic. Artifacts *inside* a bundle already
/// resolve this way ([`crate::store`]), so this makes the two consistent.
///
/// A symlink with no target is not a bundle and is skipped like any other
/// non-directory. Anything else the OS refuses is reported rather than dropped,
/// since a permissions problem in a serving root is worth surfacing at startup.
fn resolves_to_directory(entry: &std::fs::DirEntry) -> Result<bool> {
    match std::fs::metadata(entry.path()) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn directory_name(entry: &std::fs::DirEntry) -> Result<String> {
    entry
        .file_name()
        .into_string()
        .map_err(|_| Error::NonUtf8BundlePath { path: entry.path() })
}

fn lock(entry: &CatalogEntry) -> MutexGuard<'_, EntryState> {
    entry.state.lock().unwrap_or_else(PoisonError::into_inner)
}

fn bundle_open_error(entry: &CatalogEntry, source: Arc<Error>) -> Error {
    Error::BundleOpen {
        bundle: entry.bundle.path().to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pattern::IdPattern;
    use crate::testing::{Fixture, TINY_NT, published_root};
    use std::sync::Barrier;

    #[test]
    fn scan_is_lazy_and_lists_only_dataset_version_directories() {
        let fixture = Fixture::build(TINY_NT);
        let root = tempfile::tempdir().unwrap();
        fixture.copy_bundle_to(&root.path().join("alpha/2026-01"));
        std::fs::create_dir_all(root.path().join("alpha/broken")).unwrap();
        std::fs::create_dir_all(root.path().join("zeta/1")).unwrap();
        std::fs::write(root.path().join("not-a-dataset"), b"ignored").unwrap();
        // A build staging on the same filesystem as its output, mid-`rename`.
        std::fs::create_dir_all(root.path().join("alpha/.kgf-build-2026-02")).unwrap();
        std::fs::create_dir_all(root.path().join(".kgf-staging/2026-01")).unwrap();

        let catalog = Catalog::scan(published_root(root.path()), OpenOptions::default()).unwrap();
        assert_eq!(catalog.root(), root.path());
        assert_eq!(
            catalog.ids(),
            vec![
                id("alpha", "2026-01"),
                id("alpha", "broken"),
                id("zeta", "1"),
            ]
        );
        assert!(
            catalog
                .entries
                .values()
                .all(|entry| matches!(*lock(entry), EntryState::Closed))
        );
        let first = catalog
            .get(&id("alpha", "broken"))
            .expect_err("broken bundle must fail");
        let second = catalog
            .get(&id("alpha", "broken"))
            .expect_err("failure must remain cached");
        let (Error::BundleOpen { source: first, .. }, Error::BundleOpen { source: second, .. }) =
            (first, second)
        else {
            panic!("catalog failures must preserve their store-open source");
        };
        assert!(Arc::ptr_eq(&first, &second));
        assert!(matches!(&*first, Error::MissingRequiredArtifact { .. }));
        assert!(matches!(
            catalog.get(&id("absent", "version")),
            Err(Error::UnknownBundle { .. })
        ));
    }

    #[test]
    fn concurrent_first_requests_share_one_open_store() {
        const THREADS: usize = 12;

        let fixture = Fixture::build(TINY_NT);
        let root = tempfile::tempdir().unwrap();
        fixture.copy_bundle_to(&root.path().join("dataset/version"));
        let catalog =
            Arc::new(Catalog::scan(published_root(root.path()), OpenOptions::default()).unwrap());
        let bundle = id("dataset", "version");
        assert!(!catalog.is_open(&bundle));
        assert!(!catalog.is_open(&id("absent", "version")));
        let barrier = Arc::new(Barrier::new(THREADS));

        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let catalog = Arc::clone(&catalog);
                let bundle = bundle.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    catalog.get(&bundle).unwrap()
                })
            })
            .collect();
        let stores: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();

        assert!(
            stores[1..]
                .iter()
                .all(|store| Arc::ptr_eq(&stores[0], store))
        );
        assert!(catalog.is_open(&bundle));
    }

    #[test]
    fn transient_open_failures_can_be_retried() {
        let fixture = Fixture::build(TINY_NT);
        let root = tempfile::tempdir().unwrap();
        fixture.copy_bundle_to(&root.path().join("dataset/version"));
        let catalog = Catalog::scan(published_root(root.path()), OpenOptions::default()).unwrap();
        let bundle = id("dataset", "version");
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let open = |bundle: &PublishedBundle, opts| {
            if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                Err(Error::Io(std::io::Error::other(
                    "temporary descriptor pressure",
                )))
            } else {
                Store::open(bundle, opts)
            }
        };

        let first = catalog
            .get_with(&bundle, open)
            .expect_err("the injected I/O failure must escape");
        assert!(matches!(
            first,
            Error::BundleOpen { source, .. } if matches!(&*source, Error::Io(_))
        ));

        let store = catalog
            .get_with(&bundle, open)
            .expect("a later request must retry transient failures");
        assert_eq!(store.triples(), 8);
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn a_corrupt_artifact_is_cached_even_though_it_fails_through_an_io_error() {
        // hdtc reads its headers with `read_exact`, so a short artifact fails
        // with an `io::Error` inside an anyhow chain. Classifying by type would
        // call that transient and re-open the whole bundle on every request,
        // forever, for a version that immutability guarantees cannot heal.
        let fixture = Fixture::build(TINY_NT);
        let root = tempfile::tempdir().unwrap();
        let bundle = root.path().join("dataset/version");
        fixture.copy_bundle_to(&bundle);
        std::fs::write(bundle.join(crate::store::artifact::PERM), [0u8; 100]).unwrap();

        let catalog = Catalog::scan(published_root(root.path()), OpenOptions::default()).unwrap();
        let id = id("dataset", "version");
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let open = |published: &PublishedBundle, opts| {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Store::open(published, opts)
        };

        let first = catalog.get_with(&id, open).expect_err("must refuse");
        let second = catalog.get_with(&id, open).expect_err("must stay refused");
        let (Error::BundleOpen { source: first, .. }, Error::BundleOpen { source: second, .. }) =
            (first, second)
        else {
            panic!("catalog failures must preserve their store-open source");
        };
        assert!(matches!(&*first, Error::Format(_)), "{first:#}");
        assert!(!first.is_transient());
        assert!(Arc::ptr_eq(&first, &second), "the failure must be cached");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn a_panic_while_opening_does_not_wedge_the_entry() {
        // `open` runs without the entry lock, so an unwind there would leave
        // the state `Opening` with nothing left to publish a result or wake the
        // condvar — every later request for this bundle would block forever.
        let fixture = Fixture::build(TINY_NT);
        let root = tempfile::tempdir().unwrap();
        fixture.copy_bundle_to(&root.path().join("dataset/version"));
        let catalog =
            Arc::new(Catalog::scan(published_root(root.path()), OpenOptions::default()).unwrap());
        let bundle = id("dataset", "version");

        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            catalog.get_with(
                &bundle,
                |_: &PublishedBundle, _: OpenOptions| -> Result<Store> {
                    panic!("an unimplemented open path panics by convention")
                },
            )
        }));
        std::panic::set_hook(hook);
        assert!(panicked.is_err(), "the panic must reach the caller");

        let entry = catalog.entry(&bundle).unwrap();
        assert!(
            matches!(*lock(entry), EntryState::Closed),
            "an abandoned open must release its claim"
        );

        // And another thread — which under the old code would have parked on
        // the condvar forever — opens it.
        let next = {
            let catalog = Arc::clone(&catalog);
            let bundle = bundle.clone();
            std::thread::spawn(move || catalog.get(&bundle).unwrap().triples())
        };
        assert_eq!(next.join().unwrap(), 8);
    }

    #[test]
    fn a_symlinked_version_directory_is_scanned() {
        let fixture = Fixture::build(TINY_NT);
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let published = store.path().join("2026-01");
        fixture.copy_bundle_to(&published);
        std::fs::create_dir_all(root.path().join("dataset")).unwrap();
        symlink(&published, &root.path().join("dataset/current"));
        // A symlink with no target is not a bundle, and must not abort the scan.
        symlink(
            &store.path().join("absent"),
            &root.path().join("dataset/dangling"),
        );

        let catalog = Catalog::scan(published_root(root.path()), OpenOptions::default()).unwrap();
        assert_eq!(catalog.ids(), vec![id("dataset", "current")]);
        assert_eq!(catalog.get(&id("dataset", "current")).unwrap().triples(), 8);
    }

    #[cfg(unix)]
    fn symlink(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).expect("create fixture symlink");
    }

    #[cfg(windows)]
    fn symlink(target: &Path, link: &Path) {
        std::os::windows::fs::symlink_dir(target, link).expect("create fixture symlink");
    }

    #[test]
    fn eviction_drops_only_the_catalog_reference() {
        let fixture = Fixture::build(TINY_NT);
        let root = tempfile::tempdir().unwrap();
        fixture.copy_bundle_to(&root.path().join("dataset/version"));
        let catalog = Catalog::scan(published_root(root.path()), OpenOptions::default()).unwrap();
        let bundle = id("dataset", "version");

        let in_flight = catalog.get(&bundle).unwrap();
        let old = Arc::downgrade(&in_flight);
        catalog.evict(&bundle);
        assert!(old.upgrade().is_some());

        let reopened = catalog.get(&bundle).unwrap();
        assert!(!Arc::ptr_eq(&in_flight, &reopened));
        drop(in_flight);
        assert!(old.upgrade().is_none());
        assert_eq!(reopened.triples(), 8);
    }

    #[test]
    fn mixed_reads_remain_exact_under_concurrent_eviction() {
        const BUNDLES: usize = 6;
        const THREADS: usize = 8;
        const ITERATIONS: usize = 80;

        let fixture = Fixture::build(TINY_NT);
        let root = tempfile::tempdir().unwrap();
        for index in 0..BUNDLES {
            fixture.copy_bundle_to(&root.path().join(format!("dataset-{index}/version")));
        }
        let catalog =
            Arc::new(Catalog::scan(published_root(root.path()), OpenOptions::default()).unwrap());
        let ids = Arc::new(catalog.ids());
        let barrier = Arc::new(Barrier::new(THREADS));

        let handles: Vec<_> = (0..THREADS)
            .map(|thread| {
                let catalog = Arc::clone(&catalog);
                let ids = Arc::clone(&ids);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    for iteration in 0..ITERATIONS {
                        let bundle = &ids[(thread + iteration) % ids.len()];
                        let store = catalog.get(bundle).unwrap();
                        if (thread + iteration) % 3 == 0 {
                            catalog.evict(bundle);
                        }
                        let pattern = match iteration % 5 {
                            0 => IdPattern {
                                subject: None,
                                predicate: None,
                                object: None,
                            },
                            1 => IdPattern {
                                subject: Some(1),
                                predicate: None,
                                object: None,
                            },
                            2 => IdPattern {
                                subject: None,
                                predicate: Some(1),
                                object: None,
                            },
                            3 => IdPattern {
                                subject: None,
                                predicate: None,
                                object: Some(1),
                            },
                            _ => IdPattern {
                                subject: Some(1),
                                predicate: None,
                                object: Some(1),
                            },
                        };
                        let selection = store.resolve(pattern).unwrap();
                        assert_eq!(
                            selection.count().value,
                            selection.page(0, usize::MAX).count() as u64
                        );
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        for bundle in ids.iter() {
            assert_eq!(catalog.get(bundle).unwrap().triples(), 8);
        }
    }

    fn id(dataset: &str, version: &str) -> BundleId {
        BundleId {
            dataset: dataset.to_owned(),
            version: version.to_owned(),
        }
    }
}
