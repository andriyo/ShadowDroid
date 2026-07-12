//! Streaming host-file transfers with atomic destination replacement.
//!
//! Network bodies are copied into a same-directory temporary file one chunk at
//! a time. The existing destination is replaced only after the body is complete,
//! its declared length matches, and any expected SHA-256 digest is verified.

use std::path::Path;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::Response;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

const RESPONSE_BODY_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
#[error("incomplete response body: expected {expected} bytes, received {received}")]
struct IncompleteResponseBody {
    expected: u64,
    received: u64,
}

#[derive(Debug, thiserror::Error)]
#[error("response body made no progress for {seconds} seconds")]
struct ResponseBodyIdleTimeout {
    seconds: u64,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct TransferReceipt {
    pub bytes: u64,
    pub sha256: String,
}

/// Whether an error happened while receiving the remote response rather than
/// while creating/writing the local destination. File-pull callers may retry
/// this narrow class over ADB without hiding local disk/permission failures.
pub(crate) fn is_remote_response_failure(error: &anyhow::Error) -> bool {
    error.downcast_ref::<reqwest::Error>().is_some()
        || error.downcast_ref::<IncompleteResponseBody>().is_some()
        || error.downcast_ref::<ResponseBodyIdleTimeout>().is_some()
}

/// Create a same-directory staging file with the destination's existing
/// permissions. New Unix files use normal `0666 & !umask` semantics, matching
/// `File::create`, rather than tempfile's deliberately private 0600 default.
pub(crate) fn atomic_temp_for_destination(
    destination: &Path,
) -> Result<(tempfile::NamedTempFile, Option<std::fs::Permissions>)> {
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let existing_permissions = match std::fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!(
                "refusing to atomically replace symlink destination {}",
                destination.display()
            )
        }
        Ok(metadata) if !metadata.file_type().is_file() => {
            bail!(
                "refusing to atomically replace non-regular destination {}",
                destination.display()
            )
        }
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("stat {}", destination.display()));
        }
    };
    #[cfg(unix)]
    let existing_permissions = {
        use std::os::unix::fs::PermissionsExt;
        existing_permissions.map(|permissions| {
            // Replacing content must not preserve/reintroduce setuid, setgid,
            // or sticky bits. Ordinary in-place writes clear privilege bits.
            std::fs::Permissions::from_mode(permissions.mode() & 0o777)
        })
    };
    let mut builder = tempfile::Builder::new();
    builder.prefix(".shadowdroid-transfer-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = existing_permissions
            .as_ref()
            .map(PermissionsExt::mode)
            .unwrap_or(0o666)
            & 0o777;
        builder.permissions(std::fs::Permissions::from_mode(mode));
    }
    let temporary = builder
        .tempfile_in(parent)
        .with_context(|| format!("create temporary file beside {}", destination.display()))?;
    Ok((temporary, existing_permissions))
}

pub async fn response_to_path_atomic(
    response: Response,
    destination: &Path,
    expected_sha256: Option<&str>,
) -> Result<TransferReceipt> {
    response_to_path_atomic_with_idle(
        response,
        destination,
        expected_sha256,
        RESPONSE_BODY_IDLE_TIMEOUT,
    )
    .await
}

async fn response_to_path_atomic_with_idle(
    response: Response,
    destination: &Path,
    expected_sha256: Option<&str>,
    idle_timeout: std::time::Duration,
) -> Result<TransferReceipt> {
    let response = response.error_for_status()?;
    let declared_length = response.content_length();
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let (temp, existing_permissions) = atomic_temp_for_destination(destination)?;
    let (file, temp_path) = temp.into_parts();
    let mut file = tokio::fs::File::from_std(file);
    let mut stream = response.bytes_stream();
    let mut hasher = Sha256::new();
    let mut bytes = 0u64;

    loop {
        let next = tokio::time::timeout(idle_timeout, stream.next())
            .await
            .map_err(|_| ResponseBodyIdleTimeout {
                seconds: idle_timeout.as_secs(),
            })?;
        let Some(chunk) = next else {
            break;
        };
        let chunk =
            chunk.with_context(|| format!("read response body for {}", destination.display()))?;
        bytes = bytes
            .checked_add(u64::try_from(chunk.len()).context("response chunk length exceeds u64")?)
            .context("response body length exceeds u64")?;
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .with_context(|| format!("write temporary file for {}", destination.display()))?;
    }

    if let Some(expected) = declared_length
        && bytes != expected
    {
        return Err(IncompleteResponseBody {
            expected,
            received: bytes,
        })
        .with_context(|| format!("receive complete body for {}", destination.display()));
    }

    file.flush()
        .await
        .with_context(|| format!("flush temporary file for {}", destination.display()))?;
    if let Some(permissions) = existing_permissions {
        file.set_permissions(permissions)
            .await
            .with_context(|| format!("preserve permissions for {}", destination.display()))?;
    }
    file.sync_all()
        .await
        .with_context(|| format!("sync temporary file for {}", destination.display()))?;
    drop(file);

    let sha256 = hex_lower(&hasher.finalize());
    if let Some(expected) = expected_sha256 {
        let expected = expected.trim().to_ascii_lowercase();
        if sha256 != expected {
            bail!(
                "checksum mismatch for {}: expected {expected}, got {sha256}",
                destination.display()
            );
        }
    }

    temp_path
        .persist(destination)
        .map_err(|error| error.error)
        .with_context(|| format!("atomically replace {}", destination.display()))?;
    sync_parent_best_effort(parent).await;

    Ok(TransferReceipt { bytes, sha256 })
}

