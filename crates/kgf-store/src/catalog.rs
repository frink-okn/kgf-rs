//! Lazy multi-tenant bundle catalog.
//!
//! A server points at a directory of bundles and serves all of them. The
//! catalog scans at startup and **opens nothing**; a version opens on its first
//! request, and opening is cheap enough that this is invisible (doc 20 §20.6).
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
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use crate::error::{Error, Result};
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
    root: PathBuf,
    opts: OpenOptions,
    entries: BTreeMap<BundleId, CatalogEntry>,
}

#[derive(Debug)]
struct CatalogEntry {
    path: PathBuf,
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
    pub fn scan(root: &Path, opts: OpenOptions) -> Result<Self> {
        let mut entries = BTreeMap::new();
        for dataset_entry in directory_entries(root)? {
            let dataset = directory_name(&dataset_entry)?;
            for version_entry in directory_entries(&dataset_entry.path())? {
                let version = directory_name(&version_entry)?;
                entries.insert(
                    BundleId {
                        dataset: dataset.clone(),
                        version,
                    },
                    CatalogEntry {
                        path: version_entry.path(),
                        state: Mutex::new(EntryState::Closed),
                        ready: Condvar::new(),
                    },
                );
            }
        }
        Ok(Self {
            root: root.to_path_buf(),
            opts,
            entries,
        })
    }

    /// The directory whose dataset/version children were scanned.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Every known bundle, whether open or not.
    pub fn ids(&self) -> Vec<BundleId> {
        self.entries.keys().cloned().collect()
    }

    /// Get a bundle, opening it if this is its first request.
    ///
    /// A singleflight guard means concurrent first requests for the same
    /// version open it once rather than N times. A failed open is cached with
    /// its classified source until eviction: published versions are immutable,
    /// so retrying the same files on every request cannot repair the failure.
    pub fn get(&self, id: &BundleId) -> Result<Arc<Store>> {
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
                    let token = Arc::new(());
                    *state = EntryState::Opening(Arc::clone(&token));
                    drop(state);

                    let opened = Store::open(&entry.path, self.opts).map(Arc::new);
                    let mut state = lock(entry);
                    let is_current = matches!(
                        &*state,
                        EntryState::Opening(current) if Arc::ptr_eq(current, &token)
                    );
                    match opened {
                        Ok(store) => {
                            if is_current {
                                *state = EntryState::Open(Arc::clone(&store));
                                entry.ready.notify_all();
                            }
                            return Ok(store);
                        }
                        Err(error) => {
                            let error = Arc::new(error);
                            if is_current {
                                *state = EntryState::Failed(Arc::clone(&error));
                                entry.ready.notify_all();
                            }
                            return Err(bundle_open_error(entry, error));
                        }
                    }
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

fn directory_entries(path: &Path) -> Result<Vec<std::fs::DirEntry>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            entries.push(entry);
        }
    }
    entries.sort_unstable_by_key(std::fs::DirEntry::file_name);
    Ok(entries)
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
        bundle: entry.path.clone(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pattern::IdPattern;
    use crate::testing::{Fixture, TINY_NT};
    use std::sync::Barrier;

    #[test]
    fn scan_is_lazy_and_lists_only_dataset_version_directories() {
        let fixture = Fixture::build(TINY_NT);
        let root = tempfile::tempdir().unwrap();
        fixture.copy_bundle_to(&root.path().join("alpha/2026-01"));
        std::fs::create_dir_all(root.path().join("alpha/broken")).unwrap();
        std::fs::create_dir_all(root.path().join("zeta/1")).unwrap();
        std::fs::write(root.path().join("not-a-dataset"), b"ignored").unwrap();

        let catalog = Catalog::scan(root.path(), OpenOptions::default()).unwrap();
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
        let catalog = Arc::new(Catalog::scan(root.path(), OpenOptions::default()).unwrap());
        let bundle = id("dataset", "version");
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
    }

    #[test]
    fn eviction_drops_only_the_catalog_reference() {
        let fixture = Fixture::build(TINY_NT);
        let root = tempfile::tempdir().unwrap();
        fixture.copy_bundle_to(&root.path().join("dataset/version"));
        let catalog = Catalog::scan(root.path(), OpenOptions::default()).unwrap();
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
        let catalog = Arc::new(Catalog::scan(root.path(), OpenOptions::default()).unwrap());
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
