# Getting started

ShadowDroid is shipped as a host CLI plus a version-matched Android
instrumentation APK pair. Users install only the CLI; the CLI downloads,
verifies, caches, and installs the APKs during `shadowdroid connect`.

## Install the CLI

Homebrew:

```bash
brew install andriyo/tap/shadowdroid
```

macOS / Linux:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/andriyo/ShadowDroid/releases/latest/download/shadowdroid-installer.sh | sh
```

Scoop:

```powershell
scoop bucket add andriyo https://github.com/andriyo/scoop-bucket
scoop install shadowdroid
```

Windows PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/andriyo/ShadowDroid/releases/latest/download/shadowdroid-installer.ps1 | iex"
```

The shell installer writes to `~/.local/bin` by default. The PowerShell
installer writes to `%LOCALAPPDATA%\ShadowDroid\bin` and adds that directory to
the user PATH. Those direct installers also seed global agent skills. If you
install with Homebrew, Scoop, or Cargo, run `shadowdroid init` once after
install.

ShadowDroid also needs Android Platform Tools (`adb`) on PATH before
`shadowdroid connect` can talk to a device. The installers print a hint if `adb`
is missing. On macOS, use `brew install --cask android-platform-tools`; on
Windows with Scoop, use `scoop install adb`.

## Initialize host integrations

Run the first-run check:

```bash
shadowdroid init
```

This installs or updates global agent skills, installs/updates the Android
Studio plugin when Android Studio is detected, reports Studio/plugin state, and
checks whether the debugger bridge has registered recently. To skip Studio
plugin installation and only inspect Studio while installing skills:

```bash
shadowdroid init --no-studio-plugin
```

If several Android Studio installations are detected, choose one explicitly:

```bash
shadowdroid studio install --studio "/Applications/Android Studio.app"
```

`studio install` resolves the plugin ZIP in this order:

1. `--plugin PATH`
2. `SHADOWDROID_STUDIO_PLUGIN`
3. repo build outputs under `shadowdroid-plugin/build/distributions/...`
4. `~/.shadowdroid/plugins/local/*.zip`
5. `~/.shadowdroid/plugins/<version>/shadowdroid-studio-plugin.zip`
6. GitHub Release asset: `shadowdroid-studio-plugin.zip`

Release downloads are verified with `SHA256SUMS` before they are cached. The
installer unpacks the plugin into Android Studio's user plugin directory and
prints a restart reminder. If Android Studio cannot be found automatically,
pass `--studio` with the `.app`, install directory, `product-info.json`, or
launcher path.

## Configure repeated defaults

ShadowDroid loads JSON config from `~/.shadowdroid/config.json`, then from every
`.shadowdroid.json` in the current directory's ancestor chain. Project config
wins over user config, so agents can omit repeated flags:

```bash
shadowdroid config paths --json
shadowdroid config schema --json
shadowdroid config init --project --app Livd --package com.livd --project-path /Users/you/Work/Livd
shadowdroid config validate --json
```

```json
{
  "device": "emulator-5554",
  "app": "Livd",
  "project": "/Users/you/Work/Livd",
  "apps": {
    "Livd": {
      "package": "com.livd",
      "run_configuration": "app",
      "debugger": "Android Debugger"
    }
  }
}
```

With that file in place, `shadowdroid debug auto` can resolve the app, launch
it, attach the Studio debugger if available, and return a full snapshot.

## Keep the CLI updated

Check the latest GitHub Release and the right updater for your install method:

```bash
shadowdroid update --check
```

For machine-readable output:

```bash
shadowdroid update --check --json
```

The command is non-mutating. It prints `brew upgrade shadowdroid`,
`scoop update shadowdroid`, `cargo install shadowdroid --locked --force`, or the
direct installer command depending on how the current binary appears to be
installed.

## Install a pinned version

Use a tag such as `v0.1.3`:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/andriyo/ShadowDroid/releases/latest/download/shadowdroid-installer.sh \
  | sh -s -- --version v0.1.3
