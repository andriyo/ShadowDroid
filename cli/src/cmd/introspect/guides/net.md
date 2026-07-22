# Network debugging guide

`net` is a host-side MITM proxy. `net start` launches the host daemon, creates
`adb reverse`, and changes the device proxy; `net stop` restores the prior
device proxy value.

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
