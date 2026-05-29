# Release process

ShadowDroid releases are tag-driven. A `v*` tag builds and publishes:

- host CLI archives for macOS, Linux, and Windows
- `shadowdroid-server-main.apk`
- `shadowdroid-server-test.apk`
- `SHA256SUMS`
- copy-paste installer scripts

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
   git tag v0.1.0
   git push origin v0.1.0
   ```

4. Watch the `Release` workflow. It creates or updates the matching GitHub
   Release and uploads the assets.

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
rm -rf ~/.shadowdroid/apks/0.1.0
SHADOWDROID_DISABLE_DEV_SOURCES=1 shadowdroid connect
```

The connect log should mention the GitHub Release download once, then future
runs should use `~/.shadowdroid/apks/0.1.0/`.
