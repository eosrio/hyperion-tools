//! What a session sends for one block, how it is fetched ahead of the writer, and the two
//! limits on fetching: the io threads and the memory budget.
//!
//! A [`Job`] is one block result a session owes: the block number, the request version and
//! fetch flags it was taken under, and the request generation (bumped by every `get_blocks`).
//! The store turns it into a [`Package`] on the blocking pool: the encoded result base (head,
//! LIB, `this_block`, `prev_block`, the length of `block`), the block, and one [`Part`] per
//! payload field. Every read and integrity check happens there, before the writer sends the
//! first frame of the message. A payload up to the stream threshold is inflated into memory; a
//! larger one is validated by a counting pass and inflated again while it is sent.
//!
//! File reads and inflation run on the blocking pool through [`Io`], at most `--io-threads` at
//! a time. A session's [`Prefetcher`] builds the packages of the next jobs ahead of its writer:
//! at most `--prefetch` of them, never beyond the credit window, each under a reservation of the
//! global [`MemoryBudget`] (`--memory-budget`). Neither limit ever makes a prefetch wait: one is
//! started only when an io thread is free, and dropped when the budget cannot hold what its
//! plan needs, so prefetching simply stops while the server is busy. The package the writer
//! needs next is built on demand outside the budget, under the session's own head-of-line
//! reserve of at most [`FetchLimits::package_cap`] bytes, so a session never waits for memory
//! that other sessions hold. A new request makes everything built ahead stale: it is dropped
//! unsent, and its reservations are released.
//!
//! The server therefore holds at most
//! `memory_budget + sessions × (package_cap + SESSION_BUFFERS)` bytes of results
//! ([`worst_case_bytes`]).

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};
use hyperion_ship::protocol::{BlocksReq, MAX_BLOCK_BYTES};
use hyperion_ship::shiplog::{EntryHeader, ShipLog};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;

use crate::metrics::Metrics;
use crate::store::{PageCache, Store};

/// What one session holds besides its package, at most: the WebSocket write buffer (one 1 MiB
/// frame) and, while a payload streams, the inflater's 1 MiB input window, the chunk being
/// inflated and the chunk being written (1 MiB each); then the inflater's state, the read
/// buffers and the index pages, under 256 KiB together.
pub const SESSION_BUFFERS: u64 = (4 << 20) + (256 << 10);

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

/// The jobs a session's writer takes next if nothing changes: `count` consecutive blocks from
/// `first`, all under one request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Window {
    pub first: Option<Job>,
    pub count: u32,
}

impl Window {
    /// The `i`-th job of the window.
    pub fn job(&self, i: u32) -> Option<Job> {
        let first = self.first?;
        (i < self.count).then(|| Job {
            n: first.n + i,
            ..first
        })
    }
}

/// How payloads are fetched.
#[derive(Clone, Copy, Debug)]
pub struct FetchLimits {
    /// A payload inflating to more than this many bytes is streamed instead of held in memory.
    pub stream_threshold: u64,
}

/// What a package holds besides its payloads and block: the result base, at most 152 bytes.
const BASE_ALLOWANCE: u64 = 256;

impl FetchLimits {
    /// The most bytes one package holds when no block is larger than `max_block`: three inline
    /// payloads, the block, and the base.
    pub fn package_bound(&self, max_block: u64) -> u64 {
        self.stream_threshold
            .saturating_mul(3)
            .saturating_add(max_block)
            .saturating_add(BASE_ALLOWANCE)
    }

    /// The most bytes one package holds: [`Self::package_bound`] for the largest block a SHiP
    /// result carries.
    pub fn package_cap(&self) -> u64 {
        self.package_bound(MAX_BLOCK_BYTES as u64)
    }
}

