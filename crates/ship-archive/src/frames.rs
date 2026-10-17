//! Writing one block result as Spring frames it (`session.hpp`, `write_loop`): a fragmented
//! binary message of
//!
//! | # | frame                                                     | fin   |
//! |---|-----------------------------------------------------------|-------|
//! | 1 | the result variant index                                  | false |
//! | 2 | `get_blocks_result_base` (head, LIB, this, prev, block)   | false |
//! | 3 | per payload field: presence byte and varuint64 length     | false |
//! |   | ... then the inflated payload in chunks of at most 1 MiB  | false |
//! | 4 | empty                                                     | true  |
//!
//! for `traces`, `deltas` and, answering a v1 request, `finality_data`. An absent field is the
//! single frame `00`. A base made longer than 1 MiB by its block goes out in 1 MiB frames too
//! (Spring's WebSocket layer fragments its writes as well), so no frame exceeds 1 MiB. The
//! block is not copied into the base: the first frame carries the base and the head of the
//! block, and the rest of the block is framed in place.
//!
//! Before a frame larger than [`FLUSH_BEFORE`] the frames queued so far are written out, so the
//! WebSocket write buffer never holds more than one large frame.
//!
//! Inline payloads were validated when the package was built, so writing them cannot fail on
//! data. A streamed payload is inflated on the io threads one chunk ahead of the socket; it
//! passed a full counting pass first, so only an I/O error (or a file changed underneath) can
//! stop it, mid-message. The caller then drops the connection without a Close frame, as Spring
//! does on any error.

use anyhow::{Context, anyhow};
use futures::{Sink, SinkExt};
use hyperion_ship::protocol;
use hyperion_ship::shiplog::InflateStream;
use tokio::task::{JoinError, JoinHandle};
use tokio_tungstenite::tungstenite::protocol::frame::Frame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};
use tokio_tungstenite::tungstenite::{Bytes, Error as WsError, Message};

use crate::fetch::{Io, Package, Part, Streamed};
use crate::store::Source;

/// The largest payload chunk in one frame, as Spring's 1 MiB write buffer.
pub const CHUNK: usize = 1 << 20;
/// Frames larger than this are queued only once the WebSocket write buffer is empty.
pub const FLUSH_BEFORE: usize = 64 << 10;

/// Why a result could not be written.
#[derive(Debug)]
pub enum SendError {
    /// The WebSocket refused a frame: the client is gone or the connection broke.
    Ws(WsError),
    /// Reading a streamed payload failed after the message had started. The error names the
    /// log ([`Source`]).
    Read(anyhow::Error),
}

/// Write `pkg` as one fragmented message, streaming its large payloads through `io`. Returns
/// the message's payload bytes.
pub async fn send_result<S>(tx: &mut S, io: &Io, pkg: Package) -> Result<u64, SendError>
where
    S: Sink<Message, Error = WsError> + Unpin,
{
    let mut sent = 0;
    let variant = protocol::result_variant(pkg.job.v1);
    sent += feed(tx, Bytes::from(vec![variant]), true).await?;
    sent += feed_base(tx, pkg.base, pkg.block).await?;
    for part in pkg.parts {
        let head = protocol::encode_opt_len(part.payload_len());
        sent += feed(tx, Bytes::from(head), false).await?;
        match part {
            Part::Absent => {}
            Part::Inline(data) => sent += feed_chunks(tx, Bytes::from(data)).await?,
            Part::Streamed(s) => sent += stream(tx, io, s).await?,
        }
    }
    let last = Frame::message(Bytes::new(), OpCode::Data(Data::Continue), true);
    tx.send(Message::Frame(last)).await.map_err(SendError::Ws)?;
    Ok(sent)
}

/// Queue one non-final frame of the message: the first one binary, the rest continuations.
async fn feed<S>(tx: &mut S, payload: Bytes, first: bool) -> Result<u64, SendError>
where
    S: Sink<Message, Error = WsError> + Unpin,
{
    if payload.len() > FLUSH_BEFORE {
        tx.flush().await.map_err(SendError::Ws)?;
    }
    let len = payload.len() as u64;
    let opcode = OpCode::Data(if first { Data::Binary } else { Data::Continue });
    tx.feed(Message::Frame(Frame::message(payload, opcode, false)))
        .await
        .map_err(SendError::Ws)?;
    Ok(len)
}

/// Queue `data` as continuation frames of at most [`CHUNK`] bytes (none when it is empty).
async fn feed_chunks<S>(tx: &mut S, data: Bytes) -> Result<u64, SendError>
where
    S: Sink<Message, Error = WsError> + Unpin,
{
    let mut at = 0;
    while at < data.len() {
        let end = data.len().min(at + CHUNK);
        feed(tx, data.slice(at..end), false).await?;
        at = end;
    }
    Ok(data.len() as u64)
}

/// Queue the base and the block after it as frames of [`CHUNK`] bytes of their concatenation.
async fn feed_base<S>(tx: &mut S, base: Vec<u8>, block: Option<Vec<u8>>) -> Result<u64, SendError>
where
    S: Sink<Message, Error = WsError> + Unpin,
{
    let Some(block) = block else {
        return feed_chunks(tx, Bytes::from(base)).await;
    };
    let head = CHUNK.saturating_sub(base.len()).min(block.len());
    let mut first = base;
    first.extend_from_slice(&block[..head]);
    let mut sent = feed(tx, Bytes::from(first), false).await?;
    sent += feed_chunks(tx, Bytes::from(block).slice(head..)).await?;
    Ok(sent)
}

/// One inflated chunk, the stream to continue from, and whether more follows.
type Chunk = anyhow::Result<(InflateStream, Vec<u8>, bool)>;

/// Inflate the next chunk of `stream` on the io threads.
fn next_chunk(io: &Io, mut stream: InflateStream) -> JoinHandle<anyhow::Result<Chunk>> {
    io.start(move || {
        let mut buf = Vec::new();
        let more = stream.next_chunk(&mut buf, CHUNK)?;
        Ok((stream, buf, more))
    })
}

/// The result of a task running io work.
fn joined<T>(r: Result<anyhow::Result<anyhow::Result<T>>, JoinError>) -> anyhow::Result<T> {
    r.context("payload stream task")??
}

/// Inflate `s` chunk by chunk, each chunk read while the one before it is written. The bytes
/// streamed must be the length the counting pass found, which the field's header declared.
async fn stream<S>(tx: &mut S, io: &Io, s: Streamed) -> Result<u64, SendError>
where
    S: Sink<Message, Error = WsError> + Unpin,
{
    let Streamed { log, header, len } = s;
    let source = Source::Ship(log.kind());
    let failed = |e: anyhow::Error| SendError::Read(e.context(source));
    // Opening the stream may open the log file, so it happens on the io threads too.
    let stream = io
        .run(move || log.stream(&header))
        .await
        .and_then(|r| r)
        .map_err(failed)?;
    let mut pending = Some(next_chunk(io, stream));
    let mut sent = 0;
    while let Some(chunk) = pending.take() {
        let (stream, buf, more) = joined(chunk.await).map_err(failed)?;
        sent += buf.len() as u64;
        if sent > len || (!more && sent < len) {
            return Err(failed(anyhow!(
                "the payload streams {sent} bytes{}, its counting pass found {len}",
                if more { " and more" } else { "" }
            )));
        }
        if more {
            pending = Some(next_chunk(io, stream));
        }
        if !buf.is_empty() {
            feed(tx, Bytes::from(buf), false).await?;
        }
    }
    Ok(sent)
}
