//! A JDWP connection: handshake, one reader task that demultiplexes replies
//! by packet id, one writer task, and per-request deadlines.
//!
//! Command packets from the VM (`Event.Composite`, DDM chunks) are forwarded
//! unparsed on the incoming channel; the session parses them once IDSizes is
//! known. When the stream ends every pending request fails with
//! [`JdwpError::Closed`] and the incoming channel reports [`Incoming::Closed`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use super::codec::{CodecError, IdSizes, Packet, PacketKind};
use super::protocol::{self, HANDSHAKE, HEADER_LEN};

#[derive(Debug, Clone, thiserror::Error)]
pub enum JdwpError {
    #[error("JDWP handshake failed: {0}")]
    Handshake(String),
    #[error("{command} did not reply within {timeout_ms} ms")]
    Timeout { command: String, timeout_ms: u64 },
    #[error("JDWP connection closed: {0}")]
    Closed(String),
    #[error("{command} failed with JDWP error {code} ({name})")]
    Vm {
        command: String,
        code: u16,
        name: &'static str,
    },
    #[error("{command}: {source}")]
    Codec {
        command: String,
        #[source]
        source: CodecError,
    },
}

impl JdwpError {
    pub fn vm_code(&self) -> Option<u16> {
        match self {
            JdwpError::Vm { code, .. } => Some(*code),
            _ => None,
        }
    }
}

/// A command packet the VM sent us.
#[derive(Debug)]
pub struct VmCommand {
    pub id: u32,
    pub command_set: u8,
    pub command: u8,
    pub data: Vec<u8>,
}

#[derive(Debug)]
pub enum Incoming {
    Command(VmCommand),
    Closed(String),
}

type Pending = Arc<Mutex<HashMap<u32, oneshot::Sender<Result<Packet, JdwpError>>>>>;

pub struct Connection {
    next_id: AtomicU32,
    pending: Pending,
    writer: mpsc::UnboundedSender<Vec<u8>>,
    sizes: RwLock<IdSizes>,
    closed: Arc<AtomicBool>,
    close_reason: Arc<Mutex<Option<String>>>,
}

impl Connection {
    /// Perform the handshake within `handshake_timeout`, then start the
    /// reader and writer tasks.
    pub async fn start<S>(
        mut stream: S,
        handshake_timeout: Duration,
    ) -> Result<(Arc<Connection>, mpsc::UnboundedReceiver<Incoming>), JdwpError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        tokio::time::timeout(handshake_timeout, handshake(&mut stream))
            .await
            .map_err(|_| {
                JdwpError::Handshake(format!(
                    "no handshake reply within {} ms",
                    handshake_timeout.as_millis()
                ))
            })??;

        let (read_half, write_half) = tokio::io::split(stream);
        let (writer_tx, writer_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let close_reason = Arc::new(Mutex::new(None));

        tokio::spawn(write_loop(write_half, writer_rx));
        tokio::spawn(read_loop(
            read_half,
            pending.clone(),
            incoming_tx,
            closed.clone(),
            close_reason.clone(),
        ));

        Ok((
            Arc::new(Connection {
                next_id: AtomicU32::new(1),
                pending,
                writer: writer_tx,
                sizes: RwLock::new(IdSizes::default()),
                closed,
                close_reason,
            }),
            incoming_rx,
        ))
    }

    pub fn sizes(&self) -> IdSizes {
        *self.sizes.read().expect("id sizes lock")
    }

