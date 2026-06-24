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

use anyhow::{anyhow, Result};
use bytes::Bytes;
use http::uri::{Authority, Scheme};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context as TaskCtx, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_rustls::TlsAcceptor;

use crate::events::{self, Event};
use crate::net::ca::CertAuthority;
use crate::net::flow::{self, FlowRecord};
use crate::net::{Matcher, Mutation, RuleSpec};

/// Everything a proxy connection needs. Cloned (Arc) per connection.
pub struct ProxyContext {
    pub ca: Arc<CertAuthority>,
    pub client: reqwest::Client,
    /// Completed flows are pushed here; the daemon drains → store + broadcast.
    pub flow_tx: mpsc::UnboundedSender<FlowRecord>,
    pub shared: Arc<SharedState>,
}

/// Runtime-mutable proxy knobs. (Rules land here in P3.)
pub struct SharedState {
    pub anticache: bool,
    pub anticomp: bool,
    /// Host globs to MITM + capture. Empty = all hosts.
    pub host_filters: Vec<String>,
    /// Active interception config (`net intercept`), or `None`.
    pub intercept: RwLock<Option<InterceptCfg>>,
    /// Flows currently paused, awaiting `net resume`/`drop`/`respond`.
    pub held: Mutex<HashMap<String, HeldFlow>>,
    /// Live event fan-out (shared with the daemon) — carries `http_intercept`.
    pub events: broadcast::Sender<Arc<Event>>,
    /// Declarative rules (`net rule`), applied in order: `(id, spec)`.
    pub rules: RwLock<Vec<(String, RuleSpec)>>,
    /// Saved flows served as canned responses (`net replay`), or `None`.
    pub replay: RwLock<Option<Vec<FlowRecord>>>,
}

impl SharedState {
    /// Should we decrypt + capture this host (vs blind-tunnel it through)?
    pub fn host_in_scope(&self, host: &str) -> bool {
        self.host_filters.is_empty() || self.host_filters.iter().any(|h| host_glob_match(h, host))
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
    pub tx: oneshot::Sender<HoldDecision>,
    pub meta: FlowRecord,
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
/// the app) and accepts invalid upstream certs — this is a debugging proxy and
/// dev/staging backends are often self-signed.
pub fn build_upstream_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .expect("build upstream reqwest client")
}

/// Bind `127.0.0.1:port` and serve until `shutdown` fires.
pub async fn run(
    ctx: Arc<ProxyContext>,
    addr: SocketAddr,
    mut shutdown: oneshot::Receiver<()>,
) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow!("bind proxy {addr}: {e}"))?;
    tracing::info!("net proxy listening on {addr}");
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (tcp, _peer) = match accepted {
                    Ok(v) => v,
                    Err(e) => { tracing::debug!("accept: {e}"); continue; }
                };
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(tcp);
                    let svc = service_fn(move |req| {
                        let ctx = ctx.clone();
                        async move { handle(ctx, req, None).await }
                    });
                    if let Err(e) = http1::Builder::new()
                        .serve_connection(io, svc)
                        .with_upgrades()
                        .await
                    {
                        tracing::debug!("proxy connection error: {e}");
                    }
                });
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
) -> Result<Response<Full<Bytes>>, Infallible> {
    if req.method() == Method::CONNECT {
        return Ok(process_connect(ctx, req));
    }
    match proxy_request(ctx, req, tunnel).await {
        Ok(resp) => Ok(resp),
        Err(e) => {
            tracing::debug!("proxy_request error: {e}");
            Ok(error_response(StatusCode::BAD_GATEWAY, &e.to_string()))
        }
    }
}

/// Establish a CONNECT tunnel: return 200 immediately, then on a detached task
/// peek the first bytes — TLS ClientHello → MITM, else blind TCP tunnel.
fn process_connect(ctx: Arc<ProxyContext>, req: Request<Incoming>) -> Response<Full<Bytes>> {
    let authority = match req.uri().authority().cloned() {
        Some(a) => a,
        None => return error_response(StatusCode::BAD_REQUEST, "CONNECT requires an authority"),
    };
    let host = authority.host().to_string();

    tokio::spawn(async move {
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
                    return;
                }
            };
            let io = TokioIo::new(tls);
            let svc = service_fn(move |r| {
                let ctx = ctx.clone();
                let tunnel = Some((Scheme::HTTPS, authority.clone()));
                async move { handle(ctx, r, tunnel).await }
            });
            if let Err(e) = http1::Builder::new()
                .serve_connection(io, svc)
                .with_upgrades()
                .await
            {
                tracing::debug!("inner serve {host}: {e}");
            }
        } else {
            // Out of scope or not TLS: blind passthrough, no decryption.
            let mut stream = stream;
            match TcpStream::connect(authority.as_ref()).await {
                Ok(mut upstream) => {
                    let _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream).await;
                }
                Err(e) => tracing::debug!("blind tunnel connect {authority}: {e}"),
            }
        }
    });

    // 200 OK (empty body) == "tunnel established".
    Response::new(Full::new(Bytes::new()))
}

