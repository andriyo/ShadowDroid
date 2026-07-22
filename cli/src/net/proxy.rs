//! The hand-rolled MITM proxy engine.
//!
//! Built on `hyper` (http1, `.with_upgrades()`) + `tokio-rustls` for TLS
//! termination + `reqwest` for upstream forwarding (reusing the rustls stack
//! already in the binary). Modelled on hudsucker's `process_connect`/`proxy`
//! pipeline, trimmed to http/1.1-only with buffered bodies (so we can capture
//! and — later — modify them).
//!
//! Flow of a request:
//!   client ──CONNECT host:443──▶ [process_connect]: 200, then on a spawned task
//!     peek 4 bytes → TLS? mint a leaf via [crate::net::ca] + `TlsAcceptor`,
//!     serve http1 over the decrypted stream; not-TLS / out-of-scope → blind
//!     `copy_bidirectional` tunnel.
//!   each decrypted (or plaintext) request ──▶ [proxy_request]: buffer body,
//!     forward upstream via reqwest, capture a [FlowRecord], return the response.
//!
//! Footguns: peek+rewind is mandatory (rustls must
//! read the ClientHello from byte 0); `serve_connection().with_upgrades()` is
//! required or CONNECT hangs; the 200 is returned synchronously and the MITM
//! work happens on a detached task.

