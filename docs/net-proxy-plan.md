# ShadowDroid — Network Proxy Plan (`net`)

Design + phased implementation plan for giving ShadowDroid a networking layer: **see**
an app's HTTP(S) traffic alongside screen changes, **modify** it in flight, and
**transform** responses for debugging — modelled on mitmproxy, but agent-first and
inside the single Rust binary.

> Status: **IMPLEMENTED** (P1–P4). The `net` namespace ships in the CLI
> (`cli/src/net/`): hand-rolled MITM proxy + daemon + control socket, observe
> (`check`/`trust`/`start`/`stop`/`status`/`watch`/`log`/`show`), agent-in-the-loop
> intercept (`intercept`/`resume`/`drop`/`respond`), declarative rules + replay,
> doctor integration, and HAR/curl export. The proxy core, intercept, and rule
> engine are validated end-to-end (host-curl through the live daemon). The CA
> system-store install and `watch --net` interleave are device-gated (logic is in
> place, full validation needs an attached device/emulator). Supersedes the earlier
> read-only "net capture" direction (Route C / `SHADOWDROID_NET` logcat tailing).
> See [Relationship to the capture plan](#relationship-to-the-read-only-capture-plan).

---

## 1. Thesis

The earlier plan was **read-only**: the app logs its own traffic, the host tails
logcat. That cannot satisfy the new requirements — *you cannot modify a response by
reading a log line after it already arrived.* Modification and transform-scripts
require sitting **inline on the wire** with TLS termination. That is exactly what
mitmproxy is.

So the architecture inverts. An **embedded host-side MITM proxy** becomes the primary
path; log-tailing capture is demoted to a lightweight observe-only fallback for apps
you cannot point at a proxy.

Two design commitments shape everything else:

1. **HTTP is one more event type in the existing `watch` stream.** The agent already
   parses a JSON-lines timeline of `screen` / `crash` / `action` events
   ([cli/src/events.rs:18](../cli/src/events.rs)). Network traffic joins it, so the
   agent sees *"the error screen appeared right after a 401 on `/v1/login`"* — screen
   and network causally linked by timestamp.
2. **The AI agent is the proxy operator.** mitmproxy's killer interactive feature is
   *pause a flow, a human edits it in the console, resume.* ShadowDroid's "human at
   the console" is the agent. **Agent-in-the-loop interception is the primary
   modification mechanism** (chosen 2026-06-16); declarative rules are a complementary
   later tier for making a per-flow decision permanent (transform scripts deferred —
   [§6.2](#62-embedded-transform-scripts--deferred-out-of-scope-for-now)).

---

## 2. Goals / non-goals

**Goals**
- Observe decrypted HTTP(S) of one app under test as structured JSON, interleaved with
  the screen timeline.
- Let the agent intercept a flow, inspect it, and mutate or drop it before it
  continues — per flow, decided in the agent's reasoning loop.
- Promote a repeated decision to a static **rule** when it's deterministic (transform
  scripts deferred).
- Stay a **single binary, no Python** — embed the proxy (Rust) the way the rest of the
  tool embeds its logic.
- Reuse existing machinery: the `watch` event envelope, `~/.shadowdroid/` store, adb
  helpers, the doctor/collect/connect lifecycle, and ShadowDroid's own UI automation
  for cert install.

**Non-goals (for now)**
- A GUI or web console (mitmweb). The agent + JSON *is* the UI.
- Capturing arbitrary device-wide traffic with perfect per-process attribution.
- HTTP/3 / QUIC interception, raw-TCP rewriting, transparent/WireGuard modes.
- Defeating certificate pinning or instrumenting release builds (`net check` blocks
  these up front instead of failing mysteriously).

---

## 3. Architecture

```
        ┌─ shadowdroid (single Rust binary) ─────────────────────────┐
        │  UI automation (existing)        MITM proxy daemon (new)   │
        │  tap / find / screen / watch     hyper + rustls + rcgen CA │
        │         ▲                              ▲     │             │
        │         │ adb (UI server :7912)        │     │ control     │
        └─────────┼──────────────────────────────┼─────┼─────────────┘
                  │                               │     │ unix socket
   adb reverse ───┼── tcp:8080  (device localhost ⇒ host proxy)
                  ▼                               │     ▼
        ┌─────────────────────────────────────────────────────────┐
        │ device:  app under test ──HTTP(S)──► localhost:8080      │
        │ settings put global http_proxy localhost:8080            │
        │ ShadowDroid CA trusted (system store / driven Settings)  │
        └─────────────────────────────────────────────────────────┘
```

**Proxy engine (decided: hand-rolled).** A minimal CONNECT-intercepting MITM server
built directly on `hyper` + `tokio-rustls` (the stack already pulled in transitively by
`reqwest`, [cli/Cargo.toml:29](../cli/Cargo.toml)) with `rcgen` for the CA + per-host
leaf generation. **`hudsucker` is a reference, not a dependency** — read
`/Users/andrii/Work/hudsucker` for the implementation details that are fiddly to get
right (CONNECT tunnel handling, on-the-fly leaf cert minting + caching, ALPN/SNI
negotiation, WebSocket pass-through, graceful upstream errors) and reimplement the
subset we need. Hand-rolling keeps the dependency surface small and the intercept/hold
hooks (which `hudsucker` doesn't expose the way we need) first-class rather than bolted
on. This keeps the no-Python, single-binary promise.

**Device wiring.** `adb reverse tcp:8080 tcp:8080` makes the device's `localhost:8080`
tunnel to the host proxy; `settings put global http_proxy localhost:8080` points the
device at it. Loopback only — nothing is exposed off-host. **Gap:**
[adb.rs](../cli/src/device/adb.rs) has `forward`/`forward_remove` (:140/:157) but no
`reverse` — add `reverse` / `reverse_remove` helpers.

**Daemon + control socket.** This is the consequence of the agent-in-the-loop choice.
A *held* flow must survive across several discrete agent tool calls — observe the
intercept event, reason, then issue `net resume` as a **separate** invocation. That
requires shared state in a long-lived process, so the proxy runs as a **background
daemon**:
- proxy listener on `127.0.0.1:<port>`,
- control socket at `~/.shadowdroid/net/<serial>.sock`,
- event log at `~/.shadowdroid/net/<serial>.jsonl` (backs `net log`),
- an in-memory map of currently-held flows keyed by flow id.

`net start` spawns it (or runs `--foreground`); `net watch` / `resume` / `drop` /
`respond` / `status` / `rule` / `stop` are short-lived **clients** of that socket. The
existing `watch/stdin.rs` text-command model ([cli/src/watch/stdin.rs:15](../cli/src/watch/stdin.rs))
stays for humans, but the agent path is socket-backed one-shots — matching how the
agent already drives `tap`/`screen` as discrete calls.

`~/.shadowdroid/` is the established store root
([config.rs:15](../cli/src/config.rs) `USER_CONFIG_REL`, plugins under
`~/.shadowdroid/plugins/`), and the `directories` crate + a `home_dir()` helper already
exist — `~/.shadowdroid/net/` is consistent with both.

---

## 4. The unified event stream

HTTP slots into the `Event` enum next to `Screen`/`Crash`/`Action`
([events.rs:18](../cli/src/events.rs), `#[serde(tag = "type")]`). Completed flows emit a
**compact** `http` event (full bodies fetched on demand via `net show`):

```jsonc
{"type":"http","ts":1718539200.12,"id":"f3a","method":"POST","host":"api.livd.app",
 "path":"/v1/login","status":401,"ok":false,"dur_ms":143,
 "req_type":"application/json","req_len":82,"resp_type":"application/json","resp_len":63,
 "matched":"intercept","modified":true}
```

Field shape deliberately mirrors the earlier `capture-core` wire format
(`t/id/ts/ms/method/host/path/status/ok/…`), so if log-tailing capture is ever
revived, both producers emit the *same* `http` event.

```jsonc
{"type":"screen","ts":1718539200.01,"screen_hash":"a1b2","element_count":44}
{"type":"http","ts":1718539200.12,"id":"f3a","method":"POST","host":"api.livd.app","path":"/v1/login","status":401,"ok":false,"dur_ms":143}
{"type":"screen","ts":1718539200.30,"screen_hash":"c4d5"}   // ← the error screen the 401 produced
```

- `shadowdroid watch --net com.livd` interleaves screen + http + crash on one timeline.
- `net watch [matchers]` is `watch` pre-filtered to `http` events.
- `net log [matchers] [-n 50]` recalls past flows from the session JSONL.
- `net show <id> [--body|--har]` returns full headers + bodies for one flow.

---

## 5. Agent-in-the-loop interception (primary modify mechanism)

### 5.1 Model

The daemon can **pause** flows matching a filter. A paused flow is emitted as an
`http_intercept` event and **held** (the device's HTTP call blocks) until the agent
acts. Because the held flow lives in the daemon, the agent can take as many reasoning
steps as it needs and then issue a separate `net resume`/`drop`/`respond` call.

```jsonc
{"type":"http_intercept","ts":1718539200.12,"id":"f9","phase":"response",
 "method":"POST","host":"api.livd.app","path":"/v1/login","status":200,
 "req_type":"application/json","req_len":82,"resp_type":"application/json","resp_len":512,
 "hold_deadline_ms":30000,"req_preview":"{\"email\":\"…\"}","resp_preview":"{\"token\":\"…\"}"}
```

- **`phase`** — `request` (held before going upstream) or `response` (held before
  returning to the device). Default holds at `response`; `--at request` or `--at both`.
- **`hold_deadline_ms`** — the agent's budget. Real apps time out their own HTTP
  clients, so a hold cannot last forever (see [§9](#9-hard-edges)).
- Full bodies via `net show f9 --body` (synchronous — the flow is in memory).

### 5.2 Acting on a held flow

```bash
# Inspect, then release with mutations (response phase):
net resume f9 --set-status 500 --set-header content-type application/json \
              --body '{"error":"forced failure"}'

# Or rewrite a JSON field instead of the whole body:
net resume f9 --replace '"plan":"free"' '"plan":"premium"'

# Or let it through untouched:
net resume f9

# Kill it (device sees a connection error, or a chosen status):
net drop f9 --status 502

# Short-circuit at the request phase — never hits the server (Response.make equivalent):
net respond f9 --status 200 --body-file ./fixtures/login_ok.json
```

**Mutation flags** (mirror mitmproxy's flow API):

| Flag | Phase | Effect |
| --- | --- | --- |
| `--set-status <code>` | response | override status code |
| `--set-header <n> <v>` (repeatable) | both | set/replace a header |
| `--remove-header <n>` | both | delete a header |
| `--body <str>` / `--body-file <path>` | both | replace body, fix content-length |
| `--set-json <json>` | both | replace body as JSON |
| `--replace <regex> <repl>` | both | regex edit of the body |
| `--set-url` / `--set-host` / `--set-method` | request | redirect the outgoing request |
| `--delay <ms>` | both | hold a bit longer before releasing |

`net status` lists currently-held flows with remaining hold time so the agent never
loses track of a blocked call.

### 5.3 Worked example — testing error handling

```bash
net check com.livd            # verdict: debuggable + NSC trusts user CA → OK
net trust                     # install/trust the ShadowDroid CA
net start --apps com.livd     # proxy up, device pointed at it
net intercept --host api.livd.app --path /v1/login   # arm the pause

# agent taps "Log in" in the app, then on the stream sees:
#   {"type":"http_intercept","id":"f9","phase":"response","status":200, …}
net resume f9 --set-status 401 --body '{"error":"invalid_credentials"}'

# agent calls `screen` and confirms the app rendered the error state correctly
net stop --revoke-ca
```

The agent *is* the transform function — no script, no pre-declared rule, decided per
flow. A static proxy can't do this; a log-tailer can't dream of it.

---

## 6. Complementary static tiers (later phases)

Interception is for **exploring**. Once a decision is deterministic, promote it so it
applies with no round-trip.

### 6.1 Declarative rules (P3)

Covers mitmproxy's `modify_body` / `modify_headers` / `map_local` / `map_remote` /
`blocklist`. Each `rule add` is one tool call with verify-by-readback (returns the
stored rule + id):

```bash
net rule add set-status --host api.livd.app --path /v1/login 401
net rule add map-local  --host api.livd.app --path /v1/feed  ./fixtures/feed.json
net rule add map-remote --host api.livd.app https://localhost:8080
net rule add replace    --host api.livd.app --content-type json '"plan":"free"' '"plan":"premium"'
net rule add delay      --host '*.livd.app' 3000
net rule add block      --host '*.segment.io'
net rule list | net rule rm <id> | net rule clear
net rules apply ./debug-rules.json     # bulk / reproducible
net replay --from session.jsonl [matchers]   # serve saved responses, no backend
```

### 6.2 Embedded transform scripts — DEFERRED (out of scope for now)

Programmatic transforms (a mitmproxy-style `request(flow)`/`response(flow)` hook in an
embedded language — Rhai or Lua) are **explicitly deferred** (decided 2026-06-16). The
agent-in-the-loop intercept ([§5](#5-agent-in-the-loop-interception-primary-modify-mechanism))
covers programmatic-feeling transforms today (the agent's reasoning *is* the script),
and declarative rules ([§6.1](#61-declarative-rules-p3)) cover the deterministic cases.
Revisit only if a real need for in-proxy stateful logic shows up that neither tier
serves. No `net script` verb, no scripting dependency, until then.

---

## 7. Command reference

```
# Readiness & lifecycle
net check  <pkg>                 host-only verdict: is this app MITM-able?
net trust  [--system|--ui]       install/trust the ShadowDroid CA
net start  [--port 8080] [--apps com.livd] [--foreground]
net stop   [--revoke-ca]         tear down proxy + http_proxy + reverse
net status                       running? device pointed at it? held flows, counts

# Observe
net watch  [matchers]            live http events (same envelope as `watch`)
net log    [matchers] [-n 50]    recall past flows from session JSONL
net show   <id> [--body|--har]   full headers + bodies for one flow
net export <har|curl> [id]       interop hand-off

# Intercept (primary modify)
net intercept <matchers> [--at request|response|both] [--hold-ms 30000]
net resume <id> [mutations…]
net drop   <id> [--status N]
net respond <id> --status N [--body… ]     short-circuit, never hits server

# Static modify (later)
net rule add <kind> <matchers> <args…> | net rule list|rm|clear
net rules apply <file.json>
net replay --from <jsonl> [matchers]
# net script …  — DEFERRED, see §6.2
```

**Matchers** are explicit composable flags — `--host`, `--path`, `--method`,
`--status`, `--content-type` — because an agent emits structured flags more reliably
than a cryptic filter string. A `--filter '~d livd & ~t json'` escape hatch maps onto a
mitmproxy-style subset for power users (see [§10](#10-filter--matcher-model)).

---

## 8. mitmproxy capability map

| mitmproxy capability | Verdict | Lands as |
| --- | --- | --- |
| intercept / pause-resume | **Adopt (primary)** | `net intercept` / `resume` / `drop` / `respond` |
| modify_body / modify_headers | Adopt | `net rule add replace` / `set-header` |
| map_local | Adopt | `net rule add map-local` (fixtures) |
| map_remote | Adopt | `net rule add map-remote` (local backend) |
| blocklist | Adopt | `net rule add block` |
| server_replay | Adopt | `net replay --from …` (offline mock) |
| scripting / addon hooks | **Defer** | agent-in-the-loop intercept covers it for now ([§6.2](#62-embedded-transform-scripts--deferred-out-of-scope-for-now)) |
| filter language (`~u ~d ~t ~c ~m ~h ~b`) | Adapt | explicit `--host/--path/…` + `--filter` escape hatch |
| HAR / curl export | Adopt | `net export har` / `curl <id>` |
| save/load flow stream (`-w`/`-r`) | Adapt | session JSONL (text, not binary dump) |
| anticache / anticomp | Adopt (flags) | `net start --anticache --anticomp` |
| client_replay (re-fire) | Defer | regression re-runs |
| stickycookie / stickyauth | Defer | niche for single-app |
| upstream_cert / client_certs / mTLS | Defer | only for client-cert apps |
| proxy modes (transparent/upstream/socks/wireguard) | Mostly skip | `http_proxy`+`reverse` = regular mode; keep `upstream:` for corp proxies |
| mitmweb / urwid console | Skip | agent + JSON is the UI |
| HTTP/3 / QUIC, raw TCP | Skip (warn) | `net check` flags QUIC/Cronet stacks that bypass the proxy |

---

## 9. Hard edges

- **Hold timeout (decided: fail-open).** A held flow blocks the device's real HTTP
  call, which has its own timeout. Default hold deadline ~30 s (`--hold-ms`); on expiry
  the proxy **fails open** — resumes the flow unmodified so a slow/forgotten agent never
  bricks the app under test (`--on-timeout drop` opts into fail-closed for the rare case
  you'd rather kill it). The `hold_deadline_ms` field tells the agent its budget;
  `net status` shows remaining time per held flow.
- **Attribution.** A system `http_proxy` is shared by all apps, so the proxy sees more
  than the target. Best-effort per-app filter via `uid` → `/proc/net/tcp6`; for
  one-app-under-test a `--host` filter is usually enough. Exact attribution is a future
  VpnService path.
- **Pinning & non-proxy stacks.** Cert-pinned or Cronet/QUIC clients bypass a user-CA
  proxy. `net check` must warn loudly; the fallback is read-only cooperating-app
  capture or a deeper VpnService capture later.
- **CA trust on real devices.** Driving the Settings "Install a certificate" UI needs a
  set lock-screen credential. `net check` gates `net trust --ui`.

---

## 10. Filter / matcher model

Primary surface = explicit flags (`--host`, `--path`, `--method`, `--status`,
`--content-type`), ANDed together. Power escape hatch = `--filter` over a mitmproxy
subset:

| Token | Meaning | Maps to |
| --- | --- | --- |
| `~d <re>` | domain/host | `--host` |
| `~u <re>` | URL | — |
| `~m <re>` | method | `--method` |
| `~c <int>` | status code | `--status` |
| `~t <re>` | content-type | `--content-type` |
| `~h <re>` | header | — |
| `~b <re>` | body | — |
| `& | ! ( )` | and / or / not / group | — |

Glob hosts (`*.livd.app`) in the flag form; full regex in `--filter`.

---

## 11. Integration points & code placement

| Concern | Where | Change |
| --- | --- | --- |
| Command tree | [cli/src/cli.rs](../cli/src/cli.rs) `Cmd` enum (~:125–141) | add `#[command(subcommand)] Net(NetCmd)` |
| Host-only dispatch | cli.rs phase-1 block | `net check`, `net trust --system`, `net start/stop/status/watch/log/show/resume/drop` are host+adb+daemon — no on-device server |
| Server-backed dispatch | cli.rs phase-2 | only `net trust --ui` needs `ensure_ready` (uses `tap`/`find`/`screen`) |
| Event types | [cli/src/events.rs:18](../cli/src/events.rs) | add `Http { … }` and `HttpIntercept { … }` variants |
| CLI verbs | new `cli/src/cmd/net/` (mod, check, trust, proxy, intercept, rule, control) | mirror `cmd/permissions.rs` shape |
| Proxy engine | new `cli/src/net/` (server task, CA, flow model, control socket) | feeds the same event channel the crash detector uses |
| adb helpers | [cli/src/device/adb.rs:140](../cli/src/device/adb.rs) | **add `reverse` / `reverse_remove`** (only `forward` exists) |
| Store | `~/.shadowdroid/net/<serial>.{sock,jsonl}` | reuse `home_dir()` / `directories` |
| `doctor` net section | [cli/src/cmd/doctor.rs:46](../cli/src/cmd/doctor.rs) | new `Check`(s) via shared `net::check`; `--app` arg; dangling-proxy `remedy` for `--fix` — see [§11.1](#111-net-check-inside-doctor) |
| Deps | [cli/Cargo.toml](../cli/Cargo.toml) | hand-rolled proxy → add `hyper`/`hyper-util`, `tokio-rustls`/`rustls`, `rcgen` (most already present transitively via `reqwest`). **No `hudsucker` dep** — reference only |

Lifecycle hooks into existing commands:
- **`net check`** is the gate (host-only, like `doctor`): reads manifest + Network
  Security Config + HTTP engine → verdict (debuggable? NSC trusts user CA? engine
  proxy-aware OkHttp/Ktor-OkHttp vs Cronet/QUIC?). Release builds are hard-blocked.
- **`net trust`** reuses ShadowDroid's *own* UI automation: emulator/root → system
  store; real device → push `.crt`, then drive the Settings cert-install flow with
  `tap`/`find`/`screen`. The proxy installs its trust *by using the rest of the
  product* — the tightest possible integration.
- **`watch --net`** unified stream; **`doctor`** gains a net section (see
  [§11.1](#111-net-check-inside-doctor)); **`collect`** bundles recent flows (and, with
  `--app`, the `net check` verdict); **`disconnect`** / `net stop` tears down proxy +
  `http_proxy` + `reverse` so no orphaned global proxy bricks device networking.

### 11.1 `net check` inside `doctor`

Yes — and the split matters. `doctor` today is **package-agnostic** (it diagnoses the
ShadowDroid pipe: device / apk / server / owners / clock) and models every probe as a
`Check { code, status: Ok|Warn|Fail, detail, remedy }` with `--fix` running the remedy
([doctor.rs:46](../cli/src/cmd/doctor.rs)). `net check` is **package-specific**. So they
compose in two layers, sharing the same underlying check functions (single source of
truth — `net check` and `doctor` call into one `net::check` module):

1. **`doctor` (no app) gains a package-agnostic `net` check.** Probes that are pure
   device/host state, exactly doctor's category:
   - proxy daemon running? control socket alive?
   - device `settings global http_proxy` — and crucially, is it **dangling** (set, but
     nothing listening)? A dangling proxy silently kills the device's networking and is
     precisely the "device left in a weird state" failure doctor exists to catch.
   - orphaned `adb reverse` rule?
   - is the ShadowDroid CA present in the device trust store? (info)

   Status logic avoids false-positive nagging: a clean *no-proxy/no-CA/no-setting* state
   is **`Ok` — "net: inactive"**, not a failure. Only an *inconsistent* state warns —
   chiefly the dangling `http_proxy`, which gets a `remedy` ("clear `global http_proxy`
   + remove reverse") so **`doctor --fix`** repairs it, the same way it already clears a
   stale `adb forward` or kills a stuck owner.

2. **`doctor --app <pkg>` additionally runs the full `net check <pkg>` verdict** —
   debuggable? NSC trusts user CA? engine proxy-aware (OkHttp/Ktor-OkHttp ✅ vs
   Cronet/QUIC ⚠)? — emitted as extra `Check`s. (`--app` mirrors `collect --app`; doctor
   doesn't take one today, so this is a small addition. The standalone `net check <pkg>`
   stays as the focused entry point.)

This means a developer debugging "why did my emulator lose internet" runs plain
`doctor` and immediately sees (and `--fix`es) a dangling proxy — value even for people
who only occasionally touch `net`.

---

## 12. Phased delivery

| Phase | Goal | Done when… |
| --- | --- | --- |
| **P1** | Observe via inline proxy | `net check` / `trust` / `start` / `stop` / `status`; `net watch`/`log`/`show`; `http` event in `watch --net`; **`doctor` net section** (dangling-proxy detect + `--fix`) and **`doctor --app <pkg>`** verdict, sharing the `net::check` module. Hand-rolled proxy + daemon + control socket + CA pipeline proven on emulator vs `com.livd`. |
| **P2** | Agent-in-the-loop intercept (**headline**) | `net intercept`/`resume`/`drop`/`respond` with fail-open hold-timeout; agent forces a 401 on `/v1/login` and confirms the error screen. |
| **P3** | Static rules | `net rule add …` (map-local/remote, set-status/header, replace, block, delay) + `net rules apply` + `net replay`. |
| **P4** | Interop & polish | HAR/curl export, anticache/anticomp, `uid` attribution, QUIC/pinning warnings in `net check`. |
| — | ~~Transform scripts~~ | **Deferred** ([§6.2](#62-embedded-transform-scripts--deferred-out-of-scope-for-now)). |

Principle (same as the delivery plan): **end-to-end loop before depth.** P1 ships an
inline proxy that only observes — already strictly more than read-only capture, and it
validates the hardest part (CA trust) before any modification logic exists.

---

## 13. Decisions

**Settled (2026-06-16):**
1. **Proxy engine — hand-rolled** on `hyper` + `tokio-rustls` + `rcgen`, using
   `/Users/andrii/Work/hudsucker` as a *reference* (not a dependency). See [§3](#3-architecture).
2. **Primary modify mechanism — agent-in-the-loop interception** ([§5](#5-agent-in-the-loop-interception-primary-modify-mechanism)).
3. **Transform scripts — deferred** ([§6.2](#62-embedded-transform-scripts--deferred-out-of-scope-for-now)); no scripting language or dependency for now.
4. **Hold-timeout — fail-open** (resume unmodified on expiry) ([§9](#9-hard-edges)).
5. **`net check` lives in both** the standalone command *and* `doctor` (package-agnostic
   net section + `doctor --app` verdict), via a shared `net::check` module ([§11.1](#111-net-check-inside-doctor)).

**Still open:**
- **Attribution depth** — ship host-filter-only in P1–P3, add `uid`/`/proc/net`
  correlation in P4, leave exact per-process attribution to a future VpnService path.

---

## Relationship to the read-only capture plan

The earlier direction (Route C: app emits its own traffic to logcat under
`SHADOWDROID_NET`, host tails it; with host-side MITM as a *fallback* Route A) is
**inverted** by the modify/transform requirement:

- Inline MITM proxy is now **primary** (it's the only thing that can modify).
- App-cooperation log capture becomes the **observe-only fallback** for apps you can't
  proxy (pinned/QUIC/release) — still valuable, still emits the same `http` event.

Note: the `clients/` capture libraries and the prior `docs/net-capture-plan.md`
described in project memory are **not present in the current tree** — treat the
on-device capture path as un-started. This plan stands on its own.
