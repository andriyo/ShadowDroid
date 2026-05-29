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
the user PATH.

ShadowDroid also needs Android Platform Tools (`adb`) on PATH before
`shadowdroid connect` can talk to a device. The installers print a hint if `adb`
is missing. On macOS, use `brew install --cask android-platform-tools`; on
Windows with Scoop, use `scoop install adb`.

## Install a pinned version

Use a tag such as `v0.1.2`:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/andriyo/ShadowDroid/releases/latest/download/shadowdroid-installer.sh \
  | sh -s -- --version v0.1.2
```

```powershell
$installer = Join-Path $env:TEMP "shadowdroid-installer.ps1"
iwr https://github.com/andriyo/ShadowDroid/releases/latest/download/shadowdroid-installer.ps1 -OutFile $installer
powershell -ExecutionPolicy Bypass -File $installer -Version v0.1.2
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