use anyhow::{Result, anyhow};
use bytes::Bytes;
use futures_util::StreamExt;
use http::uri::{Authority, Scheme};
use http_body_util::{BodyExt, BodyStream, Empty, Full, StreamBody, combinators::UnsyncBoxBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::task::{Context as TaskCtx, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::events::{self, Event};
use crate::net::ca::CertAuthority;
use crate::net::flow::{self, FlowRecord};
use crate::net::{Matcher, Mutation, RuleSpec, store, ws};

/// The response body we hand the device: either a buffered `Full` (the common
/// case) or a live `StreamBody` for streamed responses — unified as one boxed
/// type. `Unsync` because the streamed variant wraps reqwest's `Send`-but-not-
/// `Sync` byte stream (hyper only needs the response body to be `Send`).
type ProxyBody = UnsyncBoxBody<Bytes, std::io::Error>;

/// Buffer bodies up to this size, then spill to a streamed pass-through. Bounds
/// per-response memory and, with the `text/event-stream` short-circuit, stops an
/// infinite/large response from hanging or OOMing the daemon (issue: #1/#6).
const BUFFER_CAP: usize = 8 * 1024 * 1024;

/// Decoded responses use the same memory budget as buffered wire bodies. This
/// prevents a tiny compressed payload from expanding without bound while still
/// allowing every response the proxy could have buffered uncompressed.
const DECOMPRESSED_CAP: usize = BUFFER_CAP;
const DECOMPRESSION_CONCURRENCY: usize = 4;
const DECOMPRESSION_QUEUE_TIMEOUT: Duration = Duration::from_secs(5);
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const UPSTREAM_READ_TIMEOUT: Duration = Duration::from_secs(60);

/// A fully-buffered body from `bytes`.
fn full_body(bytes: Bytes) -> ProxyBody {
    Full::new(bytes)
        .map_err(|never: Infallible| match never {})
        .boxed_unsync()
}

/// Everything a proxy connection needs. Cloned (Arc) per connection.
pub struct ProxyContext {
    pub ca: Arc<CertAuthority>,
    pub client: reqwest::Client,
    /// Completed flows are pushed here; the daemon drains → store + broadcast.
    pub flow_tx: mpsc::Sender<FlowRecord>,
    pub shared: Arc<SharedState>,
    /// This daemon's device serial — used to persist a `tls_error` to the
    /// session log so `net log` can recall handshake failures.
    pub serial: crate::ids::Serial,
    pub capture_session_id: String,
    /// Apply platform certificate and hostname verification to every upstream
    /// TLS leg, including WebSockets (reqwest handles ordinary HTTP requests).
    pub verify_upstream: bool,
    /// Every connection/tunnel task is tracked so daemon shutdown can stop
    /// accepting, cancel active I/O, and drain completed flow records before
    /// removing readiness markers.
    pub tasks: TaskTracker,
    pub shutdown: CancellationToken,
}

/// Runtime-mutable proxy knobs. (Rules land here in P3.)
pub struct SharedState {
    pub anticache: bool,
    pub anticomp: bool,
    /// Redact completed captures before store/broadcast. Forwarded traffic is
    /// never changed by this policy.
    pub redaction: Option<crate::redaction::Policy>,
    /// Host globs to MITM + capture. Empty = all hosts.
    pub host_filters: Vec<String>,
    /// Active interception config (`net intercept`), or `None`.
    pub intercept: RwLock<Option<InterceptCfg>>,
    /// Flows currently paused, awaiting `net resume`/`drop`/`respond`.
    pub held: Mutex<HashMap<String, HeldFlow>>,
    /// Recently terminal holds, retained only long enough to explain why an
    /// action raced with release, timeout, or client cancellation.
    pub terminal_holds: Mutex<TerminalHoldHistory>,
    /// Live event fan-out (shared with the daemon) — carries `http_intercept`.
    pub events: broadcast::Sender<Arc<Event>>,
    /// Declarative rules (`net rule`), applied in order: `(id, spec)`.
    pub rules: RwLock<Vec<(String, RuleSpec)>>,
    /// Saved flows served as canned responses (`net replay`), or `None`.
    pub replay: RwLock<Option<Vec<FlowRecord>>>,
    /// Hosts we've already reported a `tls_error` for, so a client that keeps
    /// retrying a rejected handshake produces one signal, not a flood.
    pub tls_errors_seen: Mutex<HashSet<String>>,
    /// Completed captures discarded because the bounded persistence queue was
    /// full or closed. Exposed in `net status` so data loss is never silent.
    pub dropped_flows: AtomicU64,
    /// Flow/TLS records delivered to the daemon but not durably appended.
    /// Kept separate from queue drops so disk-full/permission failures are
    /// visible and diagnosable through `net status`.
    pub persistence_errors: AtomicU64,
    /// Approximate retained bytes across currently held flow snapshots.
    pub held_bytes: Arc<AtomicU64>,
    /// Matching flows that failed open because the held count/byte budget was
    /// exhausted. Exposed in status so overload is observable.
    pub rejected_holds: AtomicU64,
}

impl SharedState {
    /// Should we decrypt + capture this host (vs blind-tunnel it through)?
    pub fn host_in_scope(&self, host: &str) -> bool {
        self.host_filters.is_empty() || self.host_filters.iter().any(|h| host_glob_match(h, host))
    }

    pub fn record_persistence_error(&self, kind: &str, error: &anyhow::Error) {
        let count = self.persistence_errors.fetch_add(1, Ordering::Relaxed) + 1;
        if count == 1 || count.is_power_of_two() {
            tracing::warn!(count, kind, error = %error, "network capture persistence failed");
        }
    }
}

/// Where + what to intercept (set by `net intercept`).
#[derive(Clone)]
pub struct InterceptCfg {
    pub matcher: Matcher,
    pub at_request: bool,
    pub at_response: bool,
    pub hold_ms: u32,
    /// On hold-deadline: drop (fail-closed) vs resume unmodified (fail-open).
    pub on_timeout_drop: bool,
}

/// A paused flow: the oneshot that releases it + a snapshot for `net show`.
pub struct HeldFlow {
    pub tx: Option<oneshot::Sender<HoldDecision>>,
    pub meta: FlowRecord,
    pub phase: String,
    pub held_at: f64,
    pub expires_at: f64,
    held_charge: Option<(Arc<AtomicU64>, u64)>,
}

impl HeldFlow {
    pub fn lifecycle(&self) -> HeldFlowLifecycle {
        HeldFlowLifecycle {
            id: self.meta.id.clone(),
            phase: self.phase.clone(),
            state: "held",
            held_at: self.held_at,
            expires_at: self.expires_at,
            client_connected: self.tx.as_ref().is_some_and(|tx| !tx.is_closed()),
        }
    }

    pub(crate) fn terminal(&self, state: &'static str, action: Option<&str>) -> TerminalHold {
        TerminalHold {
            id: self.meta.id.clone(),
            phase: self.phase.clone(),
            state,
            held_at: self.held_at,
            expires_at: self.expires_at,
            terminal_at: events::now_ts(),
            action: action.map(str::to_string),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HeldFlowLifecycle {
    pub id: String,
    pub phase: String,
    pub state: &'static str,
    pub held_at: f64,
    pub expires_at: f64,
    pub client_connected: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TerminalHold {
    pub id: String,
    pub phase: String,
    /// released | deadline_expired | client_canceled
    pub state: &'static str,
    pub held_at: f64,
    pub expires_at: f64,
    pub terminal_at: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

const TERMINAL_HOLD_HISTORY_CAP: usize = 256;

#[derive(Default)]
pub struct TerminalHoldHistory {
    records: HashMap<String, TerminalHold>,
    order: VecDeque<String>,
}

impl TerminalHoldHistory {
    pub fn get(&self, id: &str) -> Option<TerminalHold> {
        self.records.get(id).cloned()
    }

    pub fn remove(&mut self, id: &str) {
        self.records.remove(id);
        self.order.retain(|candidate| candidate != id);
    }

    fn record(&mut self, terminal: TerminalHold) {
        self.remove(&terminal.id);
        self.order.push_back(terminal.id.clone());
        self.records.insert(terminal.id.clone(), terminal);
        while self.order.len() > TERMINAL_HOLD_HISTORY_CAP {
            if let Some(id) = self.order.pop_front() {
                self.records.remove(&id);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum ReleaseHeldResult {
    Released(TerminalHold),
    ClientCanceled(TerminalHold),
    DeadlineExpired(TerminalHold),
    Missing,
}

/// Eagerly retire holds whose deadline elapsed or whose client-side receiver
/// disappeared. Terminal history and the active map are locked in that order
/// everywhere, so an action can never observe the entry absent from both.
pub(crate) fn prune_inactive_holds(
    held: &Mutex<HashMap<String, HeldFlow>>,
    terminal_holds: &Mutex<TerminalHoldHistory>,
) {
    let now = events::now_ts();
    let mut terminal_holds = terminal_holds.lock().unwrap();
    let mut held = held.lock().unwrap();
    let terminal = held
        .iter()
        .filter_map(|(id, flow)| {
            if now >= flow.expires_at {
                Some((id.clone(), "deadline_expired"))
            } else if flow.tx.as_ref().is_none_or(oneshot::Sender::is_closed) {
                Some((id.clone(), "client_canceled"))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    for (id, state) in terminal {
        if let Some(flow) = held.remove(&id) {
            terminal_holds.record(flow.terminal(state, None));
        }
    }
}

fn terminalize_held(
    held: &Mutex<HashMap<String, HeldFlow>>,
    terminal_holds: &Mutex<TerminalHoldHistory>,
    id: &str,
    state: &'static str,
) -> bool {
    let mut terminal_holds = terminal_holds.lock().unwrap();
    let removed = held.lock().unwrap().remove(id);
    if let Some(held) = removed {
        terminal_holds.record(held.terminal(state, None));
        true
    } else {
        false
    }
}

impl Drop for HeldFlow {
    fn drop(&mut self) {
        if let Some((counter, bytes)) = self.held_charge.take() {
            counter.fetch_sub(bytes, Ordering::Relaxed);
        }
    }
}

/// The agent's verdict on a held flow.
pub enum HoldDecision {
    /// Continue (optionally mutated).
    Resume(Mutation),
    /// Kill it; the device sees a connection error or this status.
    Drop(Option<u16>),
    /// Short-circuit with a canned response (request phase = never hits upstream).
    Respond {
        status: u16,
        body: Vec<u8>,
        headers: Vec<(String, String)>,
    },
}

/// Build the upstream reqwest client. Doesn't follow redirects (we pass them to
/// the app). By default it accepts invalid upstream certs — this is a debugging
/// proxy and dev/staging backends are often self-signed; `verify_upstream`
/// (`net start --verify-upstream`) turns validation back on.
pub fn build_upstream_client(verify_upstream: bool) -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .danger_accept_invalid_certs(!verify_upstream)
        // A total request deadline would tear down healthy SSE/gRPC-style
        // streams after a fixed wall-clock duration. Bound connection setup
        // and idle reads instead, so progressing streams may remain open.
        .connect_timeout(UPSTREAM_CONNECT_TIMEOUT)
        .read_timeout(UPSTREAM_READ_TIMEOUT)
        .build()
        .expect("build upstream reqwest client")
}

/// Bind the proxy listener without starting its accept loop.
///
/// Keeping this separate from [`serve`] lets the daemon prove that the proxy
/// port is owned before it publishes the control endpoint as ready.
pub async fn bind(addr: SocketAddr) -> Result<TcpListener> {
    TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow!("bind proxy {addr}: {e}"))
}

/// Serve an already-bound proxy listener until `shutdown` fires.
pub async fn serve(
    ctx: Arc<ProxyContext>,
    listener: TcpListener,
    mut shutdown: oneshot::Receiver<()>,
) -> Result<()> {
    let addr = listener.local_addr()?;
    tracing::info!("net proxy listening on {addr}");
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (tcp, _peer) = match accepted {
                    Ok(v) => v,
                    Err(e) => { tracing::debug!("accept: {e}"); continue; }
                };
                let task_ctx = ctx.clone();
                let shutdown = ctx.shutdown.clone();
                drop(ctx.tasks.spawn(async move {
                    let io = TokioIo::new(tcp);
                    let svc = service_fn(move |req| {
                        let ctx = task_ctx.clone();
                        async move { handle(ctx, req, None).await }
                    });
                    tokio::select! {
                        result = http1::Builder::new().serve_connection(io, svc).with_upgrades() => {
                            if let Err(e) = result {
                                tracing::debug!("proxy connection error: {e}");
                            }
                        }
                        _ = shutdown.cancelled() => {}
                    }
                }));
            }
            _ = &mut shutdown => {
                tracing::info!("net proxy shutting down");
                break;
            }
        }
    }
    Ok(())
}

/// One request on a proxy connection. `tunnel` is `Some((https, authority))` on
/// the decrypted inner connection of a CONNECT, `None` on the outer connection.
async fn handle(
    ctx: Arc<ProxyContext>,
    req: Request<Incoming>,
    tunnel: Option<(Scheme, Authority)>,
) -> Result<Response<ProxyBody>, Infallible> {
    if req.method() == Method::CONNECT {
        return Ok(process_connect(ctx, req));
    }
    // A WebSocket upgrade can't go through the buffered request path (the body is
    // a bidirectional frame stream that never "completes"). Relay the handshake
    // and raw-tunnel the two upgraded connections instead.
    if is_websocket_upgrade(req.headers()) {
        return Ok(proxy_websocket(ctx, req, tunnel).await);
    }
    let method = req.method().clone();
    match proxy_request(ctx, req, tunnel).await {
        Ok(resp) => Ok(resp),
        Err(e) => {
            tracing::debug!("proxy_request error: {e}");
            Ok(error_response_for(
                &method,
                StatusCode::BAD_GATEWAY,
                &e.to_string(),
            ))
        }
    }
}

/// Establish a CONNECT tunnel: return 200 immediately, then on a detached task
/// peek the first bytes — TLS ClientHello → MITM, else blind TCP tunnel.
fn process_connect(ctx: Arc<ProxyContext>, req: Request<Incoming>) -> Response<ProxyBody> {
    let authority = match req.uri().authority().cloned() {
        Some(a) => a,
        None => return error_response(StatusCode::BAD_REQUEST, "CONNECT requires an authority"),
    };
    let host = authority.host().to_string();

    let tasks = ctx.tasks.clone();
    let shutdown = ctx.shutdown.clone();
    let task = async move {
        let mut req = req;
        let upgraded = match hyper::upgrade::on(&mut req).await {
            Ok(u) => u,
            Err(e) => {
                tracing::debug!("CONNECT upgrade error: {e}");
                return;
            }
        };
        let mut io = TokioIo::new(upgraded);

        // Peek to sniff TLS (0x16 = handshake record) vs plaintext, then rewind.
        let mut peek = [0u8; 8];
        let n = match io.read(&mut peek).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        let is_tls = peek[0] == 0x16;
        let stream = Rewind::new(io, peek[..n].to_vec());

        if is_tls && ctx.shared.host_in_scope(&host) {
            let server_config = match ctx.ca.server_config(&host) {
                Ok(c) => c,
                Err(e) => {
                    tracing::debug!("server_config {host}: {e}");
                    return;
                }
            };
            let tls = match TlsAcceptor::from(server_config).accept(stream).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!("TLS accept {host}: {e}");
                    report_tls_error(&ctx, &host, &e);
                    return;
                }
            };
            let io = TokioIo::new(tls);
            let svc = service_fn(move |r| {
                let ctx = ctx.clone();
                let tunnel = Some((Scheme::HTTPS, authority.clone()));
                async move { handle(ctx, r, tunnel).await }
            });
            // `auto` negotiates HTTP/1.1 vs HTTP/2 from the ALPN the leaf offered,
            // so an h2 app is served h2 (not downgraded); `with_upgrades` keeps the
            // HTTP/1.1 WebSocket path working.
            if let Err(e) =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection_with_upgrades(io, svc)
                    .await
            {
                tracing::debug!("inner serve {host}: {e}");
            }
        } else {
            // Out of scope or not TLS: blind passthrough, no decryption.
            let mut stream = stream;
            let upstream = TcpStream::connect(authority.as_ref()).await;
            match upstream {
                Ok(mut upstream) => {
                    let _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream).await;
                }
                Err(e) => tracing::debug!("blind tunnel connect {authority}: {e}"),
            }
        }
    };
    drop(tasks.spawn(async move {
        tokio::select! {
            _ = task => {}
            _ = shutdown.cancelled() => {}
        }
    }));

    // 200 OK (empty body) == "tunnel established".
    Response::new(full_body(Bytes::new()))
}

/// Report a failed inner TLS handshake (issue #5): persist a `tls_error` to the
/// session log (so `net log` recalls it) and broadcast it (so `watch` shows it
/// live). Deduped per host so a retrying client yields one signal, not a flood.
fn report_tls_error(ctx: &ProxyContext, host: &str, err: &std::io::Error) {
    {
        let mut seen = ctx.shared.tls_errors_seen.lock().unwrap();
        if !seen.insert(host.to_string()) {
            return; // already reported this host this session
        }
    }
    let ev = Event::TlsError {
        ts: events::now_ts(),
        capture_session_id: ctx.capture_session_id.clone(),
        host: host.to_string(),
        reason: tls_failure_reason(err),
        next_actions: crate::net::tls_error_next_actions(&ctx.serial),
    };
    if let Err(error) = crate::net::store::append_event(&ctx.serial, &ev) {
        ctx.shared.record_persistence_error("tls_error", &error);
    }
    let _ = ctx.shared.events.send(Arc::new(ev));
}

/// Turn a `TlsAcceptor::accept` error into an agent-actionable reason. A fatal
/// alert from the peer during the handshake is, in a MITM, overwhelmingly "I
/// don't trust your certificate"; everything else is a lower-level failure.
fn tls_failure_reason(err: &std::io::Error) -> String {
    let raw = err.to_string();
    if raw.to_lowercase().contains("alert") {
        format!(
            "the app rejected the proxy's TLS certificate ({raw}) — it does not trust the MITM CA \
             (or the connection is certificate-pinned). Verify with `net check <pkg>`, install \
             trust with `net trust`, confirm the app's Network Security Config allows user CAs, \
             and see `net ca info` for the active CA."
        )
    } else {
        format!("TLS handshake with the app failed before any request: {raw}")
    }
}

// ── WebSocket tunneling (issue: in-scope wss broke) ───────────────────────────

/// A request carrying `Connection: upgrade` + `Upgrade: websocket`.
fn is_websocket_upgrade(headers: &http::HeaderMap) -> bool {
    let contains = |name: http::header::HeaderName, needle: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_lowercase().contains(needle))
    };
    contains(http::header::CONNECTION, "upgrade") && contains(http::header::UPGRADE, "websocket")
}

/// Any stream hyper's client can drive, boxed so TLS and plaintext upstreams
/// share one type.
trait IoStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> IoStream for T {}

/// Relay a WebSocket handshake to the real server and, on `101`, raw-tunnel the
/// two upgraded connections until either side closes. In-scope handshakes are
/// captured as a flow (frames aren't decoded) — the goal is that wss *works*
/// while the proxy is scoped to the host instead of silently breaking.
async fn proxy_websocket(
    ctx: Arc<ProxyContext>,
    mut req: Request<Incoming>,
    tunnel: Option<(Scheme, Authority)>,
) -> Response<ProxyBody> {
    let (scheme, host, path, _url) = match resolve_target(req.uri(), &tunnel) {
        Ok(t) => t,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    let port = ws_target_port(req.uri(), &tunnel, &scheme);
    let req_headers = header_pairs(req.headers());
    let id = flow::new_id();
    let in_scope = ctx.shared.host_in_scope(&host);

    let (status, resp_headers, upstream_resp) = match ws_handshake_upstream(
        &scheme,
        &host,
        port,
        &path,
        &req_headers,
        ctx.verify_upstream,
        &ctx.tasks,
        &ctx.shutdown,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            if in_scope {
                capture(
                    &ctx,
                    error_flow(
                        &id,
                        req.method(),
                        &scheme,
                        &host,
                        &path,
                        &req_headers,
                        &[],
                        false,
                        None,
                        &[],
                        0,
                        e.to_string(),
                        Some("websocket".into()),
                        false,
                        &[],
                    ),
                );
            }
            return error_response(StatusCode::BAD_GATEWAY, &e.to_string());
        }
    };

    if status != 101 {
        // Server declined the upgrade — this is a completed HTTP transaction,
        // captured as a flow. Then stream its response straight back; a rejection
        // body may be arbitrarily large (or long-lived), so it must never go
        // through an unbounded `collect()`.
        if in_scope {
            capture(
                &ctx,
                FlowParts {
                    id: &id,
                    method: req.method().as_str(),
                    scheme: &scheme,
                    host: &host,
                    path: &path,
                    req_headers: &req_headers,
                    req_bytes: &[],
                    req_streamed: false,
                    status: Some(status),
                    resp_headers: &resp_headers,
                    resp_bytes: &[],
                    dur_ms: 0,
                    error: None,
                    matched: Some("websocket".into()),
                    modified: false,
                    rule_ids: &[],
                },
            );
        }
        return response_with_body(
            status,
            &resp_headers,
            incoming_proxy_body(upstream_resp.into_body()),
            content_length(&resp_headers),
        );
    }

    let upstream_io = match hyper::upgrade::on(upstream_resp).await {
        Ok(u) => u,
        Err(e) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("upstream ws upgrade: {e}"),
            );
        }
    };
    let shutdown = ctx.shutdown.clone();

    if !in_scope {
        // Out-of-scope host: preserve the working blind tunnel, no capture.
        drop(ctx.tasks.spawn(async move {
            tokio::select! {
                result = hyper::upgrade::on(&mut req) => match result {
                    Ok(device_io) => {
                        let mut a = TokioIo::new(device_io);
                        let mut b = TokioIo::new(upstream_io);
                        tokio::select! {
                            _ = copy_bidirectional(&mut a, &mut b) => {}
                            _ = shutdown.cancelled() => {}
                        }
                    }
                    Err(e) => tracing::debug!("device ws upgrade: {e}"),
                },
                _ = shutdown.cancelled() => {}
            }
        }));
        return ws_switching_response(&resp_headers);
    }

    // In-scope: record the upgrade as a WebSocket session and tap its frames.
    let ws_scheme = if scheme == "https" { "wss" } else { "ws" };
    let deflate =
        ws::parse_deflate_params(flow::header_get(&resp_headers, "sec-websocket-extensions"));
    let mut session = ws::WsSessionRecord {
        kind: "ws_open".to_string(),
        id: ws::next_session_id(),
        flow_sequence: flow::next_sequence(),
        capture_session_id: ctx.capture_session_id.clone(),
        ts: events::now_ts(),
        scheme: ws_scheme.to_string(),
        host: host.clone(),
        path: path.clone(),
        status,
        subprotocol: flow::header_get(&resp_headers, "sec-websocket-protocol").map(str::to_string),
        permessage_deflate: deflate.enabled,
        req_headers: req_headers.clone(),
        resp_headers: resp_headers.clone(),
        redaction_policy: None,
        redaction_policy_version: None,
    };
    // The handshake carries Cookie/Authorization; redact them like HTTP flows.
    if let Some(policy) = &ctx.shared.redaction {
        session.redact_headers(policy);
    }
    let meta = ws::WsSessionMeta {
        id: session.id.clone(),
        capture_session_id: session.capture_session_id.clone(),
        host: host.clone(),
        started_ts: session.ts,
        deflate,
    };
    let ctx_tap = ctx.clone();
    drop(ctx.tasks.spawn(async move {
        tokio::select! {
            result = hyper::upgrade::on(&mut req) => match result {
                Ok(device_io) => {
                    // Record the session only once the device side actually
                    // upgraded — a dropped/failed upgrade must not leave an
                    // orphan `ws_open` that never gets a `ws_close`.
                    if let Err(error) = store::append_ws_session(&ctx_tap.serial, &session) {
                        ctx_tap.shared.record_persistence_error("ws_open", &error);
                    }
                    let _ = ctx_tap
                        .shared
                        .events
                        .send(Arc::new(session.open_event(&ctx_tap.serial)));
                    ws::tap(
                        ctx_tap,
                        meta,
                        TokioIo::new(device_io),
                        TokioIo::new(upstream_io),
                    )
                    .await;
                }
                Err(e) => tracing::debug!("device ws upgrade: {e}"),
            },
            _ = shutdown.cancelled() => {}
        }
    }));
    ws_switching_response(&resp_headers)
}

fn ws_target_port(uri: &Uri, tunnel: &Option<(Scheme, Authority)>, scheme: &str) -> u16 {
    if let Some((_, authority)) = tunnel {
        if let Some(p) = authority.port_u16() {
            return p;
        }
    } else if let Some(p) = uri.port_u16() {
        return p;
    }
    if scheme == "https" { 443 } else { 80 }
}

/// Open a client connection to `host:port` (TLS for https), send the handshake,
/// and return `(status, headers, response)` un-upgraded so the caller can upgrade
/// (101) or read the reject body.
#[allow(clippy::too_many_arguments)]
async fn ws_handshake_upstream(
    scheme: &str,
    host: &str,
    port: u16,
    path: &str,
    req_headers: &[(String, String)],
    verify_upstream: bool,
    tasks: &TaskTracker,
    shutdown: &CancellationToken,
) -> Result<(u16, Vec<(String, String)>, Response<Incoming>)> {
    let tcp = tokio::time::timeout(Duration::from_secs(15), TcpStream::connect((host, port)))
        .await
        .map_err(|_| anyhow!("connect {host}:{port} timed out after 15s"))?
        .map_err(|e| anyhow!("connect {host}:{port}: {e}"))?;
    let stream: Box<dyn IoStream> = if scheme == "https" {
        let sni = rustls::pki_types::ServerName::try_from(host.to_string())
            .map_err(|e| anyhow!("bad SNI {host}: {e}"))?;
        Box::new(
            tokio::time::timeout(
                Duration::from_secs(15),
                ws_tls_connector(verify_upstream)?.connect(sni, tcp),
            )
            .await
            .map_err(|_| anyhow!("upstream TLS {host} timed out after 15s"))?
            .map_err(|e| anyhow!("upstream TLS {host}: {e}"))?,
        )
    } else {
        Box::new(tcp)
    };
    let (mut sender, conn) = tokio::time::timeout(
        Duration::from_secs(15),
        hyper::client::conn::http1::handshake(TokioIo::new(stream)),
    )
    .await
    .map_err(|_| anyhow!("upstream HTTP handshake timed out after 15s"))?
    .map_err(|e| anyhow!("upstream handshake: {e}"))?;
    let shutdown = shutdown.clone();
    drop(tasks.spawn(async move {
        tokio::select! {
            _ = conn.with_upgrades() => {}
            _ = shutdown.cancelled() => {}
        }
    }));

    let mut builder = Request::builder().method(Method::GET).uri(path);
    let nominated = connection_nominated_headers(req_headers);
    for (name, value) in req_headers {
        let lower = name.to_ascii_lowercase();
        let websocket_hop_header = matches!(lower.as_str(), "connection" | "upgrade");
        if lower == "content-length"
            || matches!(
                lower.as_str(),
                "proxy-connection" | "proxy-authorization" | "proxy-authenticate"
            )
            || (is_hop_by_hop(&lower) && !websocket_hop_header)
            || (nominated.contains(&lower) && lower != "upgrade")
        {
            continue;
        }
        if let (Ok(hn), Ok(hv)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            builder = builder.header(hn, hv);
        }
    }
    if flow::header_get(req_headers, "host").is_none() {
        builder = builder.header(http::header::HOST, host);
    }
    let upstream_req = builder
        .body(Empty::<Bytes>::new())
        .map_err(|e| anyhow!("build ws request: {e}"))?;
    let resp = tokio::time::timeout(Duration::from_secs(30), sender.send_request(upstream_req))
        .await
        .map_err(|_| anyhow!("upstream WebSocket response timed out after 30s"))?
        .map_err(|e| anyhow!("upstream ws request: {e}"))?;
    let status = resp.status().as_u16();
    let headers = header_pairs(resp.headers());
    Ok((status, headers, resp))
}

/// Adapt an upstream hyper body without polling or collecting it. Keeping the
/// frame-stream conversion generic makes the no-eager-read behaviour directly
/// testable even though `Incoming` itself cannot be constructed by callers.
fn frame_stream_body<S, E>(frames: S) -> ProxyBody
where
    S: futures_util::Stream<Item = Result<Frame<Bytes>, E>> + Send + 'static,
    E: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let frames = frames.map(|result| result.map_err(std::io::Error::other));
    BodyExt::boxed_unsync(StreamBody::new(frames))
}

fn incoming_proxy_body(body: Incoming) -> ProxyBody {
    frame_stream_body(BodyStream::new(body))
}

/// The `101 Switching Protocols` handed to the device — the upstream's upgrade
/// headers (`Upgrade`, `Connection`, `Sec-WebSocket-Accept`) verbatim.
fn ws_switching_response(headers: &[(String, String)]) -> Response<ProxyBody> {
    let mut builder = Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
    for (name, value) in headers {
        let lname = name.to_lowercase();
        if lname == "content-length" || lname == "transfer-encoding" {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder
        .body(full_body(Bytes::new()))
        .unwrap_or_else(|_| Response::new(full_body(Bytes::new())))
}

/// Build the TLS client connector for the upstream WebSocket leg. The default
/// debugging posture accepts self-signed backends; `--verify-upstream` switches
/// to the host platform's trust store and hostname verification, matching the
/// ordinary reqwest upstream path.
fn ws_tls_connector(verify_upstream: bool) -> Result<tokio_rustls::TlsConnector> {
    if verify_upstream {
        use rustls_platform_verifier::BuilderVerifierExt;

        static SECURE_CONNECTOR: std::sync::OnceLock<
            std::result::Result<tokio_rustls::TlsConnector, String>,
        > = std::sync::OnceLock::new();
        return SECURE_CONNECTOR
            .get_or_init(|| {
                let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
                let cfg = rustls::ClientConfig::builder_with_provider(provider)
                    .with_safe_default_protocol_versions()
                    .map_err(|e| format!("rustls protocol versions: {e}"))?
                    .with_platform_verifier()
                    .map_err(|e| format!("platform TLS verifier: {e}"))?
                    .with_no_client_auth();
                Ok(tokio_rustls::TlsConnector::from(Arc::new(cfg)))
            })
            .clone()
            .map_err(anyhow::Error::msg);
    }

    static INSECURE_CONNECTOR: std::sync::OnceLock<tokio_rustls::TlsConnector> =
        std::sync::OnceLock::new();
    Ok(INSECURE_CONNECTOR
        .get_or_init(|| {
            let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
            let cfg = rustls::ClientConfig::builder_with_provider(provider.clone())
                .with_safe_default_protocol_versions()
                .expect("rustls client versions")
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerify(provider)))
                .with_no_client_auth();
            tokio_rustls::TlsConnector::from(Arc::new(cfg))
        })
        .clone())
}

/// Accept-any server-cert verifier (upstream WebSocket leg only; signature math
/// is still checked so the handshake is well-formed — only cert-chain/name
/// validation is skipped).
#[derive(Debug)]
struct NoVerify(Arc<rustls::crypto::CryptoProvider>);
impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Forward one (decrypted or plaintext) request upstream, applying any active
/// interception at the request and/or response phase, and capture the flow.
async fn proxy_request(
    ctx: Arc<ProxyContext>,
    req: Request<Incoming>,
    tunnel: Option<(Scheme, Authority)>,
) -> Result<Response<ProxyBody>> {
    let (parts, body) = req.into_parts();
    let method = parts.method.clone();
    let (scheme, host, path, mut url) = resolve_target(&parts.uri, &tunnel)?;
    let mut req_headers = header_pairs(&parts.headers);

    // Read the request body, buffering up to the cap then streaming it upstream —
    // a large upload would otherwise buffer whole in RAM. A streamed request, like
    // a streamed response, skips request-phase interception and body capture.
    let req_len_hint = content_length(&req_headers);
    let force_req_stream = req_len_hint.is_some_and(|n| n as usize > BUFFER_CAP);
    let req_body_stream: ByteStream = Box::pin(BodyStream::new(body).filter_map(|f| async move {
        match f {
            Ok(frame) => frame.into_data().ok().map(Ok),
            Err(e) => Some(Err(std::io::Error::other(e))),
        }
    }));
    let mut req_bytes = Bytes::new();
    let mut req_streaming = false;
    let mut req_stream: Option<(Vec<Bytes>, ByteStream)> = None;
    match read_stream_capped(req_body_stream, force_req_stream, req_len_hint, BUFFER_CAP).await {
        BodyRead::Buffered(b) => req_bytes = b,
        BodyRead::Streamed { prefix, rest, .. } => {
            req_streaming = true;
            req_stream = Some((prefix, rest));
        }
        BodyRead::Error(e) => {
            return Ok(error_response_for(
                &method,
                StatusCode::BAD_REQUEST,
                &format!("read request body: {e}"),
            ));
        }
    }

    let id = flow::new_id();
    let in_scope = ctx.shared.host_in_scope(&host);
    let mut matched: Option<String> = None;
    let mut rule_ids = Vec::<String>::new();
    let mut modified = false;

    // ── replay (P3): serve a saved response, never hitting upstream ──
    if in_scope
        && let Some((status, mut headers, body)) =
            replay_lookup(&ctx.shared, method.as_str(), &host, &path)
    {
        replace_content_length(&mut headers, synthetic_response_length(status, body.len()));
        let wire_body = if response_allows_body(&method, status) {
            body.clone()
        } else {
            Bytes::new()
        };
        capture_bypassed(
            &ctx,
            FlowParts {
                id: &id,
                method: method.as_str(),
                scheme: &scheme,
                host: &host,
                path: &path,
                req_headers: &req_headers,
                req_bytes: &req_bytes,
                req_streamed: req_streaming,
                status: Some(status),
                resp_headers: &headers,
                resp_bytes: &wire_body,
                dur_ms: 0,
                error: None,
                matched: Some("replay".into()),
                modified: true,
                rule_ids: &[],
            },
        );
        return Ok(build_client_response_for(&method, status, &headers, body));
    }

    // ── request-phase rules (P3): block / map-local short-circuit; map-remote
    //    rewrites the URL; set-request-header mutates request headers; delay
    //    sleeps before forwarding ──
    if in_scope {
        let r = apply_request_rules(
            &ctx.shared,
            method.as_str(),
            &host,
            &path,
            &req_bytes,
            &mut url,
            &mut req_headers,
        );
        if r.modified {
            modified = true;
        }
        if !r.rule_ids.is_empty() {
            matched = Some("rule".into());
            rule_ids.extend(r.rule_ids.iter().cloned());
        }
        if r.delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(r.delay_ms as u64)).await;
        }
        if let Some((status, mut headers, body)) = r.short_circuit {
            replace_content_length(&mut headers, synthetic_response_length(status, body.len()));
            let wire_body = if response_allows_body(&method, status) {
                body.clone()
            } else {
                Bytes::new()
            };
            capture_bypassed(
                &ctx,
                FlowParts {
                    id: &id,
                    method: method.as_str(),
                    scheme: &scheme,
                    host: &host,
                    path: &path,
                    req_headers: &req_headers,
                    req_bytes: &req_bytes,
                    req_streamed: req_streaming,
                    status: Some(status),
                    resp_headers: &headers,
                    resp_bytes: &wire_body,
                    dur_ms: 0,
                    error: None,
                    matched: Some("rule".into()),
                    modified: true,
                    rule_ids: &rule_ids,
                },
            );
            return Ok(build_client_response_for(&method, status, &headers, body));
        }
    }

    // ── request-phase interception ── (skipped for streamed uploads: no buffered
    //    body to preview or mutate, like a streamed response skips response intercept)
    if in_scope && !req_streaming {
        let mut snap = make_flow(FlowParts {
            id: &id,
            method: method.as_str(),
            scheme: &scheme,
            host: &host,
            path: &path,
            req_headers: &req_headers,
            req_bytes: &req_bytes,
            req_streamed: false,
            status: None,
            resp_headers: &[],
            resp_bytes: &[],
            dur_ms: 0,
            error: None,
            matched: None,
            modified: false,
            rule_ids: &rule_ids,
        });
        stamp_capture_context(&ctx, &mut snap);
        if let Some(decision) = hold(&ctx, snap, "request").await {
            match decision {
                HoldDecision::Drop(s) => return Ok(drop_response(&method, s)),
                HoldDecision::Respond {
                    status,
                    body,
                    mut headers,
                } => {
                    let resp_bytes = Bytes::from(body);
                    replace_content_length(
                        &mut headers,
                        synthetic_response_length(status, resp_bytes.len()),
                    );
                    let wire_bytes = if response_allows_body(&method, status) {
                        resp_bytes.clone()
                    } else {
                        Bytes::new()
                    };
                    capture_bypassed(
                        &ctx,
                        FlowParts {
                            id: &id,
                            method: method.as_str(),
                            scheme: &scheme,
                            host: &host,
                            path: &path,
                            req_headers: &req_headers,
                            req_bytes: &req_bytes,
                            req_streamed: false,
                            status: Some(status),
                            resp_headers: &headers,
                            resp_bytes: &wire_bytes,
                            dur_ms: 0,
                            error: None,
                            matched: Some("intercept:respond".into()),
                            modified: true,
                            rule_ids: &rule_ids,
                        },
                    );
                    return Ok(build_client_response_for(
                        &method, status, &headers, resp_bytes,
                    ));
                }
                HoldDecision::Resume(m) => {
                    if !m.is_noop() {
                        modified = true;
                        matched = Some("intercept".into());
                        apply_request_mutation(&mut url, &mut req_headers, &mut req_bytes, &m);
                    }
                    if let Some(d) = m.delay_ms {
                        tokio::time::sleep(Duration::from_millis(d as u64)).await;
                    }
                }
            }
        }
    }

    // ── forward upstream ──
    let upstream_content_length = if req_streaming {
        req_len_hint
    } else if req_bytes.is_empty() {
        req_len_hint.filter(|length| *length == 0)
    } else {
        Some(u64::try_from(req_bytes.len()).unwrap_or(u64::MAX))
    };
    replace_content_length(&mut req_headers, upstream_content_length);
    let up_body: Option<reqwest::Body> = match req_stream.take() {
        Some((prefix, rest)) => {
            let stream =
                futures_util::stream::iter(prefix.into_iter().map(Ok::<Bytes, std::io::Error>))
                    .chain(rest);
            Some(reqwest::Body::wrap_stream(stream))
        }
        None if req_bytes.is_empty() => None,
        None => Some(reqwest::Body::from(req_bytes.clone())),
    };
    let started = std::time::Instant::now();
    let resp = match send_upstream(
        &ctx.client,
        &method,
        &url,
        &req_headers,
        up_body,
        upstream_content_length,
        &ctx.shared,
    )
    .await
    {
        Ok(resp) => resp,
        Err(e) => {
            // Never reached the server (DNS / connect / upstream TLS).
            let dur_ms = started.elapsed().as_millis() as u64;
            if in_scope {
                capture(
                    &ctx,
                    error_flow(
                        &id,
                        &method,
                        &scheme,
                        &host,
                        &path,
                        &req_headers,
                        &req_bytes,
                        req_streaming,
                        None,
                        &[],
                        dur_ms,
                        e.to_string(),
                        matched.clone(),
                        modified,
                        &rule_ids,
                    ),
                );
            }
            return Ok(error_response_for(
                &method,
                StatusCode::BAD_GATEWAY,
                &e.to_string(),
            ));
        }
    };

    let status_code = resp.status().as_u16();
    let mut resp_headers = header_pairs(resp.headers());
    let original_response_length = content_length(&resp_headers);

    // Stream long-lived / oversized bodies instead of buffering (SSE would hang;
    // a huge download would OOM). A streamed flow skips response rules/intercept
    // (nothing buffered to mutate) and is captured as a note (`streamed:true`).
    let mut resp_bytes = match read_body_capped(resp, &resp_headers).await {
        BodyRead::Buffered(bytes) => bytes,
        BodyRead::Streamed {
            prefix,
            rest,
            len_hint,
        } => {
            let dur_ms = started.elapsed().as_millis() as u64;
            let mut streamed_status = Some(status_code);
            let mut no_body = Bytes::new();
            let response_rule_ids = if in_scope {
                apply_response_rules(
                    &ctx.shared,
                    method.as_str(),
                    &host,
                    &path,
                    &mut streamed_status,
                    &mut resp_headers,
                    &mut no_body,
                    false,
                )
            } else {
                Vec::new()
            };
            if !response_rule_ids.is_empty() {
                extend_rule_ids(&mut rule_ids, response_rule_ids);
                modified = true;
                if matched.is_none() {
                    matched = Some("rule".into());
                }
            }
            let final_status = streamed_status.unwrap_or(status_code);
            if in_scope {
                let parts = FlowParts {
                    id: &id,
                    method: method.as_str(),
                    scheme: &scheme,
                    host: &host,
                    path: &path,
                    req_headers: &req_headers,
                    req_bytes: &req_bytes,
                    req_streamed: req_streaming,
                    status: Some(final_status),
                    resp_headers: &resp_headers,
                    resp_bytes: &[],
                    dur_ms,
                    error: None,
                    matched: matched.clone(),
                    modified,
                    rule_ids: &rule_ids,
                };
                capture_streamed(&ctx, parts, len_hint);
            }
            let body_allowed = response_allows_body(&method, final_status);
            let response_body = if body_allowed {
                streamed_body(prefix, rest)
            } else {
                // A rule may change a streaming 200 into 204/304. Never attach
                // the original stream to a status (or HEAD response) that
                // forbids a message body.
                full_body(Bytes::new())
            };
            let response_length =
                if body_allowed || (method == Method::HEAD && !status_forbids_body(final_status)) {
                    len_hint
                } else {
                    None
                };
            return Ok(response_with_body(
                final_status,
                &resp_headers,
                response_body,
                response_length,
            ));
        }
        BodyRead::Error(e) => {
            let dur_ms = started.elapsed().as_millis() as u64;
            if in_scope {
                capture(
                    &ctx,
                    error_flow(
                        &id,
                        &method,
                        &scheme,
                        &host,
                        &path,
                        &req_headers,
                        &req_bytes,
                        req_streaming,
                        Some(status_code),
                        &resp_headers,
                        dur_ms,
                        e.clone(),
                        matched.clone(),
                        modified,
                        &rule_ids,
                    ),
                );
            }
            return Ok(error_response_for(&method, StatusCode::BAD_GATEWAY, &e));
        }
    };
    let dur_ms = started.elapsed().as_millis() as u64;
    let mut status = Some(status_code);
    let error: Option<String> = None;
    let mut plaintext_body = true;

    // Decompress in-scope responses so capture, rules, and intercept all see
    // plain text — and strip `content-encoding` so the (decompressed) body we
    // hand the client stays consistent. Decode work runs off the async worker
    // and its output is capped. Unknown, invalid, or oversized encodings are
    // passed through untouched and must skip plaintext-only mutation paths.
    if in_scope && error.is_none() {
        match decompress_bounded(&resp_headers, &resp_bytes).await {
            DecodeOutcome::Identity => {}
            DecodeOutcome::Decoded(plain) => {
                resp_bytes = Bytes::from(plain);
                resp_headers.retain(|(k, _)| !k.eq_ignore_ascii_case("content-encoding"));
                strip_body_validators(&mut resp_headers);
            }
            DecodeOutcome::PassThrough(reason) => {
                tracing::debug!(
                    ?reason,
                    wire_len = resp_bytes.len(),
                    "passing through encoded response without capture or mutation"
                );
                plaintext_body = false;
            }
        }
    }

    // ── response-phase rules (P3): set-status / set-response-header / replace ──
    let response_rule_ids = if in_scope && error.is_none() {
        apply_response_rules(
            &ctx.shared,
            method.as_str(),
            &host,
            &path,
            &mut status,
            &mut resp_headers,
            &mut resp_bytes,
            plaintext_body,
        )
    } else {
        Vec::new()
    };
    if !response_rule_ids.is_empty() {
        extend_rule_ids(&mut rule_ids, response_rule_ids);
        modified = true;
        if matched.is_none() {
            matched = Some("rule".into());
        }
    }

    // ── response-phase interception ──
    if in_scope && error.is_none() {
        let mut snap = make_flow(FlowParts {
            id: &id,
            method: method.as_str(),
            scheme: &scheme,
            host: &host,
            path: &path,
            req_headers: &req_headers,
            req_bytes: &req_bytes,
            req_streamed: req_streaming,
            status,
            resp_headers: &resp_headers,
            resp_bytes: if plaintext_body { &resp_bytes } else { &[] },
            dur_ms,
            error: None,
            matched: None,
            modified: false,
            rule_ids: &rule_ids,
        });
        stamp_capture_context(&ctx, &mut snap);
        if !plaintext_body {
            snap.streamed = true;
            snap.resp_body = None;
            snap.resp_len = u64::try_from(resp_bytes.len()).unwrap_or(u64::MAX);
        }
        if let Some(decision) = hold(&ctx, snap, "response").await {
            match decision {
                HoldDecision::Drop(s) => return Ok(drop_response(&method, s)),
                HoldDecision::Respond {
                    status: rs,
                    body,
                    headers,
                } => {
                    status = Some(rs);
                    resp_headers = headers;
                    resp_bytes = Bytes::from(body);
                    plaintext_body = true;
                    modified = true;
                    matched = Some("intercept:respond".into());
                }
                HoldDecision::Resume(m) => {
                    if apply_response_mutation(
                        &mut status,
                        &mut resp_headers,
                        &mut resp_bytes,
                        &m,
                        plaintext_body,
                    ) {
                        modified = true;
                        matched = Some("intercept".into());
                    }
                    if let Some(d) = m.delay_ms {
                        tokio::time::sleep(Duration::from_millis(d as u64)).await;
                    }
                }
            }
        }
    }

    // A response status and method decide whether a message body is legal.
    // Apply this after rules/interception so a synthetic 204/304 or a HEAD
    // interception can never leak buffered bytes onto the wire.
    let body_len_before_suppression = resp_bytes.len();
    if let Some(final_status) = status
        && !response_allows_body(&method, final_status)
        && !resp_bytes.is_empty()
    {
        resp_bytes = Bytes::new();
        modified = true;
    }
    if let Some(final_status) = status {
        let length = if status_forbids_body(final_status) {
            None
        } else if method == Method::HEAD {
            (body_len_before_suppression != 0)
                .then(|| u64::try_from(body_len_before_suppression).unwrap_or(u64::MAX))
                .or(original_response_length)
        } else {
            Some(u64::try_from(resp_bytes.len()).unwrap_or(u64::MAX))
        };
        replace_content_length(&mut resp_headers, length);
    }

    // ── capture + return ──
    if in_scope && plaintext_body {
        capture(
            &ctx,
            FlowParts {
                id: &id,
                method: method.as_str(),
                scheme: &scheme,
                host: &host,
                path: &path,
                req_headers: &req_headers,
                req_bytes: &req_bytes,
                req_streamed: req_streaming,
                status,
                resp_headers: &resp_headers,
                resp_bytes: &resp_bytes,
                dur_ms,
                error: error.clone(),
                matched,
                modified,
                rule_ids: &rule_ids,
            },
        );
    } else if in_scope {
        let wire_len = u64::try_from(resp_bytes.len()).unwrap_or(u64::MAX);
        capture_streamed(
            &ctx,
            FlowParts {
                id: &id,
                method: method.as_str(),
                scheme: &scheme,
                host: &host,
                path: &path,
                req_headers: &req_headers,
                req_bytes: &req_bytes,
                req_streamed: req_streaming,
                status,
                resp_headers: &resp_headers,
                resp_bytes: &[],
                dur_ms,
                error: error.clone(),
                matched,
                modified,
                rule_ids: &rule_ids,
            },
            Some(wire_len),
        );
    }

    Ok(match status {
        Some(status) => {
            let length = if status_forbids_body(status) {
                None
            } else if method == Method::HEAD {
                content_length(&resp_headers)
            } else {
                Some(u64::try_from(resp_bytes.len()).unwrap_or(u64::MAX))
            };
            response_with_body(status, &resp_headers, full_body(resp_bytes), length)
        }
        None => error_response_for(
            &method,
            StatusCode::BAD_GATEWAY,
            error.as_deref().unwrap_or("upstream error"),
        ),
    })
}

