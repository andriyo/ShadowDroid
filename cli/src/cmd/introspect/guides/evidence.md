# Evidence checkpoints and recording coverage

Use `evidence checkpoint` to correlate a fresh structured screen snapshot,
network checkpoint, video marker and selected app/HTTP fields. It reuses
existing sessions and never starts a device, UI server, proxy or recorder.

```sh
shadowdroid evidence checkpoint 'after refresh' --out investigation --spec probes.json
shadowdroid evidence timeline investigation
shadowdroid video coverage recording-bundle
```

The output must be a new directory or an existing evidence bundle. Checkpoints
are saved atomically under a local lock and never overwrite earlier evidence.
A missing source produces nonzero `evidence_partial` after saving what was
available; inspect the saved path and `errors`. Capture start/end timestamps
bound the observations, which explicitly declare `capture_is_atomic:false`.

## Project only the fields needed

Storage probes require a debuggable app with working `run-as`. Replace the
package, file, preference key and endpoint in this spec:

```json
{
  "storage": [{
    "name": "session",
    "package": "com.example.app",
    "path": "shared_prefs/session.xml",
    "preference_key": "session",
    "fields": {
      "token-fingerprint": {"pointer": "/accessToken", "mode": "sha256"},
      "classification": {"pointer": "/accountType"}
    }
  }],
  "flows": [{
    "name": "refresh",
    "host": "api.example.com",
    "path": "/graphql",
    "operation_name": "RefreshToken",
    "body": "response",
    "fields": {"classification": {"pointer": "/extensions/accountType"}}
  }]
}
```

Omit `preference_key` for a private JSON file. Reads are bounded to 1 MiB and
reject a file that changes size. SharedPreferences XML entities are decoded;
duplicate keys, nested string XML and DTDs are rejected.

Projection modes are `value` (default), `sha256`, and `jwt_claim` with an
explicit `claim`. Value projections preserve source keys, sensitive ancestors,
and known-value aliases during built-in/configured redaction. Fingerprints and
JWT claims use original input; JWT payload decoding does not verify signatures
and reports `signature_verified:false`. Prefer fingerprints for credentials.

Rows have `name`, `present`, and `value`, `fingerprint`, or `unavailable`.
Missing fields report `present:false`; a present JSON null stays null. An
already-redacted source or a path hidden by a redacted ancestor reports
`unavailable:"source_redacted"` rather than a fabricated value/fingerprint.

Flow matching uses host/path substrings and optional exact operation names.
`--since-seconds` defaults to 300 and `--flow-limit` to 100 recent device flows;
this can omit evidence. Retain original captures for deeper analysis: bodies
may already be redacted or truncated.

For telemetry arrays, set `events_pointer`, `event_time_pointer`, `time_unit`
(`seconds` or `milliseconds`), and optionally `correlation_pointer`. The timeline
deduplicates repeated flow/events, keeps observation and event timestamps, and
hashes correlation values. It orders by event time when present, otherwise by
observation time; clocks are not synchronized automatically. Checkpoint IDs
link screen, fields and marker; `capture_ref` distinguishes capture sessions.

Bundle files are local/private (0600, directory 0700 on Unix). No raw private
file is saved. The bundle contains a structured screen, not screenshot pixels.

## Verify media coverage

```sh
shadowdroid video record --out recording-check --duration 5s
shadowdroid video coverage recording-check
```

New video manifests have `coverage.json`; `video status` updates coverage as
segments finalize. `video coverage <bundle>` works offline, including older
bundles with a manifest and no coverage file. It reports host intervals,
verified encoded durations, missing segments, rollover gaps, concatenated
media ranges and enclosing marker ranges. Export ranges require a completed
`video.mp4` manifest artifact.

A running recorder does not prove encoded coverage: static screens can produce
few samples or a zero-duration segment. Markers are not frame-synchronized, so
`export_offset_ms` is null and only an enclosing segment is supported. Missing
segments must not move their markers into unrelated footage. Video pixels are
not redacted.