/// The most bytes of results a server holds when no block is larger than `max_block`:
/// everything built ahead within the budget, and per session one package and its buffers.
/// [`MAX_BLOCK_BYTES`] gives the bound for any store.
pub fn worst_case_bytes(
    memory_budget: u64,
    sessions: usize,
    limits: &FetchLimits,
    max_block: u64,
) -> u64 {
    let per_session = limits
        .package_bound(max_block)
        .saturating_add(SESSION_BUFFERS);
    memory_budget.saturating_add(per_session.saturating_mul(sessions as u64))
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
    /// `get_blocks_result_base` without the variant index. When the result carries `block`, the
    /// base ends with its presence byte and length, and the block follows as [`Self::block`].
    pub base: Vec<u8>,
    /// The stored `signed_block`, the last field of the base.
    pub block: Option<Vec<u8>>,
    /// `traces`, `deltas`, then for a v1 request `finality_data`.
    pub parts: Vec<Part>,
}

impl Package {
    /// Bytes the package holds in memory (its buffers' capacities).
    pub fn held(&self) -> usize {
        let parts: usize = self
            .parts
            .iter()
            .map(|p| match p {
                Part::Inline(data) => data.capacity(),
                _ => 0,
            })
            .sum();
        self.base.capacity() + self.block.as_ref().map_or(0, Vec::capacity) + parts
    }
}

/// The bytes all sessions together may hold in packages built ahead of their writers. A
/// reservation never waits: it succeeds at once or not at all.
#[derive(Debug)]
pub struct MemoryBudget {
    capacity: u64,
    used: AtomicU64,
    peak: AtomicU64,
    refused: AtomicU64,
}

impl MemoryBudget {
    pub fn new(capacity: u64) -> Arc<Self> {
        Arc::new(Self {
            capacity,
            used: AtomicU64::new(0),
            peak: AtomicU64::new(0),
            refused: AtomicU64::new(0),
        })
    }

    /// Reserve `bytes` if that many are free now; `None` (and nothing reserved) otherwise.
    pub fn try_reserve(self: &Arc<Self>, bytes: u64) -> Option<Reservation> {
        let mut used = self.used.load(Ordering::Relaxed);
        loop {
            let Some(next) = used.checked_add(bytes).filter(|&n| n <= self.capacity) else {
                self.refused.fetch_add(1, Ordering::Relaxed);
                return None;
            };
            match self
                .used
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => {
                    self.peak.fetch_max(next, Ordering::Relaxed);
                    return Some(Reservation {
                        budget: Arc::clone(self),
                        bytes,
                    });
                }
                Err(now) => used = now,
            }
        }
    }

    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Bytes reserved now.
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    /// The most bytes ever reserved at once.
    pub fn peak(&self) -> u64 {
        self.peak.load(Ordering::Relaxed)
    }

    /// How many reservations were refused.
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }
}

/// Bytes of a [`MemoryBudget`], given back when dropped.
#[derive(Debug)]
pub struct Reservation {
    budget: Arc<MemoryBudget>,
    bytes: u64,
}

impl Reservation {
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Give back everything above `bytes`.
    pub fn shrink_to(&mut self, bytes: u64) {
        if bytes < self.bytes {
            self.budget
                .used
                .fetch_sub(self.bytes - bytes, Ordering::AcqRel);
            self.bytes = bytes;
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

/// File reads and inflation: blocking work, run on the blocking pool at most `--io-threads` at
/// a time, and timed for the `read_seconds` metric.
#[derive(Clone, Debug)]
pub struct Io {
    permits: Arc<Semaphore>,
    metrics: Arc<Metrics>,
}

impl Io {
    pub fn new(threads: usize, metrics: Arc<Metrics>) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(threads.max(1))),
            metrics,
        }
    }

    /// Run `f` once an io thread is free, waiting in line for one.
    pub async fn run<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .context("the io threads are gone")?;
        self.spawn(permit, f).await.context("io task")
    }

    /// [`Self::run`] as a task of its own, so that it proceeds while the caller does
    /// something else.
    pub fn start<T, F>(&self, f: F) -> JoinHandle<Result<T>>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let io = self.clone();
        tokio::spawn(async move { io.run(f).await })
    }

    /// Run `f` now if an io thread is free; `None` (and `f` dropped) otherwise.
    pub fn try_spawn<T, F>(&self, f: F) -> Option<JoinHandle<T>>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let permit = Arc::clone(&self.permits).try_acquire_owned().ok()?;
        Some(self.spawn(permit, f))
    }

    fn spawn<T, F>(&self, permit: OwnedSemaphorePermit, f: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let metrics = Arc::clone(&self.metrics);
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let started = Instant::now();
            let out = f();
            metrics.read_done(started.elapsed());
            out
        })
    }
}