/// Reconstruct `(scheme, host, path_and_query, absolute_url)` for forwarding.
fn resolve_target(
    uri: &Uri,
    tunnel: &Option<(Scheme, Authority)>,
) -> Result<(String, String, String, String)> {
    if let Some((scheme, authority)) = tunnel {
        // Inner request: origin-form URI (`/path?q`), host from the CONNECT.
        let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
        Ok((
            scheme.as_str().to_string(),
            authority.host().to_string(),
            pq.to_string(),
            format!("{}://{}{}", scheme.as_str(), authority.as_str(), pq),
        ))
    } else {
        // Outer plaintext proxy request: absolute-form URI.
        let scheme = uri.scheme_str().unwrap_or("http").to_string();
        let host = uri
            .host()
            .ok_or_else(|| anyhow!("plaintext request missing host"))?
            .to_string();
        let authority = uri.authority().map(|a| a.as_str()).unwrap_or(&host);
        let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
        // Bind `url` before moving `host` out — `authority` may borrow `host`.
        let url = format!("{scheme}://{authority}{pq}");
        Ok((scheme, host, pq.to_string(), url))
    }
}

/// Send the (possibly mutated) request upstream and return the response with its
/// headers available — the body is read separately by [`read_body_capped`] so we
/// can decide buffer-vs-stream.
async fn send_upstream(
    client: &reqwest::Client,
    method: &Method,
    url: &str,
    req_headers: &[(String, String)],
    body: Option<reqwest::Body>,
    content_length: Option<u64>,
    shared: &SharedState,
) -> Result<reqwest::Response> {
    let headers = upstream_headers(
        req_headers,
        shared.anticache,
        shared.anticomp,
        content_length,
    );

    let mut rb = client.request(method.clone(), url).headers(headers);
    if let Some(b) = body {
        rb = rb.body(b);
    }
    rb.send().await.map_err(|e| anyhow!("upstream: {e}"))
}

