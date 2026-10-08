//! A minimal fake ADB server for integration tests: enough smart-socket
//! protocol for the CLI's device resolution and runtime admission
//! (`host:devices`, `host:transport:<serial>`, legacy `shell:`), so tests can
//! drive device-scoped commands without a real device. Shell protocol v2 is
//! refused, which makes the client fall back to the legacy shell service.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub const BOOT_ID: &str = "0f0e0d0c-0b0a-0908-0706-050403020100";

pub struct FakeAdb {
    port: u16,
    online: Arc<AtomicBool>,
}

impl FakeAdb {
    /// Serve `serial` as an online device until the process exits.
    pub fn start(serial: &str) -> FakeAdb {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake adb");
        let port = listener.local_addr().unwrap().port();
        let online = Arc::new(AtomicBool::new(true));
        let serial = serial.to_string();
        let state = online.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let serial = serial.clone();
                let online = state.clone();
                std::thread::spawn(move || {
                    let _ = serve(stream, &serial, online.load(Ordering::SeqCst));
                });
            }
        });
        FakeAdb { port, online }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Report the device as absent from `host:devices` from now on.
    pub fn set_online(&self, online: bool) {
        self.online.store(online, Ordering::SeqCst);
    }
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length)?;
    let length =
        usize::from_str_radix(std::str::from_utf8(&length).unwrap_or("0"), 16).unwrap_or(0);
    let mut body = vec![0_u8; length];
    stream.read_exact(&mut body)?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

fn okay_with_body(stream: &mut TcpStream, body: &str) -> std::io::Result<()> {
    stream.write_all(format!("OKAY{:04x}{body}", body.len()).as_bytes())
}

fn fail(stream: &mut TcpStream, message: &str) -> std::io::Result<()> {
    stream.write_all(format!("FAIL{:04x}{message}", message.len()).as_bytes())
}

fn serve(mut stream: TcpStream, serial: &str, online: bool) -> std::io::Result<()> {
    let request = read_request(&mut stream)?;
    if request == "host:devices" || request == "host:devices-l" {
        let listing = if online {
            format!("{serial}\tdevice\n")
        } else {
            String::new()
        };
        return okay_with_body(&mut stream, &listing);
    }
    if let Some(target) = request.strip_prefix("host:transport:") {
        if target != serial || !online {
            return fail(&mut stream, &format!("device '{target}' not found"));
        }
        stream.write_all(b"OKAY")?;
        let service = read_request(&mut stream)?;
        if service.starts_with("shell,v2") {
            return fail(&mut stream, "shell protocol v2 not supported by fake");
        }
        if let Some(command) = service.strip_prefix("shell:") {
            stream.write_all(b"OKAY")?;
            let output = if command.contains("/proc/sys/kernel/random/boot_id") {
                format!("{BOOT_ID}\n")
            } else if command.contains("/data/local/tmp/shadowdroid-authority") {
                authority_marker(command)
            } else {
                String::new()
            };
            stream.write_all(output.as_bytes())?;
            return Ok(());
        }
        return fail(&mut stream, &format!("unsupported service {service}"));
    }
    if request.starts_with("host:") {
        return okay_with_body(&mut stream, "");
    }
    fail(&mut stream, "unsupported request")
}

/// The device-side authority marker as a fresh device answers it: a read
/// reports `absent`; a claim writes the client's binding (the quoted
/// argument of `printf '%s'`) and reads it back; a release reports
/// `released`.
fn authority_marker(command: &str) -> String {
    if let Some(rest) = command.split("printf '%s' '").nth(1) {
        return rest.split('\'').next().unwrap_or_default().to_string();
    }
    if command.contains("printf released") {
        return "released".to_string();
    }
    "absent".to_string()
}
