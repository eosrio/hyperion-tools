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
//! | `antelope_ship_archive_api_requests_total{endpoint,code}`   | counter |
//! | `antelope_ship_archive_api_request_seconds_sum{endpoint}` / `_count` | summary |
//! | `antelope_ship_archive_api_in_flight`                       | gauge   |
//! | `antelope_ship_archive_api_rejected_total{reason}`          | counter |
//! | `antelope_ship_archive_api_failures_total{reason}`          | counter |
//! | `antelope_ship_archive_api_response_bytes_total`            | counter |
//!
//! `version` is `v0` or `v1` (the request's), `log` one of `trace`, `chain_state`, `finality`
//! and `blocks`, `state` `used` or `free`. A served range is `[begin, end)`, `0` and `0` for a
//! log not served.
//!
//! The `api_` series count the chain API (`--api`). `endpoint` is one of [`ApiRoute::ALL`]'s
//! labels and `code` the HTTP status answered; a pair appears once it has been counted. The
//! rejected `reason` is one of `in_flight`, `memory`, `body_too_large`, `timeout` and
//! `connections`, the failure `reason` one of `read`, `integrity` and `panic`.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hyperion_ship::chainapi::Endpoint;

use crate::fetch::MemoryBudget;
use crate::store::{Source, Store};

const PREFIX: &str = "antelope_ship_archive";

/// What a chain API request is counted under: the endpoint its target names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiRoute {
    GetBlockInfo,
    GetBlockHeader,
    GetRawBlock,
    /// Not served (it needs the contracts' ABIs), so always 404; counted to show the demand.
    GetBlock,
    GetSupportedApis,
    Describe,
    /// Any other target, OPTIONS, and an illegal request-target.
    Other,
}

impl ApiRoute {
    pub const ALL: [Self; 7] = [
        Self::GetBlockInfo,
        Self::GetBlockHeader,
        Self::GetRawBlock,
        Self::GetBlock,
        Self::GetSupportedApis,
        Self::Describe,
        Self::Other,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::GetBlockInfo => "get_block_info",
            Self::GetBlockHeader => "get_block_header",
            Self::GetRawBlock => "get_raw_block",
            Self::GetBlock => "get_block",
            Self::GetSupportedApis => "get_supported_apis",
            Self::Describe => "describe",
            Self::Other => "other",
        }
    }

    pub fn of(endpoint: Endpoint) -> Self {
        match endpoint {
            Endpoint::GetBlockInfo => Self::GetBlockInfo,
            Endpoint::GetBlockHeader => Self::GetBlockHeader,
            Endpoint::GetRawBlock => Self::GetRawBlock,
        }
    }

    fn at(self) -> usize {
        Self::ALL
            .iter()
            .position(|&r| r == self)
            .expect("every route is listed")
    }
}

/// Why a chain API request or connection got no whole answer of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiRejection {
    /// Every request slot stayed taken for `--api-queue-ms`.
    InFlight,
    /// The memory budget could not hold what the request needs.
    Memory,
    /// The body was larger than `--api-max-body-bytes`.
    BodyTooLarge,
    /// The headers or the body did not arrive in time, or the client took none of its answer
    /// for as long; the connection was closed.
    Timeout,
    /// `--api-max-connections` were open: the connection was answered 503 and closed, its
    /// request unread.
    Connections,
}

impl ApiRejection {
    pub const ALL: [Self; 5] = [
        Self::InFlight,
        Self::Memory,
        Self::BodyTooLarge,
        Self::Timeout,
        Self::Connections,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::InFlight => "in_flight",
            Self::Memory => "memory",
            Self::BodyTooLarge => "body_too_large",
            Self::Timeout => "timeout",
            Self::Connections => "connections",
        }
    }
}

/// A chain API request that failed on the server's side and was answered `503 Busy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiFailure {
    /// The block could not be read from its log.
    Read,
    /// The block's bytes did not decode, or its body would not render.
    Integrity,
    /// The request's job panicked.
    Panic,
}

