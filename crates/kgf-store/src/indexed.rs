//! One indexed HDT: dictionary, SPO/POS/OPS, and triple-pattern resolution.
//!
//! This is the reusable boundary shared by a bundle's queryable data graph and
//! its VoID description graph. Both are an HDT plus the ordinary hdtc
//! permutation sidecar; their different roles belong to the [`Store`](crate::Store)
//! that composes them, not to the indexed reader.
//!
//! The type owns mappings and validated specs only. It builds no term maps or
//! other state proportional to the graph, so opening a second instance for
//! `stats/void.hdt` preserves the read layer's lazy multi-tenant memory model.

use crate::dict::{DictCounts, Dictionary};
use crate::error::Result;
use crate::map::Mapping;
use crate::pattern::{IdPattern, Selection};
use crate::perm::Permutations;

/// The reusable mapped read core for one HDT and its bound permutation index.
#[derive(Debug)]
pub(crate) struct IndexedHdt {
    permutations: Permutations,
}

impl IndexedHdt {
    /// Open and bind one HDT and its canonical permutation sidecar.
    pub(crate) fn open(hdt: Mapping, sidecar: Mapping) -> Result<Self> {
        Ok(Self {
            permutations: Permutations::open(hdt, sidecar)?,
        })
    }

    /// The dictionary projected from the host HDT.
    pub(crate) fn dict(&self) -> Dictionary<'_> {
        self.permutations
            .hdt_layout()
            .dictionary()
            .view(self.permutations.hdt_mapping())
    }

    /// The host's three permutations.
    pub(crate) fn permutations(&self) -> &Permutations {
        &self.permutations
    }

    /// Total triples in the indexed HDT.
    pub(crate) fn triples(&self) -> u64 {
        self.permutations.triples()
    }

    /// Dictionary sizes in the three role-scoped id spaces.
    pub(crate) fn dict_counts(&self) -> &DictCounts {
        self.permutations.dict_counts()
    }

    /// Resolve one triple pattern without enumerating it.
    pub(crate) fn resolve(&self, pattern: IdPattern) -> Result<Selection<'_>> {
        crate::pattern::resolve(&self.permutations, pattern)
    }
}
