# Network debugging guide

`net` is a host-side MITM proxy. `net start` launches the host daemon, creates
`adb reverse`, and changes the device proxy; `net stop` restores the prior
device proxy value.

Capture scope accepts `--host example.com` or `--host '*.example.com'`; both
match that domain and its subdomains at a label boundary, ignoring case. A bare
fragment such as `example` matches host substrings. Repeat `--host` to include
multiple scopes. Other wildcard forms, such as `*example.com` or `api.*.com`,
are rejected. Omit both `--host` and configured `proxy.hosts` for all hosts.
`net log` and intercept/rule host filters use substring matching on captured
flows; their filters do not expand the proxy's capture scope.

```bash
shadowdroid net check com.example.app
shadowdroid net trust --auto
shadowdroid net start --verify-upstream
shadowdroid watch
shadowdroid net checkpoint
shadowdroid net log --after-checkpoint <checkpoint>
shadowdroid net log
shadowdroid net log clear
shadowdroid net show <id> --body-file /tmp/body.json
shadowdroid net stop
```

Use `net check` before assuming HTTPS will decrypt. A `tls_error` means the app
rejected the MITM path; inspect its reason. `--verify-upstream` validates HTTPS
and WSS upstream certificates. Captured bodies are bounded; honor
`req_truncated`/`resp_truncated` and original length fields.

On a `watch` stream, completed `http`, held `http_intercept`, and `tls_error`
events carry exact device-scoped `next_actions`; act on a held flow before its
`hold_deadline_ms` rather than waiting for the stream to finish.

`net intercept --clear` disarms HTTP interception. It first checks daemon support;
an older daemon returns `net_http_intercept_clear_unsupported` without changing
its matcher. To disarm WebSocket frame
interception, use `net intercept --dir c2s --clear` (either direction selects
the single WebSocket matcher). Each command leaves the other protocol's
matcher unchanged. Already-held flows/frames retain their deadlines and must
still be released with `net resume`, `net drop`, or `net respond` (HTTP only).

`net drop <http-id>` returns HTTP 502 by default; `--set-status` selects another
HTTP status. Use `net drop <http-id> --transport` to abort the downstream HTTP
connection/stream before sending any response status or body. Holding at
`--at response --status 200` first proves the upstream completed successfully,
so this models a lost successful response. Its persisted flow retains that
upstream status/body and is marked `intercept:transport_abort` with an error;
the app did not receive that status. `--transport` is HTTP-only and conflicts
with `--set-status`; it does not drop a WebSocket frame.

`net start` returns a stable `capture_session_id`; every flow and TLS failure
carries it. Use `net log --session`, `--since 2m`, `--after-id`,
`--after-checkpoint`, or `--rule-id` to isolate one test phase. `net checkpoint`
adds a durable boundary. `net log clear` clears queryable history without
stopping an active proxy or removing its rules; its summary explicitly reports
that preservation. A later `net start` creates a new capture session.

## WebSocket (WS/WSS) capture

Once an in-scope decryptable connection upgrades to a WebSocket, the proxy
forwards every byte unchanged and decodes a copy of the frame stream. Inspect it
hierarchically — cheapest first — so you spend tokens only on the frames you
need:

```bash
shadowdroid net ws                       # list sessions (id, url, msg/byte counts)
shadowdroid net ws w1                     # that session's messages (compact)
shadowdroid net ws w1 --dir s2c --opcode text --grep '"error"'
shadowdroid net show w1                   # session detail: upgrade + close + totals
shadowdroid net show w1.3 --body          # one message's full reassembled payload
shadowdroid net show w1.3 --body-file /tmp/frame.bin   # binary-safe artifact
shadowdroid net export jsonl --protocol websocket --out ws.jsonl
```

Ids: a session is `w1`, its messages `w1.1`, `w1.2`, …. Each message carries a
`dir` (`c2s` app→server / `s2c` server→app), `opcode` (text/binary/ping/pong/
close), `payload_len`, and a short `preview`; `net show` returns the full text
(or base64 for binary). Fragmented messages are reassembled (`frame_count`
retained); `permessage-deflate` payloads are inflated and marked
`compressed`/`decompressed` with `wire_len` (on-wire) vs `payload_len`
(decompressed). Payload retention is bounded — honor `truncated`.

