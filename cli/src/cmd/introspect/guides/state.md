# App, device, permission, and file state guide

Strict semantics for the mutation verbs. All of them verify readback:
permission/app-op changes, profile apply/reset, explicit file modes, app
clear/stop, and goal-directed scroll/focus fail non-zero when the requested
state was not reached; inspect requested and observed state in `detail`.

## App-op scoping

App-op reads keep UID and package modes separate because a UID mode governs a
package mode on modern Android. `appops set` therefore requires `--scope uid`
or `--scope package` and verifies that exact scope; inspect `effective_mode` and
`governing_scope` before deciding which scope to mutate:

```bash
shadowdroid appops get com.example.app CAMERA
shadowdroid appops set com.example.app CAMERA ignore --scope uid
```

## Device profiles

`profile apply --file` accepts only the strict JSON shape produced by `profile
snapshot`: no unknown/empty fields; finite non-negative animation scales;
positive finite font scale; positive integer density; positive `WxH`; `0`/`1`
auto-rotation and stylus flags; user rotation `0`–`3`. The file conflicts with
CLI setting overlays.

## File modes

`files push --mode` is optional: omit it for Android shared/FUSE storage, and
expect a typed postcondition failure if an explicitly requested mode cannot be
applied.

## Private files and app state

Private file and state commands require an installed debuggable package and
working Android `run-as`. They never print file contents:

```bash
shadowdroid files pull --run-as --app com.example.app files/state.json local.json
shadowdroid app state snapshot --app com.example.app --out /tmp/app-state \
  --include shared_prefs --include databases/app.db
shadowdroid app state restore --from /tmp/app-state
```

State snapshots are unencrypted sensitive directories protected as
`0700`/`0600`; the manifest records package/version/signing identity, SHA-256,
size, and mode. Restore force-stops the app, refuses incompatible
package/signature state unless explicitly overridden, stages and verifies
before deleting rollback data, and leaves a marker on interruption. Use
`app state recover --app <pkg>` when that marker is reported and
`app state cleanup --from <snapshot>` for best-effort overwrite/delete.
