//! Interop export for captured flows: `curl` commands (hand a repro to a human)
//! and HAR 1.2 (load into browser devtools / Charles / Proxyman).

use serde_json::{json, Value};

use crate::net::flow::FlowRecord;

/// A runnable `curl` command reproducing the request (textual body only).
pub fn curl_command(f: &FlowRecord) -> String {
    let url = format!("{}://{}{}", f.scheme, f.host, f.path);
    let mut parts = vec![format!("curl -X {} '{}'", f.method, sh(&url))];
    for (k, v) in &f.req_headers {
        if k.eq_ignore_ascii_case("content-length") || k.eq_ignore_ascii_case("host") {
            continue;
        }
        parts.push(format!("-H '{}: {}'", sh(k), sh(v)));
    }
    if let Some(body) = &f.req_body {
        parts.push(format!("--data '{}'", sh(body)));
    }
    parts.join(" \\\n  ")
}

/// HAR 1.2 archive for a set of flows.
pub fn to_har(flows: &[FlowRecord]) -> Value {
    let entries: Vec<Value> = flows.iter().map(har_entry).collect();
    json!({
        "log": {
            "version": "1.2",
            "creator": {"name": "shadowdroid", "version": env!("CARGO_PKG_VERSION")},
            "entries": entries,
        }
    })
}

fn har_entry(f: &FlowRecord) -> Value {
    let url = format!("{}://{}{}", f.scheme, f.host, f.path);
    let mut request = json!({
        "method": f.method,
        "url": url,
        "httpVersion": "HTTP/1.1",
        "headers": har_headers(&f.req_headers),
        "queryString": [],
        "cookies": [],
        "headersSize": -1,
        "bodySize": f.req_len,
    });
    if let Some(body) = &f.req_body {
        request["postData"] = json!({
            "mimeType": f.req_type.clone().unwrap_or_default(),
            "text": body,
        });
    }
    json!({
        "startedDateTime": iso8601(f.ts),
        "time": f.dur_ms.unwrap_or(0),
        "request": request,
        "response": {
            "status": f.status.unwrap_or(0),
            "statusText": "",
            "httpVersion": "HTTP/1.1",
            "headers": har_headers(&f.resp_headers),
            "cookies": [],
            "content": {
                "size": f.resp_len,
                "mimeType": f.resp_type.clone().unwrap_or_default(),
                "text": f.resp_body.clone().unwrap_or_default(),
            },
            "redirectURL": "",
            "headersSize": -1,
            "bodySize": f.resp_len,
        },
        "cache": {},
        "timings": {"send": 0, "wait": f.dur_ms.unwrap_or(0), "receive": 0},
    })
}

fn har_headers(h: &[(String, String)]) -> Value {
    Value::Array(h.iter().map(|(k, v)| json!({"name": k, "value": v})).collect())
}

/// Single-quote-escape for a POSIX shell.
fn sh(s: &str) -> String {
    s.replace('\'', "'\\''")
}

/// Format a Unix timestamp (seconds, fractional) as ISO-8601 UTC. Dependency-free
/// proleptic-Gregorian conversion (Howard Hinnant's `civil_from_days`).
fn iso8601(ts: f64) -> String {
    let secs = ts as i64;
    let millis = (((ts - secs as f64) * 1000.0).round() as i64).clamp(0, 999);
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{millis:03}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_known_epochs() {
        assert_eq!(iso8601(0.0), "1970-01-01T00:00:00.000Z");
        // 2021-01-01T00:00:00Z = 1609459200
        assert_eq!(iso8601(1_609_459_200.0), "2021-01-01T00:00:00.000Z");
    }

    #[test]
    fn curl_has_method_url_headers() {
        let mut f = sample();
        f.req_headers = vec![("Accept".into(), "application/json".into())];
        let c = curl_command(&f);
        assert!(c.contains("curl -X GET 'https://api.example.com/v1/me'"));
        assert!(c.contains("-H 'Accept: application/json'"));
    }

    #[test]
    fn har_shape() {
        let har = to_har(&[sample()]);
        assert_eq!(har["log"]["version"], "1.2");
        assert_eq!(har["log"]["entries"][0]["response"]["status"], 200);
    }

    fn sample() -> FlowRecord {
        FlowRecord {
            id: "f1".into(),
            ts: 1_609_459_200.0,
            method: "GET".into(),
            scheme: "https".into(),
            host: "api.example.com".into(),
            path: "/v1/me".into(),
            status: Some(200),
            dur_ms: Some(12),
            req_headers: vec![],
            resp_headers: vec![],
            req_type: None,
            resp_type: Some("application/json".into()),
            req_len: 0,
            resp_len: 2,
            req_body: None,
            resp_body: Some("{}".into()),
            req_truncated: false,
            resp_truncated: false,
            matched: None,
            modified: false,
            error: None,
        }
    }
}
