# Network capture, interception, and replay

[← README](../README.md)

`net` is a host-side MITM proxy built into the single binary — no Python, no
external mitmproxy. `net start` spawns the proxy, wires the device through
`adb reverse` and proxy settings, and decrypted HTTP(S) transactions then stream
as `http` events on the same timeline as `screen` when `watch` is running.

The pre-existing device proxy setting is persisted before wiring; a repeated
`net start` repairs wiring to an already-running daemon, while `net stop`
restores that exact setting and reports separate raw-IP and DNS connectivity
checks (`--canary-host` selects the neutral DNS probe).

## Sessions, checkpoints, and querying

Each new proxy run returns a stable `capture_session_id`, and every captured
flow records both that session and any matching rule ids. Isolate a phase with
`net log --session`, `--since 2m`, `--after-id`, `--rule-id`, or create a
durable boundary with `net checkpoint` and query it using
`net log --after-checkpoint`. `net log clear` clears only queryable history
while preserving an active proxy and its rules; restarting the proxy creates a
new session.

Completed flows enter a bounded in-memory queue; `net status` exposes
`dropped_flows` if sustained traffic outruns storage. The session JSONL keeps
one 64 MiB current generation plus one rotated generation, bounding disk use to
roughly 128 MiB per device session.

## Intercepting and mutating flows

Beyond observing, the agent can **intercept** a flow — `net intercept` pauses
matching requests/responses and emits them as `http_intercept` events on
`watch`; each held event includes device-scoped `net show`, `resume`, `drop`,
and `respond` actions so the agent can decide before the hold deadline. The
same flow ID remains actionable across `net status`, `net show`, and the release
verbs while held. Status includes its phase, held/expiry timestamps, and client
connection state; a raced action reports `already_released`, `deadline_expired`,
`client_canceled`, or `unknown_id` with the available lifecycle timestamps. The
agent inspects with `net show`, then releases with
`net resume --set-status/--body/…`, `net drop`, or `net respond` (a canned
reply).

## Declarative rules

Repeated edits can be promoted to declarative `net rule`s (map-local /
map-remote / set-status / set-request-header / set-response-header / replace /
block / delay). A `respond` rule atomically returns a synthetic status, headers,
and literal or file-backed body without contacting upstream; it can match a
GraphQL `operationName` from either the URL query or JSON POST body. Captured
flows name the rule and report `upstream_bypassed:true`.

Header rules deliberately name their phase: use `set-request-header` before
upstream or `set-response-header` before returning to the app. The ambiguous
`set-header` kind is rejected instead of guessing.

## Fixtures and replay

`net export fixtures` writes the versioned, content-addressed bundle consumed
directly by `net replay --from`: the loader validates its schema, paths, sizes,
hashes, statuses, and headers before atomically replacing the active set.
Replay keys include method, scheme, canonical host, effective port,
case-sensitive path, normalized query pairs, GraphQL operation, and canonical
JSON (or raw body) hash, so same-route requests cannot silently collide.
Redacted, truncated, streamed, errored, or otherwise incomplete captures are
refused rather than producing misleading fixtures. A corrupt or incompatible
replay candidate leaves the prior active generation untouched; an older running
daemon is detected by capability preflight and must be restarted before loading
the bundle.

`net export har|curl|fixtures` hands flows to other tools by writing a durable
artifact and returning an actionable summary; HAR defaults to
`shadowdroid-network.har`, curl to `shadowdroid-network.curl.sh`, and fixtures
to `shadowdroid-fixtures` unless `--out` selects another path.

## Verifying an app is interceptable

`net check <app>` labels its debuggable/targetSdk result as a static heuristic
and leaves the app-specific verdict unverified. With the proxy running,
`net check --probe <app>` launches a package-scoped HTTPS canary; it reports
verified/interceptable only when the app handles that intent, requests its
unique URL, and the exact decrypted flow is captured.

## Protocol coverage

The decrypted leg negotiates **HTTP/2 or HTTP/1.1** (h2 apps aren't
downgraded), streams **SSE / large bodies** through instead of buffering them —
both response and request (a big upload streams chunked; marked
`streamed`/`req_streamed` in the flow) — decodes `gzip`/`deflate`/`br`/`zstd`,
and **captures WebSocket (WS/WSS) frames**. `net start --verify-upstream`
validates the real server certificate for both HTTPS and WSS (off by default
for self-signed dev backends).

## WebSocket capture and drive

