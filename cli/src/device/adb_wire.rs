//! In-tree client for the host ADB server's wire protocol (the smart socket on
//! 127.0.0.1:5037): device listing, shell (v2, with exit status), sync push and
//! pull, and streamed install/uninstall. It replaces the `adb_client` crate,
//! whose unconditional `rsa` dependency carried RUSTSEC-2023-0071 although
//! ShadowDroid never talks to devices without the ADB server.
//!
//! Every call is synchronous and opens its own connection; `adb.rs` runs them
//! on blocking workers under host-side deadlines. Protocol references:
//! `packages/modules/adb/{SERVICES.TXT,SYNC.TXT,shell_protocol.h}`.

use anyhow::{Context, Result, anyhow, bail};
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::Duration;

const DEFAULT_SERVER_PORT: u16 = 5037;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Largest DATA chunk the sync protocol accepts.
const SYNC_MAX_CHUNK: usize = 64 * 1024;
/// Mode sent with SEND, kept from the `adb_client` implementation this replaces.
const PUSH_MODE: &str = "0777";
/// Bound on install/uninstall result text, which is a single status line.
const MAX_RESULT_TEXT: u64 = 64 * 1024;

// Shell protocol packet ids (shell_protocol.h).
const SHELL_STDOUT: u8 = 1;
const SHELL_STDERR: u8 = 2;
const SHELL_EXIT: u8 = 3;
const SHELL_CLOSE_STDIN: u8 = 4;

/// The ADB server port: `ANDROID_ADB_SERVER_PORT` like the `adb` binary, else 5037.
fn server_port() -> u16 {
    std::env::var("ANDROID_ADB_SERVER_PORT")
        .ok()
        .and_then(|port| port.trim().parse().ok())
        .unwrap_or(DEFAULT_SERVER_PORT)
}

/// Connect to the local ADB server, starting it (`adb start-server`) once if
/// nothing listens yet. `read_timeout` bounds each read; `None` suits
/// transfers and long shells whose deadline the caller enforces.
pub(crate) fn connect(read_timeout: Option<Duration>) -> Result<TcpStream> {
    let address = SocketAddr::from(([127, 0, 0, 1], server_port()));
    let stream = match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
        Ok(stream) => stream,
        Err(error) if error.kind() == ErrorKind::ConnectionRefused => {
            start_server();
            TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)
                .context("connect to local ADB server")?
        }
        Err(error) => return Err(error).context("connect to local ADB server"),
    };
    stream.set_nodelay(true)?;
    stream.set_read_timeout(read_timeout)?;
    stream.set_write_timeout(read_timeout)?;
    Ok(stream)
}

fn start_server() {
    let mut command = std::process::Command::new("adb");
    command
        .arg("start-server")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: no console flashes up for the server.
        command.creation_flags(0x0800_0000);
    }
    if let Err(error) = command.status() {
        tracing::debug!("adb start-server: {error}");
    }
}

/// Send one smart-socket request and require `OKAY`.
pub(crate) fn request(stream: &mut TcpStream, command: &str) -> Result<()> {
    let request = format!("{:04x}{command}", command.len());
    stream.write_all(request.as_bytes())?;
    let mut status = [0_u8; 4];
    stream.read_exact(&mut status)?;
    match &status {
        b"OKAY" => Ok(()),
        b"FAIL" => {
            let message = String::from_utf8_lossy(&read_hex_body(stream)?).into_owned();
            bail!("ADB server rejected {command:?}: {message}")
        }
        other => bail!(
            "unexpected ADB server response to {command:?}: {:?}",
            String::from_utf8_lossy(other)
        ),
    }
}

/// Read a hex-length-prefixed reply body (`host:` services).
pub(crate) fn read_hex_body(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length)?;
    let length = usize::from_str_radix(std::str::from_utf8(&length)?, 16)
        .context("parse ADB response length")?;
    let mut body = vec![0_u8; length];
    stream.read_exact(&mut body)?;
    Ok(body)
}

/// A connection switched to `serial`'s transport, ready for one device service.
pub(crate) fn transport(serial: &str, read_timeout: Option<Duration>) -> Result<TcpStream> {
    let mut stream = connect(read_timeout)?;
    request(&mut stream, &format!("host:transport:{serial}"))?;
    Ok(stream)
}

/// Every device the server knows, with its state (`device`, `offline`,
/// `unauthorized`, …), in the server's order.
pub(crate) fn devices() -> Result<Vec<(String, String)>> {
    let mut stream = connect(Some(CONNECT_TIMEOUT))?;
    request(&mut stream, "host:devices")?;
    let body = read_hex_body(&mut stream)?;
    Ok(parse_devices(&String::from_utf8_lossy(&body)))
}

