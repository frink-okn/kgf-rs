//! Whole-artifact downloads: `/{dataset}/v/{version}/export/{artifact}`.
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
//!   its partial file is a valid resume offset.
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

use std::ops::Range;
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
use kgf_store::Store;
use kgf_store::manifest::Manifest;
use kgf_store::store::artifact;

use crate::access::RequestShape;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExportArtifact {
    /// `data.hdt`: the dataset's triples and dictionary, standard HDT.
    Hdt,
}

impl ExportArtifact {
    /// Every exported artifact.
    pub const ALL: &'static [Self] = &[Self::Hdt];

    /// The artifact a path segment names, if it is one this server exports.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|artifact| artifact.name() == name)
    }

    /// The artifact's name in the bundle, which is also its path segment.
    pub fn name(self) -> &'static str {
        match self {
            Self::Hdt => artifact::HDT,
        }
    }

    /// The media type the artifact is served as.
    ///
    /// `application/vnd.hdt` is the type the HDT specification proposes;
    /// clients that do not recognise it still receive the bytes, which is what
    /// `Content-Disposition` is for.
    pub fn media_type(self) -> &'static str {
        match self {
            Self::Hdt => "application/vnd.hdt",
        }
    }

    /// The name a download of this artifact is saved under:
    /// `{dataset}-{version}.{extension}`, so a saved file says which release
    /// it holds.
    pub fn saved_name(self, dataset: &str, version: &str) -> String {
        format!(
            "{}-{}.{}",
            file_name_part(dataset),
            file_name_part(version),
            self.extension()
        )
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Hdt => "hdt",
        }
    }

    fn bytes(self, store: &Store) -> &[u8] {
        match self {
            Self::Hdt => store.hdt_bytes(),
        }
    }
}

/// An exported artifact as its bundle maps it.
struct Mapped {
    store: Arc<Store>,
    artifact: ExportArtifact,
}

impl Source for Mapped {
    fn bytes(&self) -> &[u8] {
        self.artifact.bytes(&self.store)
    }

    fn name(&self) -> &'static str {
        self.artifact.name()
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
    /// The manifest's entry for `artifact`, or why it cannot identify one.
    pub(crate) fn from_manifest(
        manifest: &Manifest,
        artifact: ExportArtifact,
    ) -> Result<Self, String> {
        let name = artifact.name();
        let entry = manifest.artifacts.get(name).ok_or_else(|| {
            format!("the manifest lists no size and checksum for {name}; regenerate it with `kgf manifest`")
        })?;
        let sha256 = hex::decode::<32>(&entry.sha256).ok_or_else(|| {
            format!(
                "the manifest's checksum for {name} is not 64 lowercase hex digits; \
                 regenerate it with `kgf manifest`"
            )
        })?;
        Ok(Self::new(entry.bytes, sha256))
    }

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
    /// coding every client decodes.
    pub(crate) fn read(headers: &HeaderMap, method: &Method) -> Self {
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
            .and_then(|lines| (!lines.is_empty()).then(|| lines.join(", ")));
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
    /// The saved file's name: `{dataset}-{version}.{extension}`.
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
    pub(crate) fn head(&self, store: &Store, coding: ContentCoding) -> Result<Response, Problem> {
        self.verify(store)?;
        let mut response = self.framed(coding, &Extent::Whole, self.identity.len);
        *response.body_mut() = match coding {
            ContentCoding::Identity => Body::empty(),
            ContentCoding::Zstd | ContentCoding::Gzip => Body::new(Unmeasured),
        };
        Ok(response)
    }

    /// A `200` or `206` streaming from `store`.
    pub(crate) fn send(
        &self,
        store: Arc<Store>,
        coding: ContentCoding,
        extent: &Extent,
        slot: DownloadSlot,
    ) -> Result<Response, Problem> {
        self.verify(&store)?;
        let segments = self.segments(extent);
        let length = segments.iter().map(Segment::len).sum();
        let mut response = self.framed(coding, extent, length);
        let source = Arc::new(Mapped {
            store,
            artifact: self.artifact,
        });
        let download = Download::new(source, segments, coding, slot).map_err(|error| {
            tracing::error!(%error, "a download's encoder could not be created");
            Problem::new(
                ErrorCode::InternalError,
                "the download could not be started",
            )
        })?;
        *response.body_mut() = Body::new(download);
        Ok(response)
    }

    /// Check the mapping's length against the manifest's.
    ///
    /// The two can only disagree if a published bundle was rewritten in place,
    /// and serving then would send bytes under a digest they do not have. A
    /// length is what can be checked without reading the file; the digest is
    /// `kgf manifest --check`'s job.
    fn verify(&self, store: &Store) -> Result<(), Problem> {
        let mapped = self.artifact.bytes(store).len() as u64;
        if mapped == self.identity.len {
            return Ok(());
        }
        tracing::error!(
            artifact = self.artifact.name(),
            mapped,
            manifest = self.identity.len,
            "an exported artifact's length disagrees with its manifest",
        );
        Err(Problem::new(
            ErrorCode::InternalError,
            format!(
                "{} does not match the length its manifest publishes",
                self.artifact.name()
            ),
        ))
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
        headers.insert(VARY, HeaderValue::from_static("accept-encoding"));
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
        Conditions::read(&map, &method).decide(&identity(), Some(created()))
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
    fn every_exported_artifact_is_named_by_its_own_segment() {
        for &artifact in ExportArtifact::ALL {
            assert_eq!(ExportArtifact::from_name(artifact.name()), Some(artifact));
        }
        assert_eq!(ExportArtifact::from_name("data.hdt.perm"), None);
        assert_eq!(ExportArtifact::from_name("manifest.json"), None);
    }
}
