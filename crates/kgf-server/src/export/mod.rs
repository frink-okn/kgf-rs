//! Whole-artifact downloads: `/{dataset}/v/{version}/export/{artifact}`, where
//! `{artifact}` is the artifact's name in the bundle — `data.hdt`,
//! `filters/subjects.minhash`, `keysets/shared.keys`.
//!
//! Two kinds of artifact are served, for two kinds of reader. `data.hdt` is the
//! dataset itself, for someone who would rather hold it than query it. The
//! membership filters, overlap sketches and key sets under `filters/` and
//! `keysets/` are what a reader *without* the bundle works from — a federation
//! index deciding which datasets a question is worth putting to, and what two
//! of them share. Nothing this server answers reads those, so serving them
//! whole is the only way they reach the reader they were built for; which of
//! them a release has is whatever its manifest lists and its bundle holds.
//!
//! Static-file semantics over a mapped, publication-verified artifact, built to
//! the standard of a well-configured file server because the transfers are
//! large enough that every property matters:
//!
//! - **Resumable.** Byte ranges (RFC 9110 §14), single-part and multipart, with
//!   `If-Range` so a resume can never splice two different files.
//! - **Verifiable.** The `ETag` *is* the artifact's SHA-256 from the manifest,
//!   and every identity response carries it again as `Repr-Digest` (RFC 9530),
//!   which digests the whole representation — so a file assembled from several
//!   transfers is checked against the one value.
//! - **The same everywhere.** Unlike every other validator this server issues,
//!   the tag does not mix in the deployment: these bytes do not depend on the
//!   configuration or the code, so any server or mirror publishing the bundle
//!   agrees on the tag, and a client may resume from whichever it reaches.
//! - **Uncompressed unless the client prefers otherwise.** Only an identity
//!   body has a length, byte ranges, and a strong validator, which is what a
//!   browser needs to show progress and resume. A client that weights zstd or
//!   gzip above identity gets a coded full body; a range, and a request that
//!   asserts the artifact is unchanged, are always served from the identity
//!   bytes, because a range of a compressed stream is only meaningful if the
//!   compressor reproduces that stream exactly, and nothing promises that. A
//!   client that decodes as it stores holds identity bytes, so the length of
//!   its partial file is a valid resume offset. The federation artifacts do
//!   not compress, so they are always identity.
//!
//! Nothing is ever held whole: every length, validator and digest comes from
//! the manifest, read and checked against the bundle's files at startup, and
//! the body copies out of the mapping one chunk at a time. That matters more for the federation artifacts than for
//! most HDTs — a large dataset's filter or key set runs to gigabytes.
//!
//! A download has one representation per coding and no page: a browser that
//! follows a link to `data.hdt` must get the file. That makes these routes the
//! one exception to "every URL answers HTML too".
//!
//! Because the coding decides the validator and which length and digest
//! headers can be sent, the negotiation is done here, and the routes are
//! mounted outside the compression layer every other resource passes through
//! and the middleware that weakens every other `ETag`.

mod body;
mod coding;
mod range;

use std::collections::BTreeMap;
use std::io;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::body::Body;
use axum::http::header::{
    ACCEPT_ENCODING, ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_ENCODING, CONTENT_LENGTH,
    CONTENT_RANGE, CONTENT_TYPE, RANGE, VARY,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use headers::{
    ETag, HeaderMapExt as _, IfMatch, IfModifiedSince, IfNoneMatch, IfRange, IfUnmodifiedSince,
    LastModified,
};
use kgf_store::manifest::Manifest;
use kgf_store::store::artifact;
use kgf_store::{KeyArtifact, PublishedFile, Store};

use crate::access::{Ledger, RequestShape};
use crate::admission::DownloadSlot;
use crate::envelope::{ErrorCode, Problem};
use crate::hex;
use crate::representation::CachePolicy;

use body::{Download, Segment, Source, Unmeasured};
use coding::ContentCoding;
use range::{ByteRangeSet, RangeSelection};

/// RFC 9530's field carrying the digest of the selected representation.
const REPR_DIGEST: HeaderName = HeaderName::from_static("repr-digest");

/// An artifact this server serves whole.
///
/// A closed vocabulary, not whatever a manifest happens to list. Every name
/// below is one the bundle layout defines, so a request path is matched
/// against names this build knows and never becomes a filesystem path; the
/// access record can describe a download by an artifact name that is not the
/// client's text; and adding a kind of download is a decision made here
/// rather than a side effect of what a publisher wrote. Which of these a
/// given release serves is a separate question, answered by its manifest and
/// checked against its bundle: see [`Exports`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExportArtifact {
    /// `data.hdt`: the dataset's triples and dictionary, standard HDT. Every
    /// bundle has one.
    Hdt,
    /// A membership filter or overlap sketch under `filters/`, or a key set
    /// under `keysets/`. A bundle carries whichever of them it was built with.
    Keys(KeyArtifact),
}

impl ExportArtifact {
    /// Every artifact this server can export, `data.hdt` first and the rest in
    /// name order.
    pub fn all() -> impl Iterator<Item = Self> {
        std::iter::once(Self::Hdt).chain(KeyArtifact::ALL.map(Self::Keys))
    }

    /// The artifact a request path names below `export/`, if it is one this
    /// server exports.
    ///
    /// An exact match against the vocabulary, on the path as the client spelled
    /// it: nothing is decoded or normalised, so `filters%2Fsubjects.filter`,
    /// `filters//subjects.filter` and `filters/../data.hdt` name nothing. Every
    /// name is spelled in characters a path never needs to escape, so its own
    /// spelling is its one URL.
    pub fn from_path(path: &str) -> Option<Self> {
        Self::all().find(|artifact| artifact.name() == path)
    }