fn parse_devices(listing: &str) -> Vec<(String, String)> {
    listing
        .lines()
        .filter_map(|line| {
            let (serial, state) = line.split_once('\t')?;
            Some((serial.to_string(), state.trim().to_string()))
        })
        .collect()
}

/// Run `command` through the device shell, streaming stdout and stderr, and
/// return its exit status. Devices without the shell protocol (before
/// Android 7) run it over the legacy service: no stderr split, no status.
pub(crate) fn shell(
    serial: &str,
    command: &str,
    stdout: Option<&mut dyn Write>,
    stderr: Option<&mut dyn Write>,
) -> Result<Option<u8>> {
    let mut stream = transport(serial, None)?;
    match request(&mut stream, &format!("shell,v2,raw:{command}")) {
        Ok(()) => shell_v2(stream, stdout, stderr),
        Err(v2_error) => {
            let mut stream = transport(serial, None)?;
            request(&mut stream, &format!("shell:{command}"))
                .map_err(|error| anyhow!("{error} (shell protocol v2: {v2_error})"))?;
            let mut sink = std::io::sink();
            copy_to_eof(&mut stream, stdout.unwrap_or(&mut sink))?;
            Ok(None)
        }
    }
}

fn shell_v2(
    mut stream: TcpStream,
    mut stdout: Option<&mut dyn Write>,
    mut stderr: Option<&mut dyn Write>,
) -> Result<Option<u8>> {
    // Nothing is sent on stdin: close it so a command that reads it sees EOF
    // instead of waiting forever.
    stream.write_all(&shell_packet(SHELL_CLOSE_STDIN, &[]))?;
    let mut header = [0_u8; 5];
    let mut payload = Vec::new();
    loop {
        match stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(error).context("read ADB shell output"),
        }
        let length = u32::from_le_bytes(header[1..5].try_into().expect("4 bytes")) as usize;
        payload.resize(length, 0);
        stream
            .read_exact(&mut payload)
            .context("read ADB shell output")?;
        match header[0] {
            SHELL_STDOUT => {
                if let Some(out) = stdout.as_mut() {
                    out.write_all(&payload)?;
                }
            }
            SHELL_STDERR => {
                if let Some(err) = stderr.as_mut() {
                    err.write_all(&payload)?;
                }
            }
            SHELL_EXIT => return Ok(payload.first().copied()),
            _ => {}
        }
    }
}

fn shell_packet(id: u8, data: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(5 + data.len());
    packet.push(id);
    packet.extend_from_slice(&(data.len() as u32).to_le_bytes());
    packet.extend_from_slice(data);
    packet
}

fn copy_to_eof(stream: &mut TcpStream, out: &mut dyn Write) -> Result<()> {
    let mut buffer = vec![0_u8; SYNC_MAX_CHUNK];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(n) => out.write_all(&buffer[..n])?,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("read ADB stream"),
        }
    }
}

/// A connection in sync mode for `serial`.
fn sync_session(serial: &str) -> Result<TcpStream> {
    let mut stream = transport(serial, None)?;
    request(&mut stream, "sync:")?;
    Ok(stream)
}

fn sync_request(stream: &mut TcpStream, id: &[u8; 4], data: &[u8]) -> Result<()> {
    let length = u32::try_from(data.len()).context("sync request too long")?;
    let mut packet = Vec::with_capacity(8 + data.len());
    packet.extend_from_slice(id);
    packet.extend_from_slice(&length.to_le_bytes());
    packet.extend_from_slice(data);
    stream.write_all(&packet)?;
    Ok(())
}

/// Read one sync reply header: its id and length word.
fn sync_header(stream: &mut TcpStream) -> Result<([u8; 4], u32)> {
    let mut header = [0_u8; 8];
    stream.read_exact(&mut header)?;
    let id = header[..4].try_into().expect("4 bytes");
    let length = u32::from_le_bytes(header[4..].try_into().expect("4 bytes"));
    Ok((id, length))
}

fn sync_failure(stream: &mut TcpStream, length: u32) -> anyhow::Error {
    let mut message = vec![0_u8; length as usize];
    match stream.read_exact(&mut message) {
        Ok(()) => anyhow!("{}", String::from_utf8_lossy(&message)),
        Err(error) => anyhow!("sync failure without a readable reason: {error}"),
    }
}

fn sync_quit(stream: &mut TcpStream) {
    let _ = sync_request(stream, b"QUIT", &[]);
}