fn upstream_headers(
    req_headers: &[(String, String)],
    anticache: bool,
    anticomp: bool,
    content_length: Option<u64>,
) -> http::HeaderMap {
    let mut headers = http::HeaderMap::new();
    let nominated = connection_nominated_headers(req_headers);
    for (name, value) in req_headers {
        let lname = name.to_lowercase();
        if is_hop_by_hop(&lname)
            || nominated.contains(&lname)
            || lname == "host"
            || lname == "content-length"
        {
            continue;
        }
        if anticache && (lname == "if-none-match" || lname == "if-modified-since") {
            continue;
        }
        if anticomp && lname == "accept-encoding" {
            continue;
        }
        if let (Ok(hn), Ok(hv)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            // HeaderMap::insert overwrites a previous value. Request headers
            // such as Cookie, X-Forwarded-For, and vendor extensions can be
            // repeated and their ordering is significant, so preserve every
            // field-line when forwarding upstream.
            headers.append(hn, hv);
        }
    }
    if anticomp {
        headers.insert(
            http::header::ACCEPT_ENCODING,
            http::HeaderValue::from_static("identity"),
        );
    }
    if let Some(length) = content_length
        && let Ok(value) = http::HeaderValue::from_str(&length.to_string())
    {
        headers.insert(http::header::CONTENT_LENGTH, value);
    }
    headers
}

/// A byte stream normalised to `io::Result` (reqwest's error mapped away) so the
/// cap loop is decoupled from reqwest and unit-testable.
type ByteStream = Pin<Box<dyn futures_util::Stream<Item = std::io::Result<Bytes>> + Send>>;

/// The outcome of reading an upstream response body.
enum BodyRead {
    /// Finished under the cap — fully captured, mutable.
    Buffered(Bytes),
    /// Streamed pass-through: the prefix already pulled, plus the rest of the
    /// live stream. Not captured/mutated (SSE or oversized).
    Streamed {
        prefix: Vec<Bytes>,
        rest: ByteStream,
        len_hint: Option<u64>,
    },
    /// The upstream body errored mid-read.
    Error(String),
}

/// Force streaming (never buffer) for content-types that are long-lived by
/// design — buffering them would hang the device waiting on a body that never
/// ends. Everything else is buffered up to [`BUFFER_CAP`], then spilled.
fn is_streaming_content_type(headers: &[(String, String)]) -> bool {
    flow::content_type(headers).is_some_and(|ct| {
        ct == "text/event-stream"
            || ct == "multipart/x-mixed-replace"
            || ct.starts_with("application/grpc")
    })
}

fn content_length(headers: &[(String, String)]) -> Option<u64> {
    flow::header_get(headers, "content-length").and_then(|v| v.trim().parse().ok())
}

fn replace_content_length(headers: &mut Vec<(String, String)>, length: Option<u64>) {
    headers.retain(|(name, _)| !name.eq_ignore_ascii_case("content-length"));
    if let Some(length) = length {
        headers.push(("content-length".into(), length.to_string()));
    }
}

fn synthetic_response_length(status: u16, body_len: usize) -> Option<u64> {
    if status_forbids_body(status) {
        None
    } else {
        Some(u64::try_from(body_len).unwrap_or(u64::MAX))
    }
}

/// Read the upstream body, buffering up to [`BUFFER_CAP`] then spilling to a
/// streamed pass-through; known-streaming content-types stream immediately so
/// the device gets headers without waiting on the body.
async fn read_body_capped(resp: reqwest::Response, headers: &[(String, String)]) -> BodyRead {
    let len_hint = content_length(headers);
    let force = is_streaming_content_type(headers);
    let stream: ByteStream = Box::pin(
        resp.bytes_stream()
            .map(|r| r.map_err(std::io::Error::other)),
    );
    read_stream_capped(stream, force, len_hint, BUFFER_CAP).await
}

/// Core of [`read_body_capped`], decoupled from reqwest: buffer chunks until the
/// stream ends (→ `Buffered`) or `cap` is exceeded (→ `Streamed`, prefix + rest);
/// `force_stream` spills immediately with an empty prefix (SSE et al.).
async fn read_stream_capped(
    mut rest: ByteStream,
    force_stream: bool,
    len_hint: Option<u64>,
    cap: usize,
) -> BodyRead {
    if force_stream {
        return BodyRead::Streamed {
            prefix: Vec::new(),
            rest,
            len_hint,
        };
    }
    let mut prefix: Vec<Bytes> = Vec::new();
    let mut total = 0usize;
    loop {
        let next = rest.next().await;
        match next {
            None => return BodyRead::Buffered(concat_chunks(&prefix)),
            Some(Err(e)) => return BodyRead::Error(e.to_string()),
            Some(Ok(chunk)) => {
                total += chunk.len();
                prefix.push(chunk);
                if total > cap {
                    return BodyRead::Streamed {
                        prefix,
                        rest,
                        len_hint,
                    };
                }
            }
        }
    }
}

fn concat_chunks(chunks: &[Bytes]) -> Bytes {
    match chunks {
        [] => Bytes::new(),
        [one] => one.clone(),
        many => {
            let mut buf = Vec::with_capacity(many.iter().map(|c| c.len()).sum());
            for c in many {
                buf.extend_from_slice(c);
            }
            Bytes::from(buf)
        }
    }
}

/// A streaming `ProxyBody` that emits the already-pulled `prefix` chunks, then
/// the rest of the live upstream stream.
fn streamed_body(prefix: Vec<Bytes>, rest: ByteStream) -> ProxyBody {
    let head = futures_util::stream::iter(prefix.into_iter().map(Ok::<Bytes, std::io::Error>));
    let frames = head.chain(rest).map(|r| r.map(Frame::data));
    BodyExt::boxed_unsync(StreamBody::new(frames))
}

/// Assemble a response with the given (already-decided) body, copying headers
/// except the framing ones hyper derives from the body itself.
fn response_with_body(
    status: u16,
    headers: &[(String, String)],
    body: ProxyBody,
    known_length: Option<u64>,
) -> Response<ProxyBody> {
    let mut builder = Response::builder().status(status);
    let nominated = connection_nominated_headers(headers);
    for (name, value) in headers {
        let lname = name.to_lowercase();
        // hyper sets framing headers from the body; copying them corrupts it.
        if is_hop_by_hop(&lname)
            || nominated.contains(&lname)
            || lname == "content-length"
            || lname == "transfer-encoding"
        {
            continue;
        }
        builder = builder.header(name, value);
    }
    if let Some(length) = known_length {
        builder = builder.header(http::header::CONTENT_LENGTH, length);
    }
    builder
        .body(body)
        .unwrap_or_else(|_| Response::new(full_body(Bytes::new())))
}

fn status_forbids_body(status: u16) -> bool {
    (100..200).contains(&status) || status == 204 || status == 304
}

fn response_allows_body(method: &Method, status: u16) -> bool {
    *method != Method::HEAD && !status_forbids_body(status)
}

fn build_client_response(
    status: u16,
    headers: &[(String, String)],
    body: Bytes,
) -> Response<ProxyBody> {
    let length = u64::try_from(body.len()).unwrap_or(u64::MAX);
    response_with_body(status, headers, full_body(body), Some(length))
}

fn build_client_response_for(
    method: &Method,
    status: u16,
    headers: &[(String, String)],
    body: Bytes,
) -> Response<ProxyBody> {
    if response_allows_body(method, status) {
        build_client_response(status, headers, body)
    } else {
        let length = if *method == Method::HEAD && !status_forbids_body(status) {
            Some(u64::try_from(body.len()).unwrap_or(u64::MAX))
        } else {
            None
        };
        response_with_body(status, headers, full_body(Bytes::new()), length)
    }
}

fn error_response(status: StatusCode, msg: &str) -> Response<ProxyBody> {
    error_response_for(&Method::GET, status, msg)
}

fn error_response_for(method: &Method, status: StatusCode, msg: &str) -> Response<ProxyBody> {
    build_client_response_for(
        method,
        status.as_u16(),
        &[("content-type".into(), "text/plain; charset=utf-8".into())],
        Bytes::from(format!("shadowdroid proxy: {msg}")),
    )
}

struct FlowParts<'a> {
    id: &'a str,
    method: &'a str,
    scheme: &'a str,
    host: &'a str,
    path: &'a str,
    req_headers: &'a [(String, String)],
    req_bytes: &'a [u8],
    /// Set when the request body was streamed upstream: `req_bytes` is empty, the
    /// captured `req_len` comes from the client's `content-length` instead, and no
    /// request body is stored.
    req_streamed: bool,
    status: Option<u16>,
    resp_headers: &'a [(String, String)],
    resp_bytes: &'a [u8],
    dur_ms: u64,
    error: Option<String>,
    matched: Option<String>,
    modified: bool,
    rule_ids: &'a [String],
}

