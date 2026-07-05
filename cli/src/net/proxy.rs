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
use futures::StreamExt;
use http::uri::{Authority, Scheme};
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, BodyStream, Empty, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context as TaskCtx, Poll};
use std::time::Duration;
use tokio::io::{copy_bidirectional, AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_rustls::TlsAcceptor;

use crate::events::{self, Event};
use crate::net::ca::CertAuthority;
use crate::net::flow::{self, FlowRecord};
use crate::net::{Matcher, Mutation, RuleSpec};

/// The response body we hand the device: either a buffered `Full` (the common
/// case) or a live `StreamBody` for streamed responses — unified as one boxed
/// type. `Unsync` because the streamed variant wraps reqwest's `Send`-but-not-
/// `Sync` byte stream (hyper only needs the response body to be `Send`).
type ProxyBody = UnsyncBoxBody<Bytes, std::io::Error>;

/// Buffer bodies up to this size, then spill to a streamed pass-through. Bounds
/// per-response memory and, with the `text/event-stream` short-circuit, stops an
/// infinite/large response from hanging or OOMing the daemon (issue: #1/#6).
const BUFFER_CAP: usize = 8 * 1024 * 1024;

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
    pub flow_tx: mpsc::UnboundedSender<FlowRecord>,
    pub shared: Arc<SharedState>,
    /// This daemon's device serial — used to persist a `tls_error` to the
    /// session log so `net log` can recall handshake failures.
    pub serial: crate::ids::Serial,
}

/// Runtime-mutable proxy knobs. (Rules land here in P3.)
pub struct SharedState {
    pub anticache: bool,
    pub anticomp: bool,
    /// Redact sensitive headers from captured flows before store/broadcast.
    pub redact: bool,
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
    /// Hosts we've already reported a `tls_error` for, so a client that keeps
    /// retrying a rejected handshake produces one signal, not a flood.
    pub tls_errors_seen: Mutex<HashSet<String>>,
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
/// the app). By default it accepts invalid upstream certs — this is a debugging
/// proxy and dev/staging backends are often self-signed; `verify_upstream`
/// (`net start --verify-upstream`) turns validation back on.
pub fn build_upstream_client(verify_upstream: bool) -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .danger_accept_invalid_certs(!verify_upstream)
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
fn process_connect(ctx: Arc<ProxyContext>, req: Request<Incoming>) -> Response<ProxyBody> {
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
            match TcpStream::connect(authority.as_ref()).await {
                Ok(mut upstream) => {
                    let _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream).await;
                }
                Err(e) => tracing::debug!("blind tunnel connect {authority}: {e}"),
            }
        }
    });

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
        host: host.to_string(),
        reason: tls_failure_reason(err),
    };
    let _ = crate::net::store::append_event(&ctx.serial, &ev);
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

    let (status, resp_headers, upstream_resp) =
        match ws_handshake_upstream(&scheme, &host, port, &path, &req_headers).await {
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
                        ),
                    );
                }
                return error_response(StatusCode::BAD_GATEWAY, &e.to_string());
            }
        };

    if in_scope {
        // The handshake itself is a capturable flow; frames aren't decoded.
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
            },
        );
    }

    if status != 101 {
        // Server declined the upgrade — pass its response straight back.
        let bytes = collect_incoming(upstream_resp).await.unwrap_or_default();
        return build_client_response(status, &resp_headers, bytes);
    }

    let upstream_io = match hyper::upgrade::on(upstream_resp).await {
        Ok(u) => u,
        Err(e) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("upstream ws upgrade: {e}"),
            )
        }
    };
    // Once the device sees our 101 it upgrades; copy bytes both ways until close.
    tokio::spawn(async move {
        match hyper::upgrade::on(&mut req).await {
            Ok(device_io) => {
                let mut a = TokioIo::new(device_io);
                let mut b = TokioIo::new(upstream_io);
                let _ = copy_bidirectional(&mut a, &mut b).await;
            }
            Err(e) => tracing::debug!("device ws upgrade: {e}"),
        }
    });
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
    if scheme == "https" {
        443
    } else {
        80
    }
}

