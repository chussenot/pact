//! OpenTelemetry export, behind the off-by-default `otel` feature.
//!
//! ## Why this is hand-rolled and not `opentelemetry-otlp`
//!
//! Measured, not assumed (pact-aw7.1). `opentelemetry-otlp` 0.31 with
//! `default-features = false, features = ["http-proto"]` — the documented way
//! to avoid gRPC — still resolves `opentelemetry-proto/gen-tonic-messages`,
//! which pulls `tonic` → `tokio-stream` → `tokio`. `cargo tree` showed 61
//! crates and `target/release/deps` contained `libtokio.rlib`,
//! `libtonic.rlib` and `libtokio_stream.rlib`: an async runtime *compiled in*
//! to a synchronous CLI that has six runtime dependencies in total. The same
//! is true of `http-json` (64 crates) and of 0.30 (59) and 0.27 (84). There is
//! no feature combination that avoids it.
//!
//! The second measurement decided it. A `SimpleSpanProcessor` flushing on exit
//! against a *blackholed* collector — a port that completes the TCP handshake
//! and never replies, which is what a wedged collector actually looks like —
//! cost 1031-1079 ms of exit latency, twenty times the 50 ms budget, because
//! the exporter blocks reading a response that never comes.
//!
//! So: OTLP/HTTP+JSON, written by hand, over `std::net::TcpStream`.
//!
//! The response *is* read, and that surprises people — an earlier draft of this
//! paragraph said it never was, and the code has drained it since the day a
//! real `otelcol` logged `stream insert: context canceled` and dropped a batch
//! pact had already delivered (see [`imp::post`]). What keeps the drain from
//! costing what the SDK cost is that every step of a request — connect, write,
//! read — shares one deadline of [`EXIT_BUDGET_MS`]/2, so two signals fit
//! inside the budget no matter how wedged the collector is.
//!
//! What that costs, measured on the machine this was written on (`pact whoami`,
//! interleaved min-of-25 so machine load moves every variant together):
//! feature built in but unconfigured `+0 ms`, closed port `-1 ms`, healthy
//! collector `+7 ms`, blackholed collector `+36 ms`. A blackhole is therefore
//! *not* free — it costs about the exit budget, which is what the budget is
//! for. It is the 1031 ms above that the design rules out, not the 30.
//!
//! ## What is exported
//!
//! Argv *shape* only: the subcommand name and bounded, source-controlled
//! attributes. Never a message body, a lease note, a file path or any other
//! user free text — those are unbounded as metric dimensions and are not ours
//! to ship off the machine.
//!
//! ## Configuration
//!
//! Standard `OTEL_*` variables only; pact invents no names of its own.
//!
//! | Variable | Effect |
//! |---|---|
//! | `OTEL_SDK_DISABLED=true` | off |
//! | `OTEL_EXPORTER_OTLP_ENDPOINT` | base URL; `/v1/traces` and `/v1/metrics` appended |
//! | `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | full URL, used verbatim |
//! | `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` | full URL, used verbatim |
//! | `OTEL_EXPORTER_OTLP_PROTOCOL` | must be `http/json` (or unset); anything else turns pact's export off |
//! | `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` / `_METRICS_PROTOCOL` | per-signal override |
//! | `OTEL_EXPORTER_OTLP_HEADERS` | `k=v,k=v`, sent on every request |
//! | `OTEL_SERVICE_NAME` | defaults to `pact` |
//! | `OTEL_EXPORTER_OTLP_TIMEOUT` | per-request timeout in ms; only ever lowered, never raised past pact's exit budget |
//!
//! Only `http://` is understood. `https://` disables export rather than
//! pretending: TLS would mean a new dependency, which is the thing this whole
//! module exists to avoid.
//!
//! The unset-protocol default is `http/json`, not the spec's `http/protobuf`.
//! That default exists to disambiguate between protocols a client supports
//! several of; pact speaks one. The practical reason it matters here: the
//! machine this was written on exports Claude Code to `:4317` with
//! `OTEL_EXPORTER_OTLP_PROTOCOL=grpc` — pact reads that, sees a protocol it
//! cannot speak, and stays quiet instead of POSTing HTTP at a gRPC port. Point
//! pact at the collector with the per-signal variables and leave the global
//! ones alone.
//!
//! ## The API is frozen on purpose
//!
//! pact-aw7.2 through .7 instrument call sites against this module while it is
//! finished. Everything public here is deliberately reachable-but-unused for
//! now, hence the crate-wide `allow(dead_code)` below: the alternative is five
//! agents each adding the instrument they happen to need, which is how a
//! telemetry layer ends up with three ways to count the same thing.
#![allow(dead_code)]

// ---------------------------------------------------------------------------
// Types shared by both builds. These exist whether or not the feature is on,
// so a call site never needs a #[cfg].
// ---------------------------------------------------------------------------

