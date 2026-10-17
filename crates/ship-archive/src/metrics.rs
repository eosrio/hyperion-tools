//! Server-wide counters and gauges, and their Prometheus text exposition (`/metrics`).
//!
//! | series                                                      | kind    |
//! |-------------------------------------------------------------|---------|
//! | `antelope_ship_archive_sessions`                            | gauge   |
//! | `antelope_ship_archive_sessions_rejected_total`             | counter |
//! | `antelope_ship_archive_status_requests_total{version}`      | counter |
//! | `antelope_ship_archive_requests_total{version}`             | counter |
//! | `antelope_ship_archive_results_sent_total`                  | counter |
//! | `antelope_ship_archive_bytes_sent_total`                    | counter |
//! | `antelope_ship_archive_read_errors_total{log}`              | counter |
//! | `antelope_ship_archive_read_seconds_sum` / `_count`         | summary |
//! | `antelope_ship_archive_memory_budget_bytes{state}`          | gauge   |
//! | `antelope_ship_archive_served_begin_block{log}` / `_end_`   | gauge   |
//!
//! `version` is `v0` or `v1` (the request's), `log` one of `trace`, `chain_state`, `finality`
//! and `blocks`, `state` `used` or `free`. A served range is `[begin, end)`, `0` and `0` for a
//! log not served.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::fetch::MemoryBudget;
use crate::store::{Source, Store};

const PREFIX: &str = "antelope_ship_archive";

/// The counters every session and the io threads update.
#[derive(Debug, Default)]
pub struct Metrics {
    sessions: AtomicU64,
    rejected: AtomicU64,
    /// Indexed by request version: v0, v1.
    status_requests: [AtomicU64; 2],
    requests: [AtomicU64; 2],
    results: AtomicU64,
    bytes: AtomicU64,
    /// Indexed as [`Source::ALL`].
    read_errors: [AtomicU64; 4],
    read_nanos: AtomicU64,
    reads: AtomicU64,
}

/// Counts a session as open until dropped.
#[derive(Debug)]
pub struct OpenSession<'a>(&'a Metrics);

impl Drop for OpenSession<'_> {
    fn drop(&mut self) {
        self.0.sessions.fetch_sub(1, Ordering::Relaxed);
    }
}

fn add(counter: &AtomicU64, by: u64) {
    counter.fetch_add(by, Ordering::Relaxed);
}

fn get(counter: &AtomicU64) -> u64 {
    counter.load(Ordering::Relaxed)
}

