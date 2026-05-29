# Release process

ShadowDroid releases are tag-driven. A `v*` tag builds and publishes:

- host CLI archives for macOS, Linux, and Windows
- `shadowdroid-server-main.apk`
- `shadowdroid-server-test.apk`
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
   cargo test --manifest-path cli/Cargo.toml --locked
   cargo package --manifest-path cli/Cargo.toml
   (cd server && ./gradlew --no-daemon :app:assembleDebug :app:assembleDebugAndroidTest)
   ```

3. Tag and push:

   ```bash
   git tag v0.1.1
   git push origin v0.1.1
   ```

4. Watch the `Release` workflow. It creates or updates the matching GitHub
   Release, uploads the assets, then installs the published release through the
   macOS/Linux and Windows installers.
5. Update package-manager lanes:

   ```bash
   brew tap andriyo/tap
   brew install shadowdroid

   scoop bucket add andriyo https://github.com/andriyo/scoop-bucket
   scoop install shadowdroid
   ```

## Publish to crates.io

Publish after the GitHub Release exists, because a `cargo install` build fetches
the version-matched APKs from the matching release tag on first
`shadowdroid connect`.

```bash
cargo publish --manifest-path cli/Cargo.toml --locked
```

## Smoke test the published path

Use a clean cache to force the GitHub Release APK download:

```bash
rm -rf ~/.shadowdroid/apks/0.1.1
SHADOWDROID_DISABLE_DEV_SOURCES=1 shadowdroid connect
```

The connect log should mention the GitHub Release download once, then future
runs should use `~/.shadowdroid/apks/0.1.1/`.

You can also rerun installer-only checks from GitHub Actions with the
`Installer Smoke` workflow and a release tag such as `v0.1.1`.
