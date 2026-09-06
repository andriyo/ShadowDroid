# Investigation checkpoints

`evidence checkpoint` links a fresh structured screen snapshot, a network
checkpoint, a video marker and selected app/HTTP fields. It reuses existing
sessions; it does not start a device, UI server, proxy or recorder.

```sh
shadowdroid --target tv evidence checkpoint 'after refresh' --out investigation --spec probes.json
shadowdroid evidence timeline investigation
shadowdroid video coverage recording-bundle
```

The output directory must be new or an existing evidence bundle. Checkpoints
are serialized with a local lock, saved atomically as separate JSON files,
and never overwrite earlier evidence. A missing probe returns nonzero
`evidence_partial` **after saving available evidence**. Inspect its `errors`
and saved path. Each checkpoint has capture start/end times and explicitly
states that the reads are not atomic.

A projection spec selects fields instead of copying whole private files or
network bodies. For example (replace the package, key and hosts):

```json
{
  "storage": [{
    "name": "stored-session",
    "package": "com.example.app",
    "path": "shared_prefs/session.xml",
    "preference_key": "session",
    "fields": {
      "classification": {"pointer": "/contextState/accessTokenType"},
      "access-fingerprint": {"pointer": "/context/accessToken", "mode": "sha256"},
      "subject-kind": {"pointer": "/context/accessToken", "mode": "jwt_claim", "claim": "subt"}
    }
  }],
  "flows": [{
    "name": "refresh-response",
    "host": "api.example.com",
    "path": "/graphql",
    "operation_name": "RefreshToken",
    "body": "response",
    "fields": {
      "classification": {"pointer": "/extensions/access_token_type"}
    }
  }, {
    "name": "sdk-events",
    "host": "telemetry.example.com",
    "body": "request",
    "events_pointer": "/logs",
    "event_time_pointer": "/event_start_time",
    "time_unit": "milliseconds",
    "correlation_pointer": "/conversation_id",
    "fields": {
      "action": {"pointer": "/action_name"},
      "version": {"pointer": "/sdk_version"}
    }
  }]
}
```

Omit `preference_key` for a private JSON file. Storage reads are limited to
1 MiB and reject files that change size while being read. XML entities are
decoded, and duplicate preference keys or DTDs fail rather than guessing.
Projection modes are `value`, `sha256`, and `jwt_claim`. JWT payload decoding
does not verify a signature; those rows say `signature_verified:false`.

Fields are rows with `name`, `present`, and either `value`, `fingerprint`, or
`unavailable`. An absent field is `present:false`; a present JSON null remains
`present:true,value:null`. Fingerprinting already-redacted input reports
`source_redacted` instead of hashing a shared placeholder. Select fingerprints
for credentials; redaction is applied by default to retained values, but
explicit field selection still determines what data enters the bundle.

Flow probes use host/path substrings and optional exact GraphQL operation
names. `--since-seconds` defaults to 300; `--flow-limit` defaults to 100 recent
flows across the device. These bounds can omit evidence; a checkpoint is not
a complete traffic archive. Captured bodies may already be redacted or
truncated. Keep the original capture when deeper analysis may be needed.

The timeline deduplicates the same flow/event seen in multiple checkpoints.
It retains `observed_at_ms` (HTTP capture time), `event_time_ms` (payload time),
and a hashed `correlation_ref`. Event time determines ordering when present;
otherwise observation time does. Clocks are **not** automatically synchronized.
The checkpoint ID links the projected records, screen and video marker, while
`capture_ref` separates network capture sessions.

Files are local and private (0600, directory 0700 on Unix). No raw private
file is written by the projection command. The bundle includes a structured
screen, not a screenshot; use existing screenshot/video capture for pixels.
