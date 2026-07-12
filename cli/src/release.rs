//! Fetching ShadowDroid GitHub-release assets and verifying their SHA-256
//! checksums. One home for the pipeline the server-APK installer
//! ([crate::device::installer]), `aar` ([crate::cmd::aar]), and the Studio
//! plugin installer ([crate::cmd::studio]) previously carried as per-module
//! copies (which had already started to drift).

use std::io::{BufReader, Read};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};

/// The checksums manifest published alongside every release's assets.
pub const CHECKSUMS_ASSET: &str = "SHA256SUMS";
const RELEASE_REQUEST_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const METADATA_MAX_BYTES: usize = 1024 * 1024;
const METADATA_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Base URL for the release matching `version`. `SHADOWDROID_RELEASE_BASE_URL`
/// is a compile-time override (with `{version}` substitution) so forks and
/// mirrors can redirect downloads without patching call sites.
pub fn release_base_url(version: &str) -> String {
    let template = option_env!("SHADOWDROID_RELEASE_BASE_URL")
        .unwrap_or("https://github.com/andriyo/ShadowDroid/releases/download/v{version}");
    template
        .replace("{version}", version)
        .trim_end_matches('/')
        .to_string()
}

pub fn release_asset_url(base: &str, asset: &str) -> String {
    format!("{}/{asset}", base.trim_end_matches('/'))
}

pub fn release_client() -> Result<reqwest::Client> {
    http_client(RELEASE_REQUEST_TIMEOUT)
}

pub(crate) fn http_client(request_timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("shadowdroid")
        .connect_timeout(CONNECT_TIMEOUT.min(request_timeout))
        .timeout(request_timeout)
        .build()
        .context("build hardened HTTP client")
}

pub async fn download_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let bytes = download_small_bytes(client, url, METADATA_MAX_BYTES).await?;
    String::from_utf8(bytes).with_context(|| format!("metadata from {url} is not valid UTF-8"))
}

/// Download small control-plane metadata with both an allocation bound and an
/// idle-body timeout. Release manifests and API JSON must never share the
/// effectively unbounded response collector used for large verified assets.
pub(crate) async fn download_small_bytes(
    client: &reqwest::Client,
    url: &str,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("request metadata from {url}"))?
        .error_for_status()
        .with_context(|| format!("metadata request failed for {url}"))?;
    if let Some(length) = response.content_length()
        && length > u64::try_from(max_bytes).unwrap_or(u64::MAX)
    {
        bail!("metadata from {url} is too large ({length} bytes; limit {max_bytes})");
    }

    let mut bytes = Vec::with_capacity(
        response
            .content_length()
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or(0)
            .min(max_bytes),
    );
    let mut stream = response.bytes_stream();
    loop {
        let next = tokio::time::timeout(METADATA_IDLE_TIMEOUT, stream.next())
            .await
            .with_context(|| format!("metadata body from {url} stalled"))?;
        let Some(chunk) = next else { break };
        let chunk = chunk.with_context(|| format!("read metadata body from {url}"))?;
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            bail!("metadata from {url} exceeds the {max_bytes}-byte limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Stream a release asset into a same-directory temporary file, hashing it as
/// it arrives. The destination is atomically replaced only after the complete
/// response matches `expected_sha256`; failures leave any prior cache intact.
pub async fn download_verified_file(
    client: &reqwest::Client,
    url: &str,
    path: &Path,
    expected_sha256: &str,
) -> Result<crate::transfer::TransferReceipt> {
    let expected = normalize_sha256(expected_sha256)?;
    crate::transfer::download_atomic(client, url, path, Some(&expected)).await
}

/// The digest an asset must hash to: a compile-time embedded override when the
/// build pinned one, otherwise the entry from the downloaded checksums manifest.
pub fn expected_sha(embedded: Option<&str>, checksums: &str, asset: &str) -> Result<String> {
    if let Some(value) = embedded.map(str::trim).filter(|s| !s.is_empty()) {
        return normalize_sha256(value);
    }
    checksum_for(checksums, asset)
        .ok_or_else(|| anyhow!("no checksum found for {asset} in {CHECKSUMS_ASSET}"))
}

/// Look `asset` up in `sha256sum`-format output (`<sha>  <name>`; a leading
/// `*` on the name marks binary mode and is ignored).
pub fn checksum_for(checksums: &str, asset: &str) -> Option<String> {
    checksums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let sha = parts.next()?;
        let name = parts.next()?.trim_start_matches('*');
        if name == asset {
            normalize_sha256(sha).ok()
        } else {
            None
        }
    })
}