impl ApiFailure {
    pub const ALL: [Self; 3] = [Self::Read, Self::Integrity, Self::Panic];

    pub fn label(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Integrity => "integrity",
            Self::Panic => "panic",
        }
    }
}

/// The HTTP statuses the chain API answers with, as `api_requests_total` counts them.
const API_CODES: [u16; 6] = [200, 400, 404, 413, 500, 503];

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
    /// Indexed as [`ApiRoute::ALL`], then as [`API_CODES`].
    api_requests: [[AtomicU64; 6]; 7],
    /// Indexed as [`ApiRoute::ALL`].
    api_nanos: [AtomicU64; 7],
    api_in_flight: AtomicU64,
    /// Indexed as [`ApiRejection::ALL`].
    api_rejected: [AtomicU64; 5],
    /// Indexed as [`ApiFailure::ALL`].
    api_failures: [AtomicU64; 3],
    api_bytes: AtomicU64,
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
        add(&self.read_nanos, nanos(elapsed));
    }

    /// A chain API request under `route` was answered with `status` and a body of `bytes`,
    /// `elapsed` after its headers arrived.
    pub fn api_request(&self, route: ApiRoute, status: u16, bytes: u64, elapsed: Duration) {
        let at = route.at();
        match API_CODES.iter().position(|&c| c == status) {
            Some(code) => add(&self.api_requests[at][code], 1),
            None => debug_assert!(false, "the chain API never answers {status}"),
        }
        add(&self.api_nanos[at], nanos(elapsed));
        add(&self.api_bytes, bytes);
    }

    /// Chain API requests answered with `status` under `route` so far.
    pub fn api_requests(&self, route: ApiRoute, status: u16) -> u64 {
        API_CODES
            .iter()
            .position(|&c| c == status)
            .map_or(0, |code| get(&self.api_requests[route.at()][code]))
    }

    /// Chain API requests holding a slot now.
    pub fn api_in_flight(&self) -> u64 {
        get(&self.api_in_flight)
    }

    /// A chain API request took a slot; [`Self::api_finished`] gives it back.
    pub fn api_admitted(&self) {
        add(&self.api_in_flight, 1);
    }

    pub fn api_finished(&self) {
        self.api_in_flight.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn api_rejected(&self, why: ApiRejection) {
        let at = ApiRejection::ALL.iter().position(|&r| r == why);
        add(
            &self.api_rejected[at.expect("every rejection is listed")],
            1,
        );
    }

    /// Chain API requests rejected for `why` so far.
    pub fn api_rejections(&self, why: ApiRejection) -> u64 {
        let at = ApiRejection::ALL.iter().position(|&r| r == why);
        get(&self.api_rejected[at.expect("every rejection is listed")])
    }

    pub fn api_failure(&self, what: ApiFailure) {
        let at = ApiFailure::ALL.iter().position(|&f| f == what);
        add(&self.api_failures[at.expect("every failure is listed")], 1);
    }

    /// Chain API failures of kind `what` so far.
    pub fn api_failures(&self, what: ApiFailure) -> u64 {
        let at = ApiFailure::ALL.iter().position(|&f| f == what);
        get(&self.api_failures[at.expect("every failure is listed")])
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
            Series {
                name: "api_requests_total",
                kind: "counter",
                help: "Chain API requests answered, by endpoint and HTTP status.",
                values: self.api_request_samples(),
            },
            Series {
                name: "api_request_seconds",
                kind: "summary",
                help: "Time from a chain API request's headers to its answer, by endpoint.",
                values: ApiRoute::ALL
                    .iter()
                    .flat_map(|&route| {
                        let at = route.at();
                        let label = route.label();
                        let count: u64 = self.api_requests[at].iter().map(get).sum();
                        let secs = get(&self.api_nanos[at]) as f64 / 1e9;
                        [
                            (
                                format!("_sum{{endpoint=\"{label}\"}}"),
                                format!("{secs:.6}"),
                            ),
                            (format!("_count{{endpoint=\"{label}\"}}"), count.to_string()),
                        ]
                    })
                    .collect(),
            },
            Series {
                name: "api_in_flight",
                kind: "gauge",
                help: "Chain API requests holding a slot (--api-max-in-flight).",
                values: one(self.api_in_flight()),
            },
            Series {
                name: "api_rejected_total",
                kind: "counter",
                help: "Chain API requests and connections refused, or closed before their answer was written.",
                values: ApiRejection::ALL
                    .iter()
                    .zip(&self.api_rejected)
                    .map(|(r, c)| (format!("{{reason=\"{}\"}}", r.label()), get(c).to_string()))
                    .collect(),
            },
            Series {
                name: "api_failures_total",
                kind: "counter",
                help: "Chain API requests that failed on the server's side, answered 503.",
                values: ApiFailure::ALL
                    .iter()
                    .zip(&self.api_failures)
                    .map(|(f, c)| (format!("{{reason=\"{}\"}}", f.label()), get(c).to_string()))
                    .collect(),
            },
            Series {
                name: "api_response_bytes_total",
                kind: "counter",
                help: "Chain API response body bytes.",
                values: one(get(&self.api_bytes)),
            },
        ];
        let mut out = String::with_capacity(8192);
        for s in all {
            let _ = writeln!(out, "# HELP {PREFIX}_{} {}", s.name, s.help);
            let _ = writeln!(out, "# TYPE {PREFIX}_{} {}", s.name, s.kind);
            for (suffix, value) in s.values {
                let _ = writeln!(out, "{PREFIX}_{}{suffix} {value}", s.name);
            }
        }
        out
    }

    /// `api_requests_total`: every endpoint and status pair counted so far.
    fn api_request_samples(&self) -> Vec<(String, String)> {
        let mut values = Vec::new();
        for (route, codes) in ApiRoute::ALL.iter().zip(&self.api_requests) {
            for (code, count) in API_CODES.iter().zip(codes) {
                let count = get(count);
                if count > 0 {
                    let labels = format!("{{endpoint=\"{}\",code=\"{code}\"}}", route.label());
                    values.push((labels, count.to_string()));
                }
            }
        }
        values
    }
}