impl Metrics {
    /// A session completed its upgrade; it counts as open while the guard lives.
    pub fn session_opened(&self) -> OpenSession<'_> {
        add(&self.sessions, 1);
        OpenSession(self)
    }

    /// Sessions open now.
    pub fn sessions(&self) -> u64 {
        get(&self.sessions)
    }

    /// An upgrade was refused with HTTP 503.
    pub fn rejected(&self) {
        add(&self.rejected, 1);
    }

    pub fn status_request(&self, v1: bool) {
        add(&self.status_requests[usize::from(v1)], 1);
    }

    /// A `get_blocks` request.
    pub fn blocks_request(&self, v1: bool) {
        add(&self.requests[usize::from(v1)], 1);
    }

    /// A block result of `bytes` payload bytes was sent.
    pub fn result_sent(&self, bytes: u64) {
        add(&self.results, 1);
        add(&self.bytes, bytes);
    }

    /// Block results sent so far.
    pub fn results(&self) -> u64 {
        get(&self.results)
    }

    /// Message bytes sent so far, block results and status replies.
    pub fn bytes_sent(&self) -> u64 {
        get(&self.bytes)
    }

    /// A status reply of `bytes` was sent.
    pub fn status_sent(&self, bytes: u64) {
        add(&self.bytes, bytes);
    }

    /// A read or integrity error, counted against the log it came from when it names one.
    pub fn read_error(&self, error: &anyhow::Error) {
        if let Some(source) = error.downcast_ref::<Source>() {
            let at = Source::ALL
                .iter()
                .position(|s| s == source)
                .expect("every source is listed");
            add(&self.read_errors[at], 1);
        }
    }

    /// Blocking reads and inflations done so far on the io threads.
    pub fn reads(&self) -> u64 {
        get(&self.reads)
    }

    /// One blocking read or inflation took `elapsed`.
    pub fn read_done(&self, elapsed: Duration) {
        add(&self.reads, 1);
        add(
            &self.read_nanos,
            u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX),
        );
    }

    /// The Prometheus text exposition (format 0.0.4) of every series.
    pub fn render(&self, store: &Store, budget: &MemoryBudget) -> String {
        let one = |v: u64| vec![(String::new(), v.to_string())];
        let by_version = |c: &[AtomicU64; 2]| {
            vec![
                ("{version=\"v0\"}".to_owned(), get(&c[0]).to_string()),
                ("{version=\"v1\"}".to_owned(), get(&c[1]).to_string()),
            ]
        };
        let by_log = |f: &dyn Fn(usize, Source) -> u64| {
            Source::ALL
                .iter()
                .enumerate()
                .map(|(at, &s)| (format!("{{log=\"{}\"}}", s.label()), f(at, s).to_string()))
                .collect::<Vec<_>>()
        };
        let served = |s: Source| match s {
            Source::Ship(kind) => store.served.ship(kind),
            Source::Blocks => store.served.blocks,
        };
        let used = budget.used();
        let all = [
            Series {
                name: "sessions",
                kind: "gauge",
                help: "Open SHiP sessions.",
                values: one(self.sessions()),
            },
            Series {
                name: "sessions_rejected_total",
                kind: "counter",
                help: "Upgrades refused with HTTP 503 at --max-sessions.",
                values: one(get(&self.rejected)),
            },
            Series {
                name: "status_requests_total",
                kind: "counter",
                help: "Status requests, by request version.",
                values: by_version(&self.status_requests),
            },
            Series {
                name: "requests_total",
                kind: "counter",
                help: "get_blocks requests, by request version.",
                values: by_version(&self.requests),
            },
            Series {
                name: "results_sent_total",
                kind: "counter",
                help: "Block results sent.",
                values: one(get(&self.results)),
            },
            Series {
                name: "bytes_sent_total",
                kind: "counter",
                help: "Message bytes sent: block results and status replies.",
                values: one(get(&self.bytes)),
            },
            Series {
                name: "read_errors_total",
                kind: "counter",
                help: "Read or integrity errors that dropped a session, by log.",
                values: by_log(&|at, _| get(&self.read_errors[at])),
            },
            Series {
                name: "read_seconds",
                kind: "summary",
                help: "Time spent reading and inflating logs on the io threads.",
                values: vec![
                    (
                        "_sum".to_owned(),
                        format!("{:.6}", get(&self.read_nanos) as f64 / 1e9),
                    ),
                    ("_count".to_owned(), get(&self.reads).to_string()),
                ],
            },
            Series {
                name: "memory_budget_bytes",
                kind: "gauge",
                help: "The memory budget for results built ahead (--memory-budget).",
                values: vec![
                    ("{state=\"used\"}".to_owned(), used.to_string()),
                    (
                        "{state=\"free\"}".to_owned(),
                        budget.capacity().saturating_sub(used).to_string(),
                    ),
                ],
            },
            Series {
                name: "served_begin_block",
                kind: "gauge",
                help: "First block served, by log (0 when the log is not served).",
                values: by_log(&|_, s| u64::from(served(s).0)),
            },
            Series {
                name: "served_end_block",
                kind: "gauge",
                help: "One past the last block served, by log (0 when the log is not served).",
                values: by_log(&|_, s| u64::from(served(s).1)),
            },
        ];
        let mut out = String::with_capacity(4096);
        for s in all {
            let _ = writeln!(out, "# HELP {PREFIX}_{} {}", s.name, s.help);
            let _ = writeln!(out, "# TYPE {PREFIX}_{} {}", s.name, s.kind);
            for (suffix, value) in s.values {
                let _ = writeln!(out, "{PREFIX}_{}{suffix} {value}", s.name);
            }
        }
        out
    }
}

/// One metric family: its samples are the name followed by a suffix (labels, or `_sum` and
/// `_count` for a summary) and a value.
struct Series {
    name: &'static str,
    kind: &'static str,
    help: &'static str,
    values: Vec<(String, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyperion_ship::shiplog::LogKind;

    #[test]
    fn read_errors_count_against_the_log_they_name() {
        let m = Metrics::default();
        let tagged = |s: Source| anyhow::anyhow!("bad entry").context(s);
        m.read_error(&tagged(Source::Ship(LogKind::ChainState)));
        m.read_error(&tagged(Source::Ship(LogKind::ChainState)));
        m.read_error(&tagged(Source::Blocks));
        // Context added later keeps the source findable.
        m.read_error(&tagged(Source::Ship(LogKind::Trace)).context("dropping the connection"));
        // An error naming no log is not a read error of any log.
        m.read_error(&anyhow::anyhow!("task panicked"));
        let counts: Vec<u64> = m.read_errors.iter().map(get).collect();
        assert_eq!(counts, [1, 2, 0, 1]);
    }

    #[test]
    fn sessions_count_while_open() {
        let m = Metrics::default();
        let a = m.session_opened();
        let b = m.session_opened();
        assert_eq!(m.sessions(), 2);
        drop(a);
        assert_eq!(m.sessions(), 1);
        drop(b);
        assert_eq!(m.sessions(), 0);
    }
}