`net log` shows WebSocket **lifecycle** (`ws_open`/`ws_close`) inline with HTTP
by default but withholds the per-message firehose; add `--protocol websocket`
(WebSocket records only — no HTTP) or `--protocol all` to include `ws_msg`, or
`--protocol http` to hide WebSockets. `--redact` (text frames, handshake headers,
close reasons), capture-session scoping, `--since`, and checkpoints apply to
WebSocket records exactly as to flows. On `watch`, `ws_open`, `ws_msg`,
and `ws_close` interleave live with `screen`/`http`.

Summarize a chatty socket in one call instead of paging frames, and decode a
payload without eyeballing base64:

```bash
shadowdroid net ws w1 --stats             # opcode histogram, per-dir bytes, compression ratio, rate
shadowdroid net show w1.3 --format json   # pretty JSON (hex | protobuf also; falls back to hex)
shadowdroid net show w1.3 --frames        # per-frame breakdown of a fragmented message
shadowdroid net export har --out cap.har  # HAR incl. WS (_webSocketMessages) for browser devtools
```

### Drive & modify WebSocket traffic

Beyond observing, act on a live session — the agent-in-the-loop model, per frame:

```bash
# Inject a frame (always safe, even under permessage-deflate):
shadowdroid net inject w1 --dir s2c --text '{"type":"push","seq":9}'   # simulate a server push to the app
shadowdroid net inject w1 --dir c2s --binary "$(printf x | base64)"    # send to the server as the app
shadowdroid net inject w1 --dir s2c --ping                             # ping/pong/close also

# Declarative frame rules (drop or rewrite matching frames):
shadowdroid net rule add ws-drop --host chat.app --dir c2s --opcode text
shadowdroid net rule add ws-set-text '{"forced":true}' --host chat.app --dir s2c --opcode text

# Agent-in-the-loop breakpoint on frames:
shadowdroid net intercept --dir c2s --opcode text --host chat.app --hold-ms 8000
#   → each match emits a ws_intercept event and appears in `net status` (ws_held);
shadowdroid net resume w1.7                       # forward unchanged
shadowdroid net resume w1.7 --text '<edited>'     # forward an edited payload
shadowdroid net drop   w1.7                        # drop the frame
shadowdroid net intercept --dir c2s --clear        # disarm
```

Modified/dropped/injected frames appear in `net ws` marked `injected` /
`disposition: modified|dropped` / `rule_id`. **Two limits to plan around:**
(1) drop/modify re-encode a frame, which is unsafe under `permessage-deflate`
**context takeover** — such frames are forwarded unchanged and marked
`disposition: refused_deflate`. Start the proxy with `net start --anticomp` to
negotiate an uncompressed session where drop/modify/intercept fully apply.
(2) A held frame pauses its whole direction, so act within the app's keepalive
window (OkHttp defaults to a 5 s ping timeout) or the socket may drop.

Limitations: capture requires the connection to traverse the proxy and be
decryptable. An engine that ignores the system proxy (some Cronet/QUIC clients)
or a certificate-pinned WSS handshake produces a `tls_error` (or nothing) rather
than frames — the socket is outside capture, not silently dropped. If frame
decoding ever desyncs, forwarding continues untapped (the app is never
affected).

Rules have an explicit phase. The ambiguous old `set-header` name is rejected:

```bash
shadowdroid net rule add set-request-header x-debug 1 --host api.example.com
shadowdroid net rule add set-response-header cache-control no-store --host api.example.com
shadowdroid net rule add set-status 503 --host api.example.com
shadowdroid net rule add respond --host api.example.com --method POST \
  --operation-name currentSession --status 401 \
  --header content-type=application/json \
  --body '{"errors":[{"message":"Unauthorized"}]}'
```

`respond` is a request-phase atomic rule: GraphQL `operationName` is matched in
the URL query or JSON POST body, status/headers/body are returned together, and
upstream is bypassed. `--body-file` is the binary-safe alternative to `--body`.
The rule summary reports body length without echoing its contents; captured
flows include the rule id and `upstream_bypassed:true`.

Rule files use typed Boolean matchers and explicit delay/transform/terminal
actions. Legacy `kind` + `args` files are still accepted, but list/export output
uses the canonical shape:

