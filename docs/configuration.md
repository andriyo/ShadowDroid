# Configuration

[← README](../README.md)

Put repeated values in config instead of spending prompt/context on every
command. Config lives in a folder: the global `~/.shadowdroid/config.json` is
loaded first, then a project `.shadowdroid/config.json` from each of the current
directory's ancestors, with the nearest project file winning.

```bash
shadowdroid config schema --json
shadowdroid config init --project --app Livd --package com.livd --app-target mobile \
  --default-target mobile --target-name mobile --target-avd Livd_Pixel_9_API_36 \
  --target-start if-needed --target-form-factor mobile \
  --project-path /Users/you/Work/Livd --json
shadowdroid config validate --json
```

```json
{
  "default_target": "mobile",
  "app": "Livd",
  "project": "/Users/you/Work/Livd",
  "redaction": {
    "enabled": true,
    "json_keys": ["customerId"],
    "patterns": ["ORDER-[0-9]+"]
  },
  "proxy": {
    "ca_trusted": true,
    "hosts": ["*.livd.app"],
    "trust_store": "user"
  },
  "apps": {
    "Livd": {
      "package": "com.livd",
      "run_configuration": "app",
      "debugger": "Android Debugger",
      "target": "mobile"
    }
  },
  "targets": {
    "mobile": {
      "avd": "Livd_Pixel_9_API_36",
      "start": "if-needed",
      "form_factor": "mobile"
    },
    "tv": {
      "avd": "Livd_TV_API_35",
      "start": "if-needed",
      "form_factor": "tv"
    }
  }
}
```

## Named targets

Named targets make multi-project and mobile/TV work deterministic. ShadowDroid
matches a running emulator by its stable AVD name, discovers its current adb
serial, and reuses it. If none is running, `start: "if-needed"` opts into
starting that existing AVD and waiting for Android to finish booting; the
default policy is `never`. ShadowDroid never silently creates an AVD. Physical
devices use a target entry with `"serial": "..."` and are never auto-started.

Use `shadowdroid --target tv connect` to override `default_target`; explicit
`-d/--device` has highest precedence. `shadowdroid devices` stays passive and
includes the AVD name when Android exposes it. AVD claims under
`~/.shadowdroid/targets/claims/` prevent two projects from silently sharing one
emulator — and therefore its accounts, proxy, and single UiAutomation slot. Use
`--takeover` only for an intentional reassignment. Startup uses the Android SDK
emulator found via `ANDROID_SDK_ROOT` / `ANDROID_HOME` / PATH; set
`SHADOWDROID_EMULATOR` for an explicit executable.

AVD names are host configuration. Commit target bindings only when the team
standardizes those names; otherwise define `targets` in the user config and
keep only the project's `default_target` / app `target` roles in project config.

## Project path and source mapping

The `project` path matters for debugging: `why` and `log` use it to map crash
stack frames back to files in your source tree (`project_frames`), so the agent
gets `app/src/main/java/.../CartRepo.kt:42` instead of a bare class name.

## Editing and recovery

`config init` changes only explicitly supplied fields, deep-merges an existing
app alias, validates Android identifier fields, and atomically replaces the
target file. Treat a committed project config as repository input, never as a
place for shell fragments. If a malformed config prevents an ordinary command
from loading, recovery commands still work because they run before normal
config loading:

```bash
shadowdroid config paths --json
shadowdroid config validate --json  # non-zero config_invalid with report in detail
shadowdroid config schema --json
shadowdroid commands --json --depth 1
```

## Injection safety

Repository config cannot supply executable device-shell fragments: app
packages are validated while the config is deserialized, permission/app-op
tokens are validated at their command boundary, and accepted values are quoted
before entering an Android shell command. Values containing `;`, newlines,
`$()`, quotes, or whitespace fail with a typed `invalid_*` error.

## Per-project proxy CA

The `net` MITM proxy signs with a CA that ShadowDroid resolves per invocation:
an explicit `proxy.ca_cert`/`proxy.ca_key` in config (absolute or `~/` paths),
else a per-project convention CA at `<project>/.shadowdroid/ca.{crt,key}`, else
the global `~/.shadowdroid/net/ca.{crt,key}`. Mint a project CA with
`shadowdroid net ca reset --project` (or import your own with `net ca import
--project --cert …`); `config init --project` and the project-scoped `net ca`
verbs write a `.shadowdroid/.gitignore` so the CA cert, key, and `.bak` backups
are never committed.

Set `proxy.ca_trusted: true` to tell ShadowDroid the CA is already trusted on
the device (e.g. baked into a custom emulator image) — `net trust`/`net check`
then skip the adb install and trust-store readback and report the basis as
`asserted`. Even without it, a successful `net trust`/`net check` is cached per
device (keyed by CA fingerprint), so repeat runs skip the probe; pass `--fresh`
to force a real check.

See [network.md](network.md) for the full proxy workflow.