```

```powershell
$installer = Join-Path $env:TEMP "shadowdroid-installer.ps1"
iwr https://github.com/andriyo/ShadowDroid/releases/latest/download/shadowdroid-installer.ps1 -OutFile $installer
powershell -ExecutionPolicy Bypass -File $installer -Version v0.1.3
```

## Custom install directory

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/andriyo/ShadowDroid/releases/latest/download/shadowdroid-installer.sh \
  | sh -s -- --install-dir "$HOME/bin"
```

```powershell
$installer = Join-Path $env:TEMP "shadowdroid-installer.ps1"
iwr https://github.com/andriyo/ShadowDroid/releases/latest/download/shadowdroid-installer.ps1 -OutFile $installer
powershell -ExecutionPolicy Bypass -File $installer -InstallDir "$env:USERPROFILE\bin"
```

## Uninstall

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/andriyo/ShadowDroid/releases/latest/download/shadowdroid-installer.sh \
  | sh -s -- --uninstall
```

```powershell
$installer = Join-Path $env:TEMP "shadowdroid-installer.ps1"
iwr https://github.com/andriyo/ShadowDroid/releases/latest/download/shadowdroid-installer.ps1 -OutFile $installer
powershell -ExecutionPolicy Bypass -File $installer -Uninstall -RemovePath
```

## Connect

Start an emulator or plug in an Android device with USB debugging enabled:

```bash
shadowdroid devices
shadowdroid connect
```

On first connect, the CLI resolves APKs in this order:

1. `--apk PATH`
2. `SHADOWDROID_APK`
3. repo build outputs under `server/app/build/outputs/...`
4. `~/.shadowdroid/apks/local/{main,test}.apk`
5. `~/.shadowdroid/apks/<version>/{main,test}.apk`
6. GitHub Release assets:
   `shadowdroid-server-main.apk` and `shadowdroid-server-test.apk`

Release downloads are verified with `SHA256SUMS` before they are cached.

## Troubleshoot a connection

When `connect` misbehaves — an offline/unauthorized device, a missing or
version-mismatched APK, a dropped port forward, or a stuck instrumentation
holding the device's single UiAutomation slot — run the diagnostics:

```bash
shadowdroid doctor          # human-readable report
shadowdroid doctor --json   # machine-readable, one JSON object
shadowdroid doctor --fix    # attempt repairs (reinstall, re-forward, restart)
```

`doctor` is read-only by default and never starts the server (it diagnoses the
very server `connect` would start). `--fix` applies remediation; it refuses to
kill a *competing*, non-ShadowDroid UiAutomation owner (e.g. an
openatx/uiautomator2 process) unless you also pass `--force`.

To capture a snapshot for a bug report, `collect` bundles the doctor report,
device info, recent logcat (plus the crash buffer), and — when the server is up
— a screen dump, screenshot, current activity, and app info into a directory:

```bash
shadowdroid collect --app com.example.app
# → {"type":"action","cmd":"collect","bundle":"/tmp/shadowdroid-collect-…", …}
```

The bundle is written locally and never uploaded, and still produces the
host-side diagnostics (logs, device info, doctor report) even when the on-device
server can't start. Screenshots and logs may contain sensitive data — review the
directory before sharing it.

## Manual downloads

Every GitHub Release contains:

- `shadowdroid-x86_64-unknown-linux-gnu.tar.gz`
- `shadowdroid-aarch64-unknown-linux-gnu.tar.gz`
- `shadowdroid-x86_64-apple-darwin.tar.gz`
- `shadowdroid-aarch64-apple-darwin.tar.gz`
- `shadowdroid-x86_64-pc-windows-msvc.zip`
- `shadowdroid-server-main.apk`
- `shadowdroid-server-test.apk`
- `shadowdroid-studio-plugin.zip`
- `SHA256SUMS`
- `shadowdroid-installer.sh`
- `shadowdroid-installer.ps1`

## Development install

When working from this repo, build the APK pair and run the CLI from the repo:

```bash
(cd server && ./gradlew :app:assembleDebug :app:assembleDebugAndroidTest)
(cd cli && cargo run -- connect)
```

To force the end-user GitHub Release path while inside the repo:

```bash
SHADOWDROID_DISABLE_DEV_SOURCES=1 shadowdroid connect
```
