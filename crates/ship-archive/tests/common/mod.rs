//! Helpers shared by the integration tests: log fixtures written from one synthetic chain, an
//! in-process server, a tokio-tungstenite client, a raw client that sees every WebSocket frame,
//! and an independent model of the bytes a Spring node holding the same logs would send.

#![allow(dead_code)] // every test binary uses its own subset

use std::net::{Ipv4Addr, SocketAddr};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use hyperion_ship::protocol::{self, BlockPos, BlocksReq, Request, Status};
use hyperion_ship::shiplog::LogKind;
use hyperion_ship::testkit::SyntheticChain;
use hyperion_ship::testkit::ship::Encoding;
use ship_archive::store::StoreConfig;
use ship_archive::{ServeConfig, Server};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How long a test waits for something that must happen.
pub const WAIT: Duration = Duration::from_secs(20);
/// How long a test waits to be sure that nothing more arrives.
pub const QUIET: Duration = Duration::from_millis(300);

/// A temporary directory of logs written from one chain.
pub struct Fixture {
    pub tmp: tempfile::TempDir,
    pub chain: SyntheticChain,
}

impl Fixture {
    pub fn new(chain: SyntheticChain) -> Self {
        let tmp = tempfile::Builder::new()
            .prefix("ship-archive-test-")
            .tempdir()
            .expect("temp dir");
        Self { tmp, chain }
    }

    /// The layout most tests serve, blocks 2..201 of a chain:
    ///   * trace in `head/` and `retained/` as a node with stride 50 leaves it: four retained
    ///     files (2-50, 51-100, 101-150, 151-200) and an empty head log, Spring stored zlib;
    ///   * chain_state in one head file, Leap deflate;
    ///   * finality from block 100 only (as after a Savanna switch), legacy zero-length framing;
    ///   * the block log split every 64 blocks (2-64, 65-128, 129-192, 193-200).
    pub fn standard() -> Self {
        let fx = Self::new(SyntheticChain::new(2, 199));
        let (head, retained) = (fx.dir("head"), fx.dir("retained"));
        fx.chain
            .write_ship_partitioned(
                &head,
                &retained,
                LogKind::Trace,
                2..201,
                50,
                Encoding::SizedStored,
            )
            .unwrap();
        fx.ship(&head, LogKind::ChainState, 2..201, Encoding::SizedDeflate);
        fx.ship(&head, LogKind::Finality, 100..201, Encoding::LegacyZero);
        fx.chain.write_blocks_split(&retained, 2..201, 64).unwrap();
        fx
    }

    /// What a node holding the standard layout serves.
    pub fn standard_model(&self) -> Model<'_> {
        Model {
            chain: &self.chain,
            trace: 2..201,
            chain_state: 2..201,
            finality: 100..201,
            blocks: 2..201,
        }
    }

    /// A sub-directory of the fixture (not created).
    pub fn dir(&self, name: &str) -> PathBuf {
        self.tmp.path().join(name)
    }

    /// Write the `kind` head log of `blocks` into `dir`.
    pub fn ship(&self, dir: &Path, kind: LogKind, blocks: Range<u32>, encoding: Encoding) {
        use hyperion_ship::testkit::FileName;
        self.chain
            .write_ship(dir, kind, blocks, encoding, FileName::Head)
            .unwrap();
    }

    /// A store over every directory of the fixture, recursively.
    pub fn store(&self) -> StoreConfig {
        StoreConfig {
            dirs: vec![self.tmp.path().to_path_buf()],
            recursive: true,
            ..StoreConfig::default()
        }
    }

    /// A server over [`Self::store`], listening on an ephemeral loopback port.
    pub fn serve(&self) -> ServeConfig {
        serve_config(self.store())
    }
}

/// `store` served on an ephemeral loopback port, with the other settings at their defaults.
pub fn serve_config(store: StoreConfig) -> ServeConfig {
    ServeConfig {
        store,
        listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        ..ServeConfig::default()
    }
}

/// A server running in the test's runtime. Dropping it shuts the server down.
pub struct Running {
    pub addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<anyhow::Result<()>>>,
}

impl Running {
    pub fn url(&self) -> String {
        format!("ws://{}/", self.addr)
    }

    /// Shut the server down and wait for it to finish.
    pub async fn stop(mut self) -> anyhow::Result<()> {
        drop(self.stop.take());
        let task = self.task.take().expect("running");
        tokio::time::timeout(WAIT, task)
            .await
            .expect("the server stops")
            .expect("server task")
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        drop(self.stop.take());
    }
}

