//! Captured-flow model + conversion to the `http` event / `net show` detail.
//!
//! A [FlowRecord] is the full record persisted to the session log (headers +
//! textual bodies, capped). The live `watch` stream and `net log` emit the
//! *compact* [crate::events::Event::Http] derived from it; `net show` returns
//! the full detail. Binary bodies are not stored verbatim (kept lean + readable)
//! — only textual content (JSON/text/xml/form) is captured, up to [BODY_CAP].

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::events::Event;
use crate::net::Matcher;

static FLOW_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Max bytes of a textual body stored in the session log (per direction).
/// Generous enough that most JSON/dictionary responses are captured whole;
/// `net show --body-file` writes whatever was stored, and `resp_truncated`
/// flags anything that exceeded this cap.
pub const BODY_CAP: usize = 1024 * 1024;
/// Max bytes of body shown inline in an `http_intercept` preview.
pub const PREVIEW_CAP: usize = 512;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FlowRecord {
    pub id: String,
    #[serde(default)]
    pub flow_sequence: u64,
    #[serde(default)]
    pub capture_session_id: String,
    pub ts: f64,
    pub method: String,
    pub scheme: String,
    pub host: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dur_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub req_headers: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resp_headers: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub req_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resp_type: Option<String>,
    pub req_len: u64,
    pub resp_len: u64,
    /// Textual request body (capped); `None` for binary/empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub req_body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resp_body: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub req_body_redacted: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub resp_body_redacted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redaction_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redaction_policy_version: Option<u32>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub req_truncated: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub resp_truncated: bool,
    /// The rule/intercept tag that touched this flow, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rule_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub modified: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub upstream_bypassed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The response body was streamed through (SSE / oversized), not buffered, so
    /// it isn't captured and response rules/intercept didn't run. `resp_len` is
    /// the `content-length` hint when the server sent one, else 0.
    #[serde(default, skip_serializing_if = "is_false")]
    pub streamed: bool,
    /// The request body was streamed upstream (oversized upload), not buffered, so
    /// it isn't captured and request intercept was skipped. `req_len` is the
    /// `content-length` hint when the client sent one, else 0.
    #[serde(default, skip_serializing_if = "is_false")]
    pub req_streamed: bool,
}

impl FlowRecord {
    pub fn ok(&self) -> bool {
        matches!(self.status, Some(s) if (200..400).contains(&s))
    }

    /// AND over the matcher's present fields (case-insensitive substring for
    /// host/path/method; exact for status). Empty matcher matches all.
    pub fn matches(&self, m: &Matcher) -> bool {
        let sub = |hay: &str, needle: &Option<String>| {
            needle
                .as_deref()
                .map(|n| hay.to_lowercase().contains(&n.to_lowercase()))
                .unwrap_or(true)
        };
        sub(&self.host, &m.host)
            && sub(&self.path, &m.path)
            && sub(&self.method, &m.method)
            && m.status.map(|s| self.status == Some(s)).unwrap_or(true)
    }

    /// The compact streaming event.
    pub fn http_event(&self, serial: &crate::ids::Serial) -> Event {
        Event::Http {
            ts: self.ts,
            id: self.id.clone(),
            flow_sequence: self.flow_sequence,
            capture_session_id: self.capture_session_id.clone(),
            method: self.method.clone(),
            scheme: self.scheme.clone(),
            host: self.host.clone(),
            path: self.path.clone(),
            url: format!("{}://{}{}", self.scheme, self.host, self.path),
            status: self.status,
            ok: self.ok(),
            dur_ms: self.dur_ms,
            req_type: self.req_type.clone(),
            req_len: self.req_len,
            resp_type: self.resp_type.clone(),
            resp_len: self.resp_len,
            matched: self.matched.clone(),
            rule_id: self.rule_id.clone(),
            rule_ids: self.rule_ids.clone(),
            modified: self.modified,
            upstream_bypassed: self.upstream_bypassed,
            error: self.error.clone(),
            streamed: self.streamed,
            req_streamed: self.req_streamed,
            redaction_policy: self.redaction_policy.clone(),
            redaction_policy_version: self.redaction_policy_version,
            body_redacted: self.req_body_redacted || self.resp_body_redacted,
            next_actions: crate::net::flow_next_actions(serial, &self.id),
        }
    }