/// Open a client connection to `host:port` (TLS for https), send the handshake,
/// and return `(status, headers, response)` un-upgraded so the caller can upgrade
/// (101) or read the reject body.
async fn ws_handshake_upstream(
    scheme: &str,
    host: &str,
    port: u16,
    path: &str,
    req_headers: &[(String, String)],
) -> Result<(u16, Vec<(String, String)>, Response<Incoming>)> {
    let tcp = TcpStream::connect((host, port))
        .await
        .map_err(|e| anyhow!("connect {host}:{port}: {e}"))?;
    let stream: Box<dyn IoStream> = if scheme == "https" {
        let sni = rustls::pki_types::ServerName::try_from(host.to_string())
            .map_err(|e| anyhow!("bad SNI {host}: {e}"))?;
        Box::new(
            ws_tls_connector()
                .connect(sni, tcp)
                .await
                .map_err(|e| anyhow!("upstream TLS {host}: {e}"))?,
        )
    } else {
        Box::new(tcp)
    };
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| anyhow!("upstream handshake: {e}"))?;
    tokio::spawn(conn.with_upgrades());

    let mut builder = Request::builder().method(Method::GET).uri(path);
    for (name, value) in req_headers {
        if name.eq_ignore_ascii_case("content-length") {
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
    let resp = sender
        .send_request(upstream_req)
        .await
        .map_err(|e| anyhow!("upstream ws request: {e}"))?;
    let status = resp.status().as_u16();
    let headers = header_pairs(resp.headers());
    Ok((status, headers, resp))
}

async fn collect_incoming(resp: Response<Incoming>) -> Result<Bytes> {
    Ok(resp
        .into_body()
        .collect()
        .await
        .map_err(|e| anyhow!("read ws-reject body: {e}"))?
        .to_bytes())
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

/// A permissive TLS client connector for the upstream WebSocket leg — matches the
/// proxy's default upstream posture (accepts self-signed dev backends). Built once.
fn ws_tls_connector() -> tokio_rustls::TlsConnector {
    static CONNECTOR: std::sync::OnceLock<tokio_rustls::TlsConnector> = std::sync::OnceLock::new();
    CONNECTOR
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
        .clone()
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
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                &format!("read request body: {e}"),
            ))
        }
    }

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
                    req_streamed: req_streaming,
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
    //    rewrites the URL; set-request-header mutates request headers; delay
    //    sleeps before forwarding ──
    if in_scope {
        let r = apply_request_rules(
            &ctx.shared,
            method.as_str(),
            &host,
            &path,
            &mut url,
            &mut req_headers,
        );
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
                    req_streamed: req_streaming,
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

    // ── request-phase interception ── (skipped for streamed uploads: no buffered
    //    body to preview or mutate, like a streamed response skips response intercept)
    if in_scope && !req_streaming {
        let snap = make_flow(FlowParts {
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
                            req_streamed: false,
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
    let up_body: Option<reqwest::Body> = if let Some((prefix, rest)) = req_stream.take() {
        let s =
            futures::stream::iter(prefix.into_iter().map(Ok::<Bytes, std::io::Error>)).chain(rest);
        Some(reqwest::Body::wrap_stream(s))
    } else if req_bytes.is_empty() {
        None
    } else {
        Some(reqwest::Body::from(req_bytes.clone()))
    };
    let started = std::time::Instant::now();
    let resp = match send_upstream(
        &ctx.client,
        &method,
        &url,
        &req_headers,
        up_body,
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
                    ),
                );
            }
            return Ok(error_response(StatusCode::BAD_GATEWAY, &e.to_string()));
        }
    };

    let status_code = resp.status().as_u16();
    let mut resp_headers = header_pairs(resp.headers());

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
                    status: Some(status_code),
                    resp_headers: &resp_headers,
                    resp_bytes: &[],
                    dur_ms,
                    error: None,
                    matched: matched.clone(),
                    modified,
                };
                capture_streamed(&ctx, parts, len_hint);
            }
            return Ok(response_with_body(
                status_code,
                &resp_headers,
                streamed_body(prefix, rest),
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
                    ),
                );
            }
            return Ok(error_response(StatusCode::BAD_GATEWAY, &e));
        }
    };
    let dur_ms = started.elapsed().as_millis() as u64;
    let mut status = Some(status_code);
    let error: Option<String> = None;

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

    // ── response-phase rules (P3): set-status / set-response-header / replace ──
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
            req_streamed: req_streaming,
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
                req_streamed: req_streaming,
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