pub async fn start(cfg: ServeConfig) -> Running {
    let server = Server::bind(cfg).await.expect("the server starts");
    let addr = server.local_addr();
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(server.run(async move {
        let _ = stopped.await;
    }));
    Running {
        addr,
        stop: Some(stop),
        task: Some(task),
    }
}

/// A connected client that has read the ABI message.
pub async fn connect(server: &Running) -> Ws {
    let (mut ws, _) = connect_async(server.url()).await.expect("connect");
    match recv(&mut ws).await {
        Message::Text(_) => ws,
        other => panic!("the first message is not the ABI text: {other:?}"),
    }
}

pub async fn send(ws: &mut Ws, req: &Request) {
    ws.send(Message::Binary(protocol::encode_request(req).into()))
        .await
        .expect("send a request");
}

pub async fn status(ws: &mut Ws, v1: bool) -> Vec<u8> {
    send(ws, &Request::Status { v1 }).await;
    recv_binary(ws).await
}

/// The next message, which must arrive.
pub async fn recv(ws: &mut Ws) -> Message {
    match tokio::time::timeout(WAIT, ws.next()).await {
        Ok(Some(Ok(msg))) => msg,
        Ok(other) => panic!("the connection ended while a message was expected: {other:?}"),
        Err(_) => panic!("no message within {WAIT:?}"),
    }
}

/// The next message, which must be binary.
pub async fn recv_binary(ws: &mut Ws) -> Vec<u8> {
    match recv(ws).await {
        Message::Binary(b) => b.to_vec(),
        other => panic!("expected a binary message, got {other:?}"),
    }
}

/// Nothing arrives for [`QUIET`].
pub async fn quiet(ws: &mut Ws) {
    if let Ok(msg) = tokio::time::timeout(QUIET, ws.next()).await {
        panic!("expected silence, got {msg:?}");
    }
}

/// A v0 request for `[start, end)` with `credit` and no fetch flags.
pub fn blocks(start: u32, end: u32, credit: u32) -> BlocksReq {
    BlocksReq {
        start,
        end,
        max_in_flight: credit,
        ..BlocksReq::default()
    }
}

/// `req` with every fetch flag set (finality only means something for v1).
pub fn fetch_all(mut req: BlocksReq) -> BlocksReq {
    req.fetch_block = true;
    req.fetch_traces = true;
    req.fetch_deltas = true;
    req.fetch_finality_data = req.v1;
    req
}

/// The `this_block` number of a result message, if it has one.
pub fn this_block(msg: &[u8]) -> Option<u32> {
    (msg[73] == 1).then(|| u32::from_le_bytes(msg[74..78].try_into().unwrap()))
}

/// The ranges a Spring node would serve from the same logs, and what it would send for them.
pub struct Model<'a> {
    pub chain: &'a SyntheticChain,
    pub trace: Range<u32>,
    pub chain_state: Range<u32>,
    pub finality: Range<u32>,
    /// Empty when results carry no `block`.
    pub blocks: Range<u32>,
}

impl Model<'_> {
    fn ship(&self, kind: LogKind) -> &Range<u32> {
        match kind {
            LogKind::Trace => &self.trace,
            LogKind::ChainState => &self.chain_state,
            LogKind::Finality => &self.finality,
        }
    }

    /// `L*`.
    pub fn last(&self) -> u32 {
        LogKind::ALL
            .iter()
            .map(|&k| self.ship(k).end)
            .max()
            .unwrap()
            - 1
    }

    pub fn head(&self) -> BlockPos {
        let last = self.last();
        BlockPos {
            num: last,
            id: self.chain.id(last),
        }
    }

    /// The id of block `n` as the logs give it: a state-history entry, or the block log's
    /// `previous` of block `n + 1`.
    pub fn id(&self, n: u32) -> Option<[u8; 32]> {
        let in_ship = LogKind::ALL.iter().any(|&k| self.ship(k).contains(&n));
        let in_blocks = n.checked_add(1).is_some_and(|m| self.blocks.contains(&m));
        (in_ship || in_blocks).then(|| self.chain.id(n))
    }

    fn range(r: &Range<u32>) -> (u32, u32) {
        if r.is_empty() {
            (0, 0)
        } else {
            (r.start, r.end)
        }
    }

    pub fn status(&self, v1: bool) -> Vec<u8> {
        let s = Status {
            head: self.head(),
            lib: self.head(),
            trace: Self::range(&self.trace),
            chain_state: Self::range(&self.chain_state),
            finality: Self::range(&self.finality),
            chain_id: self.chain.chain_id(),
        };
        protocol::encode_status(v1, &s)
    }

    /// The whole result message for block `n` answering `req`.
    pub fn result(&self, n: u32, req: &BlocksReq) -> Vec<u8> {
        let head = self.head();
        let mut out = vec![protocol::result_variant(req.v1)];
        let fields = if req.v1 { 3 } else { 2 };
        let Some(id) = self.id(n) else {
            out.extend(protocol::encode_result_base(head, head, None, None, None));
            for _ in 0..fields {
                out.extend(protocol::encode_opt_len(None));
            }
            return out;
        };
        let this = BlockPos { num: n, id };
        let prev = n
            .checked_sub(1)
            .and_then(|p| self.id(p).map(|id| BlockPos { num: p, id }));
        let block =
            (req.fetch_block && self.blocks.contains(&n)).then(|| &self.chain.block(n).body);
        out.extend(protocol::encode_result_base(
            head,
            head,
            Some(this),
            prev,
            block.map(|b| b.as_slice()),
        ));
        let wanted = [
            (req.fetch_traces, LogKind::Trace),
            (req.fetch_deltas, LogKind::ChainState),
            (req.fetch_finality_data, LogKind::Finality),
        ];
        for (want, kind) in wanted.into_iter().take(fields) {
            if want && self.ship(kind).contains(&n) {
                let payload = self.chain.payload(kind, n);
                out.extend(protocol::encode_opt_len(Some(payload.len() as u64)));
                out.extend(payload);
            } else {
                out.extend(protocol::encode_opt_len(None));
            }
        }
        out
    }
}