    /// The artifact's name in the bundle, which is also its path below
    /// `export/` and its key in the manifest's `artifacts`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Hdt => artifact::HDT,
            Self::Keys(key) => key.name(),
        }
    }

    /// The media type the artifact is served as.
    ///
    /// `application/vnd.hdt` is the type the HDT specification proposes;
    /// clients that do not recognise it still receive the bytes, which is what
    /// `Content-Disposition` is for. The filter, sketch and key-set formats
    /// have no media type of their own, and a reader identifies each by the
    /// magic and header its format defines, so they are plain bytes.
    pub fn media_type(self) -> &'static str {
        match self {
            Self::Hdt => "application/vnd.hdt",
            Self::Keys(_) => "application/octet-stream",
        }
    }

    /// Whether a client may be sent this artifact content-coded.
    ///
    /// `data.hdt` shrinks to between a fifth and a half under zstd's default
    /// level, so a client that weights a coding above identity gets one. A
    /// filter, a sketch and a key set are hashed keys packed tight: the same
    /// coder saves nothing on a sketch or a key set and 5–7% on a large filter.
    /// Coding one would spend the shared blocking pool's CPU on gigabytes for
    /// that, and cost the client a length and a strong validator, so they are
    /// always sent as published and their responses do not vary by
    /// `Accept-Encoding`.
    pub fn compressible(self) -> bool {
        matches!(self, Self::Hdt)
    }

    /// The name a download of this artifact is saved under:
    /// `{dataset}-{version}.{suffix}`, so a saved file says which release it
    /// holds — `tox-v1.hdt`, `tox-v1.subjects.minhash`.
    pub fn saved_name(self, dataset: &str, version: &str) -> String {
        format!(
            "{}-{}.{}",
            file_name_part(dataset),
            file_name_part(version),
            self.suffix()
        )
    }

    /// What follows the release in a saved file's name. A key file keeps its
    /// own file name, role and extension both: the extension alone would save
    /// a release's subject and object filters over each other.
    fn suffix(self) -> &'static str {
        match self {
            Self::Hdt => "hdt",
            Self::Keys(key) => {
                let name = key.name();
                name.rsplit_once('/').map_or(name, |(_, file)| file)
            }
        }
    }
}

/// The downloadable files beside a release's manifest, as its bundle directory
/// held them when the manifest was read.
///
/// One `stat` at startup for each name in the vocabulary the manifest lists —
/// at most eleven a release — and no file is opened. It lets a download the
/// manifest promises be checked against the bytes it describes before any
/// request arrives, so that every request for it is answered alike: a
/// revalidation or a range is decided from the manifest without opening the
/// bundle, and could otherwise answer `304` or `206` for a file a `GET` would
/// find missing.
#[derive(Debug, Clone, Default)]
pub(crate) struct ExportFiles(BTreeMap<ExportArtifact, OnDisk>);

/// What a bundle directory holds under one artifact's name.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OnDisk {
    /// A regular file of this many bytes.
    File(u64),
    /// Nothing.
    Missing,
    /// Something that is not a regular file, or could not be examined: why.
    Unusable(String),
}

impl ExportFiles {
    /// Examine `bundle_dir` for each exportable artifact `manifest` lists.
    ///
    /// Every path is a vocabulary name joined to the bundle's own directory,
    /// never text a client sent. Links are followed, as mapping would follow
    /// them.
    pub(crate) fn scan(bundle_dir: &Path, manifest: &Manifest) -> Self {
        let files = ExportArtifact::all()
            .filter(|artifact| manifest.artifacts.contains_key(artifact.name()))
            .map(|artifact| {
                let state = match std::fs::metadata(bundle_dir.join(artifact.name())) {
                    Ok(metadata) if metadata.is_file() => OnDisk::File(metadata.len()),
                    Ok(_) => OnDisk::Unusable("it is not a regular file".to_owned()),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => OnDisk::Missing,
                    Err(error) => OnDisk::Unusable(format!("it could not be examined: {error}")),
                };
                (artifact, state)
            })
            .collect();
        Self(files)
    }

    /// A bundle that agrees with `manifest`: every listed download present at
    /// its listed length. For tests of what follows from a manifest alone.
    #[cfg(test)]
    pub(crate) fn as_listed(manifest: &Manifest) -> Self {
        let files = ExportArtifact::all()
            .filter_map(|artifact| {
                let entry = manifest.artifacts.get(artifact.name())?;
                Some((artifact, OnDisk::File(entry.bytes)))
            })
            .collect();
        Self(files)
    }
}

/// What one release serves whole, and what its manifest lists that cannot be.
///
/// The manifest is the authority on what exists. It is what a reader without
/// the bundle has, so "listed there" and "served here" are one statement; and
/// the length and digest it gives are the validators a download sends, so an
/// artifact it does not describe could not be served verifiably anyway.
///
/// `data.hdt` is required of every bundle, so a manifest that does not
/// identify it is refused, as any other defect in what a release requires is.
/// Any other trouble with a listed download — a checksum that is not one, no
/// file in the bundle, a file of another length — withholds that download and
/// nothing else, because no query reads it: one release's broken sketch must
/// not keep every dataset of a deployment from starting. A withheld download
/// is never quietly a `404`, since the manifest told its reader it is there:
/// every request for it is a `500` naming the defect, whatever its method or
/// preconditions.
#[derive(Debug, Clone)]
pub(crate) struct Exports {
    served: Arc<[(ExportArtifact, ArtifactIdentity)]>,
    withheld: Vec<(ExportArtifact, String)>,
}

/// What a release does with one artifact of the vocabulary.
#[derive(Debug, Clone, Copy)]
pub enum Export<'a> {
    /// Served, under the length and digest its downloads carry.
    Served(&'a ArtifactIdentity),
    /// Listed by the manifest but not servable as listed, for this reason.
    Withheld(&'a str),
    /// Not listed: this release has no such artifact.
    Unlisted,
}

impl Exports {
    /// Reconcile `manifest`'s downloads with the `files` beside it.
    pub(crate) fn new(manifest: &Manifest, files: &ExportFiles) -> Result<Self, String> {
        let mut served = Vec::new();
        let mut withheld = Vec::new();
        for artifact in ExportArtifact::all() {
            let required = artifact == ExportArtifact::Hdt;
            let name = artifact.name();
            let Some(entry) = manifest.artifacts.get(name) else {
                if required {
                    return Err(format!(
                        "the manifest lists no size and checksum for {name}; regenerate it with \
                         `kgf manifest`"
                    ));
                }
                continue;
            };
            let Some(sha256) = hex::decode::<32>(&entry.sha256) else {
                let defect = format!(
                    "the manifest's checksum for {name} is not 64 lowercase hex digits; \
                     regenerate it with `kgf manifest`"
                );
                if required {
                    return Err(defect);
                }
                withheld.push((artifact, defect));
                continue;
            };
            let listed = entry.bytes;
            let defect = match files.0.get(&artifact) {
                Some(OnDisk::File(len)) if *len == listed => {
                    served.push((artifact, ArtifactIdentity::new(listed, sha256)));
                    continue;
                }
                Some(OnDisk::File(len)) => format!(
                    "the bundle's {name} is {len} bytes and its manifest lists {listed}; the two \
                     were written apart, so regenerate the manifest with `kgf manifest`"
                ),
                Some(OnDisk::Unusable(why)) => format!("the bundle's {name} is unusable: {why}"),
                Some(OnDisk::Missing) | None => format!(
                    "the bundle has no {name}, which its manifest lists; the two were written \
                     apart, so regenerate the manifest with `kgf manifest`"
                ),
            };
            withheld.push((artifact, defect));
        }
        Ok(Self {
            served: served.into(),
            withheld,
        })
    }

    /// What this release does with `artifact`.
    pub(crate) fn get(&self, artifact: ExportArtifact) -> Export<'_> {
        if let Some((_, identity)) = self.served.iter().find(|(served, _)| *served == artifact) {
            return Export::Served(identity);
        }
        match self
            .withheld
            .iter()
            .find(|(withheld, _)| *withheld == artifact)
        {
            Some((_, defect)) => Export::Withheld(defect),
            None => Export::Unlisted,
        }
    }

    /// Everything served, in vocabulary order: `data.hdt` first, when it is.
    pub(crate) fn served(&self) -> &Arc<[(ExportArtifact, ArtifactIdentity)]> {
        &self.served
    }

    /// Everything listed and not served, each with why.
    pub(crate) fn withheld(&self) -> &[(ExportArtifact, String)] {
        &self.withheld
    }
}