    /// Full detail for `net show`. `body=false` omits the (possibly large) bodies.
    pub fn detail(&self, body: bool) -> serde_json::Value {
        let mut v = serde_json::json!({
            "type": "flow",
            "id": self.id,
            "flow_sequence": self.flow_sequence,
            "capture_session_id": self.capture_session_id,
            "ts": self.ts,
            "method": self.method,
            "scheme": self.scheme,
            "host": self.host,
            "path": self.path,
            "url": format!("{}://{}{}", self.scheme, self.host, self.path),
            "status": self.status,
            "ok": self.ok(),
            "dur_ms": self.dur_ms,
            "req_headers": headers_to_map(&self.req_headers),
            "resp_headers": headers_to_map(&self.resp_headers),
            "req_type": self.req_type,
            "resp_type": self.resp_type,
            "req_len": self.req_len,
            "resp_len": self.resp_len,
            "req_body_redacted": self.req_body_redacted,
            "resp_body_redacted": self.resp_body_redacted,
            "redaction_policy": self.redaction_policy,
            "redaction_policy_version": self.redaction_policy_version,
            "matched": self.matched,
            "rule_id": self.rule_id,
            "rule_ids": self.rule_ids,
            "modified": self.modified,
            "upstream_bypassed": self.upstream_bypassed,
            "error": self.error,
            "streamed": self.streamed,
            "req_streamed": self.req_streamed,
        });
        if body {
            v["req_body"] = serde_json::json!(self.req_body);
            v["resp_body"] = serde_json::json!(self.resp_body);
            v["req_truncated"] = serde_json::json!(self.req_truncated);
            v["resp_truncated"] = serde_json::json!(self.resp_truncated);
        }
        v
    }
}

/// Short per-flow id (`f1`, `f2`, …), unique within a daemon run.
pub fn new_id() -> String {
    format!("f{:x}", FLOW_COUNTER.fetch_add(1, Ordering::Relaxed))
}

pub fn sequence_from_id(id: &str) -> Option<u64> {
    id.strip_prefix('f')
        .and_then(|value| u64::from_str_radix(value, 16).ok())
}