pub fn normalize_sha256(value: &str) -> Result<String> {
    let lower = value.trim().to_ascii_lowercase();
    if lower.len() == 64 && lower.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(lower)
    } else {
        bail!("invalid SHA-256 digest: {value}")
    }
}

pub fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let actual = sha256_file(path)?;
    if actual != expected {
        bail!(
            "checksum mismatch for {}: expected {expected}, got {actual}",
            path.display()
        );
    }
    Ok(())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .with_context(|| format!("read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    Ok(hex_lower(&digest))
}

pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http_body_util::Full;
    use hyper::service::service_fn;
    use hyper::{Response, StatusCode};
    use hyper_util::rt::TokioIo;
    use std::convert::Infallible;

    async fn serve_once(status: StatusCode, body: &'static [u8]) -> String {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let body = Bytes::from_static(body);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let service = service_fn(move |_| {
                let body = body.clone();
                async move {
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(status)
                            .body(Full::new(body))
                            .unwrap(),
                    )
                }
            });
            hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await
                .unwrap();
        });
        format!("http://{address}/asset")
    }

    fn sha256(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hex_lower(&hasher.finalize())
    }

    #[test]
    fn parses_sha256sums_lines() {
        let sums = "\
0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  shadowdroid-server-main.apk
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa *shadowdroid-server-test.apk
";
        assert_eq!(
            checksum_for(sums, "shadowdroid-server-main.apk").as_deref(),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
        assert_eq!(
            checksum_for(sums, "shadowdroid-server-test.apk").as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert!(checksum_for(sums, "missing.apk").is_none());
    }

    #[test]
    fn rejects_invalid_sha256() {
        assert!(normalize_sha256("abc").is_err());
        assert!(
            normalize_sha256("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz")
                .is_err()
        );
    }

    #[test]
    fn embedded_sha_overrides_manifest() {
        let sums = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  a.zip\n";
        let embedded = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        assert_eq!(
            expected_sha(Some(embedded), sums, "a.zip").unwrap(),
            embedded.to_ascii_lowercase()
        );
        // Blank embedded value falls through to the manifest.
        assert_eq!(
            expected_sha(Some("  "), sums, "a.zip").unwrap(),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
        assert!(expected_sha(None, sums, "missing.zip").is_err());
    }

    #[test]
    fn base_url_strips_trailing_slash() {
        assert_eq!(
            release_asset_url("https://x/y/", "a.zip"),
            "https://x/y/a.zip"
        );
        assert!(release_base_url("1.2.3").ends_with("v1.2.3"));
    }

    #[tokio::test]
    async fn verified_download_atomically_replaces_an_existing_asset() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("asset.bin");
        std::fs::write(&destination, b"old-complete-asset").unwrap();
        let url = serve_once(StatusCode::OK, b"new-complete-asset").await;
        let expected = sha256(b"new-complete-asset");

        let receipt =
            download_verified_file(&release_client().unwrap(), &url, &destination, &expected)
                .await
                .unwrap();

        assert_eq!(receipt.bytes, 18);
        assert_eq!(receipt.sha256, expected);
        assert_eq!(std::fs::read(&destination).unwrap(), b"new-complete-asset");
    }

    #[tokio::test]
    async fn checksum_failure_preserves_the_previous_asset() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("asset.bin");
        std::fs::write(&destination, b"old-complete-asset").unwrap();
        let url = serve_once(StatusCode::OK, b"corrupt-download").await;

        let error = download_verified_file(
            &release_client().unwrap(),
            &url,
            &destination,
            &sha256(b"expected-download"),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("checksum mismatch"));
        assert_eq!(std::fs::read(&destination).unwrap(), b"old-complete-asset");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn http_failure_preserves_the_previous_asset() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("asset.bin");
        std::fs::write(&destination, b"old-complete-asset").unwrap();
        let url = serve_once(StatusCode::INTERNAL_SERVER_ERROR, b"failed").await;

        assert!(
            download_verified_file(
                &release_client().unwrap(),
                &url,
                &destination,
                &sha256(b"failed"),
            )
            .await
            .is_err()
        );
        assert_eq!(std::fs::read(&destination).unwrap(), b"old-complete-asset");
    }

    #[tokio::test]
    async fn small_metadata_download_rejects_oversized_responses() {
        let url = serve_once(StatusCode::OK, b"metadata-is-too-large").await;
        let error = download_small_bytes(&release_client().unwrap(), &url, 8)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("too large"));
    }
}