/// An attribute value. Keys are `&'static str` by design: an attribute key
/// comes from the source, never from user input, which is half the cardinality
/// problem solved by the type system.
#[derive(Debug, Clone)]
pub enum Val {
    Static(&'static str),
    Text(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

impl From<&'static str> for Val {
    fn from(v: &'static str) -> Self {
        Val::Static(v)
    }
}
impl From<String> for Val {
    fn from(v: String) -> Self {
        Val::Text(v)
    }
}
impl From<i64> for Val {
    fn from(v: i64) -> Self {
        Val::Int(v)
    }
}
impl From<u64> for Val {
    fn from(v: u64) -> Self {
        Val::Int(v as i64)
    }
}
impl From<usize> for Val {
    fn from(v: usize) -> Self {
        Val::Int(v as i64)
    }
}
impl From<i32> for Val {
    fn from(v: i32) -> Self {
        Val::Int(v as i64)
    }
}
impl From<f64> for Val {
    fn from(v: f64) -> Self {
        Val::Float(v)
    }
}
impl From<bool> for Val {
    fn from(v: bool) -> Self {
        Val::Bool(v)
    }
}

/// One attribute. `&[Attr]` is what every instrument takes.
pub type Attr = (&'static str, Val);

/// Sugar for an attribute slice, so call sites are not a wall of `.into()`.
///
/// ```ignore
/// otel::count("pact.lease.acquire", 1, &otel::attrs!["pact.outcome" => "acquired"]);
/// ```
#[macro_export]
macro_rules! attrs {
    () => { [] };
    ($($k:expr => $v:expr),+ $(,)?) => {
        [$(($k, $crate::otel::Val::from($v))),+]
    };
}
#[allow(unused_imports)]
pub use crate::attrs;

/// How long a whole flush may add to process exit, in milliseconds. Not an
/// env var on purpose: it is a promise pact makes about its own exit latency,
/// not a knob for an operator to get wrong.
///
/// 30 and not 45: pact-aw7.1's budget is 50 ms added, and the worst case here
/// is the whole budget spent on a blackholed collector. Leaving 20 ms of
/// headroom means the promise survives a slower machine than this one.
pub const EXIT_BUDGET_MS: u64 = 30;

/// What `pact doctor` reports about export, in both builds. Built in and
/// configured is not the same as exporting — see [`imp::export_status`].
pub struct ExportStatus {
    /// Configured to export, and not exporting. Warns rather than fails, the
    /// same call `protocol files reach a clone` makes: pact reports the
    /// situation instead of deciding the operator is wrong.
    pub warn: bool,
    pub detail: String,
}

// ---------------------------------------------------------------------------

#[cfg(feature = "otel")]
mod imp {
    //! The feature is on. Everything here is `std` plus `serde_json`, which
    //! pact already depends on — see the module docs for why not the SDK.

    /// A canonical 8-4-4-4-12 lowercase-hex UUID, and nothing else.
    ///
    /// Deliberately strict rather than a length check: this value becomes part of
    /// every metric series identity, so anything not provably one-per-session is a
    /// cardinality bomb. Rejecting a malformed id costs one lost join; accepting
    /// one costs the metrics backend.
    pub(super) fn is_uuid(s: &str) -> bool {
        let mut parts = s.split('-');
        for len in [8, 4, 4, 4, 12] {
            match parts.next() {
                Some(p) if p.len() == len && p.bytes().all(|b| b.is_ascii_hexdigit()) => {}
                _ => return false,
            }
        }
        parts.next().is_none()
    }
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use serde_json::{json, Value as J};

    use super::{Attr, ExportStatus, Val, EXIT_BUDGET_MS};

    /// OTel's default explicit histogram bucket boundaries, in milliseconds.
    /// Hard-coded rather than configurable: every pact duration is a
    /// subprocess or a file write, and these buckets already straddle that.
    const BOUNDS: [f64; 15] = [
        0.0, 5.0, 10.0, 25.0, 50.0, 75.0, 100.0, 250.0, 500.0, 750.0, 1000.0, 2500.0, 5000.0,
        7500.0, 10000.0,
    ];

    /// Longest attribute value we will ship. `pact.agent` and friends come
    /// from the environment, and an environment variable is not a promise.
    const MAX_VALUE_LEN: usize = 256;

    /// Per-request slice of [`EXIT_BUDGET_MS`]. Halving it means a wedged
    /// traces endpoint cannot eat the whole budget and silently starve
    /// metrics, and it caps the worst case at the budget rather than twice it.
    ///
    /// It is a *whole-request* budget, shared by connect, write and read. It
    /// used to be handed to each of the three separately, which made one
    /// request's true worst case `3 x PER_REQUEST_MS` and the flush's `6 x` —
    /// twice the number this constant is named for.
    const PER_REQUEST_MS: u64 = EXIT_BUDGET_MS / 2;

    /// How many spans / metric points we keep before dropping the rest.
    ///
    /// Nothing drains these until the process exits, and `pact ui` is a process
    /// that runs for hours buffering roughly one `pact.beads.exec` span a second
    /// (pact-aw7.9). Unbounded, that is both a leak and — worse — a silent
    /// export failure: at ~1500 buffered spans, serializing the batch cost more
    /// than the whole flush budget and a release build exported *nothing at
    /// all*, metrics included, with no connection even attempted.
    ///
    /// 512 because a release build serializes 800 spans inside the 30 ms budget
    /// and a debug build is ~5x slower; 512 keeps both honest, and is far past
    /// the biggest real batch (`lease acquire` with 300 paths = 301 spans).
    ///
    /// ponytail: dropping the tail is the lazy half. The other half is the
    /// periodic flush in `tui.rs`, which keeps a long session from ever
    /// reaching the cap; raise this only if a real batch is found clipping.
    const MAX_BUFFERED: usize = 512;

    #[derive(Debug)]
    struct Endpoint {
        /// Resolved once, in [`build_state`]. `to_socket_addrs` is the one step
        /// of the export path no timeout can cover — a resolver that does not
        /// answer costs whatever glibc's `timeout x attempts` happens to be (5 s
        /// x 2 by default), and it used to run inside `post`, i.e. inside the
        /// exit budget, twice per process. Paying it up front does not make DNS
        /// interruptible, but it happens once, before any work is buffered, and
        /// a name that does not resolve now disables the signal outright — which
        /// `pact doctor` can then say out loud instead of exporting silence.
        addr: SocketAddr,
        host: String,
        port: u16,
        path: String,
    }

    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
    enum Kind {
        Sum,
        Histogram,
        Gauge,
    }

    #[derive(Debug)]
    struct SpanRec {
        id: u64,
        parent: Option<u64>,
        name: &'static str,
        start_ns: u128,
        end_ns: u128,
        attrs: Vec<Attr>,
        error: Option<&'static str>,
    }

    #[derive(Debug)]
    struct MetricRec {
        name: &'static str,
        kind: Kind,
        attrs: Vec<Attr>,
        value: f64,
    }

    struct State {
        traces: Option<Endpoint>,
        metrics: Option<Endpoint>,
        headers: Vec<(String, String)>,
        resource: Vec<Attr>,
        trace_id: u128,
        /// Whole-request budget: connect + write + read together.
        request_timeout: Duration,
        /// When this process started collecting, in epoch nanos. DELTA data
        /// points need an aggregation *window*, and `start == end` gave every
        /// one a zero-width one: a rate derived from it divides by zero, and
        /// delta-to-cumulative conversion has no interval to attribute the
        /// delta to. The collector answers `200 partialSuccess:{}` either way,
        /// so this fails silently at ingest and only shows up as an unusable
        /// chart much later.
        start_ns: u128,
        spans: Mutex<Vec<SpanRec>>,
        points: Mutex<Vec<MetricRec>>,
    }

    static STATE: OnceLock<Option<State>> = OnceLock::new();

    thread_local! {
        /// Open spans, innermost last. A child span's parent is whatever is on
        /// top when it starts, which is what gives `pact lease acquire` →
        /// `bd create` a real parent/child edge without threading a context
        /// object through every function signature.
        static STACK: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
    }

    fn state() -> Option<&'static State> {
        STATE.get().and_then(|s| s.as_ref())
    }

    // -- configuration ------------------------------------------------------

    fn env(key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|v| !v.trim().is_empty())
    }

    /// True when this signal's protocol is one we can actually speak.
    /// Unset means yes; see the module docs for why that differs from the spec
    /// default, and why the machine that exports Claude Code over gRPC needs it.
    fn protocol_ok(signal: &str) -> bool {
        let p = env(&format!("OTEL_EXPORTER_OTLP_{signal}_PROTOCOL"))
            .or_else(|| env("OTEL_EXPORTER_OTLP_PROTOCOL"));
        match p {
            None => true,
            Some(v) => v.trim().eq_ignore_ascii_case("http/json"),
        }
    }

    /// Split `http://host:port/path` without a URL crate. Returns `None` for
    /// anything we cannot honestly serve, `https` above all: TLS means a
    /// dependency, and avoiding one is the reason this module exists.
    ///
    /// Splitting only. Resolution is [`resolve`], so a test can check the parse
    /// without touching the network.
    pub(super) fn parse_endpoint(url: &str, default_path: &str) -> Option<(String, u16, String)> {
        let rest = url.trim().strip_prefix("http://")?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], rest[i..].to_string()),
            None => (rest, default_path.to_string()),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().ok()?),
            None => (authority.to_string(), 80u16),
        };
        if host.is_empty() {
            return None;
        }
        // Both go verbatim into a hand-written request — `POST {path} HTTP/1.1`
        // and `Host: {host}:{port}` — so a control character here is request
        // splitting. `parse_headers` already refuses one in a header name or
        // value for exactly this reason; the path was the half nobody checked,
        // and it was injectable: an endpoint of
        // `http://127.0.0.1:4318/v1/traces\r\nX-Injected: pwned` put that line
        // on the wire, verified against a raw socket.
        //
        // `host` was guarded only by accident, because `to_socket_addrs` will
        // not resolve a name containing CRLF. Accidental guards stop being
        // guards when the code around them changes, so it is checked here too.
        //
        // Refused rather than stripped, the same way `https://` is: a caller who
        // asked for something pact cannot honestly serve gets no export and a
        // `pact doctor` line saying so, not a quietly rewritten request.
        if host.chars().any(char::is_control) || path.chars().any(char::is_control) {
            return None;
        }
        Some((host, port, path))
    }

    fn resolve(url: &str, default_path: &str) -> Option<Endpoint> {
        let (host, port, path) = parse_endpoint(url, default_path)?;
        let addr = (host.as_str(), port).to_socket_addrs().ok()?.next()?;
        Some(Endpoint {
            addr,
            host,
            port,
            path,
        })
    }

    /// The URL configured for a signal, before we decide whether we can speak
    /// to it. Split out from [`endpoint_for`] so [`export_status`] can name the
    /// endpoint it is refusing.
    fn endpoint_url(signal: &str, path: &str) -> Option<String> {
        // Per spec the per-signal variable is the full URL, path included.
        if let Some(specific) = env(&format!("OTEL_EXPORTER_OTLP_{signal}_ENDPOINT")) {
            return Some(specific);
        }
        let base = env("OTEL_EXPORTER_OTLP_ENDPOINT")?;
        Some(format!("{}{path}", base.trim_end_matches('/')))
    }

    fn endpoint_for(signal: &str, path: &str) -> Option<Endpoint> {
        if !protocol_ok(signal) {
            return None;
        }
        resolve(&endpoint_url(signal, path)?, path)
    }

    /// `k=v,k=v`. A header whose name or value carries a control character is
    /// dropped rather than escaped: this string reaches a socket as a request
    /// header, and a CR in it is header injection.
    fn parse_headers() -> Vec<(String, String)> {
        let raw = match env("OTEL_EXPORTER_OTLP_HEADERS") {
            Some(v) => v,
            None => return Vec::new(),
        };
        raw.split(',')
            .filter_map(|pair| pair.split_once('='))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .filter(|(k, v)| {
                !k.is_empty()
                    && !k.chars().any(|c| c.is_control() || c == ':')
                    && !v.chars().any(char::is_control)
            })
            .collect()
    }

    fn random_u128() -> Option<u128> {
        let mut buf = [0u8; 16];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut buf))
            .ok()?;
        Some(u128::from_be_bytes(buf))
    }

    fn now_ns() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    }

    fn truncate(mut s: String) -> String {
        if s.len() > MAX_VALUE_LEN {
            s.truncate(MAX_VALUE_LEN);
        }
        s
    }

    // -- lifecycle ----------------------------------------------------------

    /// Held for the life of the process. Call [`Guard::finish`] before any
    /// `std::process::exit`, which skips destructors; `Drop` is the backstop
    /// for the paths that just return.
    #[derive(Debug)]
    pub struct Guard {
        root: Option<u64>,
        start: Instant,
        start_ns: u128,
        finished: bool,
        attrs: Vec<Attr>,
    }

    /// A span. Ends when dropped.
    #[derive(Debug)]
    pub struct Span {
        id: u64,
        start_ns: u128,
        name: &'static str,
        attrs: Vec<Attr>,
        error: Option<&'static str>,
        live: bool,
    }

    pub fn init(command: &'static str) -> Guard {
        STATE.get_or_init(build_state);
        // `command` is argv *shape*: a subcommand name from a fixed set in
        // main.rs, never an argument value.
        let root = state().map(|_| start_span());
        ROOT_NAME.get_or_init(|| command);
        Guard {
            root,
            start: Instant::now(),
            start_ns: now_ns(),
            finished: false,
            attrs: Vec::new(),
        }
    }

    /// Name of the root span, set once by [`init`] and read back when the
    /// guard closes it.
    static ROOT_NAME: OnceLock<&'static str> = OnceLock::new();

    fn build_state() -> Option<State> {
        if env("OTEL_SDK_DISABLED")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
        {
            return None;
        }
        let traces = endpoint_for("TRACES", "/v1/traces");
        let metrics = endpoint_for("METRICS", "/v1/metrics");
        if traces.is_none() && metrics.is_none() {
            return None;
        }
        let trace_id = random_u128()?;

        // Resource attributes only: identity that is constant for the process.
        // No service.instance.id — the OTel guidance assumes a long-lived
        // service, and a fresh id per CLI invocation would make every metric
        // series unique, which is the cardinality bomb this epic must not ship.
        let mut resource: Vec<Attr> = vec![
            (
                "service.name",
                Val::Text(env("OTEL_SERVICE_NAME").unwrap_or_else(|| "pact".into())),
            ),
            ("service.version", Val::Static(env!("CARGO_PKG_VERSION"))),
        ];
        if let Some(agent) = env("PACT_AGENT").filter(|a| crate::identity::validate(a).is_ok()) {
            // Bounded by fleet size, and the join key the whole epic wants —
            // but only once it has been through the same validation the rest of
            // pact trusts. Truncating at 256 chars was not a bound: every
            // metrics backend folds a resource attribute into series identity,
            // so `PACT_AGENT=$(uuidgen)` minted a brand-new series per
            // invocation, and a 207-character `pact.agent` was measured shipping
            // from a run pact itself had *rejected* with exit 1. That is the
            // cardinality bomb `service.instance.id` was left out to avoid,
            // walking back in through the environment.
            resource.push(("pact.agent", Val::Text(agent)));
        }

        // The join to Claude Code's telemetry, which is already in the same
        // collector. Claude Code exports CLAUDE_CODE_SESSION_ID into every
        // subprocess it spawns — pact included — and it is byte-identical to
        // the `session.id` on every metric and log record that session emits.
        // Without it, "did this agent burn tokens waiting on a lease" can only
        // be answered by eyeballing two panels on one time axis, which stops
        // working the moment two agents run concurrently, i.e. always.
        //
        // Named `session.id`, not `pact.session_id`, so both services group on
        // one key with no aliasing.
        //
        // The UUID filter is the `pact.agent` lesson applied a second time: a
        // resource attribute folds into metric series identity, so an
        // unvalidated environment variable is a cardinality bomb. A UUID is
        // bounded by "one per Claude Code session", which is the thing being
        // counted. Absent rather than empty when a human runs pact from a
        // plain terminal — an empty string is a series too.
        if let Some(id) = env("CLAUDE_CODE_SESSION_ID").filter(|s| is_uuid(s)) {
            resource.push(("session.id", Val::Text(id)));
        }

        // The spec default for this is 10 s. Honour a smaller value, ignore a
        // larger one: exit latency is pact's promise to make, not the
        // collector operator's.
        let timeout = env("OTEL_EXPORTER_OTLP_TIMEOUT")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(PER_REQUEST_MS)
            .min(PER_REQUEST_MS);

        Some(State {
            traces,
            metrics,
            headers: parse_headers(),
            resource,
            trace_id,
            request_timeout: Duration::from_millis(timeout.max(1)),
            start_ns: now_ns(),
            spans: Mutex::new(Vec::new()),
            points: Mutex::new(Vec::new()),
        })
    }

    /// What `pact doctor` says about export. Built in and configured is not the
    /// same as exporting: `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf` is the
    /// OTel *spec default* and `https://` is every hosted collector, and pact
    /// speaks neither. Both used to produce a green build, a green doctor and
    /// no data — choosing to speak one protocol is defensible, being silent
    /// about the choice is the defect.
    pub fn export_status() -> ExportStatus {
        let off = |why: String| ExportStatus {
            warn: false,
            detail: format!("built in, off ({why})"),
        };
        if env("OTEL_SDK_DISABLED").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
            return off("OTEL_SDK_DISABLED=true".into());
        }
        if endpoint_url("TRACES", "/v1/traces").is_none()
            && endpoint_url("METRICS", "/v1/metrics").is_none()
        {
            return off("no OTEL_EXPORTER_OTLP_ENDPOINT".into());
        }
        // Same memoized state the exporter uses, so doctor cannot report a
        // configuration the running process did not actually adopt.
        let st = STATE.get_or_init(build_state).as_ref();
        let live: Vec<&str> = st
            .into_iter()
            .flat_map(|s| {
                [
                    s.traces.as_ref().map(|_| "traces"),
                    s.metrics.as_ref().map(|_| "metrics"),
                ]
            })
            .flatten()
            .collect();
        if live.len() == 2 {
            let ep = st.and_then(|s| s.traces.as_ref());
            return ExportStatus {
                warn: false,
                detail: match ep {
                    Some(e) => format!("traces + metrics → http://{}:{}", e.host, e.port),
                    None => "traces + metrics".to_string(),
                },
            };
        }
        let mut reasons: Vec<(String, String)> = Vec::new();
        for (signal, path) in [("TRACES", "/v1/traces"), ("METRICS", "/v1/metrics")] {
            if endpoint_for(signal, path).is_some() {
                continue;
            }
            let why = match endpoint_url(signal, path) {
                None => "no endpoint configured".to_string(),
                Some(_) if !protocol_ok(signal) => format!(
                    "OTEL_EXPORTER_OTLP_PROTOCOL={} — pact speaks http/json and nothing else",
                    env(&format!("OTEL_EXPORTER_OTLP_{signal}_PROTOCOL"))
                        .or_else(|| env("OTEL_EXPORTER_OTLP_PROTOCOL"))
                        .unwrap_or_default()
                ),
                Some(url) if !url.trim().starts_with("http://") => {
                    "the endpoint is not http:// — pact has no TLS, which would mean the \
                     dependency this exporter exists to avoid"
                        .to_string()
                }
                Some(url) => format!(
                    "{} does not resolve",
                    parse_endpoint(&url, path).map(|(h, ..)| h).unwrap_or(url)
                ),
            };
            reasons.push((signal.to_ascii_lowercase(), why));
        }
        // Both signals normally fail for the same reason, and saying it twice
        // buries it. Name the signal only when they actually differ.
        let joined = match reasons.as_slice() {
            [(_, a), (_, b)] if a == b => a.clone(),
            rest => rest
                .iter()
                .map(|(s, w)| format!("{s}: {w}"))
                .collect::<Vec<_>>()
                .join("; "),
        };
        ExportStatus {
            warn: true,
            detail: format!("built in and configured, but NOT exporting — {joined}"),
        }
    }

    impl Guard {
        /// Put an attribute on the root span. Separate from [`init`] because
        /// the things that identify an invocation — the repo, the resolved
        /// agent, `--json` — are known to `main` only after argv is parsed,
        /// and a root span that carries none of them is a trace you cannot
        /// join to anything (pact-aw7.2).
        pub fn set(&mut self, key: &'static str, val: impl Into<Val>) {
            if self.root.is_some() {
                self.attrs.push((key, val.into()));
            }
        }

        /// Record the process exit code on the root span and flush. Takes
        /// `self` so it cannot be called twice, and must be called before
        /// `std::process::exit` — that call does not run destructors, and a
        /// trace that only appears when pact succeeds is a trace that hides
        /// every failure worth looking at.
        pub fn finish(mut self, exit_code: i32) {
            self.finished = true;
            let Some(root) = self.root.take() else {
                return;
            };
            let elapsed = self.start.elapsed().as_secs_f64() * 1000.0;
            let mut attrs = std::mem::take(&mut self.attrs);
            attrs.push(("pact.exit_code", Val::Int(exit_code as i64)));
            end_span_by_id(
                root,
                self.start_ns,
                attrs,
                (exit_code != 0).then_some("error"),
            );
            // Only the two bounded dimensions go on the metric. The subcommand
            // is what makes the histogram readable at all — "pact is slow" is
            // never the question, "`msg send` is slow" is — and it is a fixed
            // set of literals in main.rs. pact.repo and pact.agent stay off:
            // they are fine on a span and a per-fleet-member series explosion
            // on a metric.
            record_ms(
                "pact.command.duration",
                elapsed,
                &[
                    (
                        "pact.subcommand",
                        Val::Static(ROOT_NAME.get().copied().unwrap_or("unknown")),
                    ),
                    ("pact.exit_code", Val::Int(exit_code as i64)),
                ],
            );
            flush_now();
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            if self.finished {
                return;
            }
            if let Some(root) = self.root.take() {
                end_span_by_id(root, self.start_ns, std::mem::take(&mut self.attrs), None);
            }
            flush_now();
        }
    }

    // -- spans --------------------------------------------------------------

    fn start_span() -> u64 {
        let id = (now_ns() as u64) ^ ((std::process::id() as u64) << 32) ^ next_seq();
        STACK.with(|s| s.borrow_mut().push(id));
        id
    }

    /// A counter so two spans opened in the same nanosecond cannot collide.
    fn next_seq() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(1);
        SEQ.fetch_add(1, Ordering::Relaxed)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
    }

    pub fn span(name: &'static str) -> Span {
        if state().is_none() {
            return Span {
                id: 0,
                start_ns: 0,
                name,
                attrs: Vec::new(),
                error: None,
                live: false,
            };
        }
        Span {
            id: start_span(),
            start_ns: now_ns(),
            name,
            attrs: Vec::new(),
            error: None,
            live: true,
        }
    }

    impl Span {
        pub fn set(&mut self, key: &'static str, val: impl Into<Val>) {
            if self.live {
                self.attrs.push((key, val.into()));
            }
        }

        /// Mark the span failed. `kind` is a `&'static str` on purpose — a
        /// bounded reason code, not an error message, which would be user text.
        pub fn fail(&mut self, kind: &'static str) {
            if self.live {
                self.error = Some(kind);
            }
        }
    }

    impl Drop for Span {
        fn drop(&mut self) {
            if !self.live {
                return;
            }
            let parent = STACK.with(|s| {
                let mut st = s.borrow_mut();
                let pos = st.iter().rposition(|&i| i == self.id);
                if let Some(p) = pos {
                    st.remove(p);
                    st.get(p.wrapping_sub(1)).copied().filter(|_| p > 0)
                } else {
                    st.last().copied()
                }
            });
            if let Some(st) = state() {
                if let Ok(mut spans) = st.spans.lock() {
                    // Capped, not unbounded: see MAX_BUFFERED. Dropping the
                    // tail of a runaway session is strictly better than the
                    // old behaviour, which was to keep everything and then
                    // export none of it.
                    if spans.len() >= MAX_BUFFERED {
                        return;
                    }
                    spans.push(SpanRec {
                        id: self.id,
                        parent,
                        name: self.name,
                        start_ns: self.start_ns,
                        end_ns: now_ns(),
                        attrs: std::mem::take(&mut self.attrs),
                        error: self.error,
                    });
                }
            }
        }
    }

    /// End the root span, which `Guard` owns by id rather than by value so
    /// that `finish()` can close it after `run()` has already returned.
    fn end_span_by_id(id: u64, start_ns: u128, attrs: Vec<Attr>, error: Option<&'static str>) {
        STACK.with(|s| {
            let mut st = s.borrow_mut();
            if let Some(p) = st.iter().rposition(|&i| i == id) {
                st.remove(p);
            }
        });
        if let Some(st) = state() {
            if let Ok(mut spans) = st.spans.lock() {
                // Unconditional, unlike a child span: the root is the one span
                // that carries the exit code, and a trace with every child but
                // no root is worse than one span over the cap.
                spans.push(SpanRec {
                    id,
                    parent: None,
                    name: ROOT_NAME.get().copied().unwrap_or("pact"),
                    start_ns,
                    end_ns: now_ns(),
                    attrs,
                    error,
                });
            }
        }
    }

    // -- metrics ------------------------------------------------------------

    fn push_point(name: &'static str, kind: Kind, value: f64, attrs: &[Attr]) {
        let Some(st) = state() else { return };
        if let Ok(mut p) = st.points.lock() {
            if p.len() >= MAX_BUFFERED {
                return;
            }
            p.push(MetricRec {
                name,
                kind,
                attrs: attrs.to_vec(),
                value,
            });
        }
    }

    pub fn count(name: &'static str, by: u64, attrs: &[Attr]) {
        push_point(name, Kind::Sum, by as f64, attrs);
    }

    pub fn record_ms(name: &'static str, ms: f64, attrs: &[Attr]) {
        push_point(name, Kind::Histogram, ms, attrs);
    }

    pub fn gauge(name: &'static str, value: i64, attrs: &[Attr]) {
        push_point(name, Kind::Gauge, value as f64, attrs);
    }

    // -- encoding -----------------------------------------------------------

    fn any_value(v: &Val) -> J {
        match v {
            Val::Static(s) => json!({ "stringValue": s }),
            Val::Text(s) => json!({ "stringValue": s.as_str() }),
            Val::Int(i) => json!({ "intValue": i.to_string() }),
            Val::Float(f) => json!({ "doubleValue": f }),
            Val::Bool(b) => json!({ "boolValue": b }),
        }
    }

    fn kv_list(attrs: &[Attr]) -> J {
        J::Array(
            attrs
                .iter()
                .map(|(k, v)| json!({ "key": k, "value": any_value(v) }))
                .collect(),
        )
    }

    /// A stable key for "same metric, same attribute set", so repeated calls
    /// aggregate into one data point instead of N duplicates the collector
    /// would have to reconcile.
    fn series_key(m: &MetricRec) -> String {
        let mut parts: Vec<String> = m
            .attrs
            .iter()
            .map(|(k, v)| {
                let val = match v {
                    Val::Static(s) => (*s).to_string(),
                    Val::Text(s) => s.clone(),
                    Val::Int(i) => i.to_string(),
                    Val::Float(f) => f.to_string(),
                    Val::Bool(b) => b.to_string(),
                };
                format!("{k}={val}")
            })
            .collect();
        parts.sort();
        format!("{}|{:?}|{}", m.name, m.kind, parts.join(","))
    }

    fn resource_json(st: &State) -> J {
        json!({ "attributes": kv_list(&st.resource) })
    }

    fn traces_body(st: &State, spans: &[SpanRec]) -> Option<Vec<u8>> {
        if spans.is_empty() {
            return None;
        }
        let trace_id = format!("{:032x}", st.trace_id);
        let encoded: Vec<J> = spans
            .iter()
            .map(|s| {
                let mut o = json!({
                    "traceId": trace_id,
                    "spanId": format!("{:016x}", s.id),
                    "name": s.name,
                    "kind": 1,
                    "startTimeUnixNano": s.start_ns.to_string(),
                    "endTimeUnixNano": s.end_ns.to_string(),
                    "attributes": kv_list(&s.attrs),
                });
                if let Some(p) = s.parent {
                    o["parentSpanId"] = json!(format!("{p:016x}"));
                }
                if let Some(kind) = s.error {
                    o["status"] = json!({ "code": 2, "message": kind });
                }
                o
            })
            .collect();
        serde_json::to_vec(&json!({
            "resourceSpans": [{
                "resource": resource_json(st),
                "scopeSpans": [{ "scope": { "name": "pact" }, "spans": encoded }]
            }]
        }))
        .ok()
    }

    fn metrics_body(st: &State, points: &[MetricRec]) -> Option<Vec<u8>> {
        if points.is_empty() {
            return None;
        }
        let now = now_ns().to_string();
        // The DELTA window is [process start, now), not [now, now). See
        // `State::start_ns`.
        let start = st.start_ns.min(now_ns()).to_string();
        let mut series: BTreeMap<String, Vec<&MetricRec>> = BTreeMap::new();
        for p in points {
            series.entry(series_key(p)).or_default().push(p);
        }
        // DELTA (1), not CUMULATIVE: each invocation is a whole process whose
        // counters start at zero and die with it. Reporting them as cumulative
        // would make every run look like a counter reset.
        let mut metrics: Vec<J> = Vec::new();
        for group in series.values() {
            let first = group[0];
            let attrs = kv_list(&first.attrs);
            let point_base = json!({
                "attributes": attrs,
                "startTimeUnixNano": start,
                "timeUnixNano": now,
            });
            let m = match first.kind {
                Kind::Sum => {
                    let total: f64 = group.iter().map(|p| p.value).sum();
                    let mut dp = point_base;
                    dp["asInt"] = json!((total as i64).to_string());
                    json!({ "name": first.name, "unit": "1",
                            "sum": { "dataPoints": [dp], "aggregationTemporality": 1,
                                     "isMonotonic": true } })
                }
                Kind::Gauge => {
                    let mut dp = point_base;
                    dp["asInt"] = json!((group.last().unwrap().value as i64).to_string());
                    json!({ "name": first.name, "unit": "1",
                            "gauge": { "dataPoints": [dp] } })
                }
                Kind::Histogram => {
                    let values: Vec<f64> = group.iter().map(|p| p.value).collect();
                    let mut buckets = vec![0u64; BOUNDS.len() + 1];
                    for v in &values {
                        let i = BOUNDS.iter().position(|b| v <= b).unwrap_or(BOUNDS.len());
                        buckets[i] += 1;
                    }
                    let sum: f64 = values.iter().sum();
                    let mut dp = point_base;
                    dp["count"] = json!(values.len().to_string());
                    dp["sum"] = json!(sum);
                    dp["min"] = json!(values.iter().cloned().fold(f64::INFINITY, f64::min));
                    dp["max"] = json!(values.iter().cloned().fold(f64::NEG_INFINITY, f64::max));
                    dp["bucketCounts"] =
                        json!(buckets.iter().map(|c| c.to_string()).collect::<Vec<_>>());
                    dp["explicitBounds"] = json!(BOUNDS);
                    json!({ "name": first.name, "unit": "ms",
                            "histogram": { "dataPoints": [dp], "aggregationTemporality": 1 } })
                }
            };
            metrics.push(m);
        }
        serde_json::to_vec(&json!({
            "resourceMetrics": [{
                "resource": resource_json(st),
                "scopeMetrics": [{ "scope": { "name": "pact" }, "metrics": metrics }]
            }]
        }))
        .ok()
    }

    // -- transport ----------------------------------------------------------

    /// Time left before `deadline`, or `None` once it has passed. Every step of
    /// a request goes through this, so `PER_REQUEST_MS` is what one request
    /// costs in total rather than what each of its three blocking calls may
    /// cost — the difference between a 15 ms worst case and a 45 ms one.
    fn left(deadline: Instant) -> Option<Duration> {
        deadline.checked_duration_since(Instant::now())
    }

    /// POST one body, abandoning at `deadline` whatever step we are on.
    ///
    /// The response IS read, and the module docs explain why at length: closing
    /// a socket with unread inbound data makes the kernel send RST, which can
    /// make the peer discard a request it has received but not yet handed to
    /// its handler.
    fn post(ep: &Endpoint, headers: &[(String, String)], body: &[u8], deadline: Instant) {
        let Some(t) = left(deadline) else { return };
        let Ok(mut s) = TcpStream::connect_timeout(&ep.addr, t) else {
            return;
        };
        let Some(t) = left(deadline) else { return };
        let _ = s.set_write_timeout(Some(t));
        let _ = s.set_nodelay(true);

        let mut req = Vec::with_capacity(body.len() + 256);
        let _ = write!(
            req,
            "POST {} HTTP/1.1\r\nHost: {}:{}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n",
            ep.path,
            ep.host,
            ep.port,
            body.len()
        );
        for (k, v) in headers {
            let _ = write!(req, "{k}: {v}\r\n");
        }
        req.extend_from_slice(b"\r\n");
        req.extend_from_slice(body);

        // A partial write is not recoverable: the collector is reading a body
        // shorter than the Content-Length we declared, so half-closing here
        // hands it a truncated request it can only reject. Discarding this Err
        // was measured delivering 2 633 835 of 5 242 900 declared bytes under a
        // slow-draining peer. Abandoning costs the batch; sending costs the
        // batch AND a 400 in the collector's log.
        if s.write_all(&req).is_err() {
            return;
        }
        let _ = s.flush();
        // Half-close so the collector sees a complete request.
        let _ = s.shutdown(Shutdown::Write);

        // Read the status line and throw it away. The value is not the
        // response — it is that closing a socket with unread inbound data
        // makes the kernel send RST, which can make the peer discard a
        // request it has received but not yet handed to its handler. The
        // capture harness for this module showed exactly that shape: the
        // sink parsed the whole body, then died writing a reply nobody was
        // listening for.
        //
        // The budget is not theoretical. Against a real
        // `otelcol` otlpreceiver v0.126.0 a 10 ms drain was too short: the
        // receiver answered in 9.4 ms, pact gave up first, and the collector
        // logged `stream insert: context canceled` and rejected the batch.
        // SigNoz on the same machine answers in 1.6 ms. PER_REQUEST_MS is
        // sized to clear both with room to spare.
        let Some(t) = left(deadline) else { return };
        let _ = s.set_read_timeout(Some(t));
        let mut sink = [0u8; 256];
        let _ = s.read(&mut sink);
    }

    /// Drain both buffers and POST them. Safe to call more than once — the
    /// long-lived `pact ui` does, on a timer, because a whole session's
    /// telemetry arriving in one 30 ms window at exit is a session's telemetry
    /// nobody sees (pact-aw7.9).
    pub fn flush_now() {
        if let Some(st) = state() {
            flush_state(st);
        }
    }

    /// Takes the state rather than reading the global, so a test can point one
    /// at a listener it owns. Nothing exercised this function at all until the
    /// day it was found exporting nothing.
    fn flush_state(st: &State) {
        let spans = st
            .spans
            .lock()
            .map(|mut s| std::mem::take(&mut *s))
            .unwrap_or_default();
        let points = st
            .points
            .lock()
            .map(|mut p| std::mem::take(&mut *p))
            .unwrap_or_default();

        // No shared deadline across the two signals, and serialization is not
        // charged against one. Both were the same bug: the old code started a
        // 30 ms clock, serialized BOTH bodies inside the loop's array literal,
        // and then checked the clock — so once the span batch took longer than
        // the budget to encode, flush returned having posted nothing, and the
        // cheap little metrics body died with it. Silently, at exit, with no
        // connection even attempted. `PER_REQUEST_MS` is half the budget
        // precisely so two independent per-request deadlines still add up to
        // the promise; MAX_BUFFERED is what bounds the serialization.
        for (ep, body) in [
            (st.traces.as_ref(), traces_body(st, &spans)),
            (st.metrics.as_ref(), metrics_body(st, &points)),
        ] {
            let (Some(ep), Some(body)) = (ep, body) else {
                continue;
            };
            post(ep, &st.headers, &body, Instant::now() + st.request_timeout);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        use std::io::BufRead;
        use std::net::TcpListener;

        #[test]
        fn endpoint_appends_the_signal_path_to_a_base_url() {
            let (host, port, path) =
                parse_endpoint("http://localhost:4318/v1/traces", "/v1/traces").unwrap();
            assert_eq!(
                (host.as_str(), port, path.as_str()),
                ("localhost", 4318, "/v1/traces")
            );
        }

        #[test]
        fn https_is_refused_rather_than_faked() {
            assert!(parse_endpoint("https://collector:4318/v1/traces", "/v1/traces").is_none());
        }

        #[test]
        fn endpoint_without_a_port_defaults_to_80() {
            let (_, port, path) = parse_endpoint("http://collector", "/v1/traces").unwrap();
            assert_eq!((port, path.as_str()), (80, "/v1/traces"));
        }

        /// A State pointing at `addr`, with nothing read from the environment —
        /// these tests run in one process alongside every other test, so
        /// touching the global STATE or `OTEL_*` would make them each other's
        /// flakes.
        fn state_for(addr: SocketAddr) -> State {
            let ep = |path: &str| Endpoint {
                addr,
                host: addr.ip().to_string(),
                port: addr.port(),
                path: path.to_string(),
            };
            State {
                traces: Some(ep("/v1/traces")),
                metrics: Some(ep("/v1/metrics")),
                headers: Vec::new(),
                resource: Vec::new(),
                trace_id: 1,
                request_timeout: Duration::from_millis(PER_REQUEST_MS),
                start_ns: now_ns() - 1_000_000,
                spans: Mutex::new(Vec::new()),
                points: Mutex::new(Vec::new()),
            }
        }

        fn span_rec(i: u64) -> SpanRec {
            SpanRec {
                id: i,
                parent: None,
                name: "pact.lease.acquire",
                start_ns: 1,
                end_ns: 2,
                attrs: Vec::new(),
                error: None,
            }
        }

        /// THE regression that matters (pact-aw7.1/.9). `flush` used to start
        /// one 30 ms clock, serialize both bodies, and only then look at it —
        /// so a big enough span batch made the first check fail and the whole
        /// flush return having POSTed nothing, metrics included. Measured on a
        /// release build: 800 spans exported, 1500 spans exported *nothing*,
        /// with no connection even attempted. That is what this asserts: a full
        /// buffer still reaches the wire.
        #[test]
        fn a_full_buffer_still_posts_both_signals() {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let sink = std::thread::spawn(move || {
                let mut paths = Vec::new();
                for stream in listener.incoming().take(2) {
                    let mut s = stream.unwrap();
                    let mut first = String::new();
                    std::io::BufReader::new(s.try_clone().unwrap())
                        .read_line(&mut first)
                        .unwrap();
                    let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}");
                    paths.push(first);
                }
                paths
            });

            let st = state_for(addr);
            {
                let mut spans = st.spans.lock().unwrap();
                for i in 0..MAX_BUFFERED as u64 {
                    spans.push(span_rec(i + 1));
                }
                st.points.lock().unwrap().push(MetricRec {
                    name: "pact.command.duration",
                    kind: Kind::Histogram,
                    attrs: Vec::new(),
                    value: 1.0,
                });
            }
            flush_state(&st);

            let paths = sink.join().unwrap();
            assert!(paths[0].starts_with("POST /v1/traces"), "{paths:?}");
            assert!(paths[1].starts_with("POST /v1/metrics"), "{paths:?}");
            // And the buffers were drained, so a second flush is not a resend.
            assert!(st.spans.lock().unwrap().is_empty());
        }

        /// The other half: the buffer is bounded. `pact ui` buffers ~1 span a
        /// second for a whole session with nothing draining it between the
        /// periodic flushes, so an uncapped Vec is both a leak and the way back
        /// to the batch above.
        #[test]
        fn the_span_buffer_stops_growing_at_the_cap() {
            let st = state_for("127.0.0.1:1".parse().unwrap());
            {
                let mut spans = st.spans.lock().unwrap();
                for i in 0..MAX_BUFFERED as u64 + 50 {
                    if spans.len() >= MAX_BUFFERED {
                        continue; // the guard Span::drop applies
                    }
                    spans.push(span_rec(i + 1));
                }
            }
            assert_eq!(st.spans.lock().unwrap().len(), MAX_BUFFERED);
        }

        /// DELTA data points carry their aggregation window in
        /// `startTimeUnixNano`..`timeUnixNano`. Both were the flush timestamp,
        /// so every window was zero-width: any rate divides by zero, and the
        /// collector answers 200 regardless, so it fails silently at ingest.
        #[test]
        fn delta_points_carry_a_non_zero_aggregation_window() {
            let st = state_for("127.0.0.1:1".parse().unwrap());
            let body = metrics_body(
                &st,
                &[MetricRec {
                    name: "pact.lease.transitions",
                    kind: Kind::Sum,
                    attrs: Vec::new(),
                    value: 1.0,
                }],
            )
            .unwrap();
            let v: J = serde_json::from_slice(&body).unwrap();
            let dp =
                &v["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["sum"]["dataPoints"][0];
            let start: u128 = dp["startTimeUnixNano"].as_str().unwrap().parse().unwrap();
            let end: u128 = dp["timeUnixNano"].as_str().unwrap().parse().unwrap();
            assert!(end > start, "zero-width DELTA window: {start} == {end}");
        }

        /// One request's budget is the whole request, not each of its three
        /// blocking calls. `left` returning None is what stops a wedged
        /// collector turning connect+write+read into 3x PER_REQUEST_MS.
        #[test]
        fn a_passed_deadline_leaves_no_time_for_the_next_step() {
            let now = Instant::now();
            assert!(left(now + Duration::from_secs(1)).is_some());
            assert!(left(now - Duration::from_millis(1)).is_none());
        }

        /// Header injection: OTEL_EXPORTER_OTLP_HEADERS is environment input
        /// that lands verbatim in a request, so a CR in it must not survive.
        #[test]
        fn headers_with_control_characters_are_dropped() {
            let good = [("a".to_string(), "b".to_string())];
            let bad = [("x".to_string(), "y\r\nHost: evil".to_string())];
            assert!(!good[0].1.chars().any(char::is_control));
            assert!(bad[0].1.chars().any(char::is_control));
        }

        #[test]
        fn histogram_buckets_place_values_in_the_first_bound_they_fit() {
            let find = |v: f64| BOUNDS.iter().position(|b| v <= *b).unwrap_or(BOUNDS.len());
            assert_eq!(find(0.0), 0);
            assert_eq!(find(7.0), 2); // (5, 10]
            assert_eq!(find(99_999.0), BOUNDS.len());
        }
    }
}
#[cfg(feature = "otel")]
#[allow(unused_imports)]
pub use imp::{count, export_status, flush_now, gauge, init, record_ms, span, Guard, Span};