```json
[
  {
    "match_on": "original",
    "matcher": {
      "type": "all",
      "matchers": [
        {"type": "host", "contains": "api.example.com"},
        {"type": "method", "equals": "POST"},
        {"type": "not", "matcher": {"type": "path", "contains": "/health"}}
      ]
    },
    "action": {"category": "delay", "milliseconds": 250}
  },
  {
    "match_on": "transformed",
    "matcher": {"type": "status", "equals": 200},
    "action": {
      "category": "transform",
      "transform": {"type": "set_status", "status": 503}
    }
  }
]
```

`original` always means the immutable flow observed before this phase;
`transformed` means the result of earlier rules. Check a file locally before it
touches a proxy:

```bash
shadowdroid net rule lint rules.json
shadowdroid net rule explain rules.json --host api.example.com --path /v1/users --method POST
shadowdroid net rule explain rules.json --host api.example.com --status 200
```

`net rules rules.json` validates and compiles the complete candidate first,
then replaces the active set with one swap. A bad regex, unreadable local file,
impossible matcher, or any other error leaves every previously active rule in
place.

## Optional in-app AAR

The core debug-only AAR auto-starts its control provider and enables agent
status/coroutine diagnostics. It does not capture HTTP by itself. Network
capture requires the optional OkHttp companion and one explicit application
interceptor in every debug OkHttp client you want to observe:

```bash
shadowdroid aar install --okhttp --build
```

```kotlin
OkHttpClient.Builder()
    .addInterceptor(ShadowDroidCaptureInterceptor()) // debug-only
    .build()
```

That interceptor sees plaintext OkHttp traffic, including certificate-pinned
OkHttp calls. It does not instrument Cronet, QUIC, or other HTTP clients.
`aar agent` reports capture-provider availability; do not use `aar capture` or
`aar intercept` until it reports the OkHttp provider.

Use `aar install --coroutine-probes --build` to activate DebugProbes for
`aar coroutines` in debug builds.

## Guarded JSON experiments and evidence

`net rule add set-json --host api.example.com --operation-name SwitchProfile
/extensions/expiresIn 600 30 1` replaces one existing JSON pointer only when
its old value matches. The final positional is the successful-application
limit (default one), enforced across concurrent requests. Response operation
matchers inspect the request body. `net rule list` exposes `runtime.applications`,
`rejections`, and `last_error`; installing a rule does not prove it ran.

`evidence checkpoint 'after refresh' --out investigation --spec probes.json`
links existing network/video markers, UI and selected private/HTTP fields.
`evidence timeline investigation` orders saved events by payload time when
provided, retaining observation time separately. Missing probes save a partial
checkpoint and return nonzero. See docs/evidence.md for the projection spec.

## Reliable observation, cleanup, and prepared edits

`net status` reports `complete:true` only when the required observations
succeeded. A failed observation exits nonzero with `net_status_incomplete`;
`detail` retains daemon/device evidence and typed `http_proxy_error`,
`daemon_error`, and `adb_reverse_error`. Unknown match results are null.
`http_proxy_state:unknown` never means a successfully observed absent proxy.

After `net stop`, inspect `proxy_restoration:restored|not_needed|unresolved`
and `cleanup_complete`. Unowned settings are preserved and produce an explicit
warning. `network_reachable` is only raw-IP reachability plus DNS resolution;
`application_connectivity:not_checked` means no application HTTP proof was
obtained. The deprecated `connectivity_restored` field is null. Verify an app
request through its actual network stack when HTTP proof matters. Failures
include `phase`, `elapsed_ms`, `completed_phases`, and the underlying typed
`cause` in `detail`; the ADB timeout applies per operation, not to the entire
teardown. Progress goes to stderr, leaving stdout as one terminal JSON object.

For a deterministic edit, prepare the body and install a rule before triggering
the request. Match host, method, path and operation name as narrowly as possible.
For example, discover `net rule add`, install `respond --host api.example.com
--method POST --operation-name currentSession --status 200 --header
content-type=application/json --body-file response.json`, save the returned rule
id, then trigger and verify the request. Remove exactly that id using `net rule
rm <id>` in a `finally` block, including when the UI action fails; do not clear
other rules. This bounds the rule's lifetime to the operation being tested.
Use a response-phase replacement when the upstream request must still execute.
An HTTP hold of 60 seconds cannot extend a client's 10-second timeout. If a hold
was canceled or expired, observe the current app state and arrange a new request
only when replaying the operation is appropriate.
