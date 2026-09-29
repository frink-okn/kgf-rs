//! The body of a download: the artifact's bytes, one chunk at a time.
//!
//! Every chunk is read — and, for a compressed response, encoded — on the
//! blocking pool, because a slice of a mapping faults pages as it is read and a
//! page fault stalls whichever thread takes it. The next chunk is not produced
//! until hyper asks for it, which is when the socket has taken the previous
//! one: a client that stops reading holds its download slot, but no thread and
//! no more than one chunk of memory.

use std::collections::VecDeque;
use std::io::{self, Write as _};
use std::ops::Range;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use kgf_store::Store;
use tokio::task::JoinHandle;

use super::ExportArtifact;
use super::coding::ContentCoding;
use crate::admission::DownloadSlot;

/// Artifact bytes read per blocking task.
///
/// Large enough that the per-task scheduling cost vanishes against the copy,
/// small enough that a stalled download pins little memory.
const CHUNK: usize = 512 * 1024;

/// zstd's own default level: within a few percent of its best practical ratio
/// on N-Triples and HDT, at a speed that keeps a core ahead of most links.
const ZSTD_LEVEL: i32 = 3;

/// flate2's default level, the one `gzip` itself uses.
const GZIP_LEVEL: u32 = 6;

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
pub(crate) struct Download {
    state: State,
    /// Bytes still to send, when the response declared a length.
    remaining: Option<u64>,
    /// Released when the body is dropped, which is when the transfer ends for
    /// any reason.
    _slot: DownloadSlot,
}

enum State {
    Ready(Box<Producer>),
    Producing(JoinHandle<(Box<Producer>, io::Result<Option<Bytes>>)>),
    Done,
}

impl Download {
    /// Stream `segments` of `artifact` from `store`, encoded as `coding`.
    pub(crate) fn new(
        store: Arc<Store>,
        artifact: ExportArtifact,
        segments: Vec<Segment>,
        coding: ContentCoding,
        slot: DownloadSlot,
    ) -> io::Result<Self> {
        let total: u64 = segments.iter().map(Segment::len).sum();
        let encoder = Encoder::new(coding, total)?;
        let remaining = encoder.is_none().then_some(total);
        Ok(Self {
            state: State::Ready(Box::new(Producer {
                store,
                artifact,
                segments: segments.into(),
                encoder,
            })),
            remaining,
            _slot: slot,
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
                    this.state = State::Producing(tokio::task::spawn_blocking(move || {
                        let mut producer = producer;
                        let chunk = producer.next_chunk();
                        (producer, chunk)
                    }));
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
                            artifact = producer.artifact.name(),
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
    store: Arc<Store>,
    artifact: ExportArtifact,
    segments: VecDeque<Segment>,
    encoder: Option<Encoder>,
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
            let input: Bytes = match segment {
                Segment::Literal(bytes) => {
                    let bytes = bytes.clone();
                    self.segments.pop_front();
                    bytes
                }
                Segment::Artifact(span) => {
                    let start = span.start;
                    let take = (span.end - start).min(CHUNK as u64);
                    span.start += take;
                    if span.start == span.end {
                        self.segments.pop_front();
                    }
                    let bytes = self.artifact.bytes(&self.store);
                    let (Ok(from), Ok(to)) =
                        (usize::try_from(start), usize::try_from(start + take))
                    else {
                        return Err(io::Error::other("a span does not fit this platform"));
                    };
                    let Some(slice) = bytes.get(from..to) else {
                        // The handler checked the mapping's length against the
                        // manifest before streaming, so this is a bug here.
                        return Err(io::Error::other(
                            "a download span lies outside the mapped artifact",
                        ));
                    };
                    Bytes::copy_from_slice(slice)
                }
            };
            match &mut self.encoder {
                None if input.is_empty() => {}
                None => return Ok(Some(input)),
                Some(encoder) => {
                    let output = encoder.encode(&input)?;
                    if !output.is_empty() {
                        return Ok(Some(output));
                    }
                }
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
