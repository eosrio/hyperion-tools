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
//! (Spring's WebSocket layer fragments its writes as well), so no frame, and no write buffer,
//! exceeds 1 MiB.
//!
//! Inline payloads were validated when the package was built, so writing them cannot fail on
//! data. A streamed payload is inflated on the blocking pool one chunk ahead of the socket; it
//! passed a full counting pass first, so only an I/O error (or a file changed underneath) can
//! stop it, mid-message. The caller then drops the connection without a Close frame, as Spring
//! does on any error.

use anyhow::{Context, anyhow};
use futures::{Sink, SinkExt};
use hyperion_ship::protocol;
use hyperion_ship::shiplog::InflateStream;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::protocol::frame::Frame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};
use tokio_tungstenite::tungstenite::{Bytes, Error as WsError, Message};

use crate::fetch::{Package, Part, Streamed};

/// The largest payload chunk in one frame, as Spring's 1 MiB write buffer.
pub const CHUNK: usize = 1 << 20;

/// Why a result could not be written.
#[derive(Debug)]
pub enum SendError {
    /// The WebSocket refused a frame: the client is gone or the connection broke.
    Ws(WsError),
    /// Reading a streamed payload failed after the message had started.
    Read(anyhow::Error),
}

/// Write `pkg` as one fragmented message. Returns the message's payload bytes.
pub async fn send_result<S>(tx: &mut S, pkg: Package) -> Result<u64, SendError>
where
    S: Sink<Message, Error = WsError> + Unpin,
{
    let mut sent = 0;
    let variant = protocol::result_variant(pkg.job.v1);
    sent += feed(tx, Bytes::from(vec![variant]), true).await?;
    sent += feed_chunks(tx, Bytes::from(pkg.base)).await?;
    for part in pkg.parts {
        let head = protocol::encode_opt_len(part.payload_len());
        sent += feed(tx, Bytes::from(head), false).await?;
        match part {
            Part::Absent => {}
            Part::Inline(data) => sent += feed_chunks(tx, Bytes::from(data)).await?,
            Part::Streamed(s) => sent += stream(tx, s).await?,
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

/// One inflated chunk, the stream to continue from, and whether more follows.
type Chunk = anyhow::Result<(InflateStream, Vec<u8>, bool)>;

/// Inflate the next chunk of `stream` on the blocking pool.
fn next_chunk(mut stream: InflateStream) -> JoinHandle<Chunk> {
    tokio::task::spawn_blocking(move || {
        let mut buf = Vec::new();
        let more = stream.next_chunk(&mut buf, CHUNK)?;
        Ok((stream, buf, more))
    })
}

/// Inflate `s` chunk by chunk, each chunk read while the one before it is written. The bytes
/// streamed must be the length the counting pass found, which the field's header declared.
async fn stream<S>(tx: &mut S, s: Streamed) -> Result<u64, SendError>
where
    S: Sink<Message, Error = WsError> + Unpin,
{
    let Streamed { log, header, len } = s;
    // Opening the stream may open the log file, so it happens on the blocking pool too.
    let stream = tokio::task::spawn_blocking(move || log.stream(&header))
        .await
        .context("payload stream task")
        .and_then(|r| r)
        .map_err(SendError::Read)?;
    let mut pending = Some(next_chunk(stream));
    let mut sent = 0;
    while let Some(chunk) = pending.take() {
        let (stream, buf, more) = chunk
            .await
            .context("payload stream task")
            .and_then(|r| r)
            .map_err(SendError::Read)?;
        sent += buf.len() as u64;
        if sent > len || (!more && sent < len) {
            return Err(SendError::Read(anyhow!(
                "the payload streams {sent} bytes{}, its counting pass found {len}",
                if more { " and more" } else { "" }
            )));
        }
        if more {
            pending = Some(next_chunk(stream));
        }
        if !buf.is_empty() {
            feed(tx, Bytes::from(buf), false).await?;
        }
    }
    Ok(sent)
}
