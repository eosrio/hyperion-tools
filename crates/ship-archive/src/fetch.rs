//! What a session sends for one block, and how it is fetched ahead of the writer.
//!
//! A [`Job`] is one block result a session owes: the block number, the request version and
//! fetch flags it was taken under, and the request generation (bumped by every `get_blocks`).
//! [`Store::build`] turns it into a [`Package`] on the blocking pool: the encoded result base
//! (head, LIB, `this_block`, `prev_block`, `block`) and one [`Part`] per payload field. Every
//! read and integrity check happens there, before the writer sends the first frame of the
//! message. A payload up to the stream threshold is inflated into memory; a larger one is
//! validated by a counting pass and inflated again while it is sent.
//!
//! The [`Prefetcher`] keeps one package ahead: while the writer sends block `n`, block `n + 1`
//! of the same generation is built, if the credit allows it. A new request makes the package
//! built ahead stale, and it is dropped without a frame of it being sent. A session therefore
//! holds at most two packages, each at most [`FetchLimits::package_cap`] bytes.

use std::sync::Arc;

use anyhow::{Context, Result};
use hyperion_ship::protocol::{BlocksReq, MAX_BLOCK_BYTES};
use hyperion_ship::shiplog::{EntryHeader, ShipLog};
use tokio::task::JoinHandle;

use crate::store::Store;

/// The fields a request asks for. `finality` is set only for a v1 request with
/// `fetch_finality_data`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags {
    pub block: bool,
    pub traces: bool,
    pub deltas: bool,
    pub finality: bool,
}

impl Flags {
    pub fn of(req: &BlocksReq) -> Self {
        Self {
            block: req.fetch_block,
            traces: req.fetch_traces,
            deltas: req.fetch_deltas,
            finality: req.v1 && req.fetch_finality_data,
        }
    }
}

/// One block result a session owes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Job {
    /// The request generation the job was taken under.
    pub generation: u64,
    pub n: u32,
    /// A v1 request, answered with `get_blocks_result_v1` whatever its flags.
    pub v1: bool,
    pub flags: Flags,
}

/// How payloads are fetched.
#[derive(Clone, Copy, Debug)]
pub struct FetchLimits {
    /// A payload inflating to more than this many bytes is streamed instead of held in memory.
    pub stream_threshold: u64,
}

impl FetchLimits {
    /// The most bytes one package holds: three inline payloads and a block, with its encoding.
    pub fn package_cap(&self) -> u64 {
        self.stream_threshold
            .saturating_mul(3)
            .saturating_add(MAX_BLOCK_BYTES as u64 + 256)
    }
}

/// One payload field of a result (`traces`, `deltas` or `finality_data`).
#[derive(Debug)]
pub enum Part {
    /// Not requested, or not in the served range: one `0x00` byte.
    Absent,
    /// The inflated payload, validated.
    Inline(Vec<u8>),
    /// A payload above the stream threshold, validated by a counting pass; it is inflated
    /// again, chunk by chunk, while it is sent.
    Streamed(Streamed),
}

impl Part {
    /// The payload length on the wire, `None` when absent.
    pub fn payload_len(&self) -> Option<u64> {
        match self {
            Self::Absent => None,
            Self::Inline(data) => Some(data.len() as u64),
            Self::Streamed(s) => Some(s.len),
        }
    }
}

/// A payload to inflate while sending it.
#[derive(Debug)]
pub struct Streamed {
    pub log: Arc<ShipLog>,
    pub header: EntryHeader,
    /// The inflated length the counting pass found.
    pub len: u64,
}

/// Everything one result message carries, ready to send.
#[derive(Debug)]
pub struct Package {
    pub job: Job,
    /// `get_blocks_result_base`, without the variant index.
    pub base: Vec<u8>,
    /// `traces`, `deltas`, then for a v1 request `finality_data`.
    pub parts: Vec<Part>,
}

impl Package {
    /// Bytes the package holds in memory.
    pub fn held(&self) -> usize {
        let parts: usize = self
            .parts
            .iter()
            .map(|p| match p {
                Part::Inline(data) => data.len(),
                _ => 0,
            })
            .sum();
        self.base.len() + parts
    }
}

/// Builds the next package of a session ahead of the writer: at most one, for the job the
/// writer will take next.
pub struct Prefetcher {
    store: Arc<Store>,
    limits: FetchLimits,
    ahead: Option<(Job, JoinHandle<Result<Package>>)>,
}

impl Prefetcher {
    pub fn new(store: Arc<Store>, limits: FetchLimits) -> Self {
        Self {
            store,
            limits,
            ahead: None,
        }
    }

    /// Start building `next` (the job after the one being sent), unless it is already being
    /// built. A package built ahead for any other job is dropped.
    pub fn schedule(&mut self, next: Option<Job>) {
        match (&self.ahead, next) {
            (Some((job, _)), Some(next)) if *job == next => {}
            (_, Some(next)) => self.ahead = Some((next, self.spawn(next))),
            (_, None) => self.ahead = None,
        }
    }

    /// The package for `job`: the one built ahead if it is for this job, otherwise built now.
    pub async fn take(&mut self, job: Job) -> Result<Package> {
        let handle = match self.ahead.take() {
            Some((ahead, handle)) if ahead == job => handle,
            // A stale package (another generation) is dropped unsent; its build finishes on
            // the blocking pool and is discarded.
            _ => self.spawn(job),
        };
        handle.await.context("package build task")?
    }

    /// Drop the package built ahead, if any (the session went idle or got a new request).
    pub fn clear(&mut self) {
        self.ahead = None;
    }

    fn spawn(&self, job: Job) -> JoinHandle<Result<Package>> {
        let store = Arc::clone(&self.store);
        let limits = self.limits;
        tokio::task::spawn_blocking(move || {
            let package = store.build(&job, &limits)?;
            debug_assert!(
                package.held() as u64 <= limits.package_cap(),
                "block {} holds {} bytes, over the {}-byte package cap",
                job.n,
                package.held(),
                limits.package_cap()
            );
            Ok(package)
        })
    }
}
