//! Exercise CLI protocol selection and serialization without a proxy or device.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::process::Command;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

#[tokio::test]
async fn intercept_clear_selects_http_unless_direction_selects_websocket() {
    for (args, operation, command) in [
        (vec!["--clear"], "intercept", "net_intercept"),
        (
            vec!["--dir", "c2s", "--clear"],
            "ws_intercept",
            "net_ws_intercept",
        ),
        (
            vec!["--dir", "s2c", "--clear"],
            "ws_intercept",
            "net_ws_intercept",
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let net_dir = temp.path().join(".shadowdroid/net");
        std::fs::create_dir_all(&net_dir).unwrap();
        let serial = "intercept-test";
        let digest: String = Sha256::digest(serial.as_bytes())[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        std::fs::write(
            net_dir.join(format!("{serial}-{digest}.ctl")),
            listener.local_addr().unwrap().port().to_string(),
        )
        .unwrap();

        let home = temp.path().to_path_buf();
        let child = tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_shadowdroid"))
                .args(["-d", serial, "net", "intercept"])
                .args(args)
                .env("HOME", &home)
                .env("SHADOWDROID_QUIET", "1")
                .current_dir(&home)
                .output()
                .unwrap()
        });
        let request = tokio::time::timeout(Duration::from_secs(5), async {
            if operation == "intercept" {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                assert_eq!(
                    serde_json::from_str::<Value>(&line).unwrap(),
                    json!({"op":"status", "serial":serial})
                );
                stream
                    .write_all(b"{\"ok\":true,\"capabilities\":{\"http_intercept_clear\":true}}\n")
                    .await
                    .unwrap();
            }
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            stream
                .write_all(b"{\"ok\":true,\"intercepting\":false}\n")
                .await
                .unwrap();
            serde_json::from_str::<Value>(&line).unwrap()
        })
        .await
        .expect("CLI must reach only the fixture control socket");
        assert_eq!(
            request,
            json!({"op": operation, "clear": true, "serial": serial})
        );
        let output = child.await.unwrap();
        assert!(output.status.success(), "{output:?}");
        let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(reply["cmd"], command);
        assert_eq!(reply["intercepting"], false);
    }
}

#[tokio::test]
async fn http_mutations_require_daemon_capabilities_before_changing_state() {
    for (args, capability, unsupported_code, request, response) in [
        (
            vec!["drop", "f1", "--transport"],
            "http_transport_abort",
            "net_transport_abort_unsupported",
            json!({"op":"drop", "id":"f1", "status":null, "transport":true, "serial":"drop-test"}),
            json!({"ok":true, "released":true, "action":"abort_transport"}),
        ),
        (
            vec!["intercept", "--clear"],
            "http_intercept_clear",
            "net_http_intercept_clear_unsupported",
            json!({"op":"intercept", "clear":true, "serial":"drop-test"}),
            json!({"ok":true, "intercepting":false}),
        ),
    ] {
        for advertised in [None, Some(false), Some(true)] {
            let supported = advertised == Some(true);
            let temp = tempfile::tempdir().unwrap();
            let net_dir = temp.path().join(".shadowdroid/net");
            std::fs::create_dir_all(&net_dir).unwrap();
            let serial = "drop-test";
            let digest: String = Sha256::digest(serial.as_bytes())[..8]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            std::fs::write(
                net_dir.join(format!("{serial}-{digest}.ctl")),
                listener.local_addr().unwrap().port().to_string(),
            )
            .unwrap();
            let fixture_home = temp.path().to_path_buf();
            let args = args.clone();
            let child = tokio::task::spawn_blocking(move || {
                Command::new(env!("CARGO_BIN_EXE_shadowdroid"))
                    .args(["-d", serial, "net"])
                    .args(args)
                    .env("HOME", &fixture_home)
                    .current_dir(&fixture_home)
                    .output()
                    .unwrap()
            });
            let requests = tokio::time::timeout(Duration::from_secs(5), async {
                let mut requests = vec![];
                let mut status = json!({"ok": true, "capabilities": {}});
                if let Some(advertised) = advertised {
                    status["capabilities"][capability] = advertised.into();
                }
                for response in std::iter::once(status).chain(supported.then(|| response.clone())) {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut stream = BufReader::new(stream);
                    let mut line = String::new();
                    stream.read_line(&mut line).await.unwrap();
                    requests.push(serde_json::from_str::<Value>(&line).unwrap());
                    stream
                        .write_all(format!("{response}\n").as_bytes())
                        .await
                        .unwrap();
                }
                requests
            })
            .await
            .unwrap();
            assert_eq!(requests[0], json!({"op": "status", "serial": serial}));
            let output = child.await.unwrap();
            assert_eq!(output.status.success(), supported, "{output:?}");
            let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
            if supported {
                assert_eq!(requests[1], request);
                for (key, expected) in response.as_object().unwrap() {
                    assert_eq!(reply[key], *expected);
                }
            } else {
                assert_eq!(reply["code"], unsupported_code);
                assert!(
                    tokio::time::timeout(Duration::from_millis(50), listener.accept())
                        .await
                        .is_err(),
                    "unsupported daemon must receive no mutation"
                );
            }
        }
    }
}