/// Send the (possibly mutated) request upstream and return the response with its
/// headers available — the body is read separately by [`read_body_capped`] so we
/// can decide buffer-vs-stream.
async fn send_upstream(
    client: &reqwest::Client,
    method: &Method,
    url: &str,
    req_headers: &[(String, String)],
    body: Option<reqwest::Body>,
    shared: &SharedState,
) -> Result<reqwest::Response> {
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
    if let Some(b) = body {
        rb = rb.body(b);
    }
    rb.send().await.map_err(|e| anyhow!("upstream: {e}"))
}

/// A byte stream normalised to `io::Result` (reqwest's error mapped away) so the
/// cap loop is decoupled from reqwest and unit-testable.
type ByteStream = Pin<Box<dyn futures::Stream<Item = std::io::Result<Bytes>> + Send>>;

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
        match rest.next().await {
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
    let head = futures::stream::iter(prefix.into_iter().map(Ok::<Bytes, std::io::Error>));
    let frames = head.chain(rest).map(|r| r.map(Frame::data));
    BodyExt::boxed_unsync(StreamBody::new(frames))
}

/// Assemble a response with the given (already-decided) body, copying headers
/// except the framing ones hyper derives from the body itself.
fn response_with_body(
    status: u16,
    headers: &[(String, String)],
    body: ProxyBody,
) -> Response<ProxyBody> {
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
        .body(body)
        .unwrap_or_else(|_| Response::new(full_body(Bytes::new())))
}

fn build_client_response(
    status: u16,
    headers: &[(String, String)],
    body: Bytes,
) -> Response<ProxyBody> {
    response_with_body(status, headers, full_body(body))
}

fn error_response(status: StatusCode, msg: &str) -> Response<ProxyBody> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(full_body(Bytes::from(format!("shadowdroid proxy: {msg}"))))
        .unwrap_or_else(|_| Response::new(full_body(Bytes::new())))
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
        req_truncated,
        resp_truncated,
        matched: p.matched,
        modified: p.modified,
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
    }
}

/// Build the final flow record and push it to the daemon (store + broadcast).
fn capture(ctx: &ProxyContext, parts: FlowParts<'_>) {
    let mut rec = make_flow(parts);
    if ctx.shared.redact {
        redact_headers(&mut rec.req_headers);
        redact_headers(&mut rec.resp_headers);
    }
    let _ = ctx.flow_tx.send(rec);
}

/// Capture a streamed (pass-through) flow: same metadata as [`capture`] but with
/// no body, `streamed:true`, and `resp_len` set from the `content-length` hint
/// (the real streamed length isn't known when the flow is recorded).
fn capture_streamed(ctx: &ProxyContext, parts: FlowParts<'_>, len_hint: Option<u64>) {
    let mut rec = make_flow(parts);
    rec.streamed = true;
    rec.resp_body = None;
    rec.resp_len = len_hint.unwrap_or(rec.resp_len);
    if ctx.shared.redact {
        redact_headers(&mut rec.req_headers);
        redact_headers(&mut rec.resp_headers);
    }
    let _ = ctx.flow_tx.send(rec);
}

/// Headers whose values are replaced with a placeholder when `--redact` is on.
const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
];

/// Replace the values of [`SENSITIVE_HEADERS`] in place (bodies are not touched —
/// redaction is a best-effort guard, not a guarantee that no secret is logged).
fn redact_headers(headers: &mut [(String, String)]) {
    for (name, value) in headers.iter_mut() {
        if SENSITIVE_HEADERS
            .iter()
            .any(|h| name.eq_ignore_ascii_case(h))
        {
            *value = "<redacted>".to_string();
        }
    }
}

// ── interception ──────────────────────────────────────────────

