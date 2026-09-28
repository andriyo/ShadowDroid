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
    // `-X HEAD` makes curl wait for a body a HEAD response never has.
    let mut parts = vec![if f.method.eq_ignore_ascii_case("HEAD") {
        format!("curl --head '{}'", sh(&url))
    } else {
        format!("curl -X '{}' '{}'", sh(&f.method), sh(&url))
    }];
    for (k, v) in &f.req_headers {
        // Hop-by-hop headers were for the app's connection to the proxy; the
        // proxy never forwarded them, so a replay must not send them either.
        if k.eq_ignore_ascii_case("content-length")
            || k.eq_ignore_ascii_case("host")
            || crate::net::proxy::is_hop_by_hop(&k.to_ascii_lowercase())
        {
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
    } else if f
        .req_body
        .as_deref()
        .is_some_and(|body| body.contains('\u{FFFD}'))
    {
        Some("the request body was not valid UTF-8, so the captured text is not its original bytes")
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
    let http_version = f.http_version.as_deref().unwrap_or("HTTP/1.1");
    let mut request = json!({
        "method": f.method,
        "url": url,
        "httpVersion": http_version,
        "headers": har_headers(&f.req_headers),
        "queryString": har_query_string(&f.path),
        "cookies": har_request_cookies(&f.req_headers),
        "headersSize": -1,
        // HAR's -1: the size is unknown (a streamed chunked upload).
        "bodySize": if f.req_len_unknown { -1 } else { f.req_len as i64 },
    });
    if let Some(body) = &f.req_body {
        request["postData"] = json!({
            "mimeType": f.req_type.clone().unwrap_or_default(),
            "text": body,
        });
    }
    let mut content = json!({
        "size": f.resp_len,
        "mimeType": f.resp_type.clone().unwrap_or_default(),
        "text": f.resp_body.clone().unwrap_or_default(),
    });
    if f.resp_body.is_none() && f.resp_len > 0 {
        // An empty text would read as an empty body.
        content["comment"] = json!(if f.streamed {
            "response body was streamed through the proxy and not captured"
        } else {
            "response body is binary or non-textual and was not captured"
        });
    }
    json!({
        "startedDateTime": iso8601(f.ts),
        "time": f.dur_ms.unwrap_or(0),
        "request": request,
        "response": {
            "status": f.status.unwrap_or(0),
            "statusText": "",
            "httpVersion": http_version,
            "headers": har_headers(&f.resp_headers),
            "cookies": har_response_cookies(&f.resp_headers),
            "content": content,
            "redirectURL": har_redirect_url(f),
            "headersSize": -1,
            "bodySize": if f.resp_len_unknown { -1 } else { f.resp_len as i64 },
        },
        "cache": {},
        "timings": {"send": 0, "wait": f.dur_ms.unwrap_or(0), "receive": 0},
        "_upstreamHttpVersion": f.upstream_http_version,
        // The origin's encoding when the proxy decoded the body for the app.
        "_upstreamContentEncoding": f.upstream_content_encoding,
        "_upstreamBodySize": f.upstream_resp_len,
    })
}

/// HAR `queryString`: the decoded `name=value` pairs of the request target.
fn har_query_string(path: &str) -> Value {
    let Some((_, query)) = path.split_once('?') else {
        return json!([]);
    };
    let decode = |part: &str| {
        urlencoding::decode(&part.replace('+', " "))
            .map(|value| value.into_owned())
            .unwrap_or_else(|_| part.to_string())
    };
    Value::Array(
        query
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| {
                let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
                json!({"name": decode(name), "value": decode(value)})
            })
            .collect(),
    )
}

/// A redaction placeholder replaced the whole header value; nothing to parse.
fn is_redacted(value: &str) -> bool {
    value.trim_start().starts_with("<redacted")
}

/// HAR request `cookies` from the `Cookie` header(s).
fn har_request_cookies(headers: &[(String, String)]) -> Value {
    Value::Array(
        headers
            .iter()
            .filter(|(name, value)| name.eq_ignore_ascii_case("cookie") && !is_redacted(value))
            .flat_map(|(_, value)| value.split(';'))
            .filter_map(|cookie| {
                let (name, value) = cookie.trim().split_once('=')?;
                Some(json!({"name": name.trim(), "value": value.trim()}))
            })
            .collect(),
    )
}

