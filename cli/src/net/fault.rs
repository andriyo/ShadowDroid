//! Faults the proxy injects into traffic (`shadowdroid fault inject
//! http-errors|http-latency|bandwidth|connection-reset|truncated-response|
//! tls-failure`).
//!
//! Which requests a fault hits is decided from its seed and a per-fault
//! request counter, never from ambient randomness: the same seed hits the
//! same requests in the same order, so a failure can be replayed.

use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum NetFaultEffect {
    /// Answer with this status without reaching the server.
    ErrorStatus {
        status: u16,
    },
    Latency {
        delay_ms: u32,
        jitter_ms: u32,
    },
    Bandwidth {
        bytes_per_sec: u32,
    },
    /// Deliver this many body bytes, then break the connection.
    ConnectionReset {
        after_bytes: u32,
    },
    /// Deliver this many body bytes as the complete body.
    Truncate {
        keep_bytes: u32,
    },
    /// Fail the TLS handshake of a CONNECT to a matching host.
    TlsFailure,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetFaultSpec {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub percent: u8,
    pub seed: u64,
    pub effect: NetFaultEffect,
}

impl NetFaultSpec {
    pub fn validate(&self) -> Result<(), String> {
        if self.id.is_empty() || self.id.len() > 64 {
            return Err("fault id must be 1-64 characters".into());
        }
        if !(1..=100).contains(&self.percent) {
            return Err("percent must be 1-100".into());
        }
        match &self.effect {
            NetFaultEffect::ErrorStatus { status } if !(400..=599).contains(status) => {
                Err("error status must be 400-599".into())
            }
            NetFaultEffect::Bandwidth { bytes_per_sec } if *bytes_per_sec < 100 => {
                Err("bandwidth must be at least 100 bytes/s".into())
            }
            NetFaultEffect::TlsFailure if self.host.as_deref().unwrap_or("").is_empty() => {
                Err("tls-failure needs a host".into())
            }
            _ => Ok(()),
        }
    }
}

/// A fault installed in a running proxy.
#[derive(Debug)]
pub struct ActiveNetFault {
    pub spec: NetFaultSpec,
    seen: AtomicU64,
    hits: AtomicU64,
}

/// What a request drew from one fault.
#[derive(Debug, Clone, Copy)]
pub struct Draw {
    /// The request's position in this fault's sequence.
    pub n: u64,
}

fn mix(seed: u64, n: u64, salt: u64) -> u64 {
    let mut bytes = [0u8; 24];
    bytes[..8].copy_from_slice(&seed.to_le_bytes());
    bytes[8..16].copy_from_slice(&n.to_le_bytes());
    bytes[16..].copy_from_slice(&salt.to_le_bytes());
    let hash = blake3::hash(&bytes);
    u64::from_le_bytes(hash.as_bytes()[..8].try_into().expect("8 bytes"))
}

impl ActiveNetFault {
    pub fn new(spec: NetFaultSpec) -> Self {
        Self {
            spec,
            seen: AtomicU64::new(0),
            hits: AtomicU64::new(0),
        }
    }

    fn scope_matches(&self, host: &str, path: &str) -> bool {
        let contains = |hay: &str, needle: &Option<String>| {
            needle.as_deref().is_none_or(|needle| {
                hay.to_ascii_lowercase()
                    .contains(&needle.to_ascii_lowercase())
            })
        };
        contains(host, &self.spec.host) && contains(path, &self.spec.path)
    }

    /// Whether this request is hit. Every matching request advances the
    /// sequence, hit or not, so hits depend only on the seed and order.
    pub fn draw(&self, host: &str, path: &str) -> Option<Draw> {
        if !self.scope_matches(host, path) {
            return None;
        }
        let n = self.seen.fetch_add(1, Ordering::Relaxed);
        let hit = self.spec.percent >= 100
            || mix(self.spec.seed, n, 0) % 100 < u64::from(self.spec.percent);
        if hit {
            self.hits.fetch_add(1, Ordering::Relaxed);
            Some(Draw { n })
        } else {
            None
        }
    }

    /// Extra delay for a latency fault's draw.
    pub fn jitter_ms(&self, draw: Draw) -> u64 {
        match self.spec.effect {
            NetFaultEffect::Latency { jitter_ms, .. } if jitter_ms > 0 => {
                mix(self.spec.seed, draw.n, 1) % (u64::from(jitter_ms) + 1)
            }
            _ => 0,
        }
    }

    pub fn status(&self) -> Value {
        json!({
            "spec": self.spec,
            "matching_requests": self.seen.load(Ordering::Relaxed),
            "hits": self.hits.load(Ordering::Relaxed),
        })
    }
}

/// How a response body is sabotaged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyFault {
    Bandwidth(u32),
    Reset(u32),
    Truncate(u32),
}