/// One WebSocket frame as the server wrote it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawFrame {
    pub fin: bool,
    pub opcode: u8,
    pub payload: Vec<u8>,
}

pub const OP_CONTINUE: u8 = 0;
pub const OP_TEXT: u8 = 1;
pub const OP_BINARY: u8 = 2;
pub const OP_CLOSE: u8 = 8;

/// A client that does the WebSocket upgrade by hand and reads frames, not messages, so tests
/// see exactly how the server fragments and where it stops.
pub struct Raw {
    tcp: TcpStream,
    buf: Vec<u8>,
}

impl Raw {
    /// Upgrade and return the client with the response's status line and headers.
    pub async fn upgrade(addr: SocketAddr) -> (Self, String) {
        let mut tcp = TcpStream::connect(addr).await.expect("connect");
        let request = format!(
            "GET /any/path HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: \
             Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: \
             13\r\n\r\n"
        );
        tcp.write_all(request.as_bytes()).await.expect("upgrade");
        let mut raw = Self {
            tcp,
            buf: Vec::new(),
        };
        let end = loop {
            if let Some(at) = raw.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break at + 4;
            }
            assert!(
                raw.read_more().await,
                "the connection ended during the upgrade"
            );
        };
        let head = String::from_utf8(raw.buf.drain(..end).collect()).expect("an HTTP response");
        (raw, head)
    }

    /// Upgrade, which must succeed, and read the ABI frame.
    pub async fn connect(addr: SocketAddr) -> Self {
        let (mut raw, head) = Self::upgrade(addr).await;
        assert!(head.starts_with("HTTP/1.1 101"), "upgrade refused: {head}");
        let abi = raw.frame().await.expect("the ABI frame");
        assert_eq!((abi.fin, abi.opcode), (true, OP_TEXT));
        raw
    }

    /// Send `payload` as one masked binary frame.
    pub async fn send(&mut self, payload: &[u8]) {
        self.tcp
            .write_all(&masked(payload))
            .await
            .expect("send a frame");
    }

    pub async fn send_request(&mut self, req: &Request) {
        self.send(&protocol::encode_request(req)).await;
    }

    /// Send raw bytes; `false` once the connection refuses them.
    pub async fn send_bytes(&mut self, bytes: &[u8]) -> bool {
        self.tcp.write_all(bytes).await.is_ok()
    }

    /// Read more bytes into the buffer; `false` at the end of the connection.
    async fn read_more(&mut self) -> bool {
        let mut chunk = vec![0; 64 << 10];
        match tokio::time::timeout(WAIT, self.tcp.read(&mut chunk)).await {
            Ok(Ok(0)) | Ok(Err(_)) => false,
            Ok(Ok(n)) => {
                self.buf.extend_from_slice(&chunk[..n]);
                true
            }
            Err(_) => panic!("nothing from the server within {WAIT:?}"),
        }
    }

    async fn fill(&mut self, n: usize) -> bool {
        while self.buf.len() < n {
            if !self.read_more().await {
                return false;
            }
        }
        true
    }

    /// The next frame, or `None` if the connection ends cleanly between frames. A connection
    /// that ends inside a frame fails the test.
    pub async fn frame(&mut self) -> Option<RawFrame> {
        if !self.fill(2).await {
            assert!(
                self.buf.is_empty(),
                "the connection ended inside a frame header"
            );
            return None;
        }
        let (b0, b1) = (self.buf[0], self.buf[1]);
        assert_eq!(b1 & 0x80, 0, "a server frame is masked");
        let (len, at) = match b1 & 0x7f {
            126 => {
                assert!(
                    self.fill(4).await,
                    "the connection ended inside a frame header"
                );
                (u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize, 4)
            }
            127 => {
                assert!(
                    self.fill(10).await,
                    "the connection ended inside a frame header"
                );
                let len = u64::from_be_bytes(self.buf[2..10].try_into().unwrap());
                (usize::try_from(len).unwrap(), 10)
            }
            n => (usize::from(n), 2),
        };
        assert!(
            self.fill(at + len).await,
            "the connection ended inside a frame"
        );
        let payload = self.buf[at..at + len].to_vec();
        self.buf.drain(..at + len);
        Some(RawFrame {
            fin: b0 & 0x80 != 0,
            opcode: b0 & 0x0f,
            payload,
        })
    }

    /// The frames of the next whole message, or `None` if the connection ends cleanly before
    /// it starts. A connection ending inside a message fails the test.
    pub async fn message(&mut self) -> Option<Vec<RawFrame>> {
        let first = self.frame().await?;
        let mut frames = vec![first];
        while !frames.last().unwrap().fin {
            let frame = self
                .frame()
                .await
                .expect("the connection ended inside a message");
            assert_eq!(
                frame.opcode, OP_CONTINUE,
                "a message's frames are continuations"
            );
            frames.push(frame);
        }
        Some(frames)
    }

    /// Every byte the server still sends, until the connection ends (cleanly or not).
    pub async fn rest(&mut self) -> Vec<u8> {
        while self.read_more().await {}
        std::mem::take(&mut self.buf)
    }
}

