//! The body of a download: the artifact's bytes, one chunk at a time.
//!
//! Every chunk is read — and, for a compressed response, encoded — on the
//! blocking pool, because a slice of a mapping faults pages as it is read and a
//! page fault stalls whichever thread takes it. And every chunk is admitted to
//! that pool through the same work gate as a query, one ordinary unit for the
//! chunk's duration: a transfer's page faults and compression are exactly the
//! blocking work the gate bounds, and a download admitted past its own gate
//! must not then use the pool outside the bound queries are held to. The next
//! chunk is not produced until hyper asks for it, which is when the socket has
//! taken the previous one: a client that stops reading holds its download
//! slot, but no thread, no unit of work, and no more than one chunk of memory.

use std::collections::VecDeque;
use std::future::Future;
use std::io::{self, Write as _};
use std::ops::Range;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use tokio::task::JoinHandle;

use super::coding::ContentCoding;
use crate::access::Ledger;
use crate::admission::{AdmissionGuard, DownloadSlot, WorkClass};
use crate::envelope::Problem;

/// Where a download's bytes come from: an artifact, whole.
///
/// The body needs nothing of a bundle but the bytes and a name to log, and
/// asking only for those keeps its scheduling testable without one.
pub(crate) trait Source: Send + Sync + 'static {
    /// The artifact's bytes. May fault pages; called on the blocking pool only.
    fn bytes(&self) -> &[u8];

    /// The artifact's name, for a log line.
    fn name(&self) -> &'static str;
}

/// Artifact bytes read per blocking task.
///
/// Large enough that the per-task scheduling cost vanishes against the copy,
/// small enough that a stalled download pins little memory.
const CHUNK: usize = 512 * 1024;

/// zstd's own default level: within a few percent of its best practical ratio
/// on N-Triples and HDT, at a speed that keeps a core ahead of most links.
const ZSTD_LEVEL: i32 = 3;

/// gzip's fastest level, for the reason the API's own compression uses it:
/// every coded chunk is CPU on the shared blocking pool, and on an HDT level 1
/// comes within a few points of level 6 (52% of the size against 48% on the
/// demo corpus) at about twice the speed.
const GZIP_LEVEL: u32 = 1;

/// One piece of a response body, in order.
#[derive(Debug, Clone)]
pub(crate) enum Segment {
    /// Framing this server writes: a multipart boundary and part headers.
    Literal(Bytes),
    /// A span of the artifact.
    Artifact(Range<u64>),
}

impl Segment {
    /// Bytes this segment contributes before any content coding.
    pub(crate) fn len(&self) -> u64 {
        match self {
            Self::Literal(bytes) => bytes.len() as u64,
            Self::Artifact(span) => span.end - span.start,
        }
    }
}

/// A download's body.
///
/// The download slot lives in the [`Producer`], not here. A blocking task
/// cannot be cancelled once it starts, so a body dropped while a chunk is
/// being produced — the client went away — leaves that task running to the
/// end of its chunk; the slot has to stay claimed until it does, or a client
/// cancelling and retrying could put more reads and compressions in flight
/// than the gate admits. Travelling with the producer, the slot is released
/// with the body when no chunk is in progress, and with the task otherwise.
pub(crate) struct Download {
    state: State,
    /// Bytes still to send, when the response declared a length.
    remaining: Option<u64>,
}

enum State {
    Ready(Box<Producer>),
    /// Waiting, since the instant given, for the work gate to admit the next chunk.
    Admitting(Box<Producer>, Admission, Instant),
    Producing(JoinHandle<(Box<Producer>, io::Result<Option<Bytes>>)>),
    Done,
}

type Admission = Pin<Box<dyn Future<Output = Result<AdmissionGuard, Problem>> + Send>>;

impl Download {
    /// Stream `segments` of `source`, encoded as `coding`, charging each chunk's
    /// admission and work to `ledger`.
    pub(crate) fn new(
        source: Arc<dyn Source>,
        segments: Vec<Segment>,
        coding: ContentCoding,
        slot: DownloadSlot,
        ledger: Option<Arc<Ledger>>,
    ) -> io::Result<Self> {
        let total: u64 = segments.iter().map(Segment::len).sum();
        let encoder = Encoder::new(coding, total)?;
        let remaining = encoder.is_none().then_some(total);
        Ok(Self {
            state: State::Ready(Box::new(Producer {
                source,
                segments: segments.into(),
                encoder,
                slot,
                ledger,
            })),
            remaining,
        })
    }
}