/// Write `input` to `remote` on the device.
pub(crate) fn push(serial: &str, input: &mut dyn Read, remote: &str) -> Result<()> {
    let mut stream = sync_session(serial)?;
    sync_request(
        &mut stream,
        b"SEND",
        format!("{remote},{PUSH_MODE}").as_bytes(),
    )?;
    let mut buffer = vec![0_u8; SYNC_MAX_CHUNK];
    loop {
        let n = match input.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("read local file"),
        };
        sync_request(&mut stream, b"DATA", &buffer[..n])?;
    }
    let mtime = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as u32)
        .unwrap_or(0);
    let mut done = Vec::with_capacity(8);
    done.extend_from_slice(b"DONE");
    done.extend_from_slice(&mtime.to_le_bytes());
    stream.write_all(&done)?;
    let (id, length) = sync_header(&mut stream)?;
    let result = match &id {
        b"OKAY" => Ok(()),
        b"FAIL" => Err(sync_failure(&mut stream, length)),
        other => Err(anyhow!(
            "unexpected sync reply {:?}",
            String::from_utf8_lossy(other)
        )),
    };
    sync_quit(&mut stream);
    result
}

/// Copy `remote` from the device into `output`.
pub(crate) fn pull(serial: &str, remote: &str, output: &mut dyn Write) -> Result<()> {
    let mut stream = sync_session(serial)?;
    sync_request(&mut stream, b"RECV", remote.as_bytes())?;
    let mut buffer = Vec::new();
    loop {
        let (id, length) = sync_header(&mut stream)?;
        match &id {
            b"DATA" => {
                buffer.resize(length as usize, 0);
                stream.read_exact(&mut buffer)?;
                output.write_all(&buffer)?;
            }
            b"DONE" => break,
            b"FAIL" => return Err(sync_failure(&mut stream, length)),
            other => bail!("unexpected sync reply {:?}", String::from_utf8_lossy(other)),
        }
    }
    sync_quit(&mut stream);
    Ok(())
}

/// Stream-install `apk` with `cmd package install -S` (Android 7+).
pub(crate) fn install(serial: &str, apk: &Path) -> Result<()> {
    let mut file = std::fs::File::open(apk).with_context(|| format!("open {}", apk.display()))?;
    let size = file
        .metadata()
        .with_context(|| format!("stat {}", apk.display()))?
        .len();
    let mut stream = transport(serial, None)?;
    request(&mut stream, &format!("exec:cmd package install -S {size}"))?;
    std::io::copy(&mut file, &mut stream).context("stream APK to the device")?;
    package_result(stream, "install")
}

pub(crate) fn uninstall(serial: &str, package: &str) -> Result<()> {
    let mut stream = transport(serial, None)?;
    request(
        &mut stream,
        &format!(
            "exec:cmd package uninstall {}",
            crate::config::quote_device_shell_arg(package)
        ),
    )?;
    package_result(stream, "uninstall")
}

