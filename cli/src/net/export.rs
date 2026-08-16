//! Interop export for captured flows: `curl` commands (hand a repro to a human),
//! HAR 1.2 (load into browser devtools / Charles / Proxyman), and `fixtures` — a
//! replayable response set + manifest for deterministic instrumentation tests
//! (the toil this removes: hand-authoring record/replay mocks like OkReplay /
//! MockWebServer / WireMock from scratch). GraphQL POSTs are keyed by
//! `operationName` so same-endpoint operations don't collide.

use anyhow::Result;
use serde_json::{Value, json};
use std::path::Path;

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
    build_har(flows.iter().map(har_entry).collect())
}

/// HAR 1.2 archive combining HTTP flows and WebSocket sessions. WebSocket
/// entries carry Chrome/devtools' `_resourceType:"websocket"` +
/// `_webSocketMessages` extension so they load in browser devtools, Proxyman,
/// and Charles.
pub fn to_har_with_ws(flows: &[FlowRecord], sessions: &[crate::net::store::WsHarSession]) -> Value {
    let mut entries: Vec<Value> = flows.iter().map(har_entry).collect();
    entries.extend(sessions.iter().map(ws_har_entry));
    build_har(entries)
}

fn build_har(entries: Vec<Value>) -> Value {
    json!({
        "log": {
            "version": "1.2",
            "creator": {"name": "shadowdroid", "version": env!("CARGO_PKG_VERSION")},
            "entries": entries,
        }
    })
}

/// One HAR entry for a WebSocket session: the upgrade request/response plus the
/// devtools `_webSocketMessages` array (`type: send|receive`, opcode, data, time).
fn ws_har_entry(session: &crate::net::store::WsHarSession) -> Value {
    let open = &session.open;
    let duration = session.close.as_ref().map_or(0, |close| close.dur_ms);
    let messages: Vec<Value> = session
        .messages
        .iter()
        .map(|message| {
            let opcode = match message.opcode.as_str() {
                "text" => 1,
                "binary" => 2,
                "close" => 8,
                "ping" => 9,
                "pong" => 10,
                _ => 1,
            };
            let data = message
                .text
                .clone()
                .or_else(|| message.data_b64.clone())
                .unwrap_or_default();
            json!({
                "type": if message.dir == "c2s" { "send" } else { "receive" },
                "opcode": opcode,
                "data": data,
                "time": message.ts,
            })
        })
        .collect();
    json!({
        "startedDateTime": iso8601(open.ts),
        "time": duration,
        "_resourceType": "websocket",
        "request": {
            "method": "GET",
            "url": open.url(),
            "httpVersion": "HTTP/1.1",
            "headers": har_headers(&open.req_headers),
            "queryString": [],
            "cookies": [],
            "headersSize": -1,
            "bodySize": 0,
        },
        "response": {
            "status": open.status,
            "statusText": if open.status == 101 { "Switching Protocols" } else { "" },
            "httpVersion": "HTTP/1.1",
            "headers": har_headers(&open.resp_headers),
            "cookies": [],
            "content": {"size": 0, "mimeType": "x-unknown"},
            "redirectURL": "",
            "headersSize": -1,
            "bodySize": 0,
        },
        "cache": {},
        "timings": {"send": 0, "wait": duration, "receive": 0},
        "_webSocketMessages": messages,
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
    Value::Array(
        h.iter()
            .map(|(k, v)| json!({"name": k, "value": v}))
            .collect(),
    )
}

// ── fixtures (record/replay for tests) ────────────────────────────────────

/// Extract a GraphQL `operationName` from a request body, if it parses as a JSON
/// object carrying one. This is the key that lets fixtures distinguish multiple
/// operations POSTed to the same endpoint (the exact thing record/replay mocks
/// match on). Returns `None` for non-JSON bodies or absent/blank names.
pub fn graphql_operation_name(req_body: &Option<String>) -> Option<String> {
    let body = req_body.as_deref()?;
    let v: Value = serde_json::from_str(body).ok()?;
    let name = v.get("operationName")?.as_str()?.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Write the versioned, content-addressed replay bundle consumed by
/// `net replay --from`. Pre-port records deliberately fall back to their
/// scheme default; current proxy and AAR captures always carry the exact port.
pub fn write_fixtures(flows: &[FlowRecord], out: &Path) -> Result<Value> {
    let sources = flows
        .iter()
        .map(crate::net::replay::ReplaySource::from_flow_or_default_port)
        .collect::<Result<Vec<_>>>()?;
    let summary = crate::net::replay::write_bundle(&sources, out)?;
    let replay_from = crate::events::shell_token(&out.display().to_string());
    Ok(json!({
        "type": "action",
        "ok": true,
        "cmd": "export",
        "format": "fixtures",
        "out": out.display().to_string(),
        "manifest": summary.manifest.display().to_string(),
        "count": summary.count,
        "response_files": summary.response_files,
        "source_bundle_sha256": summary.source_bundle_sha256,
        "active_set_sha256": summary.active_set_sha256,
        "next_actions": [
            format!("shadowdroid net replay --from {replay_from}"),
        ],
    }))
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

    #[test]
    fn extracts_graphql_operation_name() {
        let body = Some(
            r#"{"operationName":"GetMe","query":"query GetMe {me{id}}","variables":{}}"#.into(),
        );
        assert_eq!(graphql_operation_name(&body).as_deref(), Some("GetMe"));
        assert_eq!(graphql_operation_name(&Some("not json".into())), None);
        assert_eq!(
            graphql_operation_name(&Some(r#"{"query":"{me}"}"#.into())),
            None
        );
        assert_eq!(graphql_operation_name(&None), None);
    }

    fn sample() -> FlowRecord {
        FlowRecord {
            id: "f1".into(),
            flow_sequence: 1,
            capture_session_id: "n-test".into(),
            ts: 1_609_459_200.0,
            method: "GET".into(),
            scheme: "https".into(),
            host: "api.example.com".into(),
            port: Some(443),
            path: "/v1/me".into(),
            host_redacted: false,
            path_redacted: false,
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
            request_body_modified: false,
            upstream_bypassed: false,
            error: None,
            error_redacted: false,
            streamed: false,
            req_streamed: false,
        }
    }
}