    pub fn set_sizes(&self, sizes: IdSizes) {
        *self.sizes.write().expect("id sizes lock") = sizes;
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn closed_error(&self) -> JdwpError {
        JdwpError::Closed(
            self.close_reason
                .lock()
                .expect("close reason lock")
                .clone()
                .unwrap_or_else(|| "connection closed".into()),
        )
    }

    fn allocate_id(&self) -> u32 {
        loop {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            if id != 0 {
                return id;
            }
        }
    }

    /// Send one command and wait up to `timeout` for its reply. A JDWP error
    /// code in the reply becomes [`JdwpError::Vm`].
    pub async fn request(
        &self,
        command_set: u8,
        command: u8,
        data: Vec<u8>,
        timeout: Duration,
    ) -> Result<Vec<u8>, JdwpError> {
        if self.is_closed() {
            return Err(self.closed_error());
        }
        let id = self.allocate_id();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending lock").insert(id, tx);
        let bytes = Packet::command(id, command_set, command, data).encode();
        if self.writer.send(bytes).is_err() {
            self.pending.lock().expect("pending lock").remove(&id);
            return Err(self.closed_error());
        }
        let name = || protocol::command_name(command_set, command);
        let reply = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(reply)) => reply?,
            Ok(Err(_)) => return Err(self.closed_error()),
            Err(_) => {
                self.pending.lock().expect("pending lock").remove(&id);
                return Err(JdwpError::Timeout {
                    command: name(),
                    timeout_ms: timeout.as_millis() as u64,
                });
            }
        };
        match reply.kind {
            PacketKind::Reply { error: 0 } => Ok(reply.data),
            PacketKind::Reply { error } => Err(JdwpError::Vm {
                command: name(),
                code: error,
                name: protocol::error_name(error),
            }),
            PacketKind::Command { .. } => Err(JdwpError::Codec {
                command: name(),
                source: CodecError::Malformed("reply id answered by a command packet".into()),
            }),
        }
    }

    /// Queue a command without waiting for its reply. Used from `Drop` paths
    /// (dispose on teardown) where awaiting is impossible.
    pub fn send_nowait(&self, command_set: u8, command: u8, data: Vec<u8>) {
        if self.is_closed() {
            return;
        }
        let id = self.allocate_id();
        let _ = self
            .writer
            .send(Packet::command(id, command_set, command, data).encode());
    }

    /// Reply to a command the VM sent us (DDM chunks expect none, but the
    /// protocol allows any command to be answered).
    #[cfg(test)]
    pub fn reply(&self, id: u32, error: u16, data: Vec<u8>) {
        let _ = self.writer.send(Packet::reply(id, error, data).encode());
    }
}

async fn handshake<S>(stream: &mut S) -> Result<(), JdwpError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream
        .write_all(HANDSHAKE)
        .await
        .map_err(|e| JdwpError::Handshake(format!("write: {e}")))?;
    stream
        .flush()
        .await
        .map_err(|e| JdwpError::Handshake(format!("flush: {e}")))?;
    let mut reply = [0_u8; HANDSHAKE.len()];
    let mut read = 0;
    while read < reply.len() {
        let n = stream
            .read(&mut reply[read..])
            .await
            .map_err(|e| JdwpError::Handshake(format!("read: {e}")))?;
        if n == 0 {
            return Err(JdwpError::Handshake(format!(
                "connection closed after {read} of {} handshake bytes (another debugger may hold the process)",
                HANDSHAKE.len()
            )));
        }
        read += n;
    }
    if &reply != HANDSHAKE {
        return Err(JdwpError::Handshake(format!(
            "unexpected handshake reply {:?}",
            String::from_utf8_lossy(&reply)
        )));
    }
    Ok(())
}

async fn write_loop<W>(mut writer: W, mut rx: mpsc::UnboundedReceiver<Vec<u8>>)
where
    W: AsyncWrite + Unpin,
{
    while let Some(bytes) = rx.recv().await {
        if writer.write_all(&bytes).await.is_err() || writer.flush().await.is_err() {
            break;
        }
    }
    let _ = writer.shutdown().await;
}