fn nanos(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
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

    #[test]
    fn api_requests_count_by_endpoint_and_status() {
        let m = Metrics::default();
        m.api_request(ApiRoute::GetRawBlock, 200, 100, Duration::from_millis(2));
        m.api_request(ApiRoute::GetRawBlock, 200, 50, Duration::from_millis(1));
        m.api_request(ApiRoute::Other, 404, 10, Duration::ZERO);
        assert_eq!(m.api_requests(ApiRoute::GetRawBlock, 200), 2);
        assert_eq!(m.api_requests(ApiRoute::GetRawBlock, 503), 0);
        assert_eq!(m.api_requests(ApiRoute::Other, 302), 0);
        let pair = |labels: &str, v: &str| (labels.to_owned(), v.to_owned());
        assert_eq!(
            m.api_request_samples(),
            [
                pair("{endpoint=\"get_raw_block\",code=\"200\"}", "2"),
                pair("{endpoint=\"other\",code=\"404\"}", "1"),
            ]
        );
        assert_eq!(get(&m.api_bytes), 160);
        assert_eq!(get(&m.api_nanos[ApiRoute::GetRawBlock.at()]), 3_000_000);

        m.api_admitted();
        m.api_admitted();
        m.api_finished();
        assert_eq!(m.api_in_flight(), 1);
        m.api_rejected(ApiRejection::Memory);
        m.api_failure(ApiFailure::Integrity);
        m.api_failure(ApiFailure::Integrity);
        assert_eq!(
            ApiRejection::ALL.map(|r| m.api_rejections(r)),
            [0, 1, 0, 0, 0]
        );
        assert_eq!(ApiFailure::ALL.map(|f| m.api_failures(f)), [0, 2, 0]);
        // Labels are the routes' and the endpoints' own names.
        for ep in Endpoint::ALL {
            assert_eq!(ApiRoute::of(ep).label(), ep.name());
        }
    }
}