/// `payload` as one masked, final, binary client frame.
pub fn masked(payload: &[u8]) -> Vec<u8> {
    const MASK: [u8; 4] = [0x37, 0xfa, 0x21, 0x3d];
    let mut f = vec![0x82];
    match payload.len() {
        n if n < 126 => f.push(0x80 | n as u8),
        n if n < 1 << 16 => {
            f.push(0x80 | 126);
            f.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            f.push(0x80 | 127);
            f.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    f.extend_from_slice(&MASK);
    f.extend(payload.iter().enumerate().map(|(i, b)| b ^ MASK[i % 4]));
    f
}

/// The payloads of `frames` joined: the message they carry.
pub fn joined(frames: &[RawFrame]) -> Vec<u8> {
    frames.iter().flat_map(|f| f.payload.clone()).collect()
}

/// One payload field of a result as framed: its head frame (presence and length) and the
/// payloads of its data frames.
pub type Field = (Vec<u8>, Vec<Vec<u8>>);

/// Split the frames of a result message into its pieces: the variant frame, the base frame,
/// then per payload field its head frame and its data frames. The final frame must be empty.
pub fn pieces(frames: &[RawFrame]) -> (u8, Vec<u8>, Vec<Field>) {
    let (last, frames) = frames.split_last().expect("frames");
    assert!(
        last.fin && last.payload.is_empty(),
        "a result ends with an empty final frame"
    );
    assert!(frames.iter().all(|f| !f.fin));
    assert_eq!(frames[0].opcode, OP_BINARY);
    assert_eq!(
        frames[0].payload.len(),
        1,
        "the variant index is its own frame"
    );
    let variant = frames[0].payload[0];
    let base = frames[1].payload.clone();
    let mut fields = Vec::new();
    let mut rest = &frames[2..];
    while let Some((head, tail)) = rest.split_first() {
        let len = field_len(&head.payload);
        let mut data = Vec::new();
        let mut got = 0;
        let mut tail = tail;
        while got < len {
            let (chunk, more) = tail.split_first().expect("a field's data frames");
            got += chunk.payload.len() as u64;
            data.push(chunk.payload.clone());
            tail = more;
        }
        assert_eq!(got, len, "a field's data frames carry its declared length");
        fields.push((head.payload.clone(), data));
        rest = tail;
    }
    (variant, base, fields)
}

/// The length a field head frame declares: `00` absent (0), or `01` and a varuint64.
fn field_len(head: &[u8]) -> u64 {
    match head[0] {
        0 => {
            assert_eq!(head.len(), 1);
            0
        }
        1 => {
            let (mut v, mut shift) = (0u64, 0);
            for &b in &head[1..] {
                v |= u64::from(b & 0x7f) << shift;
                shift += 7;
            }
            v
        }
        b => panic!("bad presence byte {b}"),
    }
}
