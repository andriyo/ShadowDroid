//! Byte streams to an app's JDWP endpoint, and debuggable-pid discovery.
//!
//! On Android the app's debugger socket is reachable only through adbd, so
//! the stream is an ADB server connection switched to the device transport
//! (`host:transport:<serial>`) and then to the `jdwp:<pid>` device service.
//! No `adb forward`, no host port. [`connect_tcp`] exists for the fake JDWP
//! server in tests and for a manual `adb forward tcp:N jdwp:<pid>` fallback.

use anyhow::{Context, Result, anyhow};
use std::io::Read;
use std::time::Duration;
use tokio::net::TcpStream;

use crate::device::adb_wire;

/// Test/fallback hook: connect to this `host:port` with plain TCP instead of
/// adb (`adb forward tcp:N jdwp:<pid>`, or the fake server in `cli/tests`).
pub const TCP_OVERRIDE_ENV: &str = "SHADOWDROID_JDWP_TCP";

pub fn tcp_override() -> Option<String> {
    crate::hostenv::nonempty_env(TCP_OVERRIDE_ENV).map(|value| value.to_string_lossy().into_owned())
}

/// Open the `jdwp:<pid>` stream for `serial` through the ADB server.
pub async fn connect_adb(serial: &str, pid: u32, timeout: Duration) -> Result<TcpStream> {
    let serial = serial.to_string();
    let stream = tokio::time::timeout(
        timeout,
        tokio::task::spawn_blocking(move || -> Result<std::net::TcpStream> {
            let mut stream = adb_wire::transport(&serial, Some(timeout))?;
            adb_wire::request(&mut stream, &format!("jdwp:{pid}"))?;
            // Hand the socket to tokio with no read deadline: the session
            // enforces per-request deadlines itself.
            stream.set_read_timeout(None)?;
            stream.set_write_timeout(None)?;
            stream.set_nonblocking(true)?;
            Ok(stream)
        }),
    )
    .await
    .map_err(|_| {
        anyhow!(
            "opening jdwp:{pid} timed out after {} ms",
            timeout.as_millis()
        )
    })?
    .context("adb transport worker")??;
    TcpStream::from_std(stream).context("register the JDWP stream with tokio")
}

/// Plain TCP to a forwarded or fake JDWP endpoint.
pub async fn connect_tcp(address: &str, timeout: Duration) -> Result<TcpStream> {
    let stream = tokio::time::timeout(timeout, TcpStream::connect(address))
        .await
        .map_err(|_| anyhow!("connect {address} timed out"))?
        .with_context(|| format!("connect {address}"))?;
    stream.set_nodelay(true)?;
    Ok(stream)
}

/// Pids with a JDWP endpoint. Current adbd answers the `jdwp` device service
/// like `track-jdwp`: one length-prefixed list, then the stream stays open for
/// updates. Older builds send a bare list and close.
pub async fn debuggable_pids(serial: &str, timeout: Duration) -> Result<Vec<u32>> {
    let serial = serial.to_string();
    tokio::task::spawn_blocking(move || -> Result<Vec<u32>> {
        let mut stream = adb_wire::transport(&serial, Some(timeout))?;
        adb_wire::request(&mut stream, "jdwp")?;
        let mut body = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            // A complete framed list is the whole answer: waiting for EOF
            // would cost the full read timeout on every attach.
            if let Some(list) = first_framed_list(&body) {
                return Ok(parse_pid_list(list));
            }
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => body.extend_from_slice(&chunk[..n]),
                // Some adbd builds keep the stream open like `track-jdwp`;
                // a quiet period after data means the list is complete.
                Err(error)
                    if !body.is_empty()
                        && matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                {
                    break;
                }
                Err(error) => return Err(error).context("read the jdwp pid list"),
            }
        }
        Ok(parse_pid_list(&body))
    })
    .await
    .context("adb jdwp worker")?
}

/// The first complete `%04x`-framed message in `body`, framing included.
fn first_framed_list(body: &[u8]) -> Option<&[u8]> {
    let header = body.get(..4)?;
    if !header.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let len = usize::from_str_radix(std::str::from_utf8(header).ok()?, 16).ok()?;
    body.get(..4 + len)
}