/// Max concurrently-held flows before `hold` fails open (see [`hold`]).
const MAX_HELD_FLOWS: usize = 128;

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
    {
        // Cap concurrent holds: each pins a FlowRecord (with bodies) until acted
        // on or its deadline. An app hammering a matched endpoint faster than the
        // agent can resume would otherwise grow this map without bound, so past
        // the cap we fail open — let the flow through unheld rather than OOM.
        let mut held = ctx.shared.held.lock().unwrap();
        if held.len() >= MAX_HELD_FLOWS {
            tracing::warn!(
                "net intercept: {MAX_HELD_FLOWS} flows already held; letting {id} through unheld"
            );
            return None;
        }
        held.insert(
            id.clone(),
            HeldFlow {
                tx,
                meta: snap.clone(),
            },
        );
    }
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

fn drop_response(status: Option<u16>) -> Response<ProxyBody> {
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
    headers: &mut Vec<(String, String)>,
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
            "set-request-header" => {
                if let (Some(n), Some(v)) = (spec.args.first(), spec.args.get(1)) {
                    set_header_vec(headers, n, v);
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
            "set-response-header" => {
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
    if let Some(slot) = headers
        .iter_mut()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
    {
        slot.1 = value.to_string();
    } else {
        headers.push((name.to_string(), value.to_string()));
    }
}

/// Decompress a `content-encoding` body so capture, rules, and intercept all see
/// plain bytes. Handles gzip, deflate, `br` (brotli), and `zstd` — the encodings
/// clients actually negotiate (OkHttp → gzip; WebViews/Cronet/CDNs → br/zstd).
/// Returns `None` if not encoded or on decode error, leaving the original bytes
/// untouched (the client still gets a consistent compressed body since
/// `content-encoding` is only stripped when this returns `Some`).
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
        "br" => brotli::Decompressor::new(body, 4096)
            .read_to_end(&mut out)
            .ok()?,
        "zstd" => ruzstd::decoding::StreamingDecoder::new(body)
            .ok()?
            .read_to_end(&mut out)
            .ok()?,
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
        decompress, host_glob_match, release_held, resolve_held, tls_failure_reason, HeldFlow,
        HoldDecision,
    };
    use crate::net::flow::FlowRecord;
    use crate::net::Mutation;
    use std::collections::HashMap;
    use std::io;
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::sync::oneshot;

    #[test]
    fn decompress_round_trips_every_supported_encoding() {
        use std::io::Write;
        let plain = b"{\"hello\":\"world\",\"items\":[1,2,3]}".repeat(20);
        let ce = |v: &str| vec![("Content-Encoding".to_string(), v.to_string())];

        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&plain).unwrap();
        assert_eq!(
            decompress(&ce("gzip"), &gz.finish().unwrap()).unwrap(),
            plain
        );

        let mut br = Vec::new();
        {
            let mut w = brotli::CompressorWriter::new(&mut br, 4096, 5, 22);
            w.write_all(&plain).unwrap();
        }
        assert_eq!(decompress(&ce("br"), &br).unwrap(), plain);

        let zs = ruzstd::encoding::compress_to_vec(
            &plain[..],
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        assert_eq!(decompress(&ce("zstd"), &zs).unwrap(), plain);

        // No / unknown / identity encoding → leave bytes untouched.
        assert!(decompress(&[], &plain).is_none());
        assert!(decompress(&ce("identity"), &plain).is_none());
        // Corrupt br payload → None (fall back to passthrough), never a panic.
        assert!(decompress(&ce("br"), b"not really brotli").is_none());
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
        use super::{read_stream_capped, BodyRead, ByteStream};
        use bytes::Bytes;
        let chunks = |parts: Vec<&'static [u8]>| -> ByteStream {
            Box::pin(futures::stream::iter(
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
        let erroring: ByteStream = Box::pin(futures::stream::iter(vec![
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
        super::redact_headers(&mut headers);
        assert_eq!(headers[0].1, "<redacted>"); // Authorization (case-insensitive)
        assert_eq!(headers[1].1, "application/json"); // untouched
        assert_eq!(headers[2].1, "<redacted>"); // set-cookie
        assert_eq!(headers[3].1, "keep-me"); // untouched
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

        // A domain-shaped pattern matches its subdomains at a label boundary…
        assert!(host_glob_match("livd.app", "api.livd.app"));
        assert!(host_glob_match("example.com", "example.com"));
        // …but NOT a longer domain that merely contains it (the over-capture bug).
        assert!(!host_glob_match("example.com", "example.com.evil.com"));
        assert!(!host_glob_match("livd.app", "notlivd.app"));
    }
}
