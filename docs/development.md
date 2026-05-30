# Development workflow

This doc is for **you, the person editing the Kotlin server or the Rust CLI** — not for the end user who just `cargo install shadowdroid`s and never thinks about the APK.

## The inner loop

The thing you'll do dozens of times a day is:

> "I changed a Kotlin file. Rebuild the APK, reinstall it on the emulator, run my CLI command, see what happened."

ShadowDroid is designed so this loop is **three commands**:

```bash
# 1. Rebuild the APK
(cd server && ./gradlew :app:assembleDebug :app:assembleDebugAndroidTest)

# 2. Reinstall (any of the dev-mode invocations below)
shadowdroid connect

# 3. Run your CLI verb
shadowdroid screen | jq
```

Step 2 picks up the freshly-built APK automatically (see "How `shadowdroid` finds the APK" below). You don't have to copy files anywhere or update any paths.

## How `shadowdroid` finds the APK

Resolution order (first hit wins). Documented exhaustively in
[architecture.md §4](architecture.md#first-run) and in the
[`installer.rs`](../cli/src/device/installer.rs) module header.

| # | Source                          | When you'd use it                                                                                |
| - | ------------------------------- | ------------------------------------------------------------------------------------------------ |
| 1 | `--apk PATH` flag               | One-off: testing a specific build, e.g. a colleague's artifact                                   |
| 2 | `SHADOWDROID_APK` env var       | Sticky for one shell session                                                                     |
| 3 | Repo auto-discovery             | **Default for active development.** Just `cd` into the repo and run `shadowdroid`                |
| 4 | `~/.shadowdroid/apks/local/`    | You hack on the server in one repo and use the CLI from a different one                          |
| 5 | `~/.shadowdroid/apks/<v>/`      | Cached after a previous GitHub download                                                          |
| 6 | GitHub release fallback         | What end users hit. You won't normally exercise this in dev                                      |

Sources 1–4 are **dev mode** — the CLI skips the version check and trusts that whatever APK you pointed at is what you want. A banner like

```
shadowdroid: using local APK at server/app/build/outputs/apk/androidTest/debug/app-debug-androidTest.apk (dev mode)
```

prints once per `connect` so you're never surprised.

## Three common dev setups

### A. "I'm working in the ShadowDroid repo" (most common)

```bash
cd ShadowDroid
(cd server && ./gradlew :app:assembleDebug :app:assembleDebugAndroidTest)

# Repo auto-discovery picks up the build output:
cargo run -p shadowdroid -- connect
cargo run -p shadowdroid -- screen | jq
```

No flags, no env vars. The CLI walks up from `$CWD` looking for
`server/app/build/outputs/apk/androidTest/debug/*-androidTest.apk` and
installs that pair.

### B. "I'm using the CLI from another project, with a local APK build"

```bash
# In the ShadowDroid repo:
(cd server && ./gradlew :app:assembleDebug :app:assembleDebugAndroidTest)
mkdir -p ~/.shadowdroid/apks/local
cp server/app/build/outputs/apk/debug/app-debug.apk          ~/.shadowdroid/apks/local/main.apk
cp server/app/build/outputs/apk/androidTest/debug/app-debug-androidTest.apk ~/.shadowdroid/apks/local/test.apk

# Now from anywhere:
cd ~/Work/some-other-app
shadowdroid connect      # picks up from ~/.shadowdroid/apks/local/
```

### C. "I'm testing a one-off APK from a CI run"

```bash
shadowdroid --apk ~/Downloads/shadowdroid-test.apk connect
# or
SHADOWDROID_APK=~/Downloads/shadowdroid-test.apk shadowdroid connect
```

The CLI accepts either the **test APK** (it'll find the sibling main APK in the
same directory) or a **directory** containing both.

## Forcing a reinstall

The dev-mode reinstall check is **APK-hash based**, not version-based. If the
APK file on disk has the same SHA-256 as what's installed on device, we skip
the install. Two ways to force a reinstall:

```bash
# 1. Uninstall on the device
adb shell pm uninstall io.github.andriyo.shadowdroid.test
adb shell pm uninstall io.github.andriyo.shadowdroid
shadowdroid connect

# 2. Touch the APK so its hash changes (or just rebuild)
(cd server && ./gradlew :app:assembleDebugAndroidTest)
shadowdroid connect
```

## Iterating on the Rust CLI

```bash
cd cli
cargo run -- connect             # rebuilds and runs in one go
cargo run -- screen | jq
cargo test                        # unit tests use wiremock for the HTTP server
cargo clippy
```

`cargo run -p shadowdroid -- <args>` from the repo root works equivalently.

## Iterating on the Kotlin server

The Kotlin code lives in `server/app/src/androidTest/`. Standard Android
Studio import works — open the `server/` directory as a project. Gradle sync
will set up the AndroidX UI Automator + Ktor 3 dependencies.

```bash
# Build only:
(cd server && ./gradlew :app:assembleDebugAndroidTest)

# Build + install + start (without our CLI, useful for raw debugging):
(cd server && ./gradlew :app:installDebug :app:installDebugAndroidTest)
adb shell am instrument -w \
  io.github.andriyo.shadowdroid.test/io.github.andriyo.shadowdroid.ShadowDroidRunner

# Then curl directly:
adb forward tcp:7912 tcp:7912
curl http://127.0.0.1:7912/v1/state
```

The `curl` path is useful when you want to see exactly what the server sends
without the CLI's parsing in the way.

## What happens when I `cargo install shadowdroid`?

End-user flow — none of sources 1–4 fire because there's no repo, no
`~/.shadowdroid/apks/local/`, and no env var. The CLI falls through to source
5 (versioned cache, missing on first run) and finally source 6, which downloads
the release-signed APK from GitHub and caches it under
`~/.shadowdroid/apks/0.1.3/`. From then on, every `shadowdroid` invocation hits
source 5 in a millisecond.

You can simulate this locally by setting `SHADOWDROID_DISABLE_DEV_SOURCES=1`.
That skips repo auto-discovery and `~/.shadowdroid/apks/local/`, so the CLI uses
the versioned cache or GitHub Release path even when you are running from inside
the repo.