fn make_flow(p: FlowParts<'_>) -> FlowRecord {
    let req_type = flow::content_type(p.req_headers);
    let resp_type = flow::content_type(p.resp_headers);
    let (req_body, req_truncated) = if p.req_streamed {
        (None, false)
    } else {
        flow::body_to_text(req_type.as_deref(), p.req_bytes, flow::BODY_CAP)
    };
    let (resp_body, resp_truncated) =
        flow::body_to_text(resp_type.as_deref(), p.resp_bytes, flow::BODY_CAP);
    let req_len = if p.req_streamed {
        content_length(p.req_headers).unwrap_or(0)
    } else {
        p.req_bytes.len() as u64
    };
    FlowRecord {
        id: p.id.to_string(),
        flow_sequence: flow::sequence_from_id(p.id).unwrap_or_default(),
        capture_session_id: String::new(),
        ts: events::now_ts(),
        method: p.method.to_string(),
        scheme: p.scheme.to_string(),
        host: p.host.to_string(),
        path: p.path.to_string(),
        status: p.status,
        dur_ms: Some(p.dur_ms),
        req_headers: p.req_headers.to_vec(),
        resp_headers: p.resp_headers.to_vec(),
        req_type,
        resp_type,
        req_len,
        resp_len: p.resp_bytes.len() as u64,
        req_body,
        resp_body,
        req_body_redacted: false,
        resp_body_redacted: false,
        redaction_policy: None,
        redaction_policy_version: None,
        req_truncated,
        resp_truncated,
        matched: p.matched,
        rule_id: p.rule_ids.last().cloned(),
        rule_ids: p.rule_ids.to_vec(),
        modified: p.modified,
        upstream_bypassed: false,
        error: p.error,
        streamed: false,
        req_streamed: p.req_streamed,
    }
}

/// [`FlowParts`] for a request that errored before/while reading the body — a
/// small constructor so the two upstream-error sites don't repeat the literal.
#[allow(clippy::too_many_arguments)]
fn error_flow<'a>(
    id: &'a str,
    method: &'a Method,
    scheme: &'a str,
    host: &'a str,
    path: &'a str,
    req_headers: &'a [(String, String)],
    req_bytes: &'a [u8],
    req_streamed: bool,
    status: Option<u16>,
    resp_headers: &'a [(String, String)],
    dur_ms: u64,
    error: String,
    matched: Option<String>,
    modified: bool,
    rule_ids: &'a [String],
) -> FlowParts<'a> {
    FlowParts {
        id,
        method: method.as_str(),
        scheme,
        host,
        path,
        req_headers,
        req_bytes,
        req_streamed,
        status,
        resp_headers,
        resp_bytes: &[],
        dur_ms,
        error: Some(error),
        matched,
        modified,
        rule_ids,
    }
}

/// Build the final flow record and push it to the daemon (store + broadcast).
fn capture(ctx: &ProxyContext, parts: FlowParts<'_>) {
    finish_capture(ctx, make_flow(parts));
}

fn capture_bypassed(ctx: &ProxyContext, parts: FlowParts<'_>) {
    let mut rec = make_flow(parts);
    rec.upstream_bypassed = true;
    finish_capture(ctx, rec);
}

fn finish_capture(ctx: &ProxyContext, mut rec: FlowRecord) {
    stamp_capture_context(ctx, &mut rec);
    if let Some(policy) = &ctx.shared.redaction {
        policy.redact_flow_record(&mut rec);
    }
    enqueue_flow(ctx, rec);
}

/// Capture a streamed (pass-through) flow: same metadata as [`capture`] but with
/// no body, `streamed:true`, and `resp_len` set from the `content-length` hint
/// (the real streamed length isn't known when the flow is recorded).
fn capture_streamed(ctx: &ProxyContext, parts: FlowParts<'_>, len_hint: Option<u64>) {
    let mut rec = make_flow(parts);
    stamp_capture_context(ctx, &mut rec);
    rec.streamed = true;
    rec.resp_body = None;
    rec.resp_len = len_hint.unwrap_or(rec.resp_len);
    if let Some(policy) = &ctx.shared.redaction {
        policy.redact_flow_record(&mut rec);
    }
    enqueue_flow(ctx, rec);
}

fn stamp_capture_context(ctx: &ProxyContext, rec: &mut FlowRecord) {
    rec.capture_session_id.clone_from(&ctx.capture_session_id);
}

fn enqueue_flow(ctx: &ProxyContext, rec: FlowRecord) {
    if ctx.flow_tx.try_send(rec).is_err() {
        let dropped = ctx.shared.dropped_flows.fetch_add(1, Ordering::Relaxed) + 1;
        // Log sparsely under sustained overload while keeping the exact count
        // discoverable through `net status`.
        if dropped == 1 || dropped.is_power_of_two() {
            tracing::warn!(dropped, "network capture queue full; flow discarded");
        }
    }
}

// ── interception ──────────────────────────────────────────────

/// Max concurrently-held flows before `hold` fails open (see [`hold`]).
const MAX_HELD_FLOWS: usize = 32;
const MAX_HELD_BYTES: u64 = 32 * 1024 * 1024;

fn held_flow_charge(flow: &FlowRecord) -> u64 {
    let strings = [
        flow.id.as_str(),
        flow.method.as_str(),
        flow.scheme.as_str(),
        flow.host.as_str(),
        flow.path.as_str(),
    ]
    .into_iter()
    .map(|value| u64::try_from(value.len()).unwrap_or(u64::MAX))
    .fold(0_u64, u64::saturating_add);
    let bodies = flow
        .req_body
        .as_ref()
        .into_iter()
        .chain(flow.resp_body.as_ref())
        .map(|value| u64::try_from(value.len()).unwrap_or(u64::MAX))
        .fold(0_u64, u64::saturating_add);
    let headers = flow
        .req_headers
        .iter()
        .chain(&flow.resp_headers)
        .map(|(name, value)| name.len().saturating_add(value.len()))
        .map(|bytes| u64::try_from(bytes).unwrap_or(u64::MAX))
        .fold(0_u64, u64::saturating_add);
    strings
        .saturating_add(bodies)
        .saturating_add(headers)
        .saturating_add(1024)
}

/// If interception is active and `snap` matches at this `phase`, register the
/// flow as held, emit an `http_intercept` event, and await the agent's decision
/// (fail-open / fail-closed on the hold deadline). Returns `None` when not held.
async fn hold(ctx: &ProxyContext, snap: FlowRecord, phase: &'static str) -> Option<HoldDecision> {
    let cfg = ctx.shared.intercept.read().unwrap().clone()?;
    let want = if phase == "request" {
        cfg.at_request
    } else {
        cfg.at_response
    };
    if !want || !snap.matches(&cfg.matcher) {
        return None;
    }

    let (tx, rx) = oneshot::channel();
    let id = snap.id.clone();
    let mut visible_snap = snap.clone();
    if let Some(policy) = &ctx.shared.redaction {
        policy.redact_flow_record(&mut visible_snap);
    }
    let charge = held_flow_charge(&snap);
    let held_at = events::now_ts();
    let expires_at = held_at + f64::from(cfg.hold_ms.max(1)) / 1000.0;
    {
        // Cap concurrent holds: each pins a FlowRecord (with bodies) until acted
        // on or its deadline. An app hammering a matched endpoint faster than the
        // agent can resume would otherwise grow this map without bound, so past
        // the cap we fail open — let the flow through unheld rather than OOM.
        // Terminal history is always locked before the active map. A
        // request-phase hold and a later response-phase hold deliberately reuse
        // one flow id; publishing the next active phase atomically supersedes
        // the prior terminal record for that id.
        let mut terminal_holds = ctx.shared.terminal_holds.lock().unwrap();
        let mut held = ctx.shared.held.lock().unwrap();
        let held_bytes = ctx.shared.held_bytes.load(Ordering::Relaxed);
        if held.len() >= MAX_HELD_FLOWS || held_bytes.saturating_add(charge) > MAX_HELD_BYTES {
            let rejected = ctx.shared.rejected_holds.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::warn!(
                rejected,
                held = held.len(),
                held_bytes,
                charge,
                "net intercept hold budget exhausted; letting {id} through unheld"
            );
            return None;
        }
        terminal_holds.remove(&id);
        ctx.shared.held_bytes.fetch_add(charge, Ordering::Relaxed);
        held.insert(
            id.clone(),
            HeldFlow {
                tx: Some(tx),
                meta: visible_snap.clone(),
                phase: phase.into(),
                held_at,
                expires_at,
                held_charge: Some((ctx.shared.held_bytes.clone(), charge)),
            },
        );
    }
    let _ = ctx.shared.events.send(Arc::new(intercept_event(
        &ctx.serial,
        &visible_snap,
        phase,
        cfg.hold_ms,
    )));

    let decision = resolve_held(
        &ctx.shared.held,
        &ctx.shared.terminal_holds,
        &id,
        rx,
        Duration::from_millis(cfg.hold_ms.max(1) as u64),
        || {
            // Fail-open (resume) by default; drop only if so configured.
            if cfg.on_timeout_drop {
                HoldDecision::Drop(None)
            } else {
                HoldDecision::Resume(Mutation::default())
            }
        },
    )
    .await;
    Some(decision)
}

/// Resolve a held flow into the single decision the proxy applies to the device.
///
/// Removing the active entry is the atomic claim: exactly one release, deadline,
/// or cancellation decides the flow. The receiver stays alive across the timer
/// branch so a release that already claimed the entry is delivered rather than
/// silently replaced by fail-open. The release path also checks the absolute
/// wall deadline, so an action at or after `expires_at` cannot win merely because
/// the timer task had not been scheduled yet.
pub(crate) async fn resolve_held(
    held: &Mutex<HashMap<String, HeldFlow>>,
    terminal_holds: &Mutex<TerminalHoldHistory>,
    id: &str,
    mut rx: oneshot::Receiver<HoldDecision>,
    deadline: Duration,
    fail_open: impl Fn() -> HoldDecision,
) -> HoldDecision {
    struct Cleanup<'a> {
        held: &'a Mutex<HashMap<String, HeldFlow>>,
        terminal_holds: &'a Mutex<TerminalHoldHistory>,
        id: &'a str,
    }
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            terminalize_held(self.held, self.terminal_holds, self.id, "client_canceled");
        }
    }
    // Also covers cancellation: dropping this future removes the map entry and
    // releases its byte charge instead of leaking interception capacity.
    let _cleanup = Cleanup {
        held,
        terminal_holds,
        id,
    };
    tokio::select! {
        biased;
        r = &mut rx => r.unwrap_or_else(|_| fail_open()),
        _ = tokio::time::sleep(deadline) => {
            if terminalize_held(held, terminal_holds, id, "deadline_expired") {
                // We claimed the entry first → the deadline wins → fail open.
                fail_open()
            } else {
                // A release already claimed it and is sending its decision;
                // `rx` is still alive, so receive it (fail open only if the
                // sender vanished without sending).
                (&mut rx).await.unwrap_or_else(|_| fail_open())
            }
        }
    }
}

/// Hand a held flow its decision, claiming it atomically (the mirror of
/// [`resolve_held`]'s claim). The result distinguishes delivery, deadline,
/// client cancellation, and a missing id, so the control plane never reports a
/// release that did not actually reach the proxy.
pub(crate) fn release_held(
    held: &Mutex<HashMap<String, HeldFlow>>,
    terminal_holds: &Mutex<TerminalHoldHistory>,
    id: &str,
    action: &str,
    decision: HoldDecision,
) -> ReleaseHeldResult {
    let mut terminal_holds = terminal_holds.lock().unwrap();
    match held.lock().unwrap().remove(id) {
        Some(mut held) => {
            if events::now_ts() >= held.expires_at {
                let terminal = held.terminal("deadline_expired", None);
                terminal_holds.record(terminal.clone());
                return ReleaseHeldResult::DeadlineExpired(terminal);
            }
            let mut terminal = held.terminal("released", Some(action));
            let delivered = held
                .tx
                .take()
                .is_some_and(|sender| sender.send(decision).is_ok());
            if !delivered {
                terminal.state = "client_canceled";
                terminal.action = None;
            }
            terminal_holds.record(terminal.clone());
            if delivered {
                ReleaseHeldResult::Released(terminal)
            } else {
                ReleaseHeldResult::ClientCanceled(terminal)
            }
        }
        None => ReleaseHeldResult::Missing,
    }
}

fn intercept_event(
    serial: &crate::ids::Serial,
    snap: &FlowRecord,
    phase: &str,
    hold_ms: u32,
) -> Event {
    let preview = |b: &Option<String>| {
        b.as_ref().map(|s| {
            if s.chars().count() > flow::PREVIEW_CAP {
                s.chars().take(flow::PREVIEW_CAP).collect::<String>()
            } else {
                s.clone()
            }
        })
    };
    Event::HttpIntercept {
        ts: events::now_ts(),
        id: snap.id.clone(),
        phase: phase.to_string(),
        method: snap.method.clone(),
        scheme: snap.scheme.clone(),
        host: snap.host.clone(),
        path: snap.path.clone(),
        status: snap.status,
        req_type: snap.req_type.clone(),
        req_len: snap.req_len,
        resp_type: snap.resp_type.clone(),
        resp_len: snap.resp_len,
        hold_deadline_ms: hold_ms,
        req_preview: preview(&snap.req_body),
        resp_preview: preview(&snap.resp_body),
        next_actions: crate::net::intercept_next_actions(serial, &snap.id),
    }
}

fn apply_request_mutation(
    url: &mut String,
    headers: &mut Vec<(String, String)>,
    body: &mut Bytes,
    m: &Mutation,
) {
    if let Some(u) = &m.set_url {
        *url = u.clone();
    }
    apply_header_mutations(headers, m);
    apply_body_mutation(body, m);
}

fn apply_response_mutation(
    status: &mut Option<u16>,
    headers: &mut Vec<(String, String)>,
    body: &mut Bytes,
    m: &Mutation,
    allow_body: bool,
) -> bool {
    let mut modified = false;
    if let Some(s) = m.set_status {
        *status = Some(s);
        modified = true;
    }
    if !m.remove_headers.is_empty() || !m.set_headers.is_empty() {
        modified |= apply_response_header_mutations(headers, m);
    }
    if allow_body && (m.body.is_some() || m.replace.is_some()) {
        let before = body.clone();
        apply_body_mutation(body, m);
        if *body != before {
            strip_body_validators(headers);
            modified = true;
        }
    }
    modified
}

fn apply_header_mutations(headers: &mut Vec<(String, String)>, m: &Mutation) {
    for name in &m.remove_headers {
        headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
    }
    for (name, value) in &m.set_headers {
        set_header_vec(headers, name, value);
    }
}

fn apply_response_header_mutations(
    headers: &mut Vec<(String, String)>,
    mutation: &Mutation,
) -> bool {
    let mut modified = false;
    for name in &mutation.remove_headers {
        if is_response_framing_header(name) {
            continue;
        }
        let before = headers.len();
        headers.retain(|(candidate, _)| !candidate.eq_ignore_ascii_case(name));
        modified |= headers.len() != before;
    }
    for (name, value) in &mutation.set_headers {
        if is_response_framing_header(name) {
            continue;
        }
        set_header_vec(headers, name, value);
        modified = true;
    }
    modified
}

