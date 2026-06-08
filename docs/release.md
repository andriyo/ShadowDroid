# Release process

ShadowDroid releases are tag-driven. A `v*` tag builds and publishes:

- host CLI archives for macOS, Linux, and Windows
- `shadowdroid-server-main.apk`
- `shadowdroid-server-test.apk`
- `shadowdroid-studio-plugin.zip`
- `SHA256SUMS`
- copy-paste installer scripts
- installer smoke tests on macOS, Linux, and Windows

## Cut a GitHub Release

1. Make sure these versions match:
   - `cli/Cargo.toml` package version
   - `server/app/build.gradle.kts` `versionName`
   - `server/app/src/androidTest/java/io/github/andriyo/shadowdroid/BuildInfo.kt`
2. Run local checks:

   ```bash
   tag=v0.1.4
   cargo test --manifest-path cli/Cargo.toml --locked
   cargo package --manifest-path cli/Cargo.toml
   (cd server && ./gradlew --no-daemon :app:assembleDebug :app:assembleDebugAndroidTest)
   (cd shadowdroid-plugin && ./gradlew --no-daemon -Pversion="${tag#v}" buildPlugin verifyPluginStructure)
   ```

3. Tag and push:

   ```bash
   git tag "$tag"
   git push origin "$tag"
   ```

4. Watch the `Release` workflow. It creates or updates the matching GitHub
   Release, uploads the assets, installs the published release through the
   macOS/Linux and Windows installers, then updates package-manager repos when
   `SHADOWDROID_PACKAGE_BOT_TOKEN` is configured as a repository secret. The
   token needs push access to:
   - `andriyo/homebrew-tap`
   - `andriyo/scoop-bucket`

## Update package-manager lanes manually

The release workflow runs this automatically when
`SHADOWDROID_PACKAGE_BOT_TOKEN` exists. To do the same bump locally:

```bash
tmpdir="$(mktemp -d)"
tag=v0.1.4
gh release download "$tag" \
  --repo andriyo/ShadowDroid \
  --pattern SHA256SUMS \
  --dir "$tmpdir"

python3 scripts/update-package-managers.py \
  --version "$tag" \
  --checksums "$tmpdir/SHA256SUMS" \
  --homebrew-path /Users/andrii/Work/homebrew-tap \
  --scoop-path /Users/andrii/Work/scoop-bucket
```

Then review, commit, and push those two repos.

## Update-check UX

Users can check whether their host CLI is current without touching a device:

```bash
shadowdroid update --check
shadowdroid update --check --json
```

## Publish to crates.io

Publish after the GitHub Release exists, because a `cargo install` build fetches
the version-matched APKs from the matching release tag on first
`shadowdroid connect`, and the Android Studio plugin on first
`shadowdroid studio install`.

```bash
cargo publish --manifest-path cli/Cargo.toml --locked
```

## Smoke test the published path

Use a clean cache to force the GitHub Release APK download:

```bash
tag=v0.1.4
rm -rf ~/.shadowdroid/apks/"${tag#v}"
SHADOWDROID_DISABLE_DEV_SOURCES=1 shadowdroid connect
```

The connect log should mention the GitHub Release download once, then future
runs should use `~/.shadowdroid/apks/<version>/`.

Use a clean plugin cache to force the GitHub Release plugin download:

```bash
tag=v0.1.4
rm -rf ~/.shadowdroid/plugins/"${tag#v}"
SHADOWDROID_DISABLE_DEV_SOURCES=1 shadowdroid studio install --studio "/Applications/Android Studio.app"
```

The install output should mention the GitHub Release source once, then future
runs should use `~/.shadowdroid/plugins/<version>/`.

You can also rerun installer-only checks from GitHub Actions with the
`Installer Smoke` workflow and a release tag such as `v0.1.4`.