/// A download's bytes, mapped and checked against the length its manifest
/// gives.
pub(crate) enum Mapped {
    /// `data.hdt`, through the store, whose mapping every query reads.
    Hdt(Arc<Store>),
    /// A key file, mapped for this download alone.
    Keys(KeyArtifact, PublishedFile),
}

impl Mapped {
    /// Map `artifact` from `store` and check it against `len`, the length its
    /// manifest gives.
    ///
    /// Startup found the file at that length, so neither check fails unless a
    /// bundle changed under a running server — which a published bundle must
    /// not do — or the mapping itself fails. Either is logged and refused,
    /// because serving would send nothing, or bytes under a digest they do not
    /// have. Presence and length are what can be checked without reading the
    /// file; the digest is `kgf manifest --check`'s job. Maps, so it belongs on
    /// a blocking thread.
    pub(crate) fn open(
        store: Arc<Store>,
        artifact: ExportArtifact,
        len: u64,
    ) -> Result<Self, Problem> {
        let name = artifact.name();
        let mapped = match artifact {
            ExportArtifact::Hdt => Self::Hdt(store),
            ExportArtifact::Keys(key) => match store.map_key_artifact(key) {
                Ok(Some(file)) => Self::Keys(key, file),
                Ok(None) => {
                    tracing::error!(
                        artifact = name,
                        "an exported artifact is missing from its bundle"
                    );
                    return Err(Problem::new(
                        ErrorCode::InternalError,
                        format!("{name} is listed in the manifest but missing from the bundle"),
                    ));
                }
                Err(error) => {
                    tracing::error!(
                        artifact = name,
                        %error,
                        "an exported artifact could not be mapped"
                    );
                    return Err(Problem::new(
                        ErrorCode::InternalError,
                        format!("{name} could not be read from the bundle"),
                    ));
                }
            },
        };
        let mapped_len = mapped.bytes().len() as u64;
        if mapped_len != len {
            tracing::error!(
                artifact = name,
                mapped = mapped_len,
                manifest = len,
                "an exported artifact's length disagrees with its manifest",
            );
            return Err(Problem::new(
                ErrorCode::InternalError,
                format!("{name} does not match the length its manifest publishes"),
            ));
        }
        Ok(mapped)
    }
}

impl Source for Mapped {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Hdt(store) => store.hdt_bytes(),
            Self::Keys(_, file) => file.as_bytes(),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Hdt(_) => artifact::HDT,
            Self::Keys(key, _) => key.name(),
        }
    }
}

/// What a release's manifest says about an exported artifact: its length and
/// its SHA-256, and every header value derived from the digest.
///
/// Read once at startup, so every validator, length, and digest a download
/// sends is fixed before any request arrives: no request hashes a file, and
/// none formats a digest either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactIdentity {
    len: u64,
    sha256_hex: String,
    strong: ETag,
    weak: ETag,
    repr_digest: HeaderValue,
}

impl ArtifactIdentity {
    /// An artifact of `len` bytes with this digest.
    pub(crate) fn new(len: u64, sha256: [u8; 32]) -> Self {
        let sha256_hex = hex::encode(&sha256);
        let tag = |prefix: &str| {
            format!("{prefix}\"sha256:{sha256_hex}\"")
                .parse()
                .unwrap_or_else(|error| unreachable!("a hex digest is a valid entity tag: {error}"))
        };
        let encoded = base64::engine::general_purpose::STANDARD.encode(sha256);
        Self {
            len,
            strong: tag(""),
            weak: tag("W/"),
            repr_digest: HeaderValue::from_str(&format!("sha-256=:{encoded}:"))
                .expect("base64 is a valid header value"),
            sha256_hex,
        }
    }

    /// The artifact's size in bytes.
    pub fn size(&self) -> u64 {
        self.len
    }

    /// The artifact's SHA-256, as lowercase hex.
    pub fn sha256_hex(&self) -> &str {
        &self.sha256_hex
    }

    /// The strong validator of the identity bytes.
    fn etag(&self) -> &ETag {
        &self.strong
    }

    /// The validator of a content-coded body: weak, because the encoded bytes
    /// are equivalent to the artifact rather than identical to anything fixed.
    fn weak_etag(&self) -> &ETag {
        &self.weak
    }

    /// `Repr-Digest`'s structured-field value.
    fn repr_digest(&self) -> &HeaderValue {
        &self.repr_digest
    }

    /// A multipart boundary that cannot occur in this artifact's bytes except
    /// by a collision in its own digest.
    fn boundary(&self) -> String {
        format!("kgf-byteranges-{}", &self.sha256_hex[..32])
    }
}