#[cfg(not(feature = "otel"))]
mod imp {
    //! The feature is off. Every entry point is an empty inlined function with
    //! the same signature as the real one, so no call site carries a `#[cfg]`
    //! and no `Cargo.toml` dependency is added.
    use super::{Attr, ExportStatus, Val};

    /// Held for the life of the process; dropping it flushes. A unit struct
    /// here, so `let _otel = otel::init("lease");` costs nothing.
    #[derive(Debug)]
    pub struct Guard;

    /// A span guard. Ends when dropped.
    #[derive(Debug)]
    pub struct Span;

    impl Guard {
        #[inline(always)]
        pub fn set(&mut self, _key: &'static str, _val: impl Into<Val>) {}
        #[inline(always)]
        pub fn finish(self, _exit_code: i32) {}
    }

    #[inline(always)]
    pub fn init(_command: &'static str) -> Guard {
        Guard
    }

    #[inline(always)]
    pub fn span(_name: &'static str) -> Span {
        Span
    }

    impl Span {
        #[inline(always)]
        pub fn set(&mut self, _key: &'static str, _val: impl Into<Val>) {}
        #[inline(always)]
        pub fn fail(&mut self, _kind: &'static str) {}
    }

    #[inline(always)]
    pub fn count(_name: &'static str, _by: u64, _attrs: &[Attr]) {}
    #[inline(always)]
    pub fn record_ms(_name: &'static str, _ms: f64, _attrs: &[Attr]) {}
    #[inline(always)]
    pub fn gauge(_name: &'static str, _value: i64, _attrs: &[Attr]) {}
    #[inline(always)]
    pub fn flush_now() {}

    /// Not a `#[cfg]` at the call site: `pact doctor` emits the same set of
    /// check names in both builds, which `scripts/check-docs.sh` compares
    /// against docs/tui.md as an exact set. A check that exists in one build
    /// only would red the docs gate in the other.
    pub fn export_status() -> ExportStatus {
        ExportStatus {
            warn: false,
            detail: "not built in (`cargo build --features otel`)".to_string(),
        }
    }
}

#[cfg(not(feature = "otel"))]
#[allow(unused_imports)]
pub use imp::{count, export_status, flush_now, gauge, init, record_ms, span, Guard, Span};

#[cfg(test)]
mod tests {

    /// A control character in the endpoint is request splitting: `path` goes
    /// verbatim into `POST {path} HTTP/1.1` and `host` into `Host:`. Verified
    /// against a raw socket before the guard existed — an endpoint of
    /// `http://127.0.0.1:4318/v1/traces\r\nX-Injected: pwned` put
    /// `X-Injected: pwned HTTP/1.1` on the wire as its own line.
    ///
    /// `parse_headers` already refused this in a header name or value; the path
    /// was the half nobody checked. `host` was guarded only incidentally, by
    /// `to_socket_addrs` failing to resolve a name with CRLF in it, and an
    /// accidental guard is one that disappears when the code around it moves.
    #[cfg(feature = "otel")]
    #[test]
    fn an_endpoint_carrying_a_control_character_is_refused_not_sanitised() {
        let ok = super::imp::parse_endpoint("http://127.0.0.1:4318/v1/traces", "/v1/traces");
        assert!(ok.is_some(), "a normal endpoint must still parse");

        for bad in [
            "http://127.0.0.1:4318/v1/traces\r\nX-Injected: pwned",
            "http://127.0.0.1:4318/v1/traces\nX-Injected: pwned",
            "http://127.0.0.1:4318/v1/tra\tces",
            "http://127.0.0.1\r\nX: y:4318/v1/traces",
        ] {
            assert!(
                super::imp::parse_endpoint(bad, "/v1/traces").is_none(),
                "must refuse {bad:?} outright — sanitising would silently rewrite \
                 an endpoint the operator asked for"
            );
        }
    }

    /// `session.id` joins pact's traces to Claude Code's metrics and logs in the
    /// same collector, so it must be exactly the session's UUID or absent —
    /// never a best-effort string. A resource attribute folds into metric series
    /// identity, which is the `pact.agent` lesson (a 207-char value minted a new
    /// series per invocation) applied a second time (pact-acf).
    // Only exists in the otel build: is_uuid guards a resource attribute a
    // default build never constructs.
    #[cfg(feature = "otel")]
    #[test]
    fn only_a_real_uuid_is_accepted_as_a_session_id() {
        assert!(super::imp::is_uuid("18886d2a-f31f-41ce-94f9-8694ac635753"));
        assert!(super::imp::is_uuid("00000000-0000-0000-0000-000000000000"));

        for bad in [
            "",
            "not-a-uuid",
            "18886d2a-f31f-41ce-94f9",                    // too few groups
            "18886d2a-f31f-41ce-94f9-8694ac635753-extra", // too many
            "18886d2a-f31f-41ce-94f9-8694ac63575",        // short last group
            "18886d2az-f31f-41ce-94f9-8694ac635753",      // non-hex
            "18886d2a f31f 41ce 94f9 8694ac635753",       // spaces, not dashes
        ] {
            assert!(!super::imp::is_uuid(bad), "should reject {bad:?}");
        }

        // 200 chars of hex is still not a UUID: length is the bomb, and a
        // permissive check is how it gets shipped.
        assert!(!super::imp::is_uuid(&"a".repeat(200)));
    }

    use super::*;

    /// The whole point of the shim: this compiles and runs identically with
    /// and without the feature, and `mise run check` runs it both ways.
    #[test]
    fn api_is_callable_in_both_builds() {
        let mut g = init("lease acquire");
        g.set("pact.repo", String::from("pact"));
        g.set("pact.json", true);
        let mut s = span("lease.verify");
        s.set("pact.paths", 3usize);
        s.fail("held");
        count("pact.lease.acquire", 1, &attrs!["pact.outcome" => "held"]);
        record_ms("pact.command.duration", 1.5, &attrs![]);
        gauge("pact.lease.active", 2, &attrs!["pact.repo" => "pact"]);
    }

    #[test]
    fn val_conversions_cover_the_types_call_sites_use() {
        assert!(matches!(Val::from("a"), Val::Static("a")));
        assert!(matches!(Val::from(String::from("a")), Val::Text(_)));
        assert!(matches!(Val::from(3usize), Val::Int(3)));
        assert!(matches!(Val::from(3i64), Val::Int(3)));
        assert!(matches!(Val::from(3i32), Val::Int(3)));
        assert!(matches!(Val::from(3u64), Val::Int(3)));
        assert!(matches!(Val::from(1.5f64), Val::Float(_)));
        assert!(matches!(Val::from(true), Val::Bool(true)));
    }
}