async fn read_loop<R>(
    mut reader: R,
    pending: Pending,
    incoming: mpsc::UnboundedSender<Incoming>,
    closed: Arc<AtomicBool>,
    close_reason: Arc<Mutex<Option<String>>>,
) where
    R: AsyncRead + Unpin,
{
    let reason = loop {
        let mut header = [0_u8; HEADER_LEN];
        if let Err(error) = reader.read_exact(&mut header).await {
            break if error.kind() == std::io::ErrorKind::UnexpectedEof {
                "the VM closed the connection (process exited or detached)".to_string()
            } else {
                format!("read error: {error}")
            };
        }
        let (length, mut packet) = match Packet::decode_header(&header) {
            Ok(parsed) => parsed,
            Err(error) => break format!("framing error: {error}"),
        };
        packet.data = vec![0_u8; length - HEADER_LEN];
        if let Err(error) = reader.read_exact(&mut packet.data).await {
            break format!("read error mid-packet: {error}");
        }
        match packet.kind {
            PacketKind::Reply { .. } => {
                let waiter = pending.lock().expect("pending lock").remove(&packet.id);
                match waiter {
                    Some(waiter) => {
                        let _ = waiter.send(Ok(packet));
                    }
                    None => tracing::debug!("dropping late JDWP reply {}", packet.id),
                }
            }
            PacketKind::Command {
                command_set,
                command,
            } => {
                let _ = incoming.send(Incoming::Command(VmCommand {
                    id: packet.id,
                    command_set,
                    command,
                    data: packet.data,
                }));
            }
        }
    };
    *close_reason.lock().expect("close reason lock") = Some(reason.clone());
    closed.store(true, Ordering::SeqCst);
    let waiters: Vec<_> = pending
        .lock()
        .expect("pending lock")
        .drain()
        .map(|(_, waiter)| waiter)
        .collect();
    for waiter in waiters {
        let _ = waiter.send(Err(JdwpError::Closed(reason.clone())));
    }
    let _ = incoming.send(Incoming::Closed(reason));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jdwp::protocol::{set, vm};
    use tokio::io::duplex;

    /// Drive the VM side of a duplex pipe: answer the handshake, then hand
    /// the stream to `script`.
    async fn vm_side(mut stream: tokio::io::DuplexStream) -> tokio::io::DuplexStream {
        let mut hello = [0_u8; 14];
        stream.read_exact(&mut hello).await.unwrap();
        assert_eq!(&hello, HANDSHAKE);
        stream.write_all(HANDSHAKE).await.unwrap();
        stream
    }

    async fn read_packet(stream: &mut tokio::io::DuplexStream) -> Packet {
        let mut header = [0_u8; HEADER_LEN];
        stream.read_exact(&mut header).await.unwrap();
        let (length, mut packet) = Packet::decode_header(&header).unwrap();
        packet.data = vec![0; length - HEADER_LEN];
        stream.read_exact(&mut packet.data).await.unwrap();
        packet
    }

    #[tokio::test]
    async fn replies_are_matched_by_id_even_out_of_order() {
        let (client, server) = duplex(4096);
        let server = tokio::spawn(async move {
            let mut stream = vm_side(server).await;
            let first = read_packet(&mut stream).await;
            let second = read_packet(&mut stream).await;
            // Answer in reverse order, with an unsolicited DDM chunk between.
            stream
                .write_all(&Packet::reply(second.id, 0, b"second".to_vec()).encode())
                .await
                .unwrap();
            stream
                .write_all(&Packet::command(9000, set::DDM, 1, b"APNM".to_vec()).encode())
                .await
                .unwrap();
            stream
                .write_all(&Packet::reply(first.id, 0, b"first".to_vec()).encode())
                .await
                .unwrap();
            stream
        });
        let (conn, mut incoming) = Connection::start(client, Duration::from_secs(1))
            .await
            .unwrap();
        let timeout = Duration::from_secs(1);
        let (a, b) = tokio::join!(
            conn.request(set::VIRTUAL_MACHINE, vm::VERSION, vec![], timeout),
            conn.request(set::VIRTUAL_MACHINE, vm::ID_SIZES, vec![], timeout),
        );
        assert_eq!(a.unwrap(), b"first");
        assert_eq!(b.unwrap(), b"second");
        match incoming.recv().await.unwrap() {
            Incoming::Command(command) => {
                assert_eq!(command.command_set, set::DDM);
                assert_eq!(command.data, b"APNM");
            }
            other => panic!("{other:?}"),
        }
        drop(server.await.unwrap());
    }

    #[tokio::test]
    async fn error_codes_timeouts_and_eof_are_typed() {
        let (client, server) = duplex(4096);
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let mut stream = vm_side(server).await;
            let first = read_packet(&mut stream).await;
            stream
                .write_all(&Packet::reply(first.id, 112, vec![]).encode())
                .await
                .unwrap();
            // Second request: never answered, then the VM goes away.
            let _second = read_packet(&mut stream).await;
            let _ = release_rx.await;
            drop(stream);
        });
        let (conn, mut incoming) = Connection::start(client, Duration::from_secs(1))
            .await
            .unwrap();
        let error = conn
            .request(
                set::VIRTUAL_MACHINE,
                vm::SUSPEND,
                vec![],
                Duration::from_secs(1),
            )
            .await
            .unwrap_err();
        assert_eq!(error.vm_code(), Some(112));
        assert!(error.to_string().contains("VirtualMachine.Suspend"));
        assert!(error.to_string().contains("VM_DEAD"));

        let error = conn
            .request(
                set::VIRTUAL_MACHINE,
                vm::RESUME,
                vec![],
                Duration::from_millis(50),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, JdwpError::Timeout { .. }), "{error}");
        assert!(error.to_string().contains("VirtualMachine.Resume"));

        release_tx.send(()).unwrap();
        server.await.unwrap();
        loop {
            match incoming.recv().await.unwrap() {
                Incoming::Closed(reason) => {
                    assert!(reason.contains("closed"), "{reason}");
                    break;
                }
                Incoming::Command(_) => continue,
            }
        }
        assert!(conn.is_closed());
        let error = conn
            .request(
                set::VIRTUAL_MACHINE,
                vm::VERSION,
                vec![],
                Duration::from_secs(1),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, JdwpError::Closed(_)), "{error}");
    }

    #[tokio::test]
    async fn pending_requests_fail_when_the_vm_dies_mid_request() {
        let (client, server) = duplex(4096);
        let server = tokio::spawn(async move {
            let mut stream = vm_side(server).await;
            let _request = read_packet(&mut stream).await;
            // Half a reply header, then EOF.
            stream.write_all(&[0, 0, 0]).await.unwrap();
        });
        let (conn, _incoming) = Connection::start(client, Duration::from_secs(1))
            .await
            .unwrap();
        let error = conn
            .request(
                set::VIRTUAL_MACHINE,
                vm::VERSION,
                vec![],
                Duration::from_secs(5),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, JdwpError::Closed(_)), "{error}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn handshake_failures_are_reported() {
        let (client, mut server) = duplex(64);
        tokio::spawn(async move {
            let mut hello = [0_u8; 14];
            server.read_exact(&mut hello).await.unwrap();
            // Close without answering: what a second debugger sees.
        });
        let error = Connection::start(client, Duration::from_secs(1))
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("another debugger"), "{error}");

        let (client, mut server) = duplex(64);
        tokio::spawn(async move {
            let mut hello = [0_u8; 14];
            server.read_exact(&mut hello).await.unwrap();
            server.write_all(b"NOT-A-JDWP-VM!").await.unwrap();
        });
        let error = Connection::start(client, Duration::from_secs(1))
            .await
            .err()
            .unwrap();
        assert!(
            error.to_string().contains("unexpected handshake"),
            "{error}"
        );

        let (client, _server) = duplex(64);
        let error = Connection::start(client, Duration::from_millis(30))
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("no handshake reply"), "{error}");
    }

    #[tokio::test]
    async fn the_client_can_answer_vm_commands() {
        let (client, server) = duplex(4096);
        let server = tokio::spawn(async move {
            let mut stream = vm_side(server).await;
            read_packet(&mut stream).await
        });
        let (conn, _incoming) = Connection::start(client, Duration::from_secs(1))
            .await
            .unwrap();
        conn.reply(77, 0, vec![1, 2]);
        let packet = server.await.unwrap();
        assert_eq!(packet, Packet::reply(77, 0, vec![1, 2]));
    }
}