/// What a session's packages are built from and within.
#[derive(Clone)]
pub struct FetchCtx {
    pub store: Arc<Store>,
    pub limits: FetchLimits,
    pub io: Io,
    pub budget: Arc<MemoryBudget>,
    /// Packages built ahead at most (`--prefetch`).
    pub depth: usize,
}

/// A package built ahead, and the budget it holds until the writer takes it.
struct Prefetched {
    package: Package,
    _reserved: Reservation,
}

/// A build started ahead of the writer: `None` when the budget could not hold it.
struct Ahead {
    job: Job,
    build: JoinHandle<Result<Option<Prefetched>>>,
}

/// One session's builds: the package its writer needs now, built on demand, and the packages of
/// the next jobs of the window, built ahead within the budget.
pub struct Prefetcher {
    ctx: FetchCtx,
    pages: Arc<PageCache>,
    ahead: VecDeque<Ahead>,
}

impl Prefetcher {
    pub fn new(ctx: FetchCtx) -> Self {
        Self {
            ctx,
            pages: Arc::new(PageCache::default()),
            ahead: VecDeque::new(),
        }
    }

    /// Build ahead the first jobs of `window`, the ones the writer takes after the job it is
    /// sending: keep what was built for them, drop the rest, and start the missing ones in order
    /// while an io thread is free, up to `--prefetch` of them.
    pub fn schedule(&mut self, window: Window) {
        let want = (window.count as usize).min(self.ctx.depth);
        let keep = self
            .ahead
            .iter()
            .zip(0..)
            .take_while(|(a, i)| window.job(*i) == Some(a.job))
            .count()
            .min(want);
        // A stale build (another request or block) is dropped unsent; one still running
        // finishes on the blocking pool, and dropping its result releases its reservation.
        self.ahead.truncate(keep);
        while self.ahead.len() < want {
            let Some(job) = window.job(self.ahead.len() as u32) else {
                break;
            };
            let (ctx, pages) = (self.ctx.clone(), Arc::clone(&self.pages));
            let Some(build) = self.ctx.io.try_spawn(move || prefetch(&ctx, &pages, &job)) else {
                break;
            };
            self.ahead.push_back(Ahead { job, build });
        }
    }

    /// The package for `job`: the one built ahead if it is for this job, otherwise built now,
    /// waiting in line for an io thread but not for the budget. A package built ahead for any
    /// other job is dropped, with everything built after it.
    pub async fn take(&mut self, job: Job) -> Result<Package> {
        match self.ahead.pop_front() {
            Some(ahead) if ahead.job == job => {
                // The reservation goes with `Prefetched`: from here on the package is the
                // session's own, the one it sends.
                if let Some(built) = ahead.build.await.context("package build task")?? {
                    return Ok(built.package);
                }
            }
            _ => self.ahead.clear(),
        }
        let (ctx, pages) = (self.ctx.clone(), Arc::clone(&self.pages));
        self.ctx
            .io
            .run(move || -> Result<Package> {
                let package = ctx
                    .store
                    .read(ctx.store.plan(&job, Some(&pages))?, &ctx.limits)?;
                check_cap(&package, &ctx.limits);
                Ok(package)
            })
            .await?
    }

    /// Drop everything built ahead (the session went idle or got a new request).
    pub fn clear(&mut self) {
        self.ahead.clear();
    }
}

