# Getting started

ShadowDroid is shipped as a host CLI plus a version-matched Android
instrumentation APK pair. Users install only the CLI; the CLI downloads,
verifies, caches, and installs the APKs during `shadowdroid connect`.

## Install the CLI

macOS / Linux:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/andriyo/ShadowDroid/releases/latest/download/shadowdroid-installer.sh | sh
```

Windows PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/andriyo/ShadowDroid/releases/latest/download/shadowdroid-installer.ps1 | iex"
```

The shell installer writes to `~/.local/bin` by default. The PowerShell
installer writes to `%LOCALAPPDATA%\ShadowDroid\bin` and adds that directory to
the user PATH.

## Install a pinned version

Use a tag such as `v0.1.0`:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/andriyo/ShadowDroid/releases/latest/download/shadowdroid-installer.sh | SHADOWDROID_VERSION=v0.1.0 sh
```

```powershell
$env:SHADOWDROID_VERSION = "v0.1.0"
powershell -ExecutionPolicy Bypass -c "irm https://github.com/andriyo/ShadowDroid/releases/latest/download/shadowdroid-installer.ps1 | iex"
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

## Manual downloads

Every GitHub Release contains:

- `shadowdroid-x86_64-unknown-linux-gnu.tar.gz`
- `shadowdroid-aarch64-unknown-linux-gnu.tar.gz`
- `shadowdroid-x86_64-apple-darwin.tar.gz`
- `shadowdroid-aarch64-apple-darwin.tar.gz`
- `shadowdroid-x86_64-pc-windows-msvc.zip`
- `shadowdroid-server-main.apk`
- `shadowdroid-server-test.apk`
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