impl Body for Download {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        let this = &mut *self;
        loop {
            match std::mem::replace(&mut this.state, State::Done) {
                State::Done => return Poll::Ready(None),
                State::Ready(producer) => {
                    let admission = Box::pin(producer.slot.chunk());
                    this.state = State::Admitting(producer, admission, Instant::now());
                }
                State::Admitting(producer, mut admission, waiting) => {
                    match admission.as_mut().poll(cx) {
                        Poll::Pending => {
                            this.state = State::Admitting(producer, admission, waiting);
                            return Poll::Pending;
                        }
                        Poll::Ready(Ok(admitted)) => {
                            if let Some(ledger) = &producer.ledger {
                                ledger.queued(WorkClass::Ordinary, waiting.elapsed());
                            }
                            this.state = State::Producing(tokio::task::spawn_blocking(move || {
                                // Held for the chunk's work and no longer, inside
                                // the task for the reason the slot is: the work
                                // outlives a body dropped while it runs.
                                let _admitted = admitted;
                                let mut producer = producer;
                                let chunk = match producer.ledger.clone() {
                                    Some(ledger) => ledger.run(|| producer.next_chunk()),
                                    None => producer.next_chunk(),
                                };
                                (producer, chunk)
                            }));
                        }
                        Poll::Ready(Err(problem)) => {
                            return Poll::Ready(Some(Err(io::Error::other(problem.to_string()))));
                        }
                    }
                }
                State::Producing(mut task) => match Pin::new(&mut task).poll(cx) {
                    Poll::Pending => {
                        this.state = State::Producing(task);
                        return Poll::Pending;
                    }
                    Poll::Ready(Ok((producer, Ok(Some(chunk))))) => {
                        this.state = State::Ready(producer);
                        if let Some(remaining) = &mut this.remaining {
                            *remaining = remaining.saturating_sub(chunk.len() as u64);
                        }
                        return Poll::Ready(Some(Ok(Frame::data(chunk))));
                    }
                    Poll::Ready(Ok((_, Ok(None)))) => return Poll::Ready(None),
                    // Either failure ends the body with an error, which hyper
                    // turns into an aborted connection: the client sees a
                    // short transfer it can resume, never wrong bytes.
                    Poll::Ready(Ok((producer, Err(error)))) => {
                        tracing::error!(
                            artifact = producer.source.name(),
                            %error,
                            "a download failed while producing its body",
                        );
                        return Poll::Ready(Some(Err(error)));
                    }
                    Poll::Ready(Err(error)) => {
                        tracing::error!(%error, "a download's producer panicked");
                        return Poll::Ready(Some(Err(io::Error::other(
                            "the download's producer failed",
                        ))));
                    }
                },
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        matches!(self.state, State::Done)
    }

    fn size_hint(&self) -> SizeHint {
        self.remaining
            .map_or_else(SizeHint::default, SizeHint::with_exact)
    }
}

/// The body of a `HEAD` answering a compressed `GET`: empty, and unmeasured.
///
/// A `HEAD` carries the headers its `GET` would, and a compressed `GET` has no
/// `Content-Length` because its size is not known until it has been produced.
/// An empty body with an exact size of zero would have the router write
/// `Content-Length: 0` into that `HEAD`, which is a statement about the `GET`
/// and a false one.
pub(crate) struct Unmeasured;

impl Body for Unmeasured {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        Poll::Ready(None)
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
}

/// The state that moves to the blocking pool and back for each chunk.
struct Producer {
    source: Arc<dyn Source>,
    segments: VecDeque<Segment>,
    encoder: Option<Encoder>,
    /// This download's claim on the download gate, held wherever the work is,
    /// and through which each chunk is admitted to the work gate.
    slot: DownloadSlot,
    /// The request's record, held open until the last chunk's work has been
    /// charged to it, wherever the producer is when the body ends.
    ledger: Option<Arc<Ledger>>,
}

impl Producer {
    /// The next non-empty chunk of the body, or `None` once it is complete.
    fn next_chunk(&mut self) -> io::Result<Option<Bytes>> {
        loop {
            let Some(segment) = self.segments.front_mut() else {
                return match self.encoder.take() {
                    Some(encoder) => {
                        let tail = encoder.finish()?;
                        Ok((!tail.is_empty()).then_some(tail))
                    }
                    None => Ok(None),
                };
            };
            let output = match segment {
                Segment::Literal(bytes) => {
                    let bytes = bytes.clone();
                    self.segments.pop_front();
                    match &mut self.encoder {
                        None => bytes,
                        Some(encoder) => encoder.encode(&bytes)?,
                    }
                }
                Segment::Artifact(span) => {
                    let start = span.start;
                    let take = (span.end - start).min(CHUNK as u64);
                    span.start += take;
                    if span.start == span.end {
                        self.segments.pop_front();
                    }
                    let (Ok(from), Ok(to)) =
                        (usize::try_from(start), usize::try_from(start + take))
                    else {
                        return Err(io::Error::other("a span does not fit this platform"));
                    };
                    let Some(slice) = self.source.bytes().get(from..to) else {
                        // The handler checked the mapping's length against the
                        // manifest before streaming, so this is a bug here.
                        return Err(io::Error::other(
                            "a download span lies outside the mapped artifact",
                        ));
                    };
                    // Identity hands hyper a copy it owns; a coded body is
                    // encoded straight from the mapping, with no copy between.
                    match &mut self.encoder {
                        None => Bytes::copy_from_slice(slice),
                        Some(encoder) => encoder.encode(slice)?,
                    }
                }
            };
            if !output.is_empty() {
                return Ok(Some(output));
            }
        }
    }
}

/// A streaming content-coding encoder writing into a buffer this module drains.
enum Encoder {
    Zstd(zstd::stream::write::Encoder<'static, Vec<u8>>),
    Gzip(flate2::write::GzEncoder<Vec<u8>>),
}

impl Encoder {
    /// The encoder for `coding`, or `None` for identity.
    ///
    /// A zstd frame records the uncompressed size and carries a checksum of
    /// the content, so `zstd -l` reports what the file will expand to and a
    /// decoder detects corruption without the digest.
    fn new(coding: ContentCoding, total: u64) -> io::Result<Option<Self>> {
        Ok(match coding {
            ContentCoding::Identity => None,
            ContentCoding::Zstd => {
                let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), ZSTD_LEVEL)?;
                encoder.include_checksum(true)?;
                encoder.set_pledged_src_size(Some(total))?;
                Some(Self::Zstd(encoder))
            }
            ContentCoding::Gzip => Some(Self::Gzip(flate2::write::GzEncoder::new(
                Vec::new(),
                flate2::Compression::new(GZIP_LEVEL),
            ))),
        })
    }