/// An instant as `Last-Modified` can state it: whole seconds.
///
/// An HTTP date has one-second resolution. Comparing a conditional request's
/// date against an instant with a fractional part would find every artifact
/// modified a fraction after the moment the client was told it was.
pub(crate) fn http_instant(seconds_since_epoch: i64) -> Option<SystemTime> {
    let seconds = u64::try_from(seconds_since_epoch).ok()?;
    SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

/// The request headers a download answers to.
///
/// Every precondition is read with `headers`' typed parser, and a field that
/// does not parse is treated as absent — RFC 9110 §13.1 has a server ignore a
/// precondition it cannot evaluate, which here means sending the full response
/// rather than a wrong `304` or `412`.
#[derive(Debug, Default)]
pub(crate) struct Conditions {
    if_match: Option<IfMatch>,
    if_unmodified_since: Option<IfUnmodifiedSince>,
    if_none_match: Option<IfNoneMatch>,
    if_modified_since: Option<IfModifiedSince>,
    if_range: Option<IfRange>,
    range: Option<ByteRangeSet>,
    accept_encoding: Option<String>,
}

impl Conditions {
    /// Read the headers of a `GET` or `HEAD`.
    ///
    /// `Range` is read for `GET` only: RFC 9110 §14.2 defines range handling
    /// for no other method, and a `HEAD` must describe the full `GET`. A
    /// `Range` repeated across field lines is not a list the grammar allows,
    /// so it is ignored like any other invalid one. An `Accept-Encoding` that
    /// is not text is treated as absent, which selects identity — the one
    /// coding every client decodes — and so is any `Accept-Encoding` for an
    /// artifact that is never coded (see [`ExportArtifact::compressible`]).
    pub(crate) fn read(headers: &HeaderMap, method: &Method, artifact: ExportArtifact) -> Self {
        let range = if method == Method::GET {
            let mut values = headers.get_all(RANGE).iter();
            match (values.next(), values.next()) {
                (Some(value), None) => value.to_str().ok().and_then(ByteRangeSet::parse),
                _ => None,
            }
        } else {
            None
        };
        let accept_encoding = headers
            .get_all(ACCEPT_ENCODING)
            .iter()
            .map(|value| value.to_str().ok())
            .collect::<Option<Vec<_>>>()
            .and_then(|lines| (!lines.is_empty()).then(|| lines.join(", ")))
            .filter(|_| artifact.compressible());
        Self {
            if_match: headers.typed_get(),
            if_unmodified_since: headers.typed_get(),
            if_none_match: headers.typed_get(),
            if_modified_since: headers.typed_get(),
            if_range: headers.typed_get(),
            range,
            accept_encoding,
        }
    }

    /// Decide the response, in RFC 9110 §13.2.2's order.
    ///
    /// Which representation the preconditions are evaluated against is settled
    /// first, because an identity body and a coded one carry different
    /// validators. `If-Range` goes first of all: an honoured range is served
    /// from the identity bytes. So is a request carrying `If-Match` or
    /// `If-Unmodified-Since`, which asserts the artifact is unchanged — a
    /// coded body's tag is weak and states no date, so against one `If-Match`
    /// could never pass and `If-Unmodified-Since` could never be evaluated, and
    /// an unchanged artifact would answer `412` to any client that also accepts
    /// compression. Only a request asserting neither may be coded, and a coded
    /// representation states no modification date, so `If-Modified-Since` does
    /// not bind it (§13.1.4 evaluates it only against one that has one).
    pub(crate) fn decide(
        &self,
        identity: &ArtifactIdentity,
        last_modified: Option<SystemTime>,
    ) -> Decision {
        let strong = identity.etag();
        let mut ranges = self.range.as_ref();
        if ranges.is_some()
            && let Some(if_range) = &self.if_range
        {
            // A date at or after `Last-Modified` is accepted as well as an exact
            // one. A versioned artifact never changes, so any date the client
            // could hold names these same bytes.
            let stated = last_modified.map(LastModified::from);
            if if_range.is_modified(Some(strong), stated.as_ref()) {
                ranges = None;
            }
        }
        let asserts_unchanged = self.if_match.is_some() || self.if_unmodified_since.is_some();
        let coding = if ranges.is_some() || asserts_unchanged {
            ContentCoding::Identity
        } else {
            ContentCoding::negotiate(self.accept_encoding.as_deref())
        };
        let (tag, modified) = match coding {
            ContentCoding::Identity => (strong, last_modified),
            ContentCoding::Zstd | ContentCoding::Gzip => (identity.weak_etag(), None),
        };

        if let Some(if_match) = &self.if_match {
            if !if_match.precondition_passes(tag) {
                return Decision::PreconditionFailed;
            }
        } else if let (Some(since), Some(modified)) = (&self.if_unmodified_since, modified)
            && !since.precondition_passes(modified)
        {
            return Decision::PreconditionFailed;
        }
        if let Some(if_none_match) = &self.if_none_match {
            if !if_none_match.precondition_passes(tag) {
                return Decision::NotModified { coding };
            }
        } else if let (Some(since), Some(modified)) = (&self.if_modified_since, modified)
            && !since.is_modified(modified)
        {
            return Decision::NotModified { coding };
        }

        let extent = match ranges.map(|set| set.select(identity.len)) {
            None => Extent::Whole,
            Some(RangeSelection::One(span)) => Extent::One(span),
            Some(RangeSelection::Several(spans)) => Extent::Several(spans),
            Some(RangeSelection::Unsatisfiable) => {
                return Decision::RangeNotSatisfiable { excessive: false };
            }
            Some(RangeSelection::Excessive) => {
                return Decision::RangeNotSatisfiable { excessive: true };
            }
        };
        Decision::Send { coding, extent }
    }

    /// The access record's description of this request: what it asked for,
    /// never what it named.
    pub(crate) fn shape(&self, artifact: ExportArtifact, decision: &Decision) -> RequestShape {
        let coding = match decision {
            Decision::NotModified { coding } | Decision::Send { coding, .. } => *coding,
            Decision::PreconditionFailed | Decision::RangeNotSatisfiable { .. } => {
                ContentCoding::Identity
            }
        };
        RequestShape::Export {
            artifact: artifact.name(),
            coding: coding.token(),
            ranges: self.range.as_ref().map_or(0, |set| set.len() as u64),
        }
    }
}

/// What a download request is answered with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// `304`: the client holds this representation already.
    NotModified { coding: ContentCoding },
    /// `412`: `If-Match` or `If-Unmodified-Since` failed.
    PreconditionFailed,
    /// `416`: no requested byte is in the artifact, or more parts were asked
    /// for than one response carries.
    RangeNotSatisfiable { excessive: bool },
    /// `200` or `206`.
    Send {
        coding: ContentCoding,
        extent: Extent,
    },
}

/// Which bytes of the artifact a response carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Extent {
    /// All of them: `200`.
    Whole,
    /// One span: a single-part `206`.
    One(Range<u64>),
    /// Several disjoint spans, in request order: a `multipart/byteranges`
    /// `206`.
    Several(Vec<Range<u64>>),
}

/// One release's download of one artifact: everything its headers are built
/// from, fixed before the bundle is opened.
#[derive(Debug)]
pub(crate) struct Delivery<'a> {
    pub(crate) artifact: ExportArtifact,
    pub(crate) identity: &'a ArtifactIdentity,
    pub(crate) last_modified: Option<SystemTime>,
    /// The saved file's name: `{dataset}-{version}.{suffix}`.
    pub(crate) file_name: String,
}