Once an in-scope connection upgrades, the proxy forwards every byte unchanged
and decodes a copy of the frame stream. Inspect it hierarchically so an agent
spends tokens only on the frames it needs: `net ws` lists sessions (id, url,
per-direction message/byte counts), `net ws <id>` lists that session's messages
(compact `dir`/`opcode`/`preview`, filterable by `--dir`/`--opcode`/`--grep`/
`--since`), and `net show <message-id> --body` reveals a full reassembled
payload (`--body-file` writes it binary-safe; bare `net show` returns metadata +
`preview`). Fragmented messages are reassembled and `permessage-deflate`
payloads inflated (marked `compressed`/`decompressed`, with `wire_len` vs
`payload_len`). `net log` shows `ws_open`/`ws_close` lifecycle inline with HTTP
by default; `--protocol websocket|all` adds per-message `ws_msg` events, which
also stream live on `watch`. `net ws <id> --stats` summarizes a chatty socket
(opcode histogram, per-direction bytes, compression ratio, rate) in one call;
`net show <msg> --format hex|json|protobuf` decodes a payload and `--frames`
shows a fragmented message's per-frame breakdown; `net export jsonl` and
`net export har` (with devtools `_webSocketMessages`) write durable dumps.
Payload retention is bounded (`truncated`), `--redact` scrubs text frames,
handshake headers, and close reasons, and an engine that bypasses the proxy or
pins its certificate is reported (`tls_error`) rather than silently dropped.

Beyond observing, an agent can **drive** a live session in the same
agent-in-the-loop model as HTTP: `net inject <id> --dir s2c --text …` splices a
frame in (simulate a server push, or send to the server as the app; always safe,
even under compression); `net rule add ws-drop`/`ws-set-text` declaratively drop
or rewrite matching frames; and `net intercept --dir …` pauses matching frames
(surfaced on `watch` and in `net status`) for `net resume [--text …]`/`net drop`.
Drop/modify re-encode a frame, which is unsafe under `permessage-deflate`
context takeover — those are forwarded unchanged and marked `refused_deflate`;
`net start --anticomp` negotiates an uncompressed session where they fully
apply.

## The proxy CA

By default the proxy signs with a CA it generates on first use. To reuse a CA
the device already trusts — an existing mitmproxy/Charles/corporate CA — run
`net ca import --cert <pem>` (the key can be a separate `--key`, or bundled in a
combined PEM like mitmproxy's `mitmproxy-ca.pem`); every downstream step then
signs and installs *your* CA. `net ca info` shows the active CA and
`net ca reset` returns to a generated one. Per-project CAs and the
`ca_trusted` assertion are covered in [configuration.md](configuration.md).

`net trust` installs the CA into the device's **user** trust store. Apps that
opt out of user CAs via network security config, pin their certificates, or use
stacks that bypass the system trust store will reject the proxy — that failure
is surfaced as a `tls_error` event rather than silently missing traffic.

## Redaction at the network boundary

`net start --redact` applies the built-in/configured policy to
authorization/cookie headers, nested JSON/GraphQL body fields, JWTs, email/IP
values, URL path/query values (including percent-encoded names and values),
hosts, errors, and configured patterns before completed captures are persisted
(the session log is written `0600` either way). Records flag redacted routing
or error fields and carry policy version 2. Forwarded traffic is unchanged.

A live daemon reports its capture-policy fingerprint; requesting `--redact`
refuses to reuse an unredacted or differently configured daemon and asks for an
explicit stop/start. See
[security-and-redaction.md](security-and-redaction.md).

## Above-TLS OkHttp capture (the in-app AAR)

For in-process diagnostics in an app you can build, install the debug-only core
AAR with `shadowdroid aar install --build`. Add `--coroutine-probes` for
`aar coroutines`. HTTP capture is opt-in and OkHttp-specific:

```bash
shadowdroid aar install --okhttp --build
```

Then add `ShadowDroidCaptureInterceptor()` as an application interceptor to
each target debug `OkHttpClient`. `aar agent` reports whether that provider is
actually registered before `aar capture`/`aar intercept` are used. The core AAR
alone does not capture HTTP, and the companion does not instrument Cronet,
QUIC, or other stacks. Because it sits above TLS, it sees certificate-pinned
OkHttp traffic that the MITM proxy cannot.

`aar intercept` holds at most 32 matching calls and gives each one an absolute
monotonic deadline. Unresolved, interrupted, or over-capacity calls fail open.
At or after the deadline, `aar resume`/`aar drop` returns a non-zero typed
`aar_intercept_deadline_expired` error instead of claiming that a late action
was delivered while that record remains in the bounded terminal history;
concurrent resolvers likewise have exactly one winner. `aar agent` exposes live
holds plus that history so an automation can distinguish expiration,
interruption, an earlier release, and an unknown or evicted id.

In-app OkHttp flows drained by `aar capture` cross the same redaction policy
boundary before stdout, JSONL, fixtures, or the shared store.

## Choosing between `net` and `aar`

Use `net` first for proxy-aware HTTP(S): it is built into the host CLI, requires
no app code changes, and supports capture, intercept, mutation, rules, fixtures,
HAR/curl export, and replay. Use `aar` for apps you can build when you need the
debug-only in-app agent for process/coroutine diagnostics, or above-TLS capture
of pinned OkHttp traffic.