pub async fn download_atomic(
    client: &reqwest::Client,
    url: &str,
    destination: &Path,
    expected_sha256: Option<&str>,
) -> Result<TransferReceipt> {
    let response = client
        .get(url)
        .header(reqwest::header::USER_AGENT, "shadowdroid")
        .send()
        .await?
        .error_for_status()?;
    response_to_path_atomic(response, destination, expected_sha256).await
}

#[cfg(unix)]
async fn sync_parent_best_effort(parent: &Path) {
    let parent = parent.to_path_buf();
    let directory_display = parent.display().to_string();
    let result = tokio::task::spawn_blocking(move || {
        std::fs::File::open(&parent)
            .with_context(|| format!("open {} for sync", parent.display()))?
            .sync_all()
            .with_context(|| format!("sync {}", parent.display()))
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(directory = %directory_display, error = %error, "destination committed but directory sync failed")
        }
        Err(error) => {
            tracing::warn!(directory = %directory_display, error = %error, "destination committed but directory-sync task failed")
        }
    }
}

#[cfg(not(unix))]
async fn sync_parent_best_effort(_parent: &Path) {}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn one_response(body: Vec<u8>, declared_length: u64) -> (reqwest::Client, String) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {declared_length}\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            for chunk in body.chunks(1024) {
                socket.write_all(chunk).await.unwrap();
                tokio::task::yield_now().await;
            }
        });
        (reqwest::Client::new(), format!("http://{address}/asset"))
    }

    #[tokio::test]
    async fn download_streams_and_atomically_replaces_destination() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("asset.bin");
        std::fs::write(&destination, b"old").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o4755))
                .unwrap();
        }
        let body = (0..256 * 1024)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let expected = hex_lower(&Sha256::digest(&body));
        let (client, url) = one_response(body.clone(), body.len() as u64).await;

        let receipt = download_atomic(&client, &url, &destination, Some(&expected))
            .await
            .unwrap();

        assert_eq!(receipt.bytes, body.len() as u64);
        assert_eq!(receipt.sha256, expected);
        assert_eq!(std::fs::read(&destination).unwrap(), body);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&destination)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o7777,
                0o755
            );
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn failed_or_truncated_download_keeps_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("asset.bin");
        std::fs::write(&destination, b"old").unwrap();

        let body = b"new body".to_vec();
        let (client, url) = one_response(body.clone(), body.len() as u64).await;
        assert!(
            download_atomic(&client, &url, &destination, Some(&"00".repeat(32)))
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&destination).unwrap(), b"old");

        let (client, url) = one_response(body.clone(), body.len() as u64 + 10).await;
        assert!(
            download_atomic(&client, &url, &destination, None)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&destination).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn stalled_body_hits_idle_deadline_and_keeps_existing_destination() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        });
        let response = reqwest::Client::new()
            .get(format!("http://{address}/asset"))
            .send()
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("asset.bin");
        std::fs::write(&destination, b"old").unwrap();

        let error = response_to_path_atomic_with_idle(
            response,
            &destination,
            None,
            std::time::Duration::from_millis(20),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("made no progress"));
        assert!(is_remote_response_failure(&error));
        assert_eq!(std::fs::read(&destination).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn atomic_transfer_rejects_a_symlink_destination() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.bin");
        let destination = dir.path().join("link.bin");
        std::fs::write(&target, b"keep").unwrap();
        symlink(&target, &destination).unwrap();
        let body = b"replacement".to_vec();
        let (client, url) = one_response(body.clone(), body.len() as u64).await;

        let error = download_atomic(&client, &url, &destination, None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("symlink destination"));
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        assert!(destination.is_symlink());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn atomic_transfer_rejects_a_fifo_destination() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("pipe");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&destination)
                .status()
                .unwrap()
                .success()
        );
        let body = b"replacement".to_vec();
        let (client, url) = one_response(body.clone(), body.len() as u64).await;

        let error = download_atomic(&client, &url, &destination, None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("non-regular destination"));
        assert!(destination.exists());
    }
}