fn is_response_framing_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "content-encoding" | "content-length" | "transfer-encoding"
    )
}

fn strip_body_validators(headers: &mut Vec<(String, String)>) {
    headers.retain(|(name, _)| {
        !matches!(
            name.to_ascii_lowercase().as_str(),
            "content-md5"
                | "digest"
                | "content-digest"
                | "repr-digest"
                | "etag"
                | "last-modified"
                | "content-range"
                | "accept-ranges"
        )
    });
}

fn apply_body_mutation(body: &mut Bytes, m: &Mutation) {
    if let Some(b) = &m.body {
        *body = Bytes::from(b.clone());
        return;
    }
    if let Some((re, repl)) = &m.replace
        && let Ok(rx) = regex::Regex::new(re)
        && let Ok(text) = std::str::from_utf8(body)
        && rx.is_match(text)
    {
        let new = rx.replace_all(text, repl.as_str()).into_owned();
        *body = Bytes::from(new.into_bytes());
    }
}

fn drop_response(method: &Method, status: Option<u16>) -> Response<ProxyBody> {
    match status {
        Some(s) => build_client_response_for(method, s, &[], Bytes::new()),
        None => error_response_for(method, StatusCode::BAD_GATEWAY, "dropped by net intercept"),
    }
}

// ── declarative rules (P3) ────────────────────────────────────

type SyntheticResponse = (u16, Vec<(String, String)>, Bytes);

/// Outcome of the request-phase rules: an optional short-circuit response
/// (block / map-local), an accumulated delay, and whether anything changed.
struct ReqRules {
    short_circuit: Option<SyntheticResponse>,
    delay_ms: u32,
    modified: bool,
    rule_ids: Vec<String>,
}

fn extend_rule_ids(target: &mut Vec<String>, incoming: Vec<String>) {
    for id in incoming {
        if !target.contains(&id) {
            target.push(id);
        }
    }
}

fn rule_matches(
    spec: &RuleSpec,
    method: &str,
    host: &str,
    path: &str,
    ct: Option<&str>,
    status: Option<u16>,
) -> bool {
    let m = &spec.matcher;
    let sub = |hay: &str, n: &Option<String>| {
        n.as_deref()
            .map(|x| hay.to_lowercase().contains(&x.to_lowercase()))
            .unwrap_or(true)
    };
    sub(host, &m.host)
        && sub(path, &m.path)
        && sub(method, &m.method)
        && m.status
            .map(|wanted| status == Some(wanted))
            .unwrap_or(true)
        && spec
            .content_type
            .as_deref()
            .map(|want| ct.map(|c| c.contains(want)).unwrap_or(false))
            .unwrap_or(true)
}

fn graphql_operation_matches(wanted: &str, path: &str, body: &[u8]) -> bool {
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    if reqwest::Url::parse(&format!("http://shadowdroid.invalid{path}"))
        .ok()
        .into_iter()
        .flat_map(|url| {
            url.query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect::<Vec<_>>()
        })
        .any(|(key, value)| key == "operationName" && value == wanted)
    {
        return true;
    }

    fn json_matches(value: &serde_json::Value, wanted: &str) -> bool {
        match value {
            serde_json::Value::Object(object) => {
                object
                    .get("operationName")
                    .and_then(serde_json::Value::as_str)
                    == Some(wanted)
            }
            serde_json::Value::Array(values) => {
                values.iter().any(|value| json_matches(value, wanted))
            }
            _ => false,
        }
    }

    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .is_some_and(|value| json_matches(&value, wanted))
}

fn apply_request_rules(
    shared: &SharedState,
    method: &str,
    host: &str,
    path: &str,
    body: &[u8],
    url: &mut String,
    headers: &mut Vec<(String, String)>,
) -> ReqRules {
    let rules = shared.rules.read().unwrap();
    let mut out = ReqRules {
        short_circuit: None,
        delay_ms: 0,
        modified: false,
        rule_ids: Vec::new(),
    };
    for (id, spec) in rules.iter() {
        if !rule_matches(spec, method, host, path, None, None)
            || spec
                .operation_name
                .as_deref()
                .is_some_and(|wanted| !graphql_operation_matches(wanted, path, body))
        {
            continue;
        }
        match spec.kind.as_str() {
            "respond" => {
                if let Some(response) = &spec.response {
                    out.short_circuit = Some((
                        response.status,
                        response.headers.clone(),
                        Bytes::copy_from_slice(&response.body),
                    ));
                    out.modified = true;
                    out.rule_ids.push(id.clone());
                    return out;
                }
            }
            "block" => {
                let status = spec
                    .args
                    .first()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(444);
                out.short_circuit = Some((status, Vec::new(), Bytes::new()));
                out.modified = true;
                out.rule_ids.push(id.clone());
                return out;
            }
            "map-local" => {
                if let Some(p) = spec.args.first()
                    && let Ok(bytes) = std::fs::read(p)
                {
                    out.short_circuit = Some((
                        200,
                        vec![("content-type".into(), guess_content_type(p))],
                        Bytes::from(bytes),
                    ));
                    out.modified = true;
                    out.rule_ids.push(id.clone());
                    return out;
                }
            }
            "map-remote" => {
                if let Some(repl) = spec.args.first() {
                    rewrite_url(url, repl);
                    out.modified = true;
                    out.rule_ids.push(id.clone());
                }
            }
            "set-request-header" => {
                if let (Some(n), Some(v)) = (spec.args.first(), spec.args.get(1)) {
                    set_header_vec(headers, n, v);
                    out.modified = true;
                    out.rule_ids.push(id.clone());
                }
            }
            "delay" => {
                if let Some(ms) = spec.args.first().and_then(|s| s.parse::<u32>().ok()) {
                    out.delay_ms = out.delay_ms.max(ms);
                    out.rule_ids.push(id.clone());
                }
            }
            _ => {}
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn apply_response_rules(
    shared: &SharedState,
    method: &str,
    host: &str,
    path: &str,
    status: &mut Option<u16>,
    headers: &mut Vec<(String, String)>,
    body: &mut Bytes,
    allow_body: bool,
) -> Vec<String> {
    let rules = shared.rules.read().unwrap();
    let ct = flow::content_type(headers);
    let mut rule_ids = Vec::new();
    for (id, spec) in rules.iter() {
        if !rule_matches(spec, method, host, path, ct.as_deref(), *status) {
            continue;
        }
        let mut applied = false;
        match spec.kind.as_str() {
            "set-status" => {
                if let Some(c) = spec.args.first().and_then(|s| s.parse().ok()) {
                    *status = Some(c);
                    applied = true;
                }
            }
            "set-response-header" => {
                if let (Some(n), Some(v)) = (spec.args.first(), spec.args.get(1))
                    && !is_response_framing_header(n)
                {
                    set_header_vec(headers, n, v);
                    applied = true;
                }
            }
            "replace" if allow_body => {
                if let (Some(re), Some(rp)) = (spec.args.first(), spec.args.get(1)) {
                    let regex = regex::Regex::new(re);
                    if let Ok(rx) = regex
                        && let Ok(text) = std::str::from_utf8(body)
                        && rx.is_match(text)
                    {
                        let new = rx.replace_all(text, rp.as_str()).into_owned();
                        if new.as_bytes() != body.as_ref() {
                            *body = Bytes::from(new.into_bytes());
                            strip_body_validators(headers);
                            applied = true;
                        }
                    }
                }
            }
            _ => {}
        }
        if applied {
            rule_ids.push(id.clone());
        }
    }
    rule_ids
}

/// If replay is loaded and a saved flow matches (method+host+path), return its
/// response triple.
fn replay_lookup(
    shared: &SharedState,
    method: &str,
    host: &str,
    path: &str,
) -> Option<SyntheticResponse> {
    let guard = shared.replay.read().unwrap();
    let flows = guard.as_ref()?;
    let f = flows
        .iter()
        .find(|f| f.method.eq_ignore_ascii_case(method) && f.host == host && f.path == path)?;
    let body = f
        .resp_body
        .clone()
        .map(|s| Bytes::from(s.into_bytes()))
        .unwrap_or_default();
    Some((f.status.unwrap_or(200), f.resp_headers.clone(), body))
}

/// Rewrite the scheme+authority of a URL (keeping the *original* request path),
/// or the authority only if `repl` has no scheme. `repl` is like
/// `https://localhost:8080` — pass host+port only. A path in `repl` is kept and
/// the original path appended after it, which duplicates segments; `rule_add`
/// warns about this so callers don't pass a full URL by mistake.
fn rewrite_url(url: &mut String, repl: &str) {
    let Some(scheme_end) = url.find("://").map(|i| i + 3) else {
        if repl.contains("://") {
            *url = repl.to_string();
        }
        return;
    };
    let path_start = url[scheme_end..]
        .find('/')
        .map(|j| scheme_end + j)
        .unwrap_or(url.len());
    let path = url[path_start..].to_string();
    if repl.contains("://") {
        *url = format!("{}{}", repl.trim_end_matches('/'), path);
    } else {
        *url = format!("{}{}{}", &url[..scheme_end], repl, path);
    }
}

fn guess_content_type(path: &str) -> String {
    let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "json" => "application/json",
        "html" | "htm" => "text/html",
        "js" => "application/javascript",
        "css" => "text/css",
        "xml" => "application/xml",
        "txt" => "text/plain",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn set_header_vec(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    headers.retain(|(candidate, _)| !candidate.eq_ignore_ascii_case(name));
    headers.push((name.to_string(), value.to_string()));
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ContentEncoding {
    Gzip,
    Deflate,
    Brotli,
    Zstd,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EncodingDisposition {
    Identity,
    Supported(ContentEncoding),
    Opaque,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DecodeFailure {
    Unsupported,
    Invalid,
    TooLarge,
    Overloaded,
    WorkerFailed,
}

#[derive(Debug, Eq, PartialEq)]
enum DecodeOutcome {
    Identity,
    Decoded(Vec<u8>),
    PassThrough(DecodeFailure),
}

fn encoding_disposition(headers: &[(String, String)]) -> EncodingDisposition {
    let mut values = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-encoding"))
        .map(|(_, value)| value.trim());
    let Some(value) = values.next() else {
        return EncodingDisposition::Identity;
    };
    if values.next().is_some() {
        return EncodingDisposition::Opaque;
    }

    if value.eq_ignore_ascii_case("identity") {
        EncodingDisposition::Identity
    } else if value.eq_ignore_ascii_case("gzip") || value.eq_ignore_ascii_case("x-gzip") {
        EncodingDisposition::Supported(ContentEncoding::Gzip)
    } else if value.eq_ignore_ascii_case("deflate") {
        EncodingDisposition::Supported(ContentEncoding::Deflate)
    } else if value.eq_ignore_ascii_case("br") {
        EncodingDisposition::Supported(ContentEncoding::Brotli)
    } else if value.eq_ignore_ascii_case("zstd") {
        EncodingDisposition::Supported(ContentEncoding::Zstd)
    } else {
        // Includes empty, unknown, and stacked (`gzip, br`) encodings. Applying
        // rules to bytes we cannot fully decode would corrupt the response.
        EncodingDisposition::Opaque
    }
}

/// Decode a supported response away from Tokio's async workers. `Bytes::clone`
/// is a cheap owned view, which gives the blocking closure a `'static` input
/// without moving the headers or the original pass-through body.
async fn decompress_bounded(headers: &[(String, String)], body: &Bytes) -> DecodeOutcome {
    decompress_bounded_with_cap(headers, body, DECOMPRESSED_CAP).await
}

async fn decompress_bounded_with_cap(
    headers: &[(String, String)],
    body: &Bytes,
    cap: usize,
) -> DecodeOutcome {
    let encoding = match encoding_disposition(headers) {
        EncodingDisposition::Identity => return DecodeOutcome::Identity,
        EncodingDisposition::Opaque => {
            return DecodeOutcome::PassThrough(DecodeFailure::Unsupported);
        }
        EncodingDisposition::Supported(encoding) => encoding,
    };
    let input = body.clone();
    static SLOTS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    let slots = SLOTS
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(DECOMPRESSION_CONCURRENCY)))
        .clone();
    let permit =
        match tokio::time::timeout(DECOMPRESSION_QUEUE_TIMEOUT, slots.acquire_owned()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) | Err(_) => {
                return DecodeOutcome::PassThrough(DecodeFailure::Overloaded);
            }
        };
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        decode_capped(encoding, input.as_ref(), cap)
    })
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::warn!(%error, "response decompression worker failed");
            DecodeOutcome::PassThrough(DecodeFailure::WorkerFailed)
        }
    }
}

/// Decode at most `cap + 1` bytes so an expansion beyond the limit is detected
/// without retaining the full output. Deflate accepts both the RFC zlib wrapper
/// and the raw stream emitted by some servers; an over-limit zlib stream is not
/// retried as raw deflate.
fn decode_capped(encoding: ContentEncoding, body: &[u8], cap: usize) -> DecodeOutcome {
    let decoded = match encoding {
        ContentEncoding::Gzip => read_decoder_capped(flate2::read::GzDecoder::new(body), cap),
        ContentEncoding::Deflate => {
            match read_decoder_capped(flate2::read::ZlibDecoder::new(body), cap) {
                Err(DecodeFailure::Invalid) => {
                    read_decoder_capped(flate2::read::DeflateDecoder::new(body), cap)
                }
                outcome => outcome,
            }
        }
        ContentEncoding::Brotli => read_decoder_capped(brotli::Decompressor::new(body, 4096), cap),
        ContentEncoding::Zstd => match ruzstd::decoding::StreamingDecoder::new(body) {
            Ok(decoder) => read_decoder_capped(decoder, cap),
            Err(_) => Err(DecodeFailure::Invalid),
        },
    };

    match decoded {
        Ok(plain) => DecodeOutcome::Decoded(plain),
        Err(failure) => DecodeOutcome::PassThrough(failure),
    }
}

fn read_decoder_capped<R: std::io::Read>(decoder: R, cap: usize) -> Result<Vec<u8>, DecodeFailure> {
    use std::io::Read;

    let limit = u64::try_from(cap).unwrap_or(u64::MAX).saturating_add(1);
    let mut output = Vec::with_capacity(cap.min(64 * 1024));
    decoder
        .take(limit)
        .read_to_end(&mut output)
        .map_err(|_| DecodeFailure::Invalid)?;
    if output.len() > cap {
        Err(DecodeFailure::TooLarge)
    } else {
        Ok(output)
    }
}

fn header_pairs(h: &http::HeaderMap) -> Vec<(String, String)> {
    h.iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect()
}

fn is_hop_by_hop(name_lower: &str) -> bool {
    matches!(
        name_lower,
        "connection"
            | "proxy-connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "transfer-encoding"
            | "te"
            | "trailer"
            | "upgrade"
    )
}

