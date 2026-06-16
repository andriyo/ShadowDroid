//! Captured-flow model + conversion to the `http` event / `net show` detail.
//!
//! A [FlowRecord] is the full record persisted to the session log (headers +
//! textual bodies, capped). The live `net watch` stream and `net log` emit the
//! *compact* [crate::events::Event::Http] derived from it; `net show` returns
//! the full detail. Binary bodies are not stored verbatim (kept lean + readable)
//! — only textual content (JSON/text/xml/form) is captured, up to [BODY_CAP].

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::events::Event;
use crate::net::Matcher;

/// Max bytes of a textual body stored in the session log (per direction).
pub const BODY_CAP: usize = 64 * 1024;
/// Max bytes of body shown inline in an `http_intercept` preview.
pub const PREVIEW_CAP: usize = 512;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowRecord {
    pub id: String,
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
    pub req_truncated: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub resp_truncated: bool,
    /// The rule/intercept tag that touched this flow, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub modified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
    pub fn http_event(&self) -> Event {
        Event::Http {
            ts: self.ts,
            id: self.id.clone(),
            method: self.method.clone(),
            scheme: self.scheme.clone(),
            host: self.host.clone(),
            path: self.path.clone(),
            status: self.status,
            ok: self.ok(),
            dur_ms: self.dur_ms,
            req_type: self.req_type.clone(),
            req_len: self.req_len,
            resp_type: self.resp_type.clone(),
            resp_len: self.resp_len,
            matched: self.matched.clone(),
            modified: self.modified,
            error: self.error.clone(),
        }
    }

    /// Full detail for `net show`. `body=false` omits the (possibly large) bodies.
    pub fn detail(&self, body: bool) -> serde_json::Value {
        let mut v = serde_json::json!({
            "type": "flow",
            "id": self.id,
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
            "matched": self.matched,
            "modified": self.modified,
            "error": self.error,
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
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    format!("f{:x}", COUNTER.fetch_add(1, Ordering::Relaxed))
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
        None => true, // unknown — small bodies are usually text; the cap protects us
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
/// `(text, truncated)`; `text` is `None` for binary or empty bodies.
pub fn body_to_text(content_type: Option<&str>, bytes: &[u8], cap: usize) -> (Option<String>, bool) {
    if bytes.is_empty() || !is_textual(content_type) {
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

fn headers_to_map(headers: &[(String, String)]) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for (k, v) in headers {
        m.insert(k.clone(), serde_json::Value::String(v.clone()));
    }
    serde_json::Value::Object(m)
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
    fn matcher_and_content_type() {
        let mut rec = FlowRecord {
            id: "f1".into(),
            ts: 0.0,
            method: "POST".into(),
            scheme: "https".into(),
            host: "api.livd.app".into(),
            path: "/v1/login".into(),
            status: Some(401),
            dur_ms: Some(10),
            req_headers: vec![("Content-Type".into(), "application/json; charset=utf-8".into())],
            resp_headers: vec![],
            req_type: None,
            resp_type: None,
            req_len: 0,
            resp_len: 0,
            req_body: None,
            resp_body: None,
            req_truncated: false,
            resp_truncated: false,
            matched: None,
            modified: false,
            error: None,
        };
        assert_eq!(content_type(&rec.req_headers).as_deref(), Some("application/json"));
        assert!(!rec.ok());
        assert!(rec.matches(&Matcher { host: Some("livd".into()), ..Default::default() }));
        assert!(rec.matches(&Matcher { status: Some(401), ..Default::default() }));
        assert!(!rec.matches(&Matcher { status: Some(200), ..Default::default() }));
        assert!(!rec.matches(&Matcher { method: Some("GET".into()), ..Default::default() }));
        rec.status = Some(204);
        assert!(rec.ok());
    }
}