    /// Feed `input` and take whatever compressed output it released.
    ///
    /// Draining the inner buffer between writes is sound for both encoders:
    /// each only ever appends to its writer and keeps no position in it.
    fn encode(&mut self, input: &[u8]) -> io::Result<Bytes> {
        let buffer = match self {
            Self::Zstd(encoder) => {
                encoder.write_all(input)?;
                encoder.get_mut()
            }
            Self::Gzip(encoder) => {
                encoder.write_all(input)?;
                encoder.get_mut()
            }
        };
        Ok(Bytes::from(std::mem::take(buffer)))
    }

    /// End the stream: whatever the encoder still held, and its trailer.
    fn finish(self) -> io::Result<Bytes> {
        Ok(Bytes::from(match self {
            Self::Zstd(encoder) => encoder.finish()?,
            Self::Gzip(encoder) => encoder.finish()?,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(coding: ContentCoding, input: &[u8], pieces: usize) -> Vec<u8> {
        let mut encoder = Encoder::new(coding, input.len() as u64).unwrap().unwrap();
        let mut output = Vec::new();
        for piece in input.chunks(input.len().div_ceil(pieces).max(1)) {
            output.extend_from_slice(&encoder.encode(piece).unwrap());
        }
        output.extend_from_slice(&encoder.finish().unwrap());
        output
    }

    #[test]
    fn draining_between_writes_yields_one_decodable_stream() {
        // Enough varied input that both encoders emit output mid-stream, which
        // is the case where draining the buffer could lose or reorder bytes.
        let input: Vec<u8> = (0..3_000_000u32)
            .flat_map(|index| format!("<http://example.org/{}> .\n", index % 7919).into_bytes())
            .take(3 * CHUNK + 17)
            .collect();
        for pieces in [1, 2, 7, 64] {
            let zstd = encoded(ContentCoding::Zstd, &input, pieces);
            assert_eq!(zstd::stream::decode_all(zstd.as_slice()).unwrap(), input);

            let gzip = encoded(ContentCoding::Gzip, &input, pieces);
            let mut decoded = Vec::new();
            std::io::Read::read_to_end(
                &mut flate2::read::GzDecoder::new(gzip.as_slice()),
                &mut decoded,
            )
            .unwrap();
            assert_eq!(decoded, input);
        }
    }

    impl Source for Vec<u8> {
        fn bytes(&self) -> &[u8] {
            self
        }

        fn name(&self) -> &'static str {
            "fixture"
        }
    }

    #[test]
    fn a_dropped_body_keeps_its_slot_until_its_chunk_has_been_produced() {
        use crate::admission::{Admission, AdmissionController};

        // One blocking thread, occupied until the test says otherwise, so the
        // chunk the body asks for is queued behind it: exactly the moment a
        // client that goes away leaves work nothing can cancel.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let admission = AdmissionController::new(Admission {
                max_concurrent_downloads: 1,
                ..Admission::new()
            });
            let (release, hold) = std::sync::mpsc::channel::<()>();
            let occupied = tokio::task::spawn_blocking(move || hold.recv());

            let source: Arc<dyn Source> = Arc::new(vec![7u8; 3 * CHUNK]);
            let segments = vec![Segment::Artifact(0..3 * CHUNK as u64)];
            let slot = admission.download(None).unwrap();
            let mut body =
                Download::new(source, segments, ContentCoding::Identity, slot, None).unwrap();
            let mut context = Context::from_waker(std::task::Waker::noop());
            assert!(Pin::new(&mut body).poll_frame(&mut context).is_pending());
            drop(body);

            // The client is gone, but its chunk is still to be produced.
            assert!(admission.download(None).is_err());

            release.send(()).unwrap();
            occupied.await.unwrap().unwrap();
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            while admission.download(None).is_err() {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the slot was not given back after its chunk"
                );
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        });
    }

    #[test]
    fn a_chunk_is_read_only_when_the_work_gate_admits_it() {
        use crate::admission::{Admission, AdmissionController, WorkClass};

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let admission = AdmissionController::new(Admission {
                max_concurrent_work: 1,
                heavy_request_weight: 1,
                max_queued_requests: 0,
                ..Admission::new()
            });
            let query = admission.enter(WorkClass::Ordinary).await.unwrap();

            let source: Arc<dyn Source> = Arc::new(vec![7u8; 2 * CHUNK]);
            let segments = vec![Segment::Artifact(0..2 * CHUNK as u64)];
            let slot = admission.download(None).unwrap();
            let mut body =
                Download::new(source, segments, ContentCoding::Identity, slot, None).unwrap();

            // Every unit of work is taken, so nothing is read...
            let mut context = Context::from_waker(std::task::Waker::noop());
            assert!(Pin::new(&mut body).poll_frame(&mut context).is_pending());
            assert!(matches!(body.state, State::Admitting(..)));

            // ...until the query holding it is done.
            drop(query);
            let frame = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
                .await
                .expect("a frame")
                .expect("a chunk");
            assert_eq!(frame.into_data().unwrap().len(), CHUNK);

            // The chunk's unit is given back once the chunk has been produced.
            admission.enter(WorkClass::Ordinary).await.unwrap();
        });
    }

    #[test]
    fn a_zstd_frame_states_its_content_size() {
        let input = vec![b'x'; 10_000];
        let frame = encoded(ContentCoding::Zstd, &input, 3);
        assert_eq!(
            zstd::zstd_safe::get_frame_content_size(&frame).ok(),
            Some(Some(10_000))
        );
    }
}
