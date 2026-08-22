# App, device, and state controls

[← README](../README.md)

App install/start/stop/clear/info, runtime permissions, app-ops, device
power/orientation/clipboard/notifications, display profiles, and on-device file
push/pull live in the same CLI as UI automation. This page covers the contracts
that make those mutations trustworthy.

## Verified mutations

Mutation commands verify the requested postcondition instead of trusting an
empty Android shell response. Permission/app-op changes, profile apply/reset,
explicit file modes, app clear/stop, install steps, and goal-directed
scroll/focus operations exit non-zero when readback disagrees, with requested
and observed state in `detail`.

## App-ops scopes

`appops get <package> [op]` reports `uid_mode` and `package_mode` separately,
plus the `governing_scope` and `effective_mode`; UID policy takes precedence
when Android returns both. `appops set` requires `--scope uid` or
`--scope package` and verifies that exact layer, preventing an apparently
successful package change from hiding an unchanged governing UID mode.

## Display profiles

`profile apply --file` accepts the JSON shape written by `profile snapshot` and
rejects unknown, empty, or unsafe values. Values remain JSON strings:
animation scales must be finite and non-negative, `font_scale` finite and
positive, density a positive integer, size positive `WxH`, auto-rotation and
stylus flags `0`/`1`, and user rotation `0`–`3`. A file conflicts with CLI
setting overlays so no supplied value is silently ignored.

## Files

For shared/FUSE storage, omit `files push --mode`; when `--mode` is explicit,
failure to apply it is a typed error even if the bytes were transferred.

Private debuggable-app files are available without raw shell redirection:

```bash
shadowdroid files pull --run-as --app com.example.app files/state.json local.json
shadowdroid files push --run-as --app com.example.app --mode 600 local.json files/state.json
```

Contents are byte-preserving and never printed.

## App state snapshot and restore

For cross-version regression state, snapshot selected files or whole
directories while the app is force-stopped, then restore them under the app
UID:

```bash
shadowdroid app state snapshot --app com.example.app \
  --out /tmp/example-state \
  --include shared_prefs \
  --include files/session-state.json \
  --include databases/app.db

shadowdroid app state restore --from /tmp/example-state
shadowdroid app state cleanup --from /tmp/example-state
```

The protected snapshot directory is `0700`; files and `manifest.json` are
`0600`. The manifest records package/version, a stable Android signing identity
digest, every relative path, bytes, SHA-256, mode, selected roots, automatically
included SQLite `-wal`/`-shm`/`-journal` sidecars, and
`contains_sensitive_data:true`. It is deliberately marked unencrypted.
`cleanup` overwrites before deletion but warns that SSD/COW/journaled storage
cannot guarantee physical erasure.

Restore refuses a package/signature mismatch unless `--allow-incompatible` is
explicit. It stages all data privately, atomically claims a recovery marker,
swaps each selected root, and records `verified` only after every hash/mode
check passes. The pending-marker rename is the commit point; rollback data is
garbage-collected only afterward. If a restore is interrupted while
`prepared`, `app state recover --app <pkg>` rolls it back. If it is interrupted
after `verified`, recovery finishes the commit instead. Recovery is idempotent,
and a no-pending recovery does not stop a running app. Snapshot and restore
leave the app force-stopped; recovery does so only for an active transaction.
This protects against fail-stop CLI/ADB interruptions, not sudden device
storage loss without filesystem durability guarantees.

## Screen video evidence

Use `video record` for a bounded foreground capture:

```bash
shadowdroid video record -o /tmp/checkout-repro --duration 30s
```

For an interactive run, start a detached recorder and add human-readable
timeline boundaries while driving the app:

```bash
shadowdroid video start -o /tmp/checkout-repro
shadowdroid video mark "before checkout"
shadowdroid video status
shadowdroid video mark "payment failed"
shadowdroid video stop
```

Both capture paths write a private-mode bundle on Unix (`0700` directories and
`0600` files; other platforms are labelled `platform_default`) containing
`manifest.json`, `events.jsonl`, and numbered MP4 files under `segments/`. When
compatible segments can be assembled losslessly, the bundle also contains
`video.mp4`; the segments remain the authoritative capture if assembly is
unavailable. Recording is segmented before Android's `screenrecord` time limit
so a long run does not silently end, and `stop` finalizes the active segment
before returning.

`record` and `start` accept `--backend auto|screenrecord`, plus `--size`,
`--bit-rate`, `--display-id`, `--bugreport`, and `--segment-seconds`.
`auto` selects the available built-in path; explicit `screenrecord` is useful
when reproducibility matters. Device support for size, display selection, and
bugreport overlays varies, so unsupported combinations fail rather than being
silently ignored. The built-in backend captures screen video only, with no app,
device, or microphone audio.

MP4 pixels are never redacted, including when global `--redact` is enabled.
The flag does filter marker labels, but structural device/session identifiers
needed for crash recovery remain in the manifest and timeline. The manifest
therefore labels every bundle sensitive and unencrypted. Treat the whole
directory as private evidence and inspect every clip before sharing it.

Per-segment `sample_count` and `media_duration_ms` report encoded coverage.
Marker offsets and rollover gaps are host-observed approximations, explicitly
labelled as such in the manifest; use them to navigate evidence, not as
frame-accurate media timestamps. A static or very short capture can contain only
one frame: ShadowDroid preserves that segment, reports `playable: false`, marks
the session `partial`, and does not publish a misleading `video.mp4`.