/// Parse a `jdwp` / `track-jdwp` listing. `track-jdwp` prefixes each update
/// with a 4-hex-digit length; the plain service does not.
pub fn parse_pid_list(body: &[u8]) -> Vec<u32> {
    let mut text = String::from_utf8_lossy(body).into_owned();
    if text.len() >= 4
        && text.as_bytes()[..4].iter().all(u8::is_ascii_hexdigit)
        && usize::from_str_radix(&text[..4], 16).ok() == Some(text.len() - 4)
    {
        text = text[4..].to_string();
    }
    let mut pids: Vec<u32> = text
        .split_whitespace()
        .filter_map(|line| line.parse().ok())
        .filter(|pid| *pid != 0)
        .collect();
    pids.sort_unstable();
    pids.dedup();
    pids
}

/// `pid → process name` for each pid, from `/proc/<pid>/cmdline` in one
/// shell round trip. Processes that exited in between are omitted.
pub async fn process_names(serial: &str, pids: &[u32]) -> Result<Vec<(u32, String)>> {
    if pids.is_empty() {
        return Ok(Vec::new());
    }
    let list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    let script = format!(
        "for p in {list}; do printf '%s ' \"$p\"; tr '\\0' ' ' < /proc/$p/cmdline 2>/dev/null; echo; done"
    );
    let serial = serial.to_string();
    let output = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let mut out = Vec::new();
        adb_wire::shell(&serial, &script, Some(&mut out), None)?;
        Ok(out)
    })
    .await
    .context("adb shell worker")??;
    Ok(parse_process_names(&String::from_utf8_lossy(&output)))
}

pub fn parse_process_names(output: &str) -> Vec<(u32, String)> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse().ok()?;
            let name = parts.next()?.to_string();
            Some((pid, name))
        })
        .collect()
}

/// Pick the pid for `package`: an exact process-name match wins over the
/// app's `package:subprocess` processes.
pub fn pick_package_pid(names: &[(u32, String)], package: &str) -> Vec<(u32, String)> {
    let exact: Vec<_> = names
        .iter()
        .filter(|(_, name)| name == package)
        .cloned()
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    names
        .iter()
        .filter(|(_, name)| {
            name.strip_prefix(package)
                .is_some_and(|rest| rest.starts_with(':'))
        })
        .cloned()
        .collect()
}

/// One-line shell command output, trimmed (`getprop`, `pidof`).
pub async fn shell_line(serial: &str, command: &str) -> Result<String> {
    let serial = serial.to_string();
    let command = command.to_string();
    tokio::task::spawn_blocking(move || -> Result<String> {
        let mut out = Vec::new();
        adb_wire::shell(&serial, &command, Some(&mut out), None)?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    })
    .await
    .context("adb shell worker")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_lists_parse_with_and_without_track_prefix() {
        assert_eq!(parse_pid_list(b"1234\n567\n\n"), vec![567, 1234]);
        // track-jdwp framing: "000a" = 10 bytes follow.
        assert_eq!(parse_pid_list(b"000a1234\n5678\n"), vec![1234, 5678]);
        // A pid that happens to look like hex is not mistaken for a prefix.
        assert_eq!(parse_pid_list(b"1234\n"), vec![1234]);
        assert_eq!(parse_pid_list(b"garbage\n0\n42"), vec![42]);
    }

    #[test]
    fn a_complete_framed_list_ends_the_read() {
        // Partial frame: keep reading.
        assert_eq!(first_framed_list(b"0010123\n"), None);
        // Complete frame followed by the start of an update: stop at the frame.
        let body = b"000c22736\n30578\n000a";
        let list = first_framed_list(body).expect("framed list");
        assert_eq!(parse_pid_list(list), vec![22736, 30578]);
        // An empty device answers "0000".
        assert_eq!(
            parse_pid_list(first_framed_list(b"0000").unwrap()),
            Vec::<u32>::new()
        );
        // A bare (unframed) list is not mistaken for a frame header.
        assert_eq!(first_framed_list(b"1234\n"), None);
    }

    #[test]
    fn process_names_prefer_the_exact_package() {
        let names = parse_process_names(
            "100 io.example.app \n101 io.example.app:remote \n102 io.example.apple \n103\n",
        );
        assert_eq!(names.len(), 3);
        assert_eq!(
            pick_package_pid(&names, "io.example.app"),
            vec![(100, "io.example.app".to_string())]
        );
        let only_remote = vec![(101, "io.example.app:remote".to_string())];
        assert_eq!(
            pick_package_pid(&only_remote, "io.example.app"),
            only_remote
        );
        assert!(pick_package_pid(&names, "io.example").is_empty());
    }
}