/// What the faults decided for one request.
#[derive(Debug, Default)]
pub struct RequestFaults {
    pub ids: Vec<String>,
    pub delay_ms: u64,
    pub error_status: Option<u16>,
    pub body: Option<BodyFault>,
}

/// Evaluate every HTTP fault for a request, in installation order. Latencies
/// add up; the first error status and the first body fault win.
pub fn for_request(
    faults: &[std::sync::Arc<ActiveNetFault>],
    host: &str,
    path: &str,
) -> RequestFaults {
    let mut out = RequestFaults::default();
    for fault in faults {
        if matches!(fault.spec.effect, NetFaultEffect::TlsFailure) {
            continue;
        }
        let Some(draw) = fault.draw(host, path) else {
            continue;
        };
        let applied = match fault.spec.effect {
            NetFaultEffect::Latency { delay_ms, .. } => {
                out.delay_ms += u64::from(delay_ms) + fault.jitter_ms(draw);
                true
            }
            NetFaultEffect::ErrorStatus { status } if out.error_status.is_none() => {
                out.error_status = Some(status);
                true
            }
            NetFaultEffect::Bandwidth { bytes_per_sec } if out.body.is_none() => {
                out.body = Some(BodyFault::Bandwidth(bytes_per_sec));
                true
            }
            NetFaultEffect::ConnectionReset { after_bytes } if out.body.is_none() => {
                out.body = Some(BodyFault::Reset(after_bytes));
                true
            }
            NetFaultEffect::Truncate { keep_bytes } if out.body.is_none() => {
                out.body = Some(BodyFault::Truncate(keep_bytes));
                true
            }
            _ => false,
        };
        if applied {
            out.ids.push(fault.spec.id.clone());
        }
    }
    out
}

/// The TLS-failure fault that hits a CONNECT to `host`, if any.
pub fn tls_failure_for(faults: &[std::sync::Arc<ActiveNetFault>], host: &str) -> Option<String> {
    faults
        .iter()
        .filter(|fault| matches!(fault.spec.effect, NetFaultEffect::TlsFailure))
        .find_map(|fault| fault.draw(host, "").map(|_| fault.spec.id.clone()))
}

/// A fatal TLS `handshake_failure` alert record, sent before closing.
pub const TLS_HANDSHAKE_FAILURE_ALERT: [u8; 7] = [0x15, 0x03, 0x03, 0x00, 0x02, 0x02, 0x28];

type Body = http_body_util::combinators::UnsyncBoxBody<Bytes, std::io::Error>;

/// Wrap a response body with a body fault. Returns the body and the length
/// to advertise: unchanged for bandwidth and reset (a reset breaks the
/// connection before that length arrives), the kept length for truncation
/// (`None` there when the full length is unknown: sent chunked).
pub fn sabotage(body: Body, fault: BodyFault, known_len: Option<u64>) -> (Body, Option<u64>) {
    let data = body.into_data_stream();
    match fault {
        BodyFault::Bandwidth(bytes_per_sec) => {
            // Deliver in tenth-of-a-second slices so the rate is smooth.
            let slice = (bytes_per_sec as usize / 10).max(1);
            let stream = data
                .flat_map(move |chunk| {
                    let pieces: Vec<Result<Bytes, std::io::Error>> = match chunk {
                        Ok(bytes) => (0..bytes.len())
                            .step_by(slice)
                            .map(|at| Ok(bytes.slice(at..(at + slice).min(bytes.len()))))
                            .collect(),
                        Err(error) => vec![Err(error)],
                    };
                    futures_util::stream::iter(pieces)
                })
                .then(move |piece| async move {
                    if let Ok(bytes) = &piece {
                        let millis = bytes.len() as u64 * 1000 / u64::from(bytes_per_sec.max(1));
                        tokio::time::sleep(Duration::from_millis(millis)).await;
                    }
                    piece
                })
                .map(|piece| piece.map(Frame::data));
            (BodyExt::boxed_unsync(StreamBody::new(stream)), known_len)
        }
        BodyFault::Reset(after_bytes) => {
            let stream =
                limit(data, u64::from(after_bytes)).chain(futures_util::stream::once(async {
                    // Let the server flush the headers and kept bytes first, or
                    // the app sees the connection close before any response.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "shadowdroid fault: connection reset",
                    ))
                }));
            (
                BodyExt::boxed_unsync(StreamBody::new(stream.map(|piece| piece.map(Frame::data)))),
                known_len,
            )
        }
        BodyFault::Truncate(keep_bytes) => {
            let stream = limit(data, u64::from(keep_bytes)).map(|piece| piece.map(Frame::data));
            (
                BodyExt::boxed_unsync(StreamBody::new(stream)),
                known_len.map(|len| len.min(u64::from(keep_bytes))),
            )
        }
    }
}