/// Package manager replies `Success` or a `Failure [...]` line, then closes.
fn package_result(stream: TcpStream, action: &str) -> Result<()> {
    let mut text = String::new();
    stream
        .take(MAX_RESULT_TEXT)
        .read_to_string(&mut text)
        .with_context(|| format!("read package {action} result"))?;
    let text = text.trim();
    if text.lines().any(|line| line.trim() == "Success") {
        Ok(())
    } else if text.is_empty() {
        bail!("package {action} ended without a result")
    } else {
        bail!("{text}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn devices_listing_keeps_every_state() {
        assert_eq!(
            parse_devices("emulator-5554\tdevice\nR58M\tunauthorized\n\n"),
            vec![
                ("emulator-5554".to_string(), "device".to_string()),
                ("R58M".to_string(), "unauthorized".to_string()),
            ]
        );
    }

    /// A fake device side: accepts one connection and runs `script` on it.
    fn fake_server(script: impl FnOnce(TcpStream) + Send + 'static) -> TcpStream {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            script(stream);
        });
        TcpStream::connect(address).unwrap()
    }

    #[test]
    fn shell_v2_splits_streams_and_reports_the_exit_status() {
        let stream = fake_server(|mut device| {
            let mut close = [0_u8; 5];
            device.read_exact(&mut close).unwrap();
            assert_eq!(close, [SHELL_CLOSE_STDIN, 0, 0, 0, 0]);
            device
                .write_all(&shell_packet(SHELL_STDOUT, b"out "))
                .unwrap();
            device
                .write_all(&shell_packet(SHELL_STDERR, b"err"))
                .unwrap();
            device
                .write_all(&shell_packet(SHELL_STDOUT, b"put"))
                .unwrap();
            device.write_all(&shell_packet(SHELL_EXIT, &[3])).unwrap();
        });
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let status = shell_v2(stream, Some(&mut out), Some(&mut err)).unwrap();
        assert_eq!(status, Some(3));
        assert_eq!(out, b"out put");
        assert_eq!(err, b"err");
    }

    #[test]
    fn pull_reassembles_chunks_and_surfaces_failures() {
        let mut stream = fake_server(|mut device| {
            let mut request = [0_u8; 8 + 4];
            device.read_exact(&mut request).unwrap();
            assert_eq!(&request[..4], b"RECV");
            assert_eq!(&request[8..], b"/a/b");
            for chunk in [&b"hello "[..], &b"world"[..]] {
                let mut packet = b"DATA".to_vec();
                packet.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
                packet.extend_from_slice(chunk);
                device.write_all(&packet).unwrap();
            }
            device.write_all(b"DONE\0\0\0\0").unwrap();
        });
        sync_request(&mut stream, b"RECV", b"/a/b").unwrap();
        let mut output = Vec::new();
        // Drive the same loop `pull` runs after opening its session.
        loop {
            let (id, length) = sync_header(&mut stream).unwrap();
            match &id {
                b"DATA" => {
                    let mut chunk = vec![0; length as usize];
                    stream.read_exact(&mut chunk).unwrap();
                    output.extend_from_slice(&chunk);
                }
                b"DONE" => break,
                _ => panic!("unexpected reply"),
            }
        }
        assert_eq!(output, b"hello world");

        let mut stream = fake_server(|mut device| {
            let mut packet = b"FAIL".to_vec();
            packet.extend_from_slice(&(14_u32).to_le_bytes());
            packet.extend_from_slice(b"No such file!!");
            device.write_all(&packet).unwrap();
        });
        let (id, length) = sync_header(&mut stream).unwrap();
        assert_eq!(&id, b"FAIL");
        assert_eq!(
            sync_failure(&mut stream, length).to_string(),
            "No such file!!"
        );
    }

    /// Against a real device: `SHADOWDROID_WIRE_TEST_SERIAL=emulator-5554
    /// cargo test adb_wire -- --ignored`.
    #[test]
    #[ignore = "needs an attached device"]
    fn round_trips_against_a_real_device() {
        let serial = std::env::var("SHADOWDROID_WIRE_TEST_SERIAL").expect("set the serial");
        assert!(
            devices()
                .unwrap()
                .contains(&(serial.clone(), "device".to_string()))
        );

        let (mut out, mut err) = (Vec::new(), Vec::new());
        let status = shell(
            &serial,
            "echo out; echo err >&2; exit 7",
            Some(&mut out),
            Some(&mut err),
        )
        .unwrap();
        assert_eq!(
            (status, out.as_slice(), err.as_slice()),
            (Some(7), &b"out\n"[..], &b"err\n"[..])
        );
        // stdin is closed, so a command reading it ends instead of hanging.
        let mut out = Vec::new();
        assert_eq!(
            shell(&serial, "cat; echo done", Some(&mut out), None).unwrap(),
            Some(0)
        );
        assert_eq!(out, b"done\n");

        let payload: Vec<u8> = (0..3 * SYNC_MAX_CHUNK + 17)
            .map(|i| (i * 31 % 251) as u8)
            .collect();
        let remote = "/data/local/tmp/shadowdroid-wire-test.bin";
        push(&serial, &mut payload.as_slice(), remote).unwrap();
        let mut pulled = Vec::new();
        pull(&serial, remote, &mut pulled).unwrap();
        assert_eq!(pulled, payload);
        shell(&serial, &format!("rm -f {remote}"), None, None).unwrap();
        let missing = pull(&serial, remote, &mut Vec::new()).unwrap_err();
        assert!(missing.to_string().contains("No such file"), "{missing:#}");

        let error = uninstall(&serial, "io.github.andriyo.shadowdroid.not.installed").unwrap_err();
        assert!(error.to_string().contains("Failure"), "{error:#}");
        let error = transport("no-such-serial", None).unwrap_err();
        assert!(error.to_string().contains("not found"), "{error:#}");
    }

    #[test]
    fn package_results_need_a_success_line() {
        let ok = fake_server(|mut device| device.write_all(b"Success\n").unwrap());
        package_result(ok, "install").unwrap();
        let failed = fake_server(|mut device| {
            device
                .write_all(b"Failure [INSTALL_FAILED_VERSION_DOWNGRADE]\n")
                .unwrap()
        });
        let error = package_result(failed, "install").unwrap_err().to_string();
        assert_eq!(error, "Failure [INSTALL_FAILED_VERSION_DOWNGRADE]");
    }
}