/// HAR response `cookies`: one per `Set-Cookie` header, with its attributes.
fn har_response_cookies(headers: &[(String, String)]) -> Value {
    Value::Array(
        headers
            .iter()
            .filter(|(name, value)| name.eq_ignore_ascii_case("set-cookie") && !is_redacted(value))
            .filter_map(|(_, value)| {
                let mut parts = value.split(';').map(str::trim);
                let (name, cookie_value) = parts.next()?.split_once('=')?;
                let mut cookie = json!({"name": name, "value": cookie_value});
                for attribute in parts {
                    let (key, attribute_value) =
                        attribute.split_once('=').unwrap_or((attribute, ""));
                    match key.to_ascii_lowercase().as_str() {
                        "path" => cookie["path"] = json!(attribute_value),
                        "domain" => cookie["domain"] = json!(attribute_value),
                        "expires" => cookie["expires"] = json!(attribute_value),
                        "httponly" => cookie["httpOnly"] = json!(true),
                        "secure" => cookie["secure"] = json!(true),
                        _ => {}
                    }
                }
                Some(cookie)
            })
            .collect(),
    )
}

/// HAR `redirectURL`: the `Location` of a redirect response.
fn har_redirect_url(f: &FlowRecord) -> String {
    if !f.status.is_some_and(|status| (300..400).contains(&status)) {
        return String::new();
    }
    f.resp_headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("location"))
        .map(|(_, value)| value.clone())
        .unwrap_or_default()
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
/// Write a fixtures bundle. With `strict` (the caller named one flow) any
/// unreplayable flow fails the export; otherwise flows that cannot be replayed
/// (streamed, binary, errored, modified, ...) are skipped and listed, so one
/// image download does not block exporting the rest of a session.
pub fn write_fixtures(flows: &[FlowRecord], out: &Path, strict: bool) -> Result<Value> {
    let mut skipped = Vec::new();
    let eligible: Vec<&FlowRecord> = flows
        .iter()
        .filter(
            |flow| match crate::net::replay::validate_source_flow(flow) {
                Ok(()) => true,
                Err(_) if strict => true,
                Err(error) => {
                    skipped.push(json!({"id": flow.id, "reason": format!("{error:#}")}));
                    false
                }
            },
        )
        .collect();
    if eligible.is_empty() {
        return Err(crate::diagnostic::DiagnosticError::new(
            "net_export_nothing_replayable",
            "net",
            format!(
                "none of the {} captured flows can be replayed from fixtures",
                flows.len()
            ),
        )
        .detail(json!({"skipped": skipped}))
        .next_actions([
            "inspect detail.skipped for why each flow is not replayable",
            "capture a fresh, unmodified, unredacted textual request, then retry the export",
        ])
        .into());
    }
    let sources = eligible
        .into_iter()
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
        "skipped": skipped,
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
    fn curl_export_replays_head_without_hop_by_hop_headers_or_lossy_bodies() {
        let mut flow = sample();
        flow.method = "HEAD".into();
        flow.req_body = None;
        flow.req_headers = vec![
            ("Proxy-Connection".into(), "keep-alive".into()),
            ("Accept".into(), "*/*".into()),
        ];
        let command = curl_command(&flow);
        assert!(command.starts_with("curl --head '"), "{command}");
        assert!(
            !command.to_ascii_lowercase().contains("proxy-connection"),
            "{command}"
        );
        assert!(command.contains("-H 'Accept: */*'"));

        let mut latin1 = sample();
        latin1.req_body = Some("caf\u{FFFD} au lait".into());
        assert!(curl_body_gap(&latin1).is_some_and(|gap| gap.contains("UTF-8")));
    }

    #[test]
    fn a_truncated_body_keeps_its_valid_prefix() {
        // "é" is two bytes; a cap of 2 splits it.
        let (text, truncated) =
            crate::net::flow::body_to_text(Some("text/plain"), "aé".as_bytes(), 2);
        assert_eq!(text.as_deref(), Some("a"));
        assert!(truncated);
        // A body that is simply not UTF-8 is still shown lossily.
        let (text, _) = crate::net::flow::body_to_text(Some("text/plain"), b"caf\xe9", 100);
        assert_eq!(text.as_deref(), Some("caf\u{FFFD}"));
    }

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

    #[test]
    fn session_fixture_export_skips_unreplayable_flows() {
        let dir = tempfile::tempdir().unwrap();
        let good = sample();
        let mut streamed = sample();
        streamed.id = "f2".into();
        streamed.path = "/v1/stream".into();
        streamed.streamed = true;

        let report = write_fixtures(
            &[good.clone(), streamed.clone()],
            &dir.path().join("all"),
            false,
        )
        .unwrap();
        assert_eq!(report["count"], 1, "{report}");
        assert_eq!(report["skipped"][0]["id"], "f2");
        assert!(
            report["skipped"][0]["reason"]
                .as_str()
                .unwrap()
                .contains("streamed")
        );

        // Naming one flow keeps the export strict.
        assert!(write_fixtures(&[streamed.clone()], &dir.path().join("one"), true).is_err());
        let error = write_fixtures(&[streamed], &dir.path().join("none"), false).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<crate::diagnostic::DiagnosticError>()
                .unwrap()
                .code,
            "net_export_nothing_replayable"
        );
    }

    #[test]
    fn har_fills_query_cookies_redirects_and_uncaptured_bodies() {
        let mut flow = sample();
        flow.path = "/search?q=a%20b&tag=x+y&flag".into();
        flow.status = Some(302);
        flow.req_headers = vec![("Cookie".into(), "sid=abc; theme=dark".into())];
        flow.resp_headers = vec![
            ("Location".into(), "https://api.example.com/next".into()),
            (
                "Set-Cookie".into(),
                "sid=new; Path=/; HttpOnly; Secure".into(),
            ),
        ];
        flow.resp_body = None;
        flow.resp_len = 10;
        let entry = har_entry(&flow);
        assert_eq!(
            entry["request"]["queryString"],
            json!([{"name": "q", "value": "a b"}, {"name": "tag", "value": "x y"}, {"name": "flag", "value": ""}])
        );
        assert_eq!(
            entry["request"]["cookies"][1],
            json!({"name": "theme", "value": "dark"})
        );
        assert_eq!(
            entry["response"]["cookies"][0],
            json!({"name": "sid", "value": "new", "path": "/", "httpOnly": true, "secure": true})
        );
        assert_eq!(
            entry["response"]["redirectURL"],
            "https://api.example.com/next"
        );
        assert!(
            entry["response"]["content"]["comment"]
                .as_str()
                .unwrap()
                .contains("not captured")
        );

        flow.req_headers = vec![("Cookie".into(), "<redacted:cookie>".into())];
        assert_eq!(har_entry(&flow)["request"]["cookies"], json!([]));
    }

    #[test]
    fn har_reports_the_captured_http_version() {
        let mut flow = sample();
        assert_eq!(har_entry(&flow)["request"]["httpVersion"], "HTTP/1.1");
        flow.http_version = Some("HTTP/2.0".into());
        flow.upstream_http_version = Some("HTTP/1.1".into());
        let entry = har_entry(&flow);
        assert_eq!(entry["request"]["httpVersion"], "HTTP/2.0");
        assert_eq!(entry["response"]["httpVersion"], "HTTP/2.0");
        assert_eq!(entry["_upstreamHttpVersion"], "HTTP/1.1");
    }

    #[test]
    fn har_names_the_encoding_the_proxy_decoded() {
        let mut flow = sample();
        assert!(har_entry(&flow)["_upstreamContentEncoding"].is_null());
        flow.upstream_content_encoding = Some("gzip".into());
        flow.upstream_resp_len = Some(162);
        let entry = har_entry(&flow);
        assert_eq!(entry["_upstreamContentEncoding"], "gzip");
        assert_eq!(entry["_upstreamBodySize"], 162);
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
            http_version: None,
            upstream_http_version: None,
            upstream_content_encoding: None,
            upstream_resp_len: None,
            fault_ids: Vec::new(),
            upstream_bypassed: false,
            error: None,
            error_redacted: false,
            streamed: false,
            resp_len_unknown: false,
            req_streamed: false,
            req_len_unknown: false,
        }
    }
}