impl<'a> Delivery<'a> {
    /// Describe `artifact` of one release.
    pub(crate) fn new(
        dataset: &str,
        version: &str,
        artifact: ExportArtifact,
        identity: &'a ArtifactIdentity,
        last_modified: Option<SystemTime>,
    ) -> Self {
        Self {
            artifact,
            identity,
            last_modified,
            file_name: artifact.saved_name(dataset, version),
        }
    }

    /// The `304` for a client that already holds the representation.
    ///
    /// Carries what RFC 9110 §15.4.5 asks of one — the validator, caching
    /// directives, and `Vary` the `200` would have sent — and nothing else.
    pub(crate) fn not_modified(&self, coding: ContentCoding) -> Response {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        self.validation_headers(response.headers_mut(), coding);
        response
    }

    /// The `416`, with the `Content-Range` that tells the client the length.
    pub(crate) fn not_satisfiable(&self, excessive: bool) -> Response {
        let detail = if excessive {
            format!(
                "the Range header asks for more than {} separate parts; \
                 request fewer, larger ranges",
                range::MAX_PARTS
            )
        } else {
            format!(
                "no requested range starts inside {}, which is {} bytes long",
                self.artifact.name(),
                self.identity.len
            )
        };
        let mut response = Problem::new(ErrorCode::RangeNotSatisfiable, detail).into_response();
        let headers = response.headers_mut();
        headers.insert(
            CONTENT_RANGE,
            header_value(format!("bytes */{}", self.identity.len)),
        );
        headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        response
    }

    /// A `HEAD`: the headers of the full `GET`, and no body.
    ///
    /// Takes the mapped artifact without reading it, because a `HEAD` must
    /// answer as its `GET` would: an artifact that cannot be mapped at its
    /// manifest's length is refused before either is described.
    pub(crate) fn head(&self, _mapped: &Mapped, coding: ContentCoding) -> Response {
        let mut response = self.framed(coding, &Extent::Whole, self.identity.len);
        *response.body_mut() = match coding {
            ContentCoding::Identity => Body::empty(),
            ContentCoding::Zstd | ContentCoding::Gzip => Body::new(Unmeasured),
        };
        response
    }

    /// A `200` or `206` streaming from the mapped artifact.
    pub(crate) fn send(
        &self,
        mapped: Mapped,
        coding: ContentCoding,
        extent: &Extent,
        slot: DownloadSlot,
        ledger: Option<Arc<Ledger>>,
    ) -> Result<Response, Problem> {
        let segments = self.segments(extent);
        let length = segments.iter().map(Segment::len).sum();
        let mut response = self.framed(coding, extent, length);
        let source = Arc::new(mapped);
        let download = Download::new(source, segments, coding, slot, ledger).map_err(|error| {
            tracing::error!(%error, "a download's encoder could not be created");
            Problem::new(
                ErrorCode::InternalError,
                "the download could not be started",
            )
        })?;
        *response.body_mut() = Body::new(download);
        Ok(response)
    }

    /// The body's pieces, in order: the artifact spans, and for a multipart
    /// response the framing around them (RFC 9110 §14.6).
    fn segments(&self, extent: &Extent) -> Vec<Segment> {
        match extent {
            Extent::Whole => vec![Segment::Artifact(0..self.identity.len)],
            Extent::One(span) => vec![Segment::Artifact(span.clone())],
            Extent::Several(spans) => {
                let boundary = self.identity.boundary();
                let mut segments = Vec::with_capacity(spans.len() * 2 + 1);
                for (index, span) in spans.iter().enumerate() {
                    let delimiter = if index == 0 { "" } else { "\r\n" };
                    segments.push(Segment::Literal(
                        format!(
                            "{delimiter}--{boundary}\r\nContent-Type: {}\r\nContent-Range: {}\r\n\r\n",
                            self.artifact.media_type(),
                            self.content_range(span),
                        )
                        .into(),
                    ));
                    segments.push(Segment::Artifact(span.clone()));
                }
                segments.push(Segment::Literal(format!("\r\n--{boundary}--\r\n").into()));
                segments
            }
        }
    }

    /// A response with every header the body implies, and an empty body.
    ///
    /// `length` is the uncoded body's, framing included; it is sent only when
    /// the body goes uncoded, because a coded body's length is not known until
    /// it has been produced.
    fn framed(&self, coding: ContentCoding, extent: &Extent, length: u64) -> Response {
        let mut response = Response::new(Body::empty());
        let status = match extent {
            Extent::Whole => StatusCode::OK,
            Extent::One(_) | Extent::Several(_) => StatusCode::PARTIAL_CONTENT,
        };
        *response.status_mut() = status;
        let headers = response.headers_mut();
        self.validation_headers(headers, coding);
        let content_type = match extent {
            Extent::Several(_) => header_value(format!(
                "multipart/byteranges; boundary={}",
                self.identity.boundary()
            )),
            Extent::Whole | Extent::One(_) => HeaderValue::from_static(self.artifact.media_type()),
        };
        headers.insert(CONTENT_TYPE, content_type);
        headers.insert(
            CONTENT_DISPOSITION,
            header_value(format!("attachment; filename=\"{}\"", self.file_name)),
        );
        match coding {
            ContentCoding::Identity => {
                headers.insert(CONTENT_LENGTH, HeaderValue::from(length));
                headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
                headers.insert(REPR_DIGEST, self.identity.repr_digest().clone());
                if let Some(modified) = self.last_modified {
                    headers.typed_insert(LastModified::from(modified));
                }
                if let Extent::One(span) = extent {
                    headers.insert(CONTENT_RANGE, header_value(self.content_range(span)));
                }
            }
            ContentCoding::Zstd | ContentCoding::Gzip => {
                headers.insert(CONTENT_ENCODING, HeaderValue::from_static(coding.token()));
            }
        }
        response
    }

    /// What a `200`, a `206`, and a `304` for this representation all carry.
    fn validation_headers(&self, headers: &mut HeaderMap, coding: ContentCoding) {
        let tag = match coding {
            ContentCoding::Identity => self.identity.etag(),
            ContentCoding::Zstd | ContentCoding::Gzip => self.identity.weak_etag(),
        };
        headers.typed_insert(tag.clone());
        headers.typed_insert(CachePolicy::Immutable.header().with_no_transform());
        // Only where a coding could be chosen: an artifact always sent as
        // published has one representation, and a cache keyed on every
        // client's `Accept-Encoding` would hold copies of it for nothing.
        if self.artifact.compressible() {
            headers.insert(VARY, HeaderValue::from_static("accept-encoding"));
        }
    }

