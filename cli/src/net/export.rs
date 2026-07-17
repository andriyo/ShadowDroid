//! Interop export for captured flows: `curl` commands (hand a repro to a human),
//! HAR 1.2 (load into browser devtools / Charles / Proxyman), and `fixtures` — a
//! replayable response set + manifest for deterministic instrumentation tests
//! (the toil this removes: hand-authoring record/replay mocks like OkReplay /
//! MockWebServer / WireMock from scratch). GraphQL POSTs are keyed by
//! `operationName` so same-endpoint operations don't collide.

use anyhow::{Context, Result};
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

/// Sanitize a string into a filesystem- and identifier-safe token.
fn sanitize_token(s: &str) -> String {
    let mut out = String::new();
    let mut prev_us = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_us = false;
        } else if !prev_us {
            out.push('_');
            prev_us = true;
        }
    }
    out.trim_matches('_').to_string()
}

fn body_extension(content_type: &Option<String>) -> &'static str {
    match content_type {
        Some(ct) if ct.contains("json") || ct.contains("graphql") => "json",
        _ => "txt",
    }
}

/// Build the fixtures manifest plus the list of response files to write
/// (`(relative_path, body)`). Pure so it can be unit-tested without disk I/O.
///
/// Each entry records how a test/replay layer should match the request
/// (`method` + `path` + optional `operation_name`) and which file holds the
/// canned response body.
pub fn build_fixtures(flows: &[FlowRecord]) -> (Value, Vec<(String, String)>) {
    let mut entries: Vec<Value> = Vec::new();
    let mut files: Vec<(String, String)> = Vec::new();

    for (i, f) in flows.iter().enumerate() {
        let op = graphql_operation_name(&f.req_body);
        let response_file = f.resp_body.as_ref().map(|body| {
            let path_tok = sanitize_token(&f.path);
            let op_tok = op
                .as_deref()
                .map(|o| format!("_{}", sanitize_token(o)))
                .unwrap_or_default();
            let ext = body_extension(&f.resp_type);
            let name = format!(
                "responses/{i:03}_{}_{}{}.{}",
                f.method, path_tok, op_tok, ext
            );
            files.push((name.clone(), body.clone()));
            name
        });

        entries.push(json!({
            "method": f.method,
            "scheme": f.scheme,
            "host": f.host,
            "path": f.path,
            "operation_name": op,
            "status": f.status,
            "request_content_type": f.req_type,
            "response_content_type": f.resp_type,
            "response_file": response_file,
            "response_truncated": f.resp_truncated,
            // The minimal key a replay/mock layer should match on.
            "match": {
                "method": f.method,
                "path": f.path,
                "operation_name": op,
            },
        }));
    }

    let manifest = json!({
        "version": 1,
        "generated_by": format!("shadowdroid net export fixtures ({})", env!("CARGO_PKG_VERSION")),
        "count": entries.len(),
        "fixtures": entries,
    });
    (manifest, files)
}

/// Write a fixtures bundle to `out`: `manifest.json` plus a `responses/` file per
/// captured response body. Returns an action summary for stdout.
pub fn write_fixtures(flows: &[FlowRecord], out: &Path) -> Result<Value> {
    let (manifest, files) = build_fixtures(flows);
    std::fs::create_dir_all(out.join("responses"))
        .with_context(|| format!("creating fixtures dir {}", out.display()))?;
    for (rel, body) in &files {
        let path = out.join(rel);
        std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    }
    let manifest_path = out.join("manifest.json");
    std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)
        .with_context(|| format!("writing {}", manifest_path.display()))?;
    Ok(json!({
        "type": "action",
        "ok": true,
        "cmd": "export",
        "format": "fixtures",
        "out": out.display().to_string(),
        "manifest": manifest_path.display().to_string(),
        "count": flows.len(),
        "response_files": files.len(),
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

    #[test]
    fn build_fixtures_keys_graphql_by_operation_name() {
        let mut gql = sample();
        gql.method = "POST".into();
        gql.path = "/v1/public/graphql".into();
        gql.req_type = Some("application/json".into());
        gql.req_body = Some(r#"{"operationName":"GetMe","variables":{}}"#.into());
        gql.resp_body = Some(r#"{"data":{"me":{"id":"1"}}}"#.into());

        let (manifest, files) = build_fixtures(&[gql]);
        assert_eq!(manifest["count"], 1);
        let entry = &manifest["fixtures"][0];
        assert_eq!(entry["operation_name"], "GetMe");
        assert_eq!(entry["match"]["operation_name"], "GetMe");
        // Response body is written to a file keyed by path + operation.
        assert_eq!(files.len(), 1);
        assert!(files[0].0.contains("GetMe"), "{}", files[0].0);
        assert!(files[0].1.contains("\"me\""));
        assert_eq!(entry["response_file"], files[0].0);
    }

    #[test]
    fn build_fixtures_omits_file_when_no_response_body() {
        let mut f = sample();
        f.resp_body = None;
        let (manifest, files) = build_fixtures(&[f]);
        assert!(files.is_empty());
        assert_eq!(manifest["fixtures"][0]["response_file"], Value::Null);
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
            rule_id: None,
            rule_ids: vec![],
            modified: false,
            error: None,
            streamed: false,
            req_streamed: false,
        }
    }
}
