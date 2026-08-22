# Security, privacy, and redaction

[← README](../README.md)

ShadowDroid reads real screens, logs, files, and network traffic from a real
device. Everything below exists to keep that evidence from leaking secrets or
personal data by accident — and to be explicit where redaction *cannot* help.

## Redacting secrets and PII

Pass global `--redact` before or after any subcommand to filter JSON and text at
the shared output boundary:

```bash
shadowdroid --redact ui dump
shadowdroid log --last 2m --redact
shadowdroid --redact collect --app com.example.app --redact-screenshots
shadowdroid net start --redact
```

Built-ins cover password/secret/token/cookie fields, JWTs, bearer values,
emails, IPv4/IPv6 addresses, usernames/phone values, and common session,
device, transaction, and serial identifiers. JSON embedded in log or GraphQL
body strings is parsed and retains its structure. Each emitted record carries
the policy version and replacement count. Config can enable the policy for all
commands and add key names or Rust-regex patterns; user and project additions
are merged, with the nearest `enabled` value winning.

## The network boundary

`net start --redact` applies the same policy to completed capture copies
before they reach the session log, including URL path/query values, hosts,
connection errors, WebSocket metadata, and TLS failures. In-app OkHttp flows
drained by `aar capture` cross the same policy boundary before stdout, JSONL,
fixtures, or the shared store. A live daemon reports its capture-policy
fingerprint; requesting `--redact` refuses to reuse an unredacted or differently
configured daemon and asks for an explicit stop/start. Request/response bytes
forwarded to the app or upstream are never changed. The session log is written
`0600` either way.

## Pixels are different

`ui screenshot` reports raw pixels as potentially sensitive. Explicit
`--redact-pixels` (PNG only) blacks out matching accessibility bounds, but
still reports that Android may not expose every rendered glyph. `collect`
marks screenshots potentially sensitive unless `--redact-screenshots`
explicitly blacks out matching bounds.

Video is a stricter boundary: global `--redact` does not alter MP4 pixels.
Every `video` bundle is marked as containing sensitive, unencrypted data;
review its clips before sharing it. For video bundles, `--redact` filters
marker labels; device/session identity and recovery paths remain in the
protected manifest, so the JSON timeline is still marked potentially sensitive.
Screen recording is video-only — it does not capture device, app, or
microphone audio.

## Secrets in commands

`ui pin` enters a numeric PIN without echoing the digits back in action JSON,
and deliberately refuses `--observe`/`--expect-*` so a returned screen cannot
reveal an unmasked keypad. The PIN is still present in the local process
invocation, so use the host's normal shell-history and process-visibility
protections. See [agent-loop.md](agent-loop.md).

App state snapshots (`app state snapshot`) are written `0700`/`0600`, record
SHA-256 hashes and a signing-identity digest, and are deliberately marked
`contains_sensitive_data:true` and unencrypted. `cleanup` overwrites before
deletion but cannot guarantee physical erasure on SSD/COW/journaled storage.
See [device-state.md](device-state.md).

## Config cannot inject shell

Repository config cannot supply executable device-shell fragments: app
packages are validated at deserialization, permission/app-op tokens at their
command boundary, and accepted values are quoted before entering an Android
shell command. Values containing `;`, newlines, `$()`, quotes, or whitespace
fail with a typed `invalid_*` error. See
[configuration.md](configuration.md).

## Trust boundaries of the on-device pieces

- On first `connect`, the CLI auto-installs a **version-matched APK pair**
  downloaded from the matching GitHub Release, **SHA-256 verified**, and cached
  under `~/.shadowdroid/apks/<version>/`. When working inside the ShadowDroid
  repository it prefers local Gradle build outputs.
- The wire protocol is loopback HTTP/JSON over an adb forward; the supported
  public interface for agents is the CLI surface and
  `shadowdroid commands --json`.
- The `net` MITM proxy installs its CA into the device's **user** trust store
  (`net trust`); apps that opt out of user CAs, pin certificates, or bypass the
  system store reject it, reported as `tls_error`. Forwarded bytes are never
  modified unless you explicitly intercept, rule, or replay.
- The in-app AAR is **debug-only**, added explicitly to builds you control, and
  its OkHttp capture requires the app to register the interceptor itself. See
  [network.md](network.md).

## The optional local usage log

Opt in to a **local usage log** with `shadowdroid usage enable`. Schema
version 2 records only the command path, duration, CLI version, outcome, and
typed error code/stage/retry posture — never argument values — and never
uploads anything:

```bash
shadowdroid usage enable
shadowdroid usage report --days 30 | jq \
  '{verbs, error_codes, error_stages, versions, recommendations, feedback_loop}'
```

Recommendations require repeated evidence: high error rates, slow p95s, or
recurring error codes. The report suggests the next engineering action but does
not edit code. Reproduce the evidence, add a regression, implement the change,
then compare error rate and p95 by version.

## Diagnostics stay passive

`why` is non-mutating: it reads the on-device server only if it is already
reachable and never installs, starts, or forwards anything to get a screen.
`collect` is passive with respect to device lifecycle: the selected device/AVD
must already be online, it only reads server-backed evidence through an
already-established session, and it never starts an AVD, server, or adb
forward. Its manifest records privacy status per artifact.