/// Reserve the next global capture sequence. Shared with WebSocket records
/// ([crate::net::ws]) so a checkpoint boundary — which snapshots
/// [last_sequence] — orders HTTP flows and WS sessions/messages on one timeline.
pub fn next_sequence() -> u64 {
    FLOW_COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Last flow sequence assigned in this daemon process. A checkpoint uses the
/// assignment boundary rather than persistence order, so an older in-flight
/// flow that completes later remains correctly before the checkpoint.
pub fn last_sequence() -> u64 {
    FLOW_COUNTER.load(Ordering::Relaxed).saturating_sub(1)
}

/// The mime portion of a Content-Type header value (drops `; charset=…`).
pub fn content_type(headers: &[(String, String)]) -> Option<String> {
    header_get(headers, "content-type")
        .map(|v| v.split(';').next().unwrap_or("").trim().to_lowercase())
        .filter(|s| !s.is_empty())
}

/// Case-insensitive header lookup (first match).
pub fn header_get<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Is a body with this content-type worth capturing as text? text/*, JSON, XML,
/// JS, form-encoded, and `+json`/`+xml` structured-suffix types.
pub fn is_textual(content_type: Option<&str>) -> bool {
    match content_type {
        None => true, // unknown — give it a chance; body_to_text sniffs for binary
        Some(ct) => {
            ct.starts_with("text/")
                || ct.contains("json")
                || ct.contains("xml")
                || ct.contains("javascript")
                || ct == "application/x-www-form-urlencoded"
                || ct.contains("graphql")
        }
    }
}

/// Reduce a raw body to a capped textual string for storage. Returns
/// `(text, truncated)`; `text` is `None` for binary or empty bodies. A body
/// with no declared content-type is sniffed: a NUL among the first bytes
/// means binary. Declared-text bodies that aren't valid UTF-8 (e.g. Latin-1)
/// are still stored, lossily — mangled text beats nothing when debugging.
pub fn body_to_text(
    content_type: Option<&str>,
    bytes: &[u8],
    cap: usize,
) -> (Option<String>, bool) {
    if bytes.is_empty() || !is_textual(content_type) {
        return (None, false);
    }
    // `is_textual(None)` is benefit-of-the-doubt; don't let an undeclared
    // binary body become a megabyte of U+FFFD in the session log.
    if content_type.is_none() && bytes.iter().take(512).any(|&b| b == 0) {
        return (None, false);
    }
    let truncated = bytes.len() > cap;
    let slice = &bytes[..bytes.len().min(cap)];
    match std::str::from_utf8(slice) {
        Ok(s) => (Some(s.to_string()), truncated),
        // Truncation may have split a multibyte char; fall back to lossy.
        Err(_) => (Some(String::from_utf8_lossy(slice).into_owned()), truncated),
    }
}

/// Headers as a JSON object. A header that appears once maps to a string; a
/// repeated header (case-insensitive — think multiple `Set-Cookie`) collects
/// every value into an array under the first-seen key casing.
fn headers_to_map(headers: &[(String, String)]) -> serde_json::Value {
    use serde_json::Value;
    let mut m = serde_json::Map::new();
    for (k, v) in headers {
        let key = m
            .keys()
            .find(|existing| existing.eq_ignore_ascii_case(k))
            .cloned()
            .unwrap_or_else(|| k.clone());
        let val = Value::String(v.clone());
        match m.get_mut(&key) {
            None => {
                m.insert(key, val);
            }
            Some(Value::Array(arr)) => arr.push(val),
            Some(prev) => {
                let first = prev.take();
                *prev = Value::Array(vec![first, val]);
            }
        }
    }
    Value::Object(m)
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn textual_gate() {
        assert!(is_textual(Some("application/json")));
        assert!(is_textual(Some("text/html")));
        assert!(is_textual(Some("application/vnd.api+json")));
        assert!(is_textual(None));
        assert!(!is_textual(Some("image/png")));
        assert!(!is_textual(Some("application/octet-stream")));
    }

    #[test]
    fn body_text_caps_and_skips_binary() {
        let (t, trunc) = body_to_text(Some("application/json"), b"{\"a\":1}", 1024);
        assert_eq!(t.as_deref(), Some("{\"a\":1}"));
        assert!(!trunc);

        let (t, trunc) = body_to_text(Some("application/json"), b"abcdef", 3);
        assert_eq!(t.as_deref(), Some("abc"));
        assert!(trunc);

        let (t, _) = body_to_text(Some("image/png"), &[0u8, 1, 2], 1024);
        assert!(t.is_none());
    }

    #[test]
    fn unknown_type_sniffs_binary() {
        // No content-type + NUL in the head = binary, not stored.
        let (t, trunc) = body_to_text(None, b"\x00\x01\x02garbage", 1024);
        assert!(t.is_none());
        assert!(!trunc);
        // No content-type but clean text is still captured.
        let (t, _) = body_to_text(None, b"plain text", 1024);
        assert_eq!(t.as_deref(), Some("plain text"));
        // A declared-text body keeps the benefit of the doubt (lossy path).
        let (t, _) = body_to_text(Some("text/html"), b"caf\xe9", 1024);
        assert_eq!(t.as_deref(), Some("caf\u{fffd}"));
    }

    #[test]
    fn repeated_headers_collect_into_array() {
        let headers = vec![
            ("Content-Type".to_string(), "text/html".to_string()),
            ("Set-Cookie".to_string(), "a=1".to_string()),
            ("set-cookie".to_string(), "b=2".to_string()),
            ("Set-Cookie".to_string(), "c=3".to_string()),
        ];
        let m = headers_to_map(&headers);
        assert_eq!(m["Content-Type"], serde_json::json!("text/html"));
        // Case-insensitive merge under the first-seen casing, values in order.
        assert_eq!(m["Set-Cookie"], serde_json::json!(["a=1", "b=2", "c=3"]));
        assert!(m.get("set-cookie").is_none());
    }

    #[test]
    fn matcher_and_content_type() {
        let mut rec = FlowRecord {
            id: "f1".into(),
            flow_sequence: 1,
            capture_session_id: "n-test".into(),
            ts: 0.0,
            method: "POST".into(),
            scheme: "https".into(),
            host: "api.livd.app".into(),
            path: "/v1/login".into(),
            status: Some(401),
            dur_ms: Some(10),
            req_headers: vec![(
                "Content-Type".into(),
                "application/json; charset=utf-8".into(),
            )],
            resp_headers: vec![],
            req_type: None,
            resp_type: None,
            req_len: 0,
            resp_len: 0,
            req_body: None,
            resp_body: None,
            req_body_redacted: false,
            resp_body_redacted: false,
            redaction_policy: None,
            redaction_policy_version: None,
            req_truncated: false,
            resp_truncated: false,
            matched: None,
            rule_id: None,
            rule_ids: vec![],
            modified: false,
            upstream_bypassed: false,
            error: None,
            streamed: false,
            req_streamed: false,
        };
        assert_eq!(
            content_type(&rec.req_headers).as_deref(),
            Some("application/json")
        );
        assert!(!rec.ok());
        assert!(rec.matches(&Matcher {
            host: Some("livd".into()),
            ..Default::default()
        }));
        assert!(rec.matches(&Matcher {
            status: Some(401),
            ..Default::default()
        }));
        assert!(!rec.matches(&Matcher {
            status: Some(200),
            ..Default::default()
        }));
        assert!(!rec.matches(&Matcher {
            method: Some("GET".into()),
            ..Default::default()
        }));
        let event =
            serde_json::to_value(rec.http_event(&crate::ids::Serial::new("device"))).unwrap();
        assert_eq!(event["url"], "https://api.livd.app/v1/login");
        rec.status = Some(204);
        assert!(rec.ok());
    }
}