/// The first `max` bytes of a data stream.
fn limit(
    data: impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
    max: u64,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
    futures_util::stream::unfold((Box::pin(data), max), |(mut data, left)| async move {
        if left == 0 {
            return None;
        }
        match data.next().await? {
            Ok(bytes) => {
                let take = (bytes.len() as u64).min(left) as usize;
                Some((Ok(bytes.slice(..take)), (data, left - take as u64)))
            }
            Err(error) => Some((Err(error), (data, 0))),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn fault(percent: u8, effect: NetFaultEffect) -> Arc<ActiveNetFault> {
        Arc::new(ActiveNetFault::new(NetFaultSpec {
            id: "flt_x".into(),
            host: Some("api".into()),
            path: None,
            percent,
            seed: 7,
            effect,
        }))
    }

    #[test]
    fn the_same_seed_hits_the_same_requests() {
        let pattern = |seed: u64| {
            let active = ActiveNetFault::new(NetFaultSpec {
                id: "a".into(),
                host: None,
                path: None,
                percent: 30,
                seed,
                effect: NetFaultEffect::ErrorStatus { status: 503 },
            });
            (0..200)
                .map(|_| active.draw("h", "/").is_some())
                .collect::<Vec<_>>()
        };
        assert_eq!(pattern(1), pattern(1));
        assert_ne!(pattern(1), pattern(2));
        let hits = pattern(1).iter().filter(|hit| **hit).count();
        assert!((30..=90).contains(&hits), "{hits} of 200 at 30%");
    }

    #[test]
    fn requests_combine_latency_and_take_the_first_error_and_body_fault() {
        let faults = vec![
            fault(
                100,
                NetFaultEffect::Latency {
                    delay_ms: 100,
                    jitter_ms: 0,
                },
            ),
            fault(100, NetFaultEffect::ErrorStatus { status: 503 }),
            fault(100, NetFaultEffect::ErrorStatus { status: 500 }),
            fault(100, NetFaultEffect::Truncate { keep_bytes: 4 }),
        ];
        let out = for_request(&faults, "api.example.com", "/x");
        assert_eq!(out.delay_ms, 100);
        assert_eq!(out.error_status, Some(503));
        assert_eq!(out.body, Some(BodyFault::Truncate(4)));
        assert_eq!(out.ids.len(), 3);
        // Scope: other hosts are untouched.
        assert!(for_request(&faults, "cdn.example.com", "/x").ids.is_empty());
        let tls = vec![fault(100, NetFaultEffect::TlsFailure)];
        assert_eq!(
            tls_failure_for(&tls, "api.example.com").as_deref(),
            Some("flt_x")
        );
        assert!(for_request(&tls, "api.example.com", "/").ids.is_empty());
    }

    async fn collect(body: Body) -> (Vec<u8>, bool) {
        let mut stream = body.into_data_stream();
        let mut out = Vec::new();
        while let Some(piece) = stream.next().await {
            match piece {
                Ok(bytes) => out.extend_from_slice(&bytes),
                Err(_) => return (out, true),
            }
        }
        (out, false)
    }

    fn body(text: &'static str) -> Body {
        http_body_util::Full::new(Bytes::from_static(text.as_bytes()))
            .map_err(|never: std::convert::Infallible| match never {})
            .boxed_unsync()
    }

    #[tokio::test]
    async fn body_faults_truncate_break_and_throttle() {
        let (truncated, length) = sabotage(body("hello world"), BodyFault::Truncate(5), Some(11));
        assert_eq!(length, Some(5));
        assert_eq!(collect(truncated).await, (b"hello".to_vec(), false));

        let (reset, length) = sabotage(body("hello world"), BodyFault::Reset(3), Some(11));
        assert_eq!(length, Some(11));
        assert_eq!(collect(reset).await, (b"hel".to_vec(), true));

        let started = std::time::Instant::now();
        let (slow, length) = sabotage(body("0123456789"), BodyFault::Bandwidth(100), None);
        assert_eq!(length, None);
        assert_eq!(collect(slow).await, (b"0123456789".to_vec(), false));
        assert!(started.elapsed() >= Duration::from_millis(90));
    }
}