    fn content_range(&self, span: &Range<u64>) -> String {
        format!(
            "bytes {}-{}/{}",
            span.start,
            span.end - 1,
            self.identity.len
        )
    }
}

/// A dataset id or version as it may appear in a saved file's name.
///
/// Anything outside a conservative set becomes `_`, so the name needs no
/// quoting rules beyond the quotes around it and is valid on every filesystem
/// a client is likely to save to.
fn file_name_part(text: &str) -> String {
    text.chars()
        .map(|character| match character {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' => character,
            _ => '_',
        })
        .collect()
}

fn header_value(text: String) -> HeaderValue {
    HeaderValue::try_from(text).expect("a download header is built from ASCII this module controls")
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "35f3d0b7114f5880217e947cfd2ea8524780090425c4f4cc61d984131a44b443";

    fn identity() -> ArtifactIdentity {
        ArtifactIdentity::new(1000, hex::decode(DIGEST).unwrap())
    }

    fn created() -> SystemTime {
        http_instant(1_788_000_000).unwrap()
    }

    fn decide(headers: &[(&str, &str)]) -> Decision {
        decide_as(Method::GET, headers)
    }

    fn decide_as(method: Method, headers: &[(&str, &str)]) -> Decision {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        Conditions::read(&map, &method, ExportArtifact::Hdt).decide(&identity(), Some(created()))
    }

    fn strong() -> String {
        format!("\"sha256:{DIGEST}\"")
    }

    fn date(offset: i64) -> String {
        let instant = http_instant(1_788_000_000 + offset).unwrap();
        rendered(LastModified::from(instant))
    }

    /// A typed header as it goes on the wire.
    fn rendered(header: impl headers::Header) -> String {
        let mut map = HeaderMap::new();
        map.typed_insert(header);
        map.values().next().unwrap().to_str().unwrap().to_owned()
    }

    fn whole(coding: ContentCoding) -> Decision {
        Decision::Send {
            coding,
            extent: Extent::Whole,
        }
    }

    #[test]
    fn the_digest_is_parsed_exactly_and_is_the_tag() {
        assert_eq!(identity().sha256_hex(), DIGEST);
        assert_eq!(rendered(identity().etag().clone()), strong());
        assert_eq!(
            rendered(identity().weak_etag().clone()),
            format!("W/{}", strong())
        );
        assert_eq!(
            identity().repr_digest(),
            "sha-256=:NfPQtxFPWIAhfpR8/S6oUkeACQQlxPTMYdmEExpEtEM=:"
        );
    }

    /// What a client that prefers compression to identity sends.
    const PREFERS_ZSTD: (&str, &str) = ("accept-encoding", "zstd, identity;q=0.5");
    const PREFERS_GZIP: (&str, &str) = ("accept-encoding", "gzip, identity;q=0.5");

    #[test]
    fn a_range_is_identity_and_a_whole_body_is_negotiated() {
        assert_eq!(decide(&[]), whole(ContentCoding::Identity));
        // A browser's default is identity: a download it can size and resume.
        assert_eq!(
            decide(&[("accept-encoding", "gzip, deflate, br, zstd")]),
            whole(ContentCoding::Identity)
        );
        assert_eq!(decide(&[PREFERS_ZSTD]), whole(ContentCoding::Zstd));
        assert_eq!(
            decide(&[PREFERS_ZSTD, ("range", "bytes=10-19")]),
            Decision::Send {
                coding: ContentCoding::Identity,
                extent: Extent::One(10..20),
            }
        );
        // HEAD describes the full GET, so it never ranges.
        assert_eq!(
            decide_as(Method::HEAD, &[("range", "bytes=10-19")]),
            whole(ContentCoding::Identity)
        );
        // An invalid Range is ignored, not refused.
        assert_eq!(
            decide(&[("range", "bytes=9-3")]),
            whole(ContentCoding::Identity)
        );
        assert_eq!(
            decide(&[("range", "bytes=0-1"), ("range", "bytes=5-6")]),
            whole(ContentCoding::Identity)
        );
    }

    #[test]
    fn if_range_honours_the_range_only_for_this_exact_artifact() {
        let range = ("range", "bytes=10-19");
        let honoured = Decision::Send {
            coding: ContentCoding::Identity,
            extent: Extent::One(10..20),
        };
        assert_eq!(decide(&[range, ("if-range", &strong())]), honoured);
        assert_eq!(decide(&[range, ("if-range", &date(0))]), honoured);
        // A weak tag never satisfies If-Range, and neither does another
        // artifact's: the full body is sent instead of a splice.
        let weak = format!("W/{}", strong());
        assert_eq!(
            decide(&[range, ("if-range", &weak)]),
            whole(ContentCoding::Identity)
        );
        assert_eq!(
            decide(&[range, ("if-range", "\"sha256:00\"")]),
            whole(ContentCoding::Identity)
        );
        assert_eq!(
            decide(&[range, ("if-range", &date(-1))]),
            whole(ContentCoding::Identity)
        );
        // A failed If-Range falls back to the full body, which is negotiable.
        assert_eq!(
            decide(&[range, ("if-range", "\"other\""), PREFERS_GZIP]),
            whole(ContentCoding::Gzip)
        );
    }

    #[test]
    fn preconditions_are_evaluated_in_rfc_9110_order() {
        let weak = format!("W/{}", strong());
        assert_eq!(
            decide(&[("if-match", &strong())]),
            whole(ContentCoding::Identity)
        );
        assert_eq!(decide(&[("if-match", "*")]), whole(ContentCoding::Identity));
        assert_eq!(
            decide(&[("if-match", "\"other\"")]),
            Decision::PreconditionFailed
        );
        // A request asserting the artifact is unchanged is answered from the
        // identity bytes, whose tag is strong and whose date is stated, even
        // from a client that prefers compression: against a coded body's weak
        // tag, If-Match could never pass.
        assert_eq!(
            decide(&[("if-match", &strong()), PREFERS_GZIP]),
            whole(ContentCoding::Identity)
        );
        assert_eq!(decide(&[("if-match", &weak)]), Decision::PreconditionFailed);
        assert_eq!(
            decide(&[("if-unmodified-since", &date(-1)), PREFERS_GZIP]),
            Decision::PreconditionFailed
        );
        assert_eq!(
            decide(&[("if-unmodified-since", &date(0)), PREFERS_GZIP]),
            whole(ContentCoding::Identity)
        );
        assert_eq!(
            decide(&[("if-unmodified-since", &date(-1))]),
            Decision::PreconditionFailed
        );
        assert_eq!(
            decide(&[("if-unmodified-since", &date(0))]),
            whole(ContentCoding::Identity)
        );
        // If-Match present: If-Unmodified-Since is not evaluated.
        assert_eq!(
            decide(&[("if-match", "*"), ("if-unmodified-since", &date(-1))]),
            whole(ContentCoding::Identity)
        );

        // If-None-Match compares weakly, so either tag revalidates either body,
        // and the 304 names the coding the 200 would have used.
        for tag in [strong(), weak.clone()] {
            assert_eq!(
                decide(&[("if-none-match", &tag)]),
                Decision::NotModified {
                    coding: ContentCoding::Identity
                }
            );
            assert_eq!(
                decide(&[("if-none-match", &tag), PREFERS_ZSTD]),
                Decision::NotModified {
                    coding: ContentCoding::Zstd
                }
            );
        }
        assert_eq!(
            decide(&[("if-modified-since", &date(0))]),
            Decision::NotModified {
                coding: ContentCoding::Identity
            }
        );
        assert_eq!(
            decide(&[("if-modified-since", &date(-1))]),
            whole(ContentCoding::Identity)
        );
        // If-None-Match present: If-Modified-Since is not evaluated.
        assert_eq!(
            decide(&[
                ("if-none-match", "\"other\""),
                ("if-modified-since", &date(0))
            ]),
            whole(ContentCoding::Identity)
        );
        // A coded body states no date, so date preconditions do not bind it.
        assert_eq!(
            decide(&[("if-modified-since", &date(0)), PREFERS_GZIP]),
            whole(ContentCoding::Gzip)
        );
        // A precondition failure outranks a range the artifact cannot satisfy.
        assert_eq!(
            decide(&[("if-match", "\"other\""), ("range", "bytes=5000-")]),
            Decision::PreconditionFailed
        );
        assert_eq!(
            decide(&[("range", "bytes=5000-")]),
            Decision::RangeNotSatisfiable { excessive: false }
        );
    }

    #[test]
    fn a_multipart_body_is_framed_by_rfc_9110_and_its_length_is_exact() {
        let identity = identity();
        let delivery = Delivery::new("tox", "v1", ExportArtifact::Hdt, &identity, None);
        let spans = vec![0..10, 500..510];
        let segments = delivery.segments(&Extent::Several(spans));
        let boundary = identity.boundary();
        let framing: Vec<String> = segments
            .iter()
            .filter_map(|segment| match segment {
                Segment::Literal(bytes) => Some(String::from_utf8(bytes.to_vec()).unwrap()),
                Segment::Artifact(_) => None,
            })
            .collect();
        assert_eq!(
            framing,
            [
                format!(
                    "--{boundary}\r\nContent-Type: application/vnd.hdt\r\n\
                     Content-Range: bytes 0-9/1000\r\n\r\n"
                ),
                format!(
                    "\r\n--{boundary}\r\nContent-Type: application/vnd.hdt\r\n\
                     Content-Range: bytes 500-509/1000\r\n\r\n"
                ),
                format!("\r\n--{boundary}--\r\n"),
            ]
        );
        let framed: u64 = framing.iter().map(|text| text.len() as u64).sum();
        let length: u64 = segments.iter().map(Segment::len).sum();
        assert_eq!(length, framed + 20);
    }

    #[test]
    fn a_saved_file_is_named_for_its_version_and_nothing_else() {
        let identity = identity();
        let delivery = Delivery::new("tox", "2026-06-01", ExportArtifact::Hdt, &identity, None);
        assert_eq!(delivery.file_name, "tox-2026-06-01.hdt");
        let odd = Delivery::new("t\"o x", "v/1\r\n", ExportArtifact::Hdt, &identity, None);
        assert_eq!(odd.file_name, "t_o_x-v_1__.hdt");
    }

    #[test]
    fn every_exported_artifact_is_named_by_its_own_path_and_nothing_else_is() {
        let all: Vec<_> = ExportArtifact::all().collect();
        assert_eq!(all[0], ExportArtifact::Hdt);
        assert_eq!(
            all.len(),
            1 + artifact::FILTERS.len() + artifact::KEYSETS.len()
        );
        for &artifact in &all {
            assert_eq!(ExportArtifact::from_path(artifact.name()), Some(artifact));
        }
        for name in [
            "filters/subjects.filter",
            "filters/objects.minhash",
            "keysets/shared.keys",
        ] {
            assert!(
                matches!(
                    ExportArtifact::from_path(name),
                    Some(ExportArtifact::Keys(_))
                ),
                "{name}"
            );
        }
        // Other artifacts of a bundle, the one that is a directory, a role the
        // bundle profile leaves out, and spellings that would reach another
        // file if a path were ever built from them.
        for other in [
            "data.hdt.perm",
            "manifest.json",
            "data.hdt.text",
            "stats/void.hdt",
            "keysets/terms.keys",
            "filters",
            "filters/",
            "filters//subjects.filter",
            "filters/../data.hdt",
            "filters%2Fsubjects.filter",
            "data%2Ehdt",
            "/data.hdt",
            "Data.hdt",
            "",
        ] {
            assert_eq!(ExportArtifact::from_path(other), None, "{other:?}");
        }
    }

    #[test]
    fn no_two_artifacts_of_a_release_are_saved_under_one_name() {
        let names: std::collections::BTreeSet<_> = ExportArtifact::all()
            .map(|artifact| artifact.saved_name("tox", "v1"))
            .collect();
        assert_eq!(names.len(), ExportArtifact::all().count());
        assert!(names.contains("tox-v1.hdt"));
        assert!(names.contains("tox-v1.subjects.minhash"));
        assert!(names.contains("tox-v1.objects.filter"));
        assert!(names.contains("tox-v1.subjects-only.keys"));
    }

    fn manifest_listing(artifacts: &[(&str, &str)]) -> Manifest {
        let listed: serde_json::Map<_, _> = artifacts
            .iter()
            .map(|&(name, sha256)| {
                (
                    name.to_owned(),
                    serde_json::json!({"bytes": 7, "sha256": sha256}),
                )
            })
            .collect();
        serde_json::from_value(serde_json::json!({
            "id": "tox",
            "version": "v1",
            "content_digest": "sha256:00",
            "counts": {"triples": 0, "subjects": 0, "predicates": 0, "objects": 0},
            "artifacts": listed,
        }))
        .expect("a minimal manifest")
    }

    /// The exports of a bundle that agrees with its manifest.
    fn agreeing(manifest: &Manifest) -> Result<Exports, String> {
        Exports::new(manifest, &ExportFiles::as_listed(manifest))
    }

    fn served_names(exports: &Exports) -> Vec<&'static str> {
        exports
            .served()
            .iter()
            .map(|(artifact, _)| artifact.name())
            .collect()
    }

    #[test]
    fn a_release_exports_the_hdt_and_the_key_files_its_manifest_lists() {
        // `data.hdt.perm` and `stats/summary.md` are listed and not exported;
        // the object-side sketch is exportable and not listed.
        let manifest = manifest_listing(&[
            ("data.hdt", DIGEST),
            ("data.hdt.perm", DIGEST),
            ("filters/subjects.minhash", DIGEST),
            ("keysets/shared.keys", DIGEST),
            ("stats/summary.md", DIGEST),
        ]);
        let exports = agreeing(&manifest).unwrap();
        assert_eq!(
            served_names(&exports),
            [
                "data.hdt",
                "filters/subjects.minhash",
                "keysets/shared.keys"
            ]
        );
        assert!(exports.withheld().is_empty());
        for (_, identity) in exports.served().iter() {
            assert_eq!(identity.size(), 7);
            assert_eq!(identity.sha256_hex(), DIGEST);
        }
        let sketch = ExportArtifact::Keys(KeyArtifact::SubjectsSketch);
        assert!(matches!(exports.get(sketch), Export::Served(_)));
        let unlisted = ExportArtifact::Keys(KeyArtifact::ObjectsSketch);
        assert!(matches!(exports.get(unlisted), Export::Unlisted));

        // A bundle with no key files exports its HDT alone.
        let plain = agreeing(&manifest_listing(&[("data.hdt", DIGEST)])).unwrap();
        assert_eq!(served_names(&plain), ["data.hdt"]);
    }

    #[test]
    fn a_manifest_that_cannot_identify_data_hdt_is_refused() {
        let missing = agreeing(&manifest_listing(&[("keysets/shared.keys", DIGEST)]));
        assert!(missing.unwrap_err().contains("data.hdt"));
        let unusable = agreeing(&manifest_listing(&[("data.hdt", "not-a-digest")]));
        assert!(unusable.unwrap_err().contains("data.hdt"));
    }

    #[test]
    fn a_download_that_cannot_be_served_as_listed_is_withheld_alone() {
        let manifest = manifest_listing(&[
            ("data.hdt", DIGEST),
            ("filters/objects.minhash", DIGEST),
            ("filters/subjects.minhash", DIGEST),
            ("keysets/objects-only.keys", DIGEST),
            ("keysets/shared.keys", "not-a-digest"),
            ("keysets/subjects-only.keys", DIGEST),
        ]);
        let artifact = |name| ExportArtifact::from_path(name).unwrap();
        let mut files = ExportFiles::as_listed(&manifest);
        files
            .0
            .insert(artifact("filters/objects.minhash"), OnDisk::File(6));
        files
            .0
            .insert(artifact("keysets/objects-only.keys"), OnDisk::Missing);
        files.0.insert(
            artifact("keysets/subjects-only.keys"),
            OnDisk::Unusable("it is not a regular file".to_owned()),
        );
        let exports = Exports::new(&manifest, &files).expect("only data.hdt is required");

        assert_eq!(
            served_names(&exports),
            ["data.hdt", "filters/subjects.minhash"]
        );
        for (name, says) in [
            (
                "filters/objects.minhash",
                "is 6 bytes and its manifest lists 7",
            ),
            (
                "keysets/objects-only.keys",
                "the bundle has no keysets/objects-only.keys",
            ),
            ("keysets/shared.keys", "is not 64 lowercase hex digits"),
            ("keysets/subjects-only.keys", "it is not a regular file"),
        ] {
            match exports.get(artifact(name)) {
                Export::Withheld(defect) => assert!(defect.contains(says), "{name}: {defect}"),
                other => panic!("{name} is {other:?}"),
            }
        }

        // `data.hdt` too: its download is withheld, and the release stands.
        let mut files = ExportFiles::as_listed(&manifest);
        files.0.insert(ExportArtifact::Hdt, OnDisk::File(8));
        let exports = Exports::new(&manifest, &files).unwrap();
        assert!(matches!(
            exports.get(ExportArtifact::Hdt),
            Export::Withheld(_)
        ));
    }

    #[test]
    fn a_bundle_directory_is_examined_for_the_downloads_its_manifest_lists() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.hdt"), b"1234567").unwrap();
        std::fs::create_dir_all(dir.path().join("keysets/shared.keys")).unwrap();
        let manifest = manifest_listing(&[
            ("data.hdt", DIGEST),
            ("data.hdt.perm", DIGEST),
            ("filters/subjects.filter", DIGEST),
            ("keysets/shared.keys", DIGEST),
        ]);
        let files = ExportFiles::scan(dir.path(), &manifest);
        assert_eq!(files.0.get(&ExportArtifact::Hdt), Some(&OnDisk::File(7)));
        assert_eq!(
            files
                .0
                .get(&ExportArtifact::Keys(KeyArtifact::SubjectsFilter)),
            Some(&OnDisk::Missing)
        );
        assert!(matches!(
            files.0.get(&ExportArtifact::Keys(KeyArtifact::SharedKeys)),
            Some(OnDisk::Unusable(_))
        ));
        // Only what the manifest lists is examined.
        assert_eq!(files.0.len(), 3);
    }

    #[test]
    fn a_key_file_is_never_coded_and_does_not_vary() {
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT_ENCODING,
            HeaderValue::from_static("zstd, identity;q=0.5"),
        );
        let sketch = ExportArtifact::Keys(KeyArtifact::SubjectsSketch);
        assert!(!sketch.compressible());
        assert!(ExportArtifact::Hdt.compressible());
        let decision =
            Conditions::read(&headers, &Method::GET, sketch).decide(&identity(), Some(created()));
        assert_eq!(
            decision,
            Decision::Send {
                coding: ContentCoding::Identity,
                extent: Extent::Whole
            }
        );
        let hdt = Conditions::read(&headers, &Method::GET, ExportArtifact::Hdt)
            .decide(&identity(), Some(created()));
        assert!(matches!(
            hdt,
            Decision::Send {
                coding: ContentCoding::Zstd,
                ..
            }
        ));

        let identity = identity();
        let sketch = Delivery::new("tox", "v1", sketch, &identity, Some(created()));
        let response = sketch.not_modified(ContentCoding::Identity);
        assert_eq!(response.headers().get(VARY), None);
        let hdt = Delivery::new("tox", "v1", ExportArtifact::Hdt, &identity, Some(created()));
        let response = hdt.not_modified(ContentCoding::Identity);
        assert_eq!(response.headers()[VARY], "accept-encoding");
    }
}
