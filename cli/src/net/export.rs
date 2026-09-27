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
///
/// The body is piped through the shell's builtin `printf` into
/// `--data-binary @-`: `--data` reads a file when the value starts with `@`
/// (a captured body could upload any local file to the captured URL) and drops
/// line breaks, and a body passed as one argument hits the exec argument limit.
/// A builtin never execs, so the body size is unbounded.
pub fn curl_command(f: &FlowRecord) -> String {
    let url = crate::net::flow::url(&f.scheme, &f.host, f.port, &f.path);
    // Every captured field is attacker-influenced: the method is an HTTP token,
    // and tokens may contain shell metacharacters such as `` ` ``, `$`, `|`, `&`.
    let mut parts = vec![format!("curl -X '{}' '{}'", sh(&f.method), sh(&url))];
    for (k, v) in &f.req_headers {
        if k.eq_ignore_ascii_case("content-length") || k.eq_ignore_ascii_case("host") {
            continue;
        }
        parts.push(format!("-H '{}: {}'", sh(k), sh(v)));
    }
    match &f.req_body {
        Some(body) => {
            parts.push("--data-binary @-".into());
            format!("printf '%s' '{}' | {}", sh(body), parts.join(" \\\n  "))
        }
        None => parts.join(" \\\n  "),
    }
}

/// Why [`curl_command`] cannot send the request body the app sent, if it can't.
pub fn curl_body_gap(f: &FlowRecord) -> Option<&'static str> {
    if f.req_streamed {
        Some("the request body was streamed upstream and not captured")
    } else if f.req_body_redacted {
        Some("the request body was redacted")
    } else if f.req_truncated {
        Some("the captured request body was truncated")
    } else if f.req_body.is_none() && f.req_len > 0 {
        Some("the request body is binary or non-textual and was not captured")
    } else {
        None
    }
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
    let url = crate::net::flow::url(&f.scheme, &f.host, f.port, &f.path);
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
        assert!(c.contains("curl -X 'GET' 'https://api.example.com/v1/me'"));
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
    fn exports_keep_non_default_ports() {
        let mut flow = sample();
        flow.scheme = "http".into();
        flow.host = "127.0.0.1".into();
        flow.port = Some(8080);
        assert!(curl_command(&flow).contains("'http://127.0.0.1:8080/v1/me'"));
        assert_eq!(
            har_entry(&flow)["request"]["url"],
            "http://127.0.0.1:8080/v1/me"
        );
        flow.port = Some(80);
        assert!(curl_command(&flow).contains("'http://127.0.0.1/v1/me'"));
        flow.scheme = "https".into();
        flow.host = "::1".into();
        flow.port = Some(8443);
        assert!(curl_command(&flow).contains("'https://[::1]:8443/v1/me'"));
    }

    #[cfg(unix)]
    #[test]
    fn curl_export_never_executes_captured_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut flow = sample();
        // All valid RFC 9110 token characters; unquoted they run `touch`.
        flow.method = "`touch${IFS}m`".into();
        flow.path = "/x'$(touch${IFS}p)'".into();
        flow.req_headers = vec![("X-A".into(), "'`touch h`'".into())];
        flow.req_body = Some("'; touch b; '".into());
        let script = format!(
            "curl() {{ printf '%s\\n' \"$@\"; }}\n{}\n",
            curl_command(&flow)
        );
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let created: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(created.is_empty(), "script created files: {created:?}");
        let args = String::from_utf8(output.stdout).unwrap();
        let args: Vec<&str> = args.lines().collect();
        assert_eq!(args[0], "-X");
        assert_eq!(args[1], "`touch${IFS}m`");
        assert_eq!(args[2], "https://api.example.com/x'$(touch${IFS}p)'");
    }

    /// Runs `curl_command(flow)` under `sh` with `curl` replaced by a function
    /// that prints its arguments, then the bytes it read from stdin.
    #[cfg(unix)]
    fn run_with_fake_curl(flow: &FlowRecord, dir: &std::path::Path) -> std::process::Output {
        let script = format!(
            "curl() {{ printf '%s\\n' \"$@\"; printf 'STDIN:'; cat; }}\n{}\n",
            curl_command(flow)
        );
        let path = dir.join("replay.sh");
        std::fs::write(&path, script).unwrap();
        std::process::Command::new("sh")
            .arg(&path)
            .current_dir(dir)
            .output()
            .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn curl_export_sends_an_at_prefixed_body_literally() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("secret.txt"), "LOCAL-SECRET").unwrap();
        let mut flow = sample();
        flow.method = "POST".into();
        flow.req_body = Some("@secret.txt".into());
        let output = run_with_fake_curl(&flow, dir.path());
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("--data-binary\n@-\n"), "{stdout}");
        assert!(stdout.ends_with("STDIN:@secret.txt"), "{stdout}");
        assert!(!stdout.contains("LOCAL-SECRET"));
    }

    #[cfg(unix)]
    #[test]
    fn curl_export_keeps_large_multiline_bodies_exact() {
        let dir = tempfile::tempdir().unwrap();
        let mut flow = sample();
        flow.method = "POST".into();
        let body = "line with 'quotes' and $vars\r\n".repeat(80_000);
        assert!(body.len() > 2_000_000);
        flow.req_body = Some(body.clone());
        let output = run_with_fake_curl(&flow, dir.path());
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        let (_, sent) = stdout.split_once("STDIN:").unwrap();
        assert_eq!(sent, body);
    }

    #[test]
    fn curl_body_gap_names_every_unreproducible_body() {
        let mut flow = sample();
        assert_eq!(curl_body_gap(&flow), None);
        flow.req_len = 5;
        assert!(curl_body_gap(&flow).unwrap().contains("binary"));
        flow.req_body = Some("abc".into());
        assert_eq!(curl_body_gap(&flow), None);
        flow.req_truncated = true;
        assert!(curl_body_gap(&flow).unwrap().contains("truncated"));
        flow.req_body_redacted = true;
        assert!(curl_body_gap(&flow).unwrap().contains("redacted"));
        flow.req_streamed = true;
        assert!(curl_body_gap(&flow).unwrap().contains("streamed"));
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
            original_url: None,
            upstream_bypassed: false,
            error: None,
            error_redacted: false,
            streamed: false,
            resp_len_unknown: false,
            req_streamed: false,
        }
    }
}