/// Forward one (decrypted or plaintext) request upstream, applying any active
/// interception at the request and/or response phase, and capture the flow.
async fn proxy_request(
    ctx: Arc<ProxyContext>,
    req: Request<Incoming>,
    tunnel: Option<(Scheme, Authority)>,
) -> Result<Response<Full<Bytes>>> {
    let (parts, body) = req.into_parts();
    let method = parts.method.clone();
    let (scheme, host, path, mut url) = resolve_target(&parts.uri, &tunnel)?;
    let mut req_headers = header_pairs(&parts.headers);
    let mut req_bytes = body
        .collect()
        .await
        .map_err(|e| anyhow!("read request body: {e}"))?
        .to_bytes();

    let id = flow::new_id();
    let in_scope = ctx.shared.host_in_scope(&host);
    let mut matched: Option<String> = None;
    let mut modified = false;

    // ── replay (P3): serve a saved response, never hitting upstream ──
    if in_scope {
        if let Some((status, headers, body)) =
            replay_lookup(&ctx.shared, method.as_str(), &host, &path)
        {
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
                    status: Some(status),
                    resp_headers: &headers,
                    resp_bytes: &body,
                    dur_ms: 0,
                    error: None,
                    matched: Some("replay".into()),
                    modified: true,
                },
            );
            return Ok(build_client_response(status, &headers, body));
        }
    }

    // ── request-phase rules (P3): block / map-local short-circuit; map-remote
    //    rewrites the URL; delay sleeps before forwarding ──
    if in_scope {
        let r = apply_request_rules(&ctx.shared, method.as_str(), &host, &path, &mut url);
        if r.modified {
            modified = true;
            matched = Some("rule".into());
        }
        if r.delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(r.delay_ms as u64)).await;
        }
        if let Some((status, headers, body)) = r.short_circuit {
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
                    status: Some(status),
                    resp_headers: &headers,
                    resp_bytes: &body,
                    dur_ms: 0,
                    error: None,
                    matched: Some("rule".into()),
                    modified: true,
                },
            );
            return Ok(build_client_response(status, &headers, body));
        }
    }

    // ── request-phase interception ──
    if in_scope {
        let snap = make_flow(FlowParts {
            id: &id,
            method: method.as_str(),
            scheme: &scheme,
            host: &host,
            path: &path,
            req_headers: &req_headers,
            req_bytes: &req_bytes,
            status: None,
            resp_headers: &[],
            resp_bytes: &[],
            dur_ms: 0,
            error: None,
            matched: None,
            modified: false,
        });
        if let Some(decision) = hold(&ctx, snap, "request").await {
            match decision {
                HoldDecision::Drop(s) => return Ok(drop_response(s)),
                HoldDecision::Respond {
                    status,
                    body,
                    headers,
                } => {
                    let resp_bytes = Bytes::from(body);
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
                            status: Some(status),
                            resp_headers: &headers,
                            resp_bytes: &resp_bytes,
                            dur_ms: 0,
                            error: None,
                            matched: Some("intercept:respond".into()),
                            modified: true,
                        },
                    );
                    return Ok(build_client_response(status, &headers, resp_bytes));
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
    let started = std::time::Instant::now();
    let outcome = forward_upstream(
        &ctx.client,
        &method,
        &url,
        &req_headers,
        req_bytes.clone(),
        &ctx.shared,
    )
    .await;
    let dur_ms = started.elapsed().as_millis() as u64;

    let (mut status, mut resp_headers, mut resp_bytes, error) = match outcome {
        Ok((status, headers, bytes)) => (Some(status), headers, bytes, None),
        Err(e) => (None, Vec::new(), Bytes::new(), Some(e.to_string())),
    };

    // Decompress in-scope responses so capture, rules, and intercept all see
    // plain text — and strip `content-encoding` so the (decompressed) body we
    // hand the client stays consistent. (The app decompresses gzip anyway, so
    // serving it already-decompressed is transparent.)
    if in_scope && error.is_none() {
        if let Some(plain) = decompress(&resp_headers, &resp_bytes) {
            resp_bytes = Bytes::from(plain);
            resp_headers.retain(|(k, _)| !k.eq_ignore_ascii_case("content-encoding"));
        }
    }

    // ── response-phase rules (P3): set-status / set-header / replace ──
    if in_scope
        && error.is_none()
        && apply_response_rules(
            &ctx.shared,
            method.as_str(),
            &host,
            &path,
            &mut status,
            &mut resp_headers,
            &mut resp_bytes,
        )
    {
        modified = true;
        if matched.is_none() {
            matched = Some("rule".into());
        }
    }

    // ── response-phase interception ──
    if in_scope && error.is_none() {
        let snap = make_flow(FlowParts {
            id: &id,
            method: method.as_str(),
            scheme: &scheme,
            host: &host,
            path: &path,
            req_headers: &req_headers,
            req_bytes: &req_bytes,
            status,
            resp_headers: &resp_headers,
            resp_bytes: &resp_bytes,
            dur_ms,
            error: None,
            matched: None,
            modified: false,
        });
        if let Some(decision) = hold(&ctx, snap, "response").await {
            match decision {
                HoldDecision::Drop(s) => return Ok(drop_response(s)),
                HoldDecision::Respond {
                    status: rs,
                    body,
                    headers,
                } => {
                    status = Some(rs);
                    resp_headers = headers;
                    resp_bytes = Bytes::from(body);
                    modified = true;
                    matched = Some("intercept:respond".into());
                }
                HoldDecision::Resume(m) => {
                    if !m.is_noop() {
                        modified = true;
                        matched = Some("intercept".into());
                        apply_response_mutation(
                            &mut status,
                            &mut resp_headers,
                            &mut resp_bytes,
                            &m,
                        );
                    }
                    if let Some(d) = m.delay_ms {
                        tokio::time::sleep(Duration::from_millis(d as u64)).await;
                    }
                }
            }
        }
    }

    // ── capture + return ──
    if in_scope {
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
                status,
                resp_headers: &resp_headers,
                resp_bytes: &resp_bytes,
                dur_ms,
                error: error.clone(),
                matched,
                modified,
            },
        );
    }

    Ok(match status {
        Some(status) => build_client_response(status, &resp_headers, resp_bytes),
        None => error_response(
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

async fn forward_upstream(
    client: &reqwest::Client,
    method: &Method,
    url: &str,
    req_headers: &[(String, String)],
    body: Bytes,
    shared: &SharedState,
) -> Result<(u16, Vec<(String, String)>, Bytes)> {
    let mut headers = http::HeaderMap::new();
    for (name, value) in req_headers {
        let lname = name.to_lowercase();
        if is_hop_by_hop(&lname) || lname == "host" || lname == "content-length" {
            continue;
        }
        if shared.anticache && (lname == "if-none-match" || lname == "if-modified-since") {
            continue;
        }
        if shared.anticomp && lname == "accept-encoding" {
            continue;
        }
        if let (Ok(hn), Ok(hv)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            headers.insert(hn, hv);
        }
    }
    if shared.anticomp {
        headers.insert(
            http::header::ACCEPT_ENCODING,
            http::HeaderValue::from_static("identity"),
        );
    }

    let mut rb = client.request(method.clone(), url).headers(headers);
    if !body.is_empty() {
        rb = rb.body(body.to_vec());
    }
    let resp = rb.send().await.map_err(|e| anyhow!("upstream: {e}"))?;
    let status = resp.status().as_u16();
    let headers = header_pairs(resp.headers());
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| anyhow!("upstream body: {e}"))?;
    Ok((status, headers, bytes))
}

fn build_client_response(
    status: u16,
    headers: &[(String, String)],
    body: Bytes,
) -> Response<Full<Bytes>> {
    let mut builder = Response::builder().status(status);
    for (name, value) in headers {
        let lname = name.to_lowercase();
        // hyper sets framing headers from the body; copying them corrupts it.
        if is_hop_by_hop(&lname) || lname == "content-length" || lname == "transfer-encoding" {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder
        .body(Full::new(body))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}

fn error_response(status: StatusCode, msg: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(format!("shadowdroid proxy: {msg}"))))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}

struct FlowParts<'a> {
    id: &'a str,
    method: &'a str,
    scheme: &'a str,
    host: &'a str,
    path: &'a str,
    req_headers: &'a [(String, String)],
    req_bytes: &'a [u8],
    status: Option<u16>,
    resp_headers: &'a [(String, String)],
    resp_bytes: &'a [u8],
    dur_ms: u64,
    error: Option<String>,
    matched: Option<String>,
    modified: bool,
}

fn make_flow(p: FlowParts<'_>) -> FlowRecord {
    let req_type = flow::content_type(p.req_headers);
    let resp_type = flow::content_type(p.resp_headers);
    let (req_body, req_truncated) =
        flow::body_to_text(req_type.as_deref(), p.req_bytes, flow::BODY_CAP);
    let (resp_body, resp_truncated) =
        flow::body_to_text(resp_type.as_deref(), p.resp_bytes, flow::BODY_CAP);
    FlowRecord {
        id: p.id.to_string(),
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
        req_len: p.req_bytes.len() as u64,
        resp_len: p.resp_bytes.len() as u64,
        req_body,
        resp_body,
        req_truncated,
        resp_truncated,
        matched: p.matched,
        modified: p.modified,
        error: p.error,
    }
}

/// Build the final flow record and push it to the daemon (store + broadcast).
fn capture(ctx: &ProxyContext, parts: FlowParts<'_>) {
    let _ = ctx.flow_tx.send(make_flow(parts));
}

// ── interception ──────────────────────────────────────────────

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
    ctx.shared.held.lock().unwrap().insert(
        id.clone(),
        HeldFlow {
            tx,
            meta: snap.clone(),
        },
    );
    let _ = ctx
        .shared
        .events
        .send(Arc::new(intercept_event(&snap, phase, cfg.hold_ms)));

    let decision = resolve_held(
        &ctx.shared.held,
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
/// `held.remove` is the **atomic claim**: for a given id, exactly one of {a
/// release, this deadline} removes the entry and decides the flow. `rx` is kept
/// alive across the deadline via `select!` rather than a plain
/// `tokio::time::timeout` (which holds — then drops — `rx` for the whole match):
/// a release landing in the window between the deadline firing and the fail-open
/// decision used to `send` successfully (so `net resume` reported `released:true`)
/// while the device got fail-open. Now the claim decides the single winner, and
/// a release that wins the claim after the deadline still delivers via the
/// still-open `rx`. `fail_open` is evaluated only on a genuine timeout / dropped
/// sender.
pub(crate) async fn resolve_held(
    held: &Mutex<HashMap<String, HeldFlow>>,
    id: &str,
    mut rx: oneshot::Receiver<HoldDecision>,
    deadline: Duration,
    fail_open: impl Fn() -> HoldDecision,
) -> HoldDecision {
    tokio::select! {
        biased;
        r = &mut rx => r.unwrap_or_else(|_| fail_open()),
        _ = tokio::time::sleep(deadline) => {
            if held.lock().unwrap().remove(id).is_some() {
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
/// [`resolve_held`]'s claim). Returns whether a held flow with that id was
/// present **and** its receiver still alive — i.e. whether the agent's decision
/// actually reached the proxy, so `net resume` never reports a delivery that
/// didn't happen.
pub(crate) fn release_held(
    held: &Mutex<HashMap<String, HeldFlow>>,
    id: &str,
    decision: HoldDecision,
) -> bool {
    match held.lock().unwrap().remove(id) {
        Some(h) => h.tx.send(decision).is_ok(),
        None => false,
    }
}

fn intercept_event(snap: &FlowRecord, phase: &str, hold_ms: u32) -> Event {
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
) {
    if let Some(s) = m.set_status {
        *status = Some(s);
    }
    apply_header_mutations(headers, m);
    apply_body_mutation(body, m);
}

fn apply_header_mutations(headers: &mut Vec<(String, String)>, m: &Mutation) {
    for name in &m.remove_headers {
        headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
    }
    for (name, value) in &m.set_headers {
        if let Some(slot) = headers
            .iter_mut()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
        {
            slot.1 = value.clone();
        } else {
            headers.push((name.clone(), value.clone()));
        }
    }
}

fn apply_body_mutation(body: &mut Bytes, m: &Mutation) {
    if let Some(b) = &m.body {
        *body = Bytes::from(b.clone());
        return;
    }
    if let Some((re, repl)) = &m.replace {
        if let Ok(rx) = regex::Regex::new(re) {
            let text = String::from_utf8_lossy(body);
            let new = rx.replace_all(&text, repl.as_str()).into_owned();
            *body = Bytes::from(new.into_bytes());
        }
    }
}

fn drop_response(status: Option<u16>) -> Response<Full<Bytes>> {
    match status {
        Some(s) => build_client_response(s, &[], Bytes::new()),
        None => error_response(StatusCode::BAD_GATEWAY, "dropped by net intercept"),
    }
}

// ── declarative rules (P3) ────────────────────────────────────

/// Outcome of the request-phase rules: an optional short-circuit response
/// (block / map-local), an accumulated delay, and whether anything changed.
struct ReqRules {
    short_circuit: Option<(u16, Vec<(String, String)>, Bytes)>,
    delay_ms: u32,
    modified: bool,
}

fn rule_matches(spec: &RuleSpec, method: &str, host: &str, path: &str, ct: Option<&str>) -> bool {
    let m = &spec.matcher;
    let sub = |hay: &str, n: &Option<String>| {
        n.as_deref()
            .map(|x| hay.to_lowercase().contains(&x.to_lowercase()))
            .unwrap_or(true)
    };
    sub(host, &m.host)
        && sub(path, &m.path)
        && sub(method, &m.method)
        && spec
            .content_type
            .as_deref()
            .map(|want| ct.map(|c| c.contains(want)).unwrap_or(false))
            .unwrap_or(true)
}

fn apply_request_rules(
    shared: &SharedState,
    method: &str,
    host: &str,
    path: &str,
    url: &mut String,
) -> ReqRules {
    let rules = shared.rules.read().unwrap();
    let mut out = ReqRules {
        short_circuit: None,
        delay_ms: 0,
        modified: false,
    };
    for (_, spec) in rules.iter() {
        if !rule_matches(spec, method, host, path, None) {
            continue;
        }
        match spec.kind.as_str() {
            "block" => {
                let status = spec
                    .args
                    .first()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(444);
                out.short_circuit = Some((status, Vec::new(), Bytes::new()));
                out.modified = true;
                return out;
            }
            "map-local" => {
                if let Some(p) = spec.args.first() {
                    if let Ok(bytes) = std::fs::read(p) {
                        out.short_circuit = Some((
                            200,
                            vec![("content-type".into(), guess_content_type(p))],
                            Bytes::from(bytes),
                        ));
                        out.modified = true;
                        return out;
                    }
                }
            }
            "map-remote" => {
                if let Some(repl) = spec.args.first() {
                    rewrite_url(url, repl);
                    out.modified = true;
                }
            }
            "delay" => {
                if let Some(ms) = spec.args.first().and_then(|s| s.parse::<u32>().ok()) {
                    out.delay_ms = out.delay_ms.max(ms);
                }
            }
            _ => {}
        }
    }
    out
}

fn apply_response_rules(
    shared: &SharedState,
    method: &str,
    host: &str,
    path: &str,
    status: &mut Option<u16>,
    headers: &mut Vec<(String, String)>,
    body: &mut Bytes,
) -> bool {
    let rules = shared.rules.read().unwrap();
    let ct = flow::content_type(headers);
    let mut modified = false;
    for (_, spec) in rules.iter() {
        if !rule_matches(spec, method, host, path, ct.as_deref()) {
            continue;
        }
        match spec.kind.as_str() {
            "set-status" => {
                if let Some(c) = spec.args.first().and_then(|s| s.parse().ok()) {
                    *status = Some(c);
                    modified = true;
                }
            }
            "set-header" => {
                if let (Some(n), Some(v)) = (spec.args.first(), spec.args.get(1)) {
                    set_header_vec(headers, n, v);
                    modified = true;
                }
            }
            "replace" => {
                if let (Some(re), Some(rp)) = (spec.args.first(), spec.args.get(1)) {
                    if let Ok(rx) = regex::Regex::new(re) {
                        let text = String::from_utf8_lossy(body);
                        let new = rx.replace_all(&text, rp.as_str()).into_owned();
                        if new.as_bytes() != body.as_ref() {
                            *body = Bytes::from(new.into_bytes());
                            modified = true;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    modified
}

/// If replay is loaded and a saved flow matches (method+host+path), return its
/// response triple.
fn replay_lookup(
    shared: &SharedState,
    method: &str,
    host: &str,
    path: &str,
) -> Option<(u16, Vec<(String, String)>, Bytes)> {
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

/// Rewrite the scheme+authority of a URL (keeping the path), or the authority
/// only if `repl` has no scheme. `repl` like `https://localhost:8080`.
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
    if let Some(slot) = headers
        .iter_mut()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
    {
        slot.1 = value.to_string();
    } else {
        headers.push((name.to_string(), value.to_string()));
    }
}

/// Decompress a gzip/deflate body. Returns `None` if not encoded (or on error,
/// leaving the original bytes untouched). `br` (brotli) isn't handled — use
/// `--anticomp` for those servers.
fn decompress(headers: &[(String, String)], body: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    let enc = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-encoding"))
        .map(|(_, v)| v.trim().to_lowercase())?;
    let mut out = Vec::new();
    match enc.as_str() {
        "gzip" | "x-gzip" => flate2::read::GzDecoder::new(body)
            .read_to_end(&mut out)
            .ok()?,
        "deflate" => match flate2::read::ZlibDecoder::new(body).read_to_end(&mut out) {
            Ok(n) => n,
            Err(_) => {
                out.clear();
                flate2::read::DeflateDecoder::new(body)
                    .read_to_end(&mut out)
                    .ok()?
            }
        },
        _ => return None,
    };
    Some(out)
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
            | "te"
            | "trailer"
            | "upgrade"
    )
}

/// Glob host match: `*.example.com` matches the domain + any subdomain; an
/// exact/plain pattern is an exact-or-substring match.
fn host_glob_match(pattern: &str, host: &str) -> bool {
    let p = pattern.to_lowercase();
    let h = host.to_lowercase();
    if let Some(suffix) = p.strip_prefix("*.") {
        h == suffix || h.ends_with(&format!(".{suffix}"))
    } else {
        h == p || h.contains(&p)
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
    use super::{host_glob_match, release_held, resolve_held, HeldFlow, HoldDecision};
    use crate::net::flow::FlowRecord;
    use crate::net::Mutation;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::sync::oneshot;

    fn insert_held(
        held: &Mutex<HashMap<String, HeldFlow>>,
        id: &str,
    ) -> oneshot::Receiver<HoldDecision> {
        let (tx, rx) = oneshot::channel();
        held.lock().unwrap().insert(
            id.to_string(),
            HeldFlow {
                tx,
                meta: FlowRecord::default(),
            },
        );
        rx
    }

    fn fail_open() -> HoldDecision {
        HoldDecision::Resume(Mutation::default())
    }

    /// The core of the held-flow fix: an agent's decision that arrives before the
    /// deadline must reach the proxy — never get silently replaced by fail-open.
    #[tokio::test]
    async fn release_decision_is_delivered_not_dropped() {
        let held = Mutex::new(HashMap::new());
        let rx = insert_held(&held, "f1");
        // Agent releases with a distinctive decision before the (long) deadline.
        assert!(release_held(&held, "f1", HoldDecision::Drop(Some(599))));
        let d = resolve_held(&held, "f1", rx, Duration::from_secs(5), fail_open).await;
        assert!(
            matches!(d, HoldDecision::Drop(Some(599))),
            "the agent's decision must win, not fail-open"
        );
    }

    /// The honest-reporting half: when the deadline wins, it claims the entry, so
    /// a later release reports `false` instead of lying that it was delivered.
    #[tokio::test]
    async fn timeout_claims_the_entry_and_a_late_release_reports_false() {
        let held = Mutex::new(HashMap::new());
        let rx = insert_held(&held, "f1");
        let d = resolve_held(&held, "f1", rx, Duration::from_millis(0), fail_open).await;
        assert!(matches!(d, HoldDecision::Resume(_)), "fail-open on timeout");
        assert!(
            held.lock().unwrap().is_empty(),
            "the deadline removed (claimed) the entry"
        );
        // A release arriving after the timeout finds nothing → must report false.
        assert!(
            !release_held(&held, "f1", HoldDecision::Drop(Some(1))),
            "a release after the deadline must not claim success"
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
    }
}