/// Build `job` ahead of the writer if the budget holds what its plan may need, keeping only
/// what the package holds once read.
fn prefetch(ctx: &FetchCtx, pages: &PageCache, job: &Job) -> Result<Option<Prefetched>> {
    let plan = ctx.store.plan(job, Some(pages))?;
    let Some(mut reserved) = ctx.budget.try_reserve(plan.bound(&ctx.limits)) else {
        return Ok(None);
    };
    let package = ctx.store.read(plan, &ctx.limits)?;
    check_cap(&package, &ctx.limits);
    reserved.shrink_to(package.held() as u64);
    Ok(Some(Prefetched {
        package,
        _reserved: reserved,
    }))
}

fn check_cap(package: &Package, limits: &FetchLimits) {
    debug_assert!(
        package.held() as u64 <= limits.package_cap(),
        "block {} holds {} bytes, over the {}-byte package cap",
        package.job.n,
        package.held(),
        limits.package_cap()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_budget_reserves_only_what_is_free_and_gets_it_back() {
        let budget = MemoryBudget::new(100);
        let a = budget.try_reserve(60).expect("60 of 100");
        assert!(budget.try_reserve(41).is_none());
        let mut b = budget.try_reserve(40).expect("the last 40");
        assert_eq!(
            (budget.used(), budget.peak(), budget.refused()),
            (100, 100, 1)
        );
        assert!(budget.try_reserve(1).is_none());
        b.shrink_to(10);
        assert_eq!((b.bytes(), budget.used()), (10, 70));
        // Shrinking never grows a reservation.
        b.shrink_to(50);
        assert_eq!((b.bytes(), budget.used()), (10, 70));
        drop(a);
        assert_eq!(budget.used(), 10);
        drop(b);
        assert_eq!(
            (budget.used(), budget.peak(), budget.refused()),
            (0, 100, 2)
        );
        // Nothing fits a budget of 0 but nothing; nothing overflows.
        let none = MemoryBudget::new(0);
        assert!(none.try_reserve(0).is_some());
        assert!(none.try_reserve(1).is_none());
        let full = MemoryBudget::new(u64::MAX);
        let _max = full.try_reserve(u64::MAX).unwrap();
        assert!(full.try_reserve(1).is_none());
    }

    #[test]
    fn concurrent_reservations_never_exceed_the_budget() {
        let budget = MemoryBudget::new(1000);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    for i in 0..2000u64 {
                        if let Some(r) = budget.try_reserve(1 + i % 300) {
                            assert!(budget.used() <= 1000);
                            drop(r);
                        }
                    }
                });
            }
        });
        assert_eq!(budget.used(), 0);
        assert!(budget.peak() <= 1000);
    }

    #[test]
    fn a_window_is_consecutive_jobs_of_one_request() {
        let first = Job {
            generation: 3,
            n: 10,
            v1: true,
            flags: Flags {
                traces: true,
                ..Flags::default()
            },
        };
        let w = Window {
            first: Some(first),
            count: 3,
        };
        let jobs: Vec<_> = (0..5).map(|i| w.job(i).map(|j| j.n)).collect();
        assert_eq!(jobs, [Some(10), Some(11), Some(12), None, None]);
        assert_eq!(w.job(2).unwrap(), Job { n: 12, ..first });
        assert_eq!(Window::default().job(0), None);
    }

    #[test]
    fn the_worst_case_is_the_budget_and_one_package_per_session() {
        let limits = FetchLimits {
            stream_threshold: 4 << 20,
        };
        assert_eq!(
            limits.package_cap(),
            12 * (1 << 20) + MAX_BLOCK_BYTES as u64 + 256
        );
        assert_eq!(limits.package_bound(1000), 12 * (1 << 20) + 1000 + 256);
        let max_block = MAX_BLOCK_BYTES as u64;
        assert_eq!(
            worst_case_bytes(1 << 30, 256, &limits, max_block),
            (1 << 30) + 256 * (limits.package_cap() + SESSION_BUFFERS)
        );
        assert_eq!(
            worst_case_bytes(0, 2, &limits, 0),
            2 * (12 * (1 << 20) + 256 + SESSION_BUFFERS)
        );
        // Nothing overflows.
        let huge = FetchLimits {
            stream_threshold: u64::MAX,
        };
        assert_eq!(worst_case_bytes(1, usize::MAX, &huge, max_block), u64::MAX);
    }
}