fn connection_nominated_headers(headers: &[(String, String)]) -> HashSet<String> {
    headers
        .iter()
        .filter(|(name, _)| {
            name.eq_ignore_ascii_case("connection") || name.eq_ignore_ascii_case("proxy-connection")
        })
        .flat_map(|(_, value)| value.split(','))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Glob host match used to scope which hosts the proxy MITMs (`net start --host`).
/// `*.example.com` matches the apex + any subdomain. A **domain-shaped** pattern
/// (contains a dot) matches that domain and its subdomains at a label boundary —
/// so `example.com` matches `api.example.com` but NOT `example.com.evil.com`. A
/// bare fragment with no dot (`livd`) keeps the substring convenience.
///
/// (This is stricter than [`FlowRecord::matches`], which stays a plain substring
/// match — that only filters flows already captured, so looseness is harmless
/// there; scoping decides what to intercept, so it must not over-capture.)
fn host_glob_match(pattern: &str, host: &str) -> bool {
    let p = pattern.to_lowercase();
    let h = host.to_lowercase();
    if let Some(suffix) = p.strip_prefix("*.") {
        h == suffix || h.ends_with(&format!(".{suffix}"))
    } else if p.contains('.') {
        h == p || h.ends_with(&format!(".{p}"))
    } else {
        h.contains(&p)
    }
}

/// An `AsyncRead`/`AsyncWrite` that replays a saved prefix before delegating to
/// the inner stream — lets rustls see the ClientHello bytes we peeked. (Copy of
/// hudsucker's `rewind.rs`, trimmed.)
struct Rewind<T> {
    prefix: Vec<u8>,
    pos: usize,
    inner: T,
}

impl<T> Rewind<T> {
    fn new(inner: T, prefix: Vec<u8>) -> Self {
        Self {
            prefix,
            pos: 0,
            inner,
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Rewind<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskCtx<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.pos < this.prefix.len() {
            let n = (this.prefix.len() - this.pos).min(buf.remaining());
            buf.put_slice(&this.prefix[this.pos..this.pos + n]);
            this.pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Rewind<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskCtx<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskCtx<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskCtx<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ContentEncoding, DecodeFailure, DecodeOutcome, EncodingDisposition, HeldFlow, HoldDecision,
        ReleaseHeldResult, TerminalHoldHistory, decode_capped, decompress_bounded_with_cap,
        encoding_disposition, frame_stream_body, graphql_operation_matches, host_glob_match,
        release_held, resolve_held, tls_failure_reason, upstream_headers, ws_tls_connector,
    };
    use crate::net::Mutation;
    use crate::net::flow::FlowRecord;
    use bytes::Bytes;
    use http_body_util::BodyExt;
    use hyper::body::Frame;
    use std::collections::HashMap;
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::Poll;
    use std::time::Duration;
    use tokio::sync::oneshot;

    async fn self_signed_tls_handshake(verify_upstream: bool) -> bool {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let server_cfg = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
            )
            .unwrap();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(server_cfg));
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            acceptor.accept(tcp).await.is_ok()
        });

        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let client_ok = ws_tls_connector(verify_upstream)
            .unwrap()
            .connect(name, tcp)
            .await
            .is_ok();
        let _ = server.await;
        client_ok
    }

    #[tokio::test]
    async fn verify_upstream_applies_to_websocket_tls() {
        assert!(self_signed_tls_handshake(false).await);
        assert!(!self_signed_tls_handshake(true).await);
    }

    #[test]
    fn graphql_operation_matches_url_json_and_batch_bodies() {
        assert!(graphql_operation_matches(
            "currentSession",
            "/graphql?operationName=currentSession",
            b""
        ));
        assert!(graphql_operation_matches(
            "current Session",
            "/graphql?operationName=current%20Session",
            b""
        ));
        assert!(graphql_operation_matches(
            "currentSession",
            "/graphql",
            br#"{"operationName":"currentSession","variables":{}}"#
        ));
        assert!(graphql_operation_matches(
            "currentSession",
            "/graphql",
            br#"[{"operationName":"other"},{"operationName":"currentSession"}]"#
        ));
        assert!(!graphql_operation_matches(
            "currentSession",
            "/graphql?operationName=other",
            br#"{"operationName":"alsoOther"}"#
        ));
    }

    fn gzip(plain: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(plain).unwrap();
        gz.finish().unwrap()
    }

    fn zlib_deflate(plain: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut deflate =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        deflate.write_all(plain).unwrap();
        deflate.finish().unwrap()
    }

    fn raw_deflate(plain: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut deflate =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        deflate.write_all(plain).unwrap();
        deflate.finish().unwrap()
    }

    fn brotli(plain: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut br = Vec::new();
        {
            let mut w = brotli::CompressorWriter::new(&mut br, 4096, 5, 22);
            w.write_all(plain).unwrap();
        }
        br
    }

    fn zstd(plain: &[u8]) -> Vec<u8> {
        ruzstd::encoding::compress_to_vec(plain, ruzstd::encoding::CompressionLevel::Fastest)
    }

    fn encoded_bodies(plain: &[u8]) -> Vec<(ContentEncoding, Vec<u8>)> {
        vec![
            (ContentEncoding::Gzip, gzip(plain)),
            (ContentEncoding::Deflate, zlib_deflate(plain)),
            (ContentEncoding::Deflate, raw_deflate(plain)),
            (ContentEncoding::Brotli, brotli(plain)),
            (ContentEncoding::Zstd, zstd(plain)),
        ]
    }

    fn ce(value: &str) -> Vec<(String, String)> {
        vec![("Content-Encoding".to_string(), value.to_string())]
    }

    #[test]
    fn decompress_round_trips_every_supported_encoding() {
        let plain = b"{\"hello\":\"world\",\"items\":[1,2,3]}".repeat(20);
        for (encoding, encoded) in encoded_bodies(&plain) {
            assert_eq!(
                decode_capped(encoding, &encoded, plain.len()),
                DecodeOutcome::Decoded(plain.clone()),
                "failed to decode {encoding:?}"
            );
        }
    }

    #[test]
    fn decompression_cap_accepts_limit_and_rejects_next_byte() {
        const CAP: usize = 1024;
        let exact = vec![b'x'; CAP];
        for (encoding, encoded) in encoded_bodies(&exact) {
            assert_eq!(
                decode_capped(encoding, &encoded, CAP),
                DecodeOutcome::Decoded(exact.clone()),
                "exact cap failed for {encoding:?}"
            );
        }

        let oversized = vec![b'y'; CAP + 1];
        for (encoding, encoded) in encoded_bodies(&oversized) {
            assert_eq!(
                decode_capped(encoding, &encoded, CAP),
                DecodeOutcome::PassThrough(DecodeFailure::TooLarge),
                "cap was not enforced for {encoding:?}"
            );
        }
    }

    #[test]
    fn corrupt_encodings_are_passed_through() {
        let corrupt = [
            (ContentEncoding::Gzip, b"not really gzip".as_slice()),
            (ContentEncoding::Deflate, b"\xff\xff\xff\xff".as_slice()),
            (ContentEncoding::Brotli, b"not really brotli".as_slice()),
            (ContentEncoding::Zstd, b"not really zstd".as_slice()),
        ];
        for (encoding, body) in corrupt {
            assert_eq!(
                decode_capped(encoding, body, 1024),
                DecodeOutcome::PassThrough(DecodeFailure::Invalid),
                "corrupt {encoding:?} payload was accepted"
            );
        }
    }

    #[tokio::test]
    async fn async_decompression_classifies_and_bounds_encoded_responses() {
        assert_eq!(encoding_disposition(&[]), EncodingDisposition::Identity);
        assert_eq!(
            encoding_disposition(&ce("identity")),
            EncodingDisposition::Identity
        );
        assert_eq!(
            encoding_disposition(&ce("X-GZIP")),
            EncodingDisposition::Supported(ContentEncoding::Gzip)
        );
        assert_eq!(
            encoding_disposition(&ce("gzip, br")),
            EncodingDisposition::Opaque
        );
        assert_eq!(
            encoding_disposition(&ce("compress")),
            EncodingDisposition::Opaque
        );
        assert_eq!(
            encoding_disposition(&[
                ("Content-Encoding".into(), "gzip".into()),
                ("content-encoding".into(), "br".into()),
            ]),
            EncodingDisposition::Opaque
        );

        let plain = b"spawn-blocking decode".repeat(8);
        let wire = gzip(&plain);
        let encoded = Bytes::from(wire.clone());
        assert_eq!(
            decompress_bounded_with_cap(&ce("gzip"), &encoded, plain.len()).await,
            DecodeOutcome::Decoded(plain.clone())
        );
        assert_eq!(
            decompress_bounded_with_cap(&ce("gzip"), &encoded, plain.len() - 1).await,
            DecodeOutcome::PassThrough(DecodeFailure::TooLarge)
        );
        // The owned `Bytes` remains available for exact pass-through after the
        // blocking decoder reports a failure.
        assert_eq!(encoded.as_ref(), wire);

        assert_eq!(
            decompress_bounded_with_cap(&ce("gzip, br"), &encoded, 1).await,
            DecodeOutcome::PassThrough(DecodeFailure::Unsupported)
        );
        assert_eq!(
            decompress_bounded_with_cap(&[], &Bytes::from_static(b"plain"), 1).await,
            DecodeOutcome::Identity
        );
    }

    #[tokio::test]
    async fn websocket_rejection_body_adapter_is_lazy_and_propagates_errors() {
        let polls = Arc::new(AtomicUsize::new(0));
        let step = Arc::new(AtomicUsize::new(0));
        let stream_polls = Arc::clone(&polls);
        let stream_step = Arc::clone(&step);
        let frames = futures_util::stream::poll_fn(move |_| {
            stream_polls.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(match stream_step.fetch_add(1, Ordering::SeqCst) {
                0 => Some(Ok::<_, io::Error>(Frame::data(Bytes::from_static(b"too ")))),
                1 => Some(Ok(Frame::data(Bytes::from_static(b"large")))),
                _ => None,
            })
        });

        let body = frame_stream_body(frames);
        assert_eq!(polls.load(Ordering::SeqCst), 0, "adapter eagerly polled");
        assert_eq!(
            body.collect().await.unwrap().to_bytes(),
            Bytes::from_static(b"too large")
        );
        assert_eq!(polls.load(Ordering::SeqCst), 3);

        let erroring = futures_util::stream::iter(vec![
            Ok(Frame::data(Bytes::from_static(b"prefix"))),
            Err(io::Error::other("boom")),
        ]);
        match frame_stream_body(erroring).collect().await {
            Ok(_) => panic!("stream error was discarded"),
            Err(error) => assert!(error.to_string().contains("boom")),
        }
    }

    #[test]
    fn streaming_content_types_and_length() {
        use super::{content_length, is_streaming_content_type};
        let h = |ct: &str| vec![("content-type".to_string(), ct.to_string())];
        assert!(is_streaming_content_type(&h("text/event-stream")));
        assert!(is_streaming_content_type(&h("application/grpc+proto")));
        assert!(!is_streaming_content_type(&h("application/json")));
        assert!(!is_streaming_content_type(&[]));
        assert_eq!(
            content_length(&[("Content-Length".into(), "42".into())]),
            Some(42)
        );
        assert_eq!(content_length(&[]), None);
    }

    #[tokio::test]
    async fn stream_cap_buffers_small_and_spills_large() {
        use super::{BodyRead, ByteStream, read_stream_capped};
        use bytes::Bytes;
        let chunks = |parts: Vec<&'static [u8]>| -> ByteStream {
            Box::pin(futures_util::stream::iter(
                parts.into_iter().map(|b| Ok(Bytes::from_static(b))),
            ))
        };

        // Finishes under the cap → one buffered blob (fully captured).
        match read_stream_capped(chunks(vec![b"hel", b"lo"]), false, None, 64).await {
            BodyRead::Buffered(b) => assert_eq!(&b[..], b"hello"),
            _ => panic!("expected Buffered"),
        }

        // Exceeds the cap → spill to Streamed, keeping the pulled prefix.
        match read_stream_capped(chunks(vec![b"aaaa", b"bbbb", b"cccc"]), false, Some(12), 6).await
        {
            BodyRead::Streamed {
                prefix, len_hint, ..
            } => {
                assert_eq!(len_hint, Some(12));
                assert!(prefix.iter().map(|c| c.len()).sum::<usize>() > 6);
            }
            _ => panic!("expected Streamed"),
        }

        // force_stream → immediate Streamed with an empty prefix (SSE).
        match read_stream_capped(chunks(vec![b"data: 1\n\n"]), true, None, 999).await {
            BodyRead::Streamed { prefix, .. } => assert!(prefix.is_empty()),
            _ => panic!("expected Streamed"),
        }

        // A stream error surfaces as Error, not a panic.
        let erroring: ByteStream = Box::pin(futures_util::stream::iter(vec![
            Ok(Bytes::from_static(b"x")),
            Err(std::io::Error::other("boom")),
        ]));
        match read_stream_capped(erroring, false, None, 64).await {
            BodyRead::Error(e) => assert!(e.contains("boom")),
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn make_flow_streamed_request_drops_body_and_uses_content_length() {
        let headers = vec![
            ("Content-Length".to_string(), "1048576".to_string()),
            (
                "Content-Type".to_string(),
                "application/octet-stream".to_string(),
            ),
        ];
        let rec = super::make_flow(super::FlowParts {
            id: "f1",
            method: "POST",
            scheme: "https",
            host: "h",
            path: "/upload",
            req_headers: &headers,
            req_bytes: &[], // streamed: no buffered body
            req_streamed: true,
            status: Some(200),
            resp_headers: &[],
            resp_bytes: b"ok",
            dur_ms: 5,
            error: None,
            matched: None,
            modified: false,
            rule_ids: &[],
        });
        assert!(rec.req_streamed);
        assert!(rec.req_body.is_none());
        // req_len comes from content-length, not the (empty) buffer.
        assert_eq!(rec.req_len, 1_048_576);
    }

    #[test]
    fn detects_websocket_upgrade() {
        use super::is_websocket_upgrade;
        let mut h = http::HeaderMap::new();
        assert!(!is_websocket_upgrade(&h));
        h.insert(http::header::CONNECTION, "Upgrade".parse().unwrap());
        h.insert(http::header::UPGRADE, "websocket".parse().unwrap());
        assert!(is_websocket_upgrade(&h));
        // A keep-alive upgrade to something else is not a websocket.
        h.insert(http::header::UPGRADE, "h2c".parse().unwrap());
        assert!(!is_websocket_upgrade(&h));
    }

    #[test]
    fn upstream_headers_preserve_repeated_field_lines() {
        let input = vec![
            ("Cookie".into(), "a=1".into()),
            ("cookie".into(), "b=2".into()),
            ("X-Debug".into(), "first".into()),
            ("X-Debug".into(), "second".into()),
            ("Host".into(), "example.test".into()),
            ("Content-Length".into(), "123".into()),
            ("Transfer-Encoding".into(), "chunked".into()),
            ("Connection".into(), "X-Hop".into()),
            ("connection".into(), "x-second, keep-alive".into()),
            ("X-Hop".into(), "secret".into()),
            ("X-Second".into(), "also-secret".into()),
        ];
        let headers = upstream_headers(&input, false, false, Some(1_048_576));

        let cookies = headers
            .get_all(http::header::COOKIE)
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(cookies, ["a=1", "b=2"]);
        let debug = headers
            .get_all("x-debug")
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(debug, ["first", "second"]);
        assert!(!headers.contains_key(http::header::HOST));
        assert!(!headers.contains_key(http::header::CONNECTION));
        assert!(!headers.contains_key(http::header::TRANSFER_ENCODING));
        assert!(!headers.contains_key("x-hop"));
        assert!(!headers.contains_key("x-second"));
        assert_eq!(headers[http::header::CONTENT_LENGTH], "1048576");

        let headers = upstream_headers(&input, false, true, None);
        assert_eq!(headers[http::header::ACCEPT_ENCODING], "identity");
    }

    #[test]
    fn response_filters_connection_nominated_headers() {
        let response = super::response_with_body(
            200,
            &[
                ("Connection".into(), "X-Hop".into()),
                ("X-Hop".into(), "secret".into()),
                ("X-End-To-End".into(), "keep".into()),
            ],
            super::full_body(Bytes::new()),
            Some(0),
        );
        assert!(!response.headers().contains_key(http::header::CONNECTION));
        assert!(!response.headers().contains_key("x-hop"));
        assert_eq!(response.headers()["x-end-to-end"], "keep");
    }

    #[test]
    fn setting_a_header_replaces_every_duplicate_field_line() {
        let mut headers: Vec<(String, String)> = vec![
            ("Set-Cookie".into(), "old-a=1".into()),
            ("X-Keep".into(), "yes".into()),
            ("set-cookie".into(), "old-b=2".into()),
        ];
        super::set_header_vec(&mut headers, "Set-Cookie", "new=3");
        let set_cookie = headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
            .collect::<Vec<_>>();
        assert_eq!(set_cookie.len(), 1);
        assert_eq!(set_cookie[0].0, "Set-Cookie");
        assert_eq!(set_cookie[0].1, "new=3");
        assert!(headers.contains(&("X-Keep".into(), "yes".into())));

        let mutation = Mutation {
            set_headers: vec![("X-Keep".into(), "replaced".into())],
            ..Default::default()
        };
        headers.push(("x-keep".into(), "stale".into()));
        super::apply_header_mutations(&mut headers, &mutation);
        let kept = headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("x-keep"))
            .collect::<Vec<_>>();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].1, "replaced");
    }

    #[test]
    fn opaque_response_metadata_edits_preserve_wire_encoding() {
        let mut status = Some(200);
        let mut headers = vec![
            ("Content-Encoding".into(), "gzip".into()),
            ("ETag".into(), "old-validator".into()),
        ];
        let mut body = Bytes::from_static(b"opaque-gzip-bytes");
        let mutation = Mutation {
            set_status: Some(201),
            set_headers: vec![
                ("Content-Encoding".into(), "identity".into()),
                ("X-Debug".into(), "yes".into()),
            ],
            remove_headers: vec!["content-encoding".into()],
            body: Some(b"must-not-apply".to_vec()),
            ..Default::default()
        };

        assert!(super::apply_response_mutation(
            &mut status,
            &mut headers,
            &mut body,
            &mutation,
            false,
        ));
        assert_eq!(status, Some(201));
        assert_eq!(body, Bytes::from_static(b"opaque-gzip-bytes"));
        assert!(headers.contains(&("Content-Encoding".into(), "gzip".into())));
        assert!(headers.contains(&("X-Debug".into(), "yes".into())));
    }

    #[test]
    fn textual_replace_never_corrupts_unmatched_or_binary_bodies() {
        let mutation = Mutation {
            replace: Some(("needle".into(), "replacement".into())),
            ..Default::default()
        };
        let mut unmatched = Bytes::from_static(b"plain text without a match");
        super::apply_body_mutation(&mut unmatched, &mutation);
        assert_eq!(unmatched, Bytes::from_static(b"plain text without a match"));

        let mut binary = Bytes::from_static(&[0xff, 0xfe, b'n', b'e', b'e', b'd', b'l', b'e']);
        super::apply_body_mutation(&mut binary, &mutation);
        assert_eq!(
            binary,
            Bytes::from_static(&[0xff, 0xfe, b'n', b'e', b'e', b'd', b'l', b'e'])
        );
    }

    #[test]
    fn head_and_bodyless_statuses_never_forward_a_message_body() {
        assert!(!super::response_allows_body(&hyper::Method::HEAD, 200));
        assert!(!super::response_allows_body(&hyper::Method::GET, 101));
        assert!(!super::response_allows_body(&hyper::Method::GET, 204));
        assert!(!super::response_allows_body(&hyper::Method::GET, 304));
        assert!(super::response_allows_body(&hyper::Method::GET, 200));
    }

    #[test]
    fn response_framing_keeps_known_stream_length() {
        let response = super::response_with_body(
            200,
            &[("Content-Length".into(), "999".into())],
            super::streamed_body(Vec::new(), Box::pin(futures_util::stream::empty())),
            Some(42),
        );
        assert_eq!(response.headers()[http::header::CONTENT_LENGTH], "42");
    }

    #[test]
    fn redact_headers_masks_only_sensitive() {
        let mut headers = vec![
            (
                "Authorization".to_string(),
                "Bearer secret-token".to_string(),
            ),
            ("Content-Type".to_string(), "application/json".to_string()),
            ("set-cookie".to_string(), "session=abc".to_string()),
            ("X-Trace".to_string(), "keep-me".to_string()),
        ];
        let policy = crate::redaction::Policy::builtin();
        for (name, value) in &mut headers {
            *value = policy.redact_header_value(name, value);
        }
        assert_eq!(headers[0].1, "<redacted:token>"); // Authorization (case-insensitive)
        assert_eq!(headers[1].1, "application/json"); // untouched
        assert_eq!(headers[2].1, "<redacted:cookie>"); // set-cookie
        assert_eq!(headers[3].1, "keep-me"); // untouched
    }

    #[test]
    fn redact_flow_masks_nested_bodies_without_changing_shape_metadata() {
        let policy = crate::redaction::Policy::builtin();
        let mut flow = FlowRecord {
            req_body: Some(
                r#"{"variables":{"email":"person@example.com","password":"secret"}}"#.into(),
            ),
            resp_body: Some(
                r#"{"data":{"accessToken":"eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.sig"}}"#.into(),
            ),
            req_len: 71,
            resp_len: 73,
            ..Default::default()
        };
        policy.redact_flow_record(&mut flow);
        let request: serde_json::Value =
            serde_json::from_str(flow.req_body.as_deref().unwrap()).unwrap();
        let response: serde_json::Value =
            serde_json::from_str(flow.resp_body.as_deref().unwrap()).unwrap();
        assert_eq!(request["variables"]["email"], "<redacted:email>");
        assert_eq!(request["variables"]["password"], "<redacted:secret>");
        assert_eq!(response["data"]["accessToken"], "<redacted:jwt>");
        assert_eq!(flow.req_len, 71);
        assert_eq!(flow.resp_len, 73);
    }

    #[test]
    fn tls_failure_reason_flags_cert_rejection_vs_lower_level() {
        // A peer alert during the handshake reads as "app doesn't trust the CA".
        let alert = io::Error::other("received fatal alert: UnknownCA");
        let reason = tls_failure_reason(&alert);
        assert!(
            reason.contains("rejected the proxy's TLS certificate"),
            "{reason}"
        );
        assert!(reason.contains("net trust"), "{reason}");
        assert!(
            reason.contains("UnknownCA"),
            "raw error preserved: {reason}"
        );

        // Anything else is reported as a lower-level handshake failure.
        let reset = io::Error::other("connection reset by peer");
        let reason = tls_failure_reason(&reset);
        assert!(reason.contains("failed before any request"), "{reason}");
        assert!(!reason.contains("net trust"), "{reason}");
    }

    fn insert_held(
        held: &Mutex<HashMap<String, HeldFlow>>,
        id: &str,
    ) -> oneshot::Receiver<HoldDecision> {
        let (tx, rx) = oneshot::channel();
        let meta = FlowRecord {
            id: id.into(),
            ..Default::default()
        };
        let held_at = crate::events::now_ts();
        held.lock().unwrap().insert(
            id.to_string(),
            HeldFlow {
                tx: Some(tx),
                meta,
                phase: "request".into(),
                held_at,
                expires_at: held_at + 60.0,
                held_charge: None,
            },
        );
        rx
    }

    fn fail_open() -> HoldDecision {
        HoldDecision::Resume(Mutation::default())
    }

    #[test]
    fn held_lifecycle_carries_actionable_phase_deadline_and_connection() {
        for phase in ["request", "response"] {
            let held = Mutex::new(HashMap::new());
            let _rx = insert_held(&held, "f1");
            held.lock().unwrap().get_mut("f1").unwrap().phase = phase.into();
            let lifecycle = held.lock().unwrap().get("f1").unwrap().lifecycle();
            assert_eq!(lifecycle.id, "f1");
            assert_eq!(lifecycle.phase, phase);
            assert_eq!(lifecycle.state, "held");
            assert!(lifecycle.expires_at > lifecycle.held_at);
            assert!(lifecycle.client_connected);
        }
    }

    #[test]
    fn release_at_or_after_wall_deadline_reports_expired() {
        let held = Mutex::new(HashMap::new());
        let terminal = Mutex::new(TerminalHoldHistory::default());
        let _rx = insert_held(&held, "f1");
        held.lock().unwrap().get_mut("f1").unwrap().expires_at = crate::events::now_ts() - 1.0;
        assert!(matches!(
            release_held(
                &held,
                &terminal,
                "f1",
                "resume",
                HoldDecision::Resume(Mutation::default())
            ),
            ReleaseHeldResult::DeadlineExpired(_)
        ));
        assert!(held.lock().unwrap().is_empty());
        assert_eq!(
            terminal.lock().unwrap().get("f1").unwrap().state,
            "deadline_expired"
        );
    }

    #[test]
    fn terminal_hold_history_is_bounded_and_replaces_reused_ids() {
        let mut history = TerminalHoldHistory::default();
        for index in 0..=super::TERMINAL_HOLD_HISTORY_CAP {
            history.record(super::TerminalHold {
                id: format!("f{index}"),
                phase: "request".into(),
                state: "deadline_expired",
                held_at: 1.0,
                expires_at: 2.0,
                terminal_at: 2.0,
                action: None,
            });
        }
        assert!(history.get("f0").is_none());
        assert!(
            history
                .get(&format!("f{}", super::TERMINAL_HOLD_HISTORY_CAP))
                .is_some()
        );

        history.record(super::TerminalHold {
            id: "f1".into(),
            phase: "response".into(),
            state: "released",
            held_at: 3.0,
            expires_at: 4.0,
            terminal_at: 3.5,
            action: Some("respond".into()),
        });
        let replaced = history.get("f1").unwrap();
        assert_eq!(replaced.phase, "response");
        assert_eq!(replaced.action.as_deref(), Some("respond"));
    }

    /// The core of the held-flow fix: an agent's decision that arrives before the
    /// deadline must reach the proxy — never get silently replaced by fail-open.
    #[tokio::test]
    async fn release_decision_is_delivered_not_dropped() {
        let held = Mutex::new(HashMap::new());
        let terminal = Mutex::new(TerminalHoldHistory::default());
        let rx = insert_held(&held, "f1");
        // Agent releases with a distinctive decision before the (long) deadline.
        assert!(matches!(
            release_held(
                &held,
                &terminal,
                "f1",
                "drop",
                HoldDecision::Drop(Some(599))
            ),
            ReleaseHeldResult::Released(_)
        ));
        let d = resolve_held(
            &held,
            &terminal,
            "f1",
            rx,
            Duration::from_secs(5),
            fail_open,
        )
        .await;
        assert!(
            matches!(d, HoldDecision::Drop(Some(599))),
            "the agent's decision must win, not fail-open"
        );
        let lifecycle = terminal.lock().unwrap().get("f1").unwrap();
        assert_eq!(lifecycle.state, "released");
        assert_eq!(lifecycle.action.as_deref(), Some("drop"));
    }

    /// The honest-reporting half: when the deadline wins, it claims the entry, so
    /// a later release reports `false` instead of lying that it was delivered.
    #[tokio::test]
    async fn timeout_claims_the_entry_and_a_late_release_reports_false() {
        let held = Mutex::new(HashMap::new());
        let terminal = Mutex::new(TerminalHoldHistory::default());
        let rx = insert_held(&held, "f1");
        let d = resolve_held(
            &held,
            &terminal,
            "f1",
            rx,
            Duration::from_millis(0),
            fail_open,
        )
        .await;
        assert!(matches!(d, HoldDecision::Resume(_)), "fail-open on timeout");
        assert!(
            held.lock().unwrap().is_empty(),
            "the deadline removed (claimed) the entry"
        );
        // A release arriving after the timeout finds nothing → must report false.
        assert!(
            matches!(
                release_held(
                    &held,
                    &terminal,
                    "f1",
                    "drop",
                    HoldDecision::Drop(Some(200))
                ),
                ReleaseHeldResult::Missing
            ),
            "a release after the deadline must not claim success"
        );
        assert_eq!(
            terminal.lock().unwrap().get("f1").unwrap().state,
            "deadline_expired"
        );
    }

    #[tokio::test]
    async fn cancelling_a_held_request_removes_its_map_entry() {
        let held = Arc::new(Mutex::new(HashMap::new()));
        let terminal = Arc::new(Mutex::new(TerminalHoldHistory::default()));
        let rx = insert_held(&held, "f1");
        let task_held = held.clone();
        let task_terminal = terminal.clone();
        let task = tokio::spawn(async move {
            resolve_held(
                &task_held,
                &task_terminal,
                "f1",
                rx,
                Duration::from_secs(60),
                fail_open,
            )
            .await
        });
        tokio::task::yield_now().await;
        task.abort();
        let _ = task.await;
        assert!(held.lock().unwrap().is_empty());
        assert_eq!(
            terminal.lock().unwrap().get("f1").unwrap().state,
            "client_canceled"
        );
    }

    #[test]
    fn host_globs() {
        assert!(host_glob_match("*.livd.app", "api.livd.app"));
        assert!(host_glob_match("*.livd.app", "livd.app"));
        assert!(!host_glob_match("*.livd.app", "evil-livd.app"));
        assert!(host_glob_match("api.livd.app", "api.livd.app"));
        assert!(host_glob_match("livd", "api.livd.app"));
        assert!(!host_glob_match("segment.io", "api.livd.app"));

        // A domain-shaped pattern matches its subdomains at a label boundary…
        assert!(host_glob_match("livd.app", "api.livd.app"));
        assert!(host_glob_match("example.com", "example.com"));
        // …but NOT a longer domain that merely contains it (the over-capture bug).
        assert!(!host_glob_match("example.com", "example.com.evil.com"));
        assert!(!host_glob_match("livd.app", "notlivd.app"));
    }

    #[test]
    fn response_rule_status_matcher_is_enforced() {
        let spec = crate::net::RuleSpec {
            kind: "set-status".into(),
            matcher: crate::net::Matcher {
                status: Some(200),
                ..Default::default()
            },
            content_type: None,
            operation_name: None,
            response: None,
            args: vec!["201".into()],
        };
        assert!(super::rule_matches(
            &spec,
            "GET",
            "api.example",
            "/",
            None,
            Some(200),
        ));
        assert!(!super::rule_matches(
            &spec,
            "GET",
            "api.example",
            "/",
            None,
            Some(404),
        ));
    }
}
