//! WebSocket (WS/WSS) frame capture — issue #32.
//!
//! After an HTTP request upgrades to a WebSocket, the connection becomes a
//! bidirectional stream of RFC 6455 frames that [crate::net::proxy] previously
//! only blind-tunnelled. This module decodes that stream **for observation
//! only**: the proxy still forwards the original bytes verbatim (masking and
//! all), and a *copy* is fed here to reconstruct messages. Nothing decoded here
//! ever re-enters the wire, so captured traffic is never altered.
//!
//! Shape:
//!   - [FrameDecoder] is an incremental parser over a per-direction byte buffer.
//!     It retains a bounded prefix of each frame's payload (skipping the rest to
//!     stay in sync) so one firehose frame can't exhaust memory.
//!   - [MessageAssembler] reassembles fragmented data messages, surfaces control
//!     frames (ping/pong/close) as their own units, and applies
//!     `permessage-deflate` inflation (RFC 7692) with per-direction context.
//!   - The record types ([WsSessionRecord], [WsMessageRecord], [WsCloseRecord])
//!     are the durable session-log lines; their `*_event` methods derive the
//!     compact [Event]s that `net log`/`watch` stream.
//!
//! The async tap driver that wires two [FrameDecoder]s onto the upgraded
//! sockets lives in [crate::net::proxy] (it needs the proxy's runtime handles);
//! everything token- and protocol-shaped is here and unit-tested in isolation.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::events::Event;
use crate::ids::Serial;
use crate::net::proxy::ProxyContext;
use crate::net::store;

/// Max bytes of a single message payload retained in the session log (per
/// message, after any decompression). `net show` writes whatever was retained;
/// `truncated` flags anything larger. Smaller than an HTTP body cap because a
/// socket can carry thousands of messages.
pub const RETAIN_CAP: usize = 64 * 1024;
/// Max compressed bytes buffered for one message before inflation — bounds
/// per-message memory. A `permessage-deflate` message whose on-wire compressed
/// size exceeds this can't be inflated; under context takeover that also
/// desyncs the shared window, so later compressed messages are reported
/// un-decoded rather than guessed at.
pub const COMPRESSED_ACCUM_CAP: usize = 256 * 1024;
/// Max decompressed bytes to *produce* for one message before treating it as a
/// decompression bomb. The inflater still consumes all input up to here so the
/// sliding window stays in sync across messages (RFC 7692 context takeover).
pub const MAX_DECOMPRESS: usize = 8 * 1024 * 1024;
/// Chars of payload shown inline in the compact `ws_msg` event / `net ws` row.
pub const PREVIEW_CAP: usize = 200;
/// A frame whose declared length exceeds this is a protocol error, not real
/// traffic — refuse it and mark the direction desynced rather than trusting a
/// bogus 8-byte length field.
const MAX_FRAME_LEN: u64 = 512 * 1024 * 1024;

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Direction of a frame relative to the app under test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// App → server (client frames; masked on the wire).
    ClientToServer,
    /// Server → app (server frames; unmasked).
    ServerToClient,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::ClientToServer => "c2s",
            Direction::ServerToClient => "s2c",
        }
    }
}

/// A WebSocket frame opcode (RFC 6455 §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
    Continuation,
    Text,
    Binary,
    Close,
    Ping,
    Pong,
    /// A reserved / unknown opcode (kept so an odd frame is reported, not hidden).
    Reserved(u8),
}

impl Opcode {
    fn from_u8(value: u8) -> Opcode {
        match value {
            0x0 => Opcode::Continuation,
            0x1 => Opcode::Text,
            0x2 => Opcode::Binary,
            0x8 => Opcode::Close,
            0x9 => Opcode::Ping,
            0xA => Opcode::Pong,
            other => Opcode::Reserved(other),
        }
    }

    fn is_control(self) -> bool {
        matches!(self, Opcode::Close | Opcode::Ping | Opcode::Pong)
    }

    /// The wire label used in records/events.
    pub fn as_str(self) -> &'static str {
        match self {
            Opcode::Continuation => "continuation",
            Opcode::Text => "text",
            Opcode::Binary => "binary",
            Opcode::Close => "close",
            Opcode::Ping => "ping",
            Opcode::Pong => "pong",
            Opcode::Reserved(_) => "reserved",
        }
    }
}

/// One decoded frame with a bounded, unmasked payload prefix.
#[derive(Debug, Clone)]
pub struct RawFrame {
    pub fin: bool,
    /// RSV1 — on the first frame of a message it signals `permessage-deflate`.
    pub rsv1: bool,
    pub opcode: Opcode,
    /// Unmasked payload, retained up to the decoder's cap.
    pub payload: Vec<u8>,
    /// Full on-wire payload length (may exceed `payload.len()` if retention
    /// capped it).
    pub payload_len: u64,
    /// The retained prefix is shorter than `payload_len`.
    pub truncated: bool,
}

#[derive(Debug)]
struct Pending {
    fin: bool,
    rsv1: bool,
    opcode: Opcode,
    masked: bool,
    mask: [u8; 4],
    /// Total payload bytes still to consume from the stream.
    remaining: u64,
    /// Unmasked retained prefix (bounded by `retain_cap`).
    retained: Vec<u8>,
    payload_len: u64,
    /// Absolute payload offset consumed so far (drives the masking index across
    /// chunk boundaries).
    consumed: u64,
    retain_cap: usize,
}

/// Incremental RFC 6455 frame parser for one direction. Feed it wire bytes with
/// [FrameDecoder::push]; it returns every frame that completed. On a malformed
/// header it latches `desynced` and stops decoding (the tunnel keeps flowing —
/// decoding is best-effort observation).
#[derive(Debug)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    pending: Option<Pending>,
    retain_cap: usize,
    desynced: bool,
}

impl FrameDecoder {
    pub fn new() -> FrameDecoder {
        FrameDecoder {
            buf: Vec::new(),
            pending: None,
            // Retain enough of a frame to feed the compressed-message
            // accumulator; the assembler applies the finer per-message cap.
            retain_cap: COMPRESSED_ACCUM_CAP,
            desynced: false,
        }
    }

    pub fn desynced(&self) -> bool {
        self.desynced
    }

    /// Feed wire bytes; return frames completed by this chunk (possibly empty).
    pub fn push(&mut self, data: &[u8]) -> Vec<RawFrame> {
        let mut out = Vec::new();
        if self.desynced {
            return out;
        }
        self.buf.extend_from_slice(data);
        loop {
            if self.pending.is_some() {
                if let Some(frame) = self.drive_payload() {
                    out.push(frame);
                    continue;
                }
                break; // need more payload bytes
            }
            match parse_header(&self.buf) {
                HeaderParse::Need => break,
                HeaderParse::Malformed => {
                    self.desynced = true;
                    self.buf.clear();
                    break;
                }
                HeaderParse::Parsed { header, consumed } => {
                    self.buf.drain(..consumed);
                    self.pending = Some(Pending {
                        fin: header.fin,
                        rsv1: header.rsv1,
                        opcode: header.opcode,
                        masked: header.masked,
                        mask: header.mask,
                        remaining: header.payload_len,
                        retained: Vec::new(),
                        payload_len: header.payload_len,
                        consumed: 0,
                        retain_cap: self.retain_cap,
                    });
                }
            }
        }
        out
    }

    /// Consume as much of the current frame's payload as is buffered; return the
    /// finished frame once its last byte lands.
    fn drive_payload(&mut self) -> Option<RawFrame> {
        let pending = self.pending.as_mut()?;
        let available = self.buf.len() as u64;
        let take = pending.remaining.min(available) as usize;
        if take > 0 {
            let chunk: Vec<u8> = self.buf.drain(..take).collect();
            for (index, &byte) in chunk.iter().enumerate() {
                let absolute = pending.consumed + index as u64;
                let plain = if pending.masked {
                    byte ^ pending.mask[(absolute & 3) as usize]
                } else {
                    byte
                };
                if pending.retained.len() < pending.retain_cap {
                    pending.retained.push(plain);
                }
            }
            pending.consumed += take as u64;
            pending.remaining -= take as u64;
        }
        if pending.remaining == 0 {
            let pending = self.pending.take().unwrap();
            let truncated = (pending.retained.len() as u64) < pending.payload_len;
            Some(RawFrame {
                fin: pending.fin,
                rsv1: pending.rsv1,
                opcode: pending.opcode,
                payload: pending.retained,
                payload_len: pending.payload_len,
                truncated,
            })
        } else {
            None
        }
    }
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

struct FrameHeader {
    fin: bool,
    rsv1: bool,
    opcode: Opcode,
    masked: bool,
    mask: [u8; 4],
    payload_len: u64,
}

enum HeaderParse {
    /// Not enough bytes buffered yet.
    Need,
    /// A header was parsed; `consumed` bytes precede the payload.
    Parsed {
        header: FrameHeader,
        consumed: usize,
    },
    /// The bytes cannot be a valid frame header.
    Malformed,
}

fn parse_header(buf: &[u8]) -> HeaderParse {
    if buf.len() < 2 {
        return HeaderParse::Need;
    }
    let b0 = buf[0];
    let b1 = buf[1];
    let fin = b0 & 0x80 != 0;
    let rsv1 = b0 & 0x40 != 0;
    let opcode = Opcode::from_u8(b0 & 0x0F);
    let masked = b1 & 0x80 != 0;
    let len7 = b1 & 0x7F;

    let (payload_len, len_bytes) = match len7 {
        126 => {
            if buf.len() < 4 {
                return HeaderParse::Need;
            }
            (u16::from_be_bytes([buf[2], buf[3]]) as u64, 2)
        }
        127 => {
            if buf.len() < 10 {
                return HeaderParse::Need;
            }
            let mut raw = [0u8; 8];
            raw.copy_from_slice(&buf[2..10]);
            (u64::from_be_bytes(raw), 8)
        }
        other => (other as u64, 0),
    };

    // Control frames must be <=125 bytes and never fragmented (RFC 6455 §5.5).
    if opcode.is_control() && (payload_len > 125 || !fin) {
        return HeaderParse::Malformed;
    }
    if payload_len > MAX_FRAME_LEN {
        return HeaderParse::Malformed;
    }

    let header_len = 2 + len_bytes + if masked { 4 } else { 0 };
    if buf.len() < header_len {
        return HeaderParse::Need;
    }
    let mut mask = [0u8; 4];
    if masked {
        mask.copy_from_slice(&buf[2 + len_bytes..2 + len_bytes + 4]);
    }
    HeaderParse::Parsed {
        header: FrameHeader {
            fin,
            rsv1,
            opcode,
            masked,
            mask,
            payload_len,
        },
        consumed: header_len,
    }
}

/// A reassembled application message (or a single control frame). This is the
/// unit an agent inspects.
#[derive(Debug, Clone)]
pub struct Message {
    /// The message's data type (text/binary) or the control opcode.
    pub opcode: Opcode,
    /// Retained payload prefix, after any decompression.
    pub payload: Vec<u8>,
    /// Full application payload length (decompressed); may exceed `payload.len()`.
    pub payload_len: u64,
    /// On-wire payload length summed across frames (compressed if applicable).
    pub wire_len: u64,
    /// Retained prefix is shorter than `payload_len` (payload cap or a skipped
    /// oversized frame).
    pub truncated: bool,
    /// Frames that composed this message (1 for control / unfragmented).
    pub frame_count: u32,
    /// The first frame carried RSV1 (`permessage-deflate`).
    pub compressed: bool,
    /// The compressed payload was successfully inflated.
    pub decompressed: bool,
}

impl Message {
    /// WebSocket close frames carry a 2-byte big-endian code then a UTF-8 reason.
    pub fn close_code_reason(&self) -> Option<(u16, String)> {
        if self.opcode != Opcode::Close {
            return None;
        }
        if self.payload.len() < 2 {
            return Some((1005, String::new())); // "no status received"
        }
        let code = u16::from_be_bytes([self.payload[0], self.payload[1]]);
        let reason = String::from_utf8_lossy(&self.payload[2..]).into_owned();
        Some((code, reason))
    }
}

/// `permessage-deflate` parameters negotiated in the handshake response.
#[derive(Debug, Clone, Copy, Default)]
pub struct DeflateParams {
    pub enabled: bool,
    pub client_no_context_takeover: bool,
    pub server_no_context_takeover: bool,
}

/// Parse the negotiated `Sec-WebSocket-Extensions` response header. Only
/// `permessage-deflate` is understood; window-bits are accepted but ignored
/// (inflating with the max 32 KiB window is always safe regardless of the
/// compressor's smaller window).
pub fn parse_deflate_params(extensions: Option<&str>) -> DeflateParams {
    let Some(value) = extensions else {
        return DeflateParams::default();
    };
    for offer in value.split(',') {
        let mut parts = offer.split(';').map(str::trim);
        if parts.next().map(str::to_ascii_lowercase).as_deref() != Some("permessage-deflate") {
            continue;
        }
        let mut params = DeflateParams {
            enabled: true,
            ..Default::default()
        };
        for param in parts {
            let key = param
                .split('=')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            match key.as_str() {
                "client_no_context_takeover" => params.client_no_context_takeover = true,
                "server_no_context_takeover" => params.server_no_context_takeover = true,
                _ => {}
            }
        }
        return params;
    }
    DeflateParams::default()
}

/// Per-direction inflate context for `permessage-deflate`. Kept across messages
/// unless `no_context_takeover` was negotiated for this direction.
struct Inflater {
    decompress: flate2::Decompress,
    reset_each_message: bool,
    /// The stream reached a final DEFLATE block (BFINAL=1). RFC 7692 forbids
    /// this (messages must sync-flush), so a peer that emits it leaves the
    /// context-takeover window dead: later messages can't be decoded.
    terminated: bool,
}

impl Inflater {
    fn new(reset_each_message: bool) -> Inflater {
        Inflater {
            decompress: flate2::Decompress::new(false),
            reset_each_message,
            terminated: false,
        }
    }

    /// Inflate one message's concatenated compressed payload (RFC 7692 §7.2.2:
    /// append the empty-block trailer `00 00 FF FF`). Retains at most `retain`
    /// bytes of output but **consumes all input** so the sliding window advances
    /// even when the message is larger than we store — otherwise context takeover
    /// would desync every later message. `complete=false` marks a decompression
    /// bomb (over [MAX_DECOMPRESS]): we stopped feeding, so the window (and every
    /// later compressed message under context takeover) is now unreliable.
    /// Returns `None` on a genuine zlib error or a dead (BFINAL-terminated) stream.
    fn inflate(&mut self, compressed: &[u8], retain: usize) -> Option<InflateResult> {
        if self.reset_each_message {
            self.decompress.reset(false);
            self.terminated = false;
        } else if self.terminated {
            // A prior message ended the shared stream; anything after is
            // undecodable — report a desync rather than a bogus empty decode.
            return None;
        }
        let mut input = Vec::with_capacity(compressed.len() + 4);
        input.extend_from_slice(compressed);
        input.extend_from_slice(&[0x00, 0x00, 0xFF, 0xFF]);

        let start_out = self.decompress.total_out();
        let mut out = Vec::new();
        let mut chunk = vec![0u8; 32 * 1024];
        let mut offset = 0usize;
        let mut complete = true;
        loop {
            let before_in = self.decompress.total_in();
            let before_out = self.decompress.total_out();
            let status = self
                .decompress
                .decompress(&input[offset..], &mut chunk, flate2::FlushDecompress::Sync)
                .ok()?;
            let produced = (self.decompress.total_out() - before_out) as usize;
            if out.len() < retain {
                let take = produced.min(retain - out.len());
                out.extend_from_slice(&chunk[..take]);
            }
            offset += (self.decompress.total_in() - before_in) as usize;
            if (self.decompress.total_out() - start_out) as usize > MAX_DECOMPRESS {
                complete = false; // bomb guard: stop consuming, window now desynced
                break;
            }
            match status {
                flate2::Status::StreamEnd => {
                    self.terminated = true; // BFINAL — the shared stream is now dead
                    break;
                }
                flate2::Status::Ok | flate2::Status::BufError => {
                    if offset >= input.len() && produced == 0 {
                        break;
                    }
                }
            }
        }
        Some(InflateResult {
            retained: out,
            full_len: self.decompress.total_out() - start_out,
            complete,
        })
    }
}

struct InflateResult {
    /// Decompressed bytes, capped at the retain limit.
    retained: Vec<u8>,
    /// Full decompressed length (may exceed `retained.len()`).
    full_len: u64,
    /// The whole compressed input was consumed (window stays in sync).
    complete: bool,
}

/// Reassembles frames into [Message]s for one direction: concatenates
/// continuation frames, passes control frames straight through, and inflates
/// `permessage-deflate` messages.
pub struct MessageAssembler {
    deflate: DeflateParams,
    inflater: Option<Inflater>,
    // In-progress fragmented data message.
    active: Option<ActiveMessage>,
    /// Cap on retained *decompressed* (or raw, for uncompressed) output.
    retain_cap: usize,
    /// Under context takeover, a message we couldn't fully consume leaves the
    /// shared window unusable; once set, later compressed messages are reported
    /// un-decoded rather than silently corrupted.
    deflate_desynced: bool,
    /// `true` when this direction negotiated `*_no_context_takeover`. A decode
    /// failure then affects only the current message because the inflater is
    /// reset before the next one.
    reset_deflate_each_message: bool,
}

struct ActiveMessage {
    opcode: Opcode,
    payload: Vec<u8>,
    payload_len: u64,
    wire_len: u64,
    truncated: bool,
    frame_count: u32,
    compressed: bool,
}

impl MessageAssembler {
    pub fn new(direction: Direction, deflate: DeflateParams) -> MessageAssembler {
        let reset_deflate_each_message = deflate.enabled
            && match direction {
                Direction::ClientToServer => deflate.client_no_context_takeover,
                Direction::ServerToClient => deflate.server_no_context_takeover,
            };
        let inflater = if deflate.enabled {
            Some(Inflater::new(reset_deflate_each_message))
        } else {
            None
        };
        MessageAssembler {
            deflate,
            inflater,
            active: None,
            retain_cap: RETAIN_CAP,
            deflate_desynced: false,
            reset_deflate_each_message,
        }
    }

    fn mark_deflate_desynced(&mut self) {
        if !self.reset_deflate_each_message {
            self.deflate_desynced = true;
        }
    }

    /// Feed one decoded frame; return a completed [Message] when one finishes.
    pub fn accept(&mut self, frame: RawFrame) -> Option<Message> {
        if frame.opcode.is_control() {
            // Control frames are self-contained and may interleave a fragmented
            // data message, so they never touch `active`.
            return Some(Message {
                opcode: frame.opcode,
                payload_len: frame.payload_len,
                wire_len: frame.payload_len,
                truncated: frame.truncated,
                frame_count: 1,
                compressed: false,
                decompressed: false,
                payload: frame.payload,
            });
        }

        match frame.opcode {
            Opcode::Text | Opcode::Binary => {
                // A new data message. (A prior unfinished one is abandoned — the
                // peer violated framing; don't leak it into this message.)
                let mut active = ActiveMessage {
                    opcode: frame.opcode,
                    payload: Vec::new(),
                    payload_len: 0,
                    wire_len: 0,
                    truncated: false,
                    frame_count: 0,
                    compressed: self.deflate.enabled && frame.rsv1,
                };
                let fin = frame.fin;
                self.append_frame(&mut active, frame);
                if fin {
                    Some(self.finish(active))
                } else {
                    self.active = Some(active);
                    None
                }
            }
            Opcode::Continuation => {
                // Continuation with no start — ignore, stay resilient.
                let mut active = self.active.take()?;
                let fin = frame.fin;
                self.append_frame(&mut active, frame);
                if fin {
                    Some(self.finish(active))
                } else {
                    self.active = Some(active);
                    None
                }
            }
            Opcode::Reserved(_) => None,
            Opcode::Close | Opcode::Ping | Opcode::Pong => None, // handled above
        }
    }

    fn append_frame(&self, active: &mut ActiveMessage, frame: RawFrame) {
        active.frame_count += 1;
        active.wire_len += frame.payload_len;
        if frame.truncated {
            active.truncated = true;
        }
        // Compressed messages accumulate the raw compressed bytes up to the
        // (larger) input budget so we can inflate the whole message and keep the
        // window in sync; uncompressed ones only ever retain the output cap.
        let cap = if active.compressed {
            COMPRESSED_ACCUM_CAP
        } else {
            self.retain_cap
        };
        let room = cap.saturating_sub(active.payload.len());
        if room >= frame.payload.len() {
            active.payload.extend_from_slice(&frame.payload);
        } else {
            active.payload.extend_from_slice(&frame.payload[..room]);
            active.truncated = true;
        }
        active.payload_len += frame.payload_len;
    }

    fn finish(&mut self, active: ActiveMessage) -> Message {
        let mut message = Message {
            opcode: active.opcode,
            payload: active.payload,
            payload_len: active.payload_len,
            wire_len: active.wire_len,
            truncated: active.truncated,
            frame_count: active.frame_count,
            compressed: active.compressed,
            decompressed: false,
        };
        if !message.compressed {
            return message;
        }
        // A compressed message we couldn't buffer whole (`truncated`) can't be
        // inflated. Feeding a partial stream would corrupt a takeover context;
        // with no context takeover the next message starts from a clean inflater.
        if message.truncated {
            self.mark_deflate_desynced();
            message.truncated = true; // retained bytes are still compressed
            return message;
        }
        if self.deflate_desynced {
            message.truncated = true; // retained bytes are still compressed
            return message;
        }
        let Some(inflater) = self.inflater.as_mut() else {
            return message;
        };
        // `wire_len` is the on-wire compressed size; `payload_len` becomes the
        // application (decompressed) size.
        message.wire_len = message.payload_len;
        match inflater.inflate(&message.payload, self.retain_cap) {
            Some(result) if result.complete => {
                message.payload = result.retained;
                message.payload_len = result.full_len;
                message.truncated = message.payload.len() as u64 != result.full_len;
                message.decompressed = true;
            }
            // Hit the decompression-bomb guard: the window is now unreliable for
            // every later message only when context takeover is active.
            Some(_) => {
                self.mark_deflate_desynced();
                message.truncated = true;
            }
            // Genuine zlib error: nothing decoded, retained bytes stay compressed.
            None => {
                self.mark_deflate_desynced();
                message.truncated = true;
            }
        }
        message
    }
}

// ── Durable records + compact events ──────────────────────────────────────────

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// Allocate the next per-daemon WebSocket session id (`w1`, `w2`, …). Distinct
/// from flow ids (`f…`) so `net show` can route by prefix.
pub fn next_session_id() -> String {
    format!("w{}", SESSION_COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// The `ws_open` line: the upgrade handshake + negotiated session parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsSessionRecord {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    #[serde(default)]
    pub flow_sequence: u64,
    #[serde(default)]
    pub capture_session_id: String,
    pub ts: f64,
    pub scheme: String,
    pub host: String,
    pub path: String,
    pub status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subprotocol: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub permessage_deflate: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub req_headers: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resp_headers: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redaction_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redaction_policy_version: Option<u32>,
}

impl WsSessionRecord {
    pub fn url(&self) -> String {
        format!("{}://{}{}", self.scheme, self.host, self.path)
    }

    /// Redact the handshake headers (a WSS upgrade carries `Cookie`/
    /// `Authorization` just like an HTTP request), mirroring how completed HTTP
    /// flows are redacted before persistence. Forwarded traffic is untouched.
    pub fn redact_headers(&mut self, policy: &crate::redaction::Policy) {
        for (name, value) in self
            .req_headers
            .iter_mut()
            .chain(self.resp_headers.iter_mut())
        {
            *value = policy.redact_header_value(name, value);
        }
        self.redaction_policy = Some(policy.label().to_string());
        self.redaction_policy_version = Some(crate::redaction::POLICY_VERSION);
    }

    pub fn open_event(&self, serial: &Serial) -> Event {
        Event::WsOpen {
            ts: self.ts,
            id: self.id.clone(),
            flow_sequence: self.flow_sequence,
            capture_session_id: self.capture_session_id.clone(),
            scheme: self.scheme.clone(),
            host: self.host.clone(),
            path: self.path.clone(),
            url: self.url(),
            status: self.status,
            subprotocol: self.subprotocol.clone(),
            permessage_deflate: self.permessage_deflate,
            next_actions: crate::net::ws_session_next_actions(serial, &self.id),
        }
    }

    pub fn detail(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "ws_open",
            "id": self.id,
            "flow_sequence": self.flow_sequence,
            "capture_session_id": self.capture_session_id,
            "ts": self.ts,
            "scheme": self.scheme,
            "host": self.host,
            "path": self.path,
            "url": self.url(),
            "status": self.status,
            "subprotocol": self.subprotocol,
            "permessage_deflate": self.permessage_deflate,
            "req_headers": headers_to_map(&self.req_headers),
            "resp_headers": headers_to_map(&self.resp_headers),
            "redaction_policy": self.redaction_policy,
            "redaction_policy_version": self.redaction_policy_version,
        })
    }
}

/// The `ws_msg` line: one reassembled application message (or control frame).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsMessageRecord {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    pub session_id: String,
    #[serde(default)]
    pub flow_sequence: u64,
    #[serde(default)]
    pub capture_session_id: String,
    pub ts: f64,
    pub host: String,
    pub dir: String,
    pub seq: u64,
    pub opcode: String,
    pub payload_len: u64,
    pub wire_len: u64,
    pub retained_len: u64,
    pub frame_count: u32,
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub compressed: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub decompressed: bool,
    /// Textual payload (text messages + UTF-8 binary/control), capped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Base64 of the retained bytes for a non-UTF-8 binary payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_b64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_code: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_reason: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub body_redacted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redaction_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redaction_policy_version: Option<u32>,
}

impl WsMessageRecord {
    /// The compact streaming event (`net log`/`watch`). Bodies are dropped — the
    /// preview stays; full payloads are fetched via `net show <id> --body`.
    pub fn msg_event(&self, serial: &Serial) -> Event {
        Event::WsMsg {
            ts: self.ts,
            id: self.id.clone(),
            session_id: self.session_id.clone(),
            capture_session_id: self.capture_session_id.clone(),
            host: self.host.clone(),
            dir: self.dir.clone(),
            seq: self.seq,
            opcode: self.opcode.clone(),
            len: self.payload_len,
            wire_len: if self.compressed {
                Some(self.wire_len)
            } else {
                None
            },
            compressed: self.compressed,
            truncated: self.truncated,
            close_code: self.close_code,
            preview: self.preview.clone(),
            body_redacted: self.body_redacted,
            next_actions: crate::net::ws_message_next_actions(serial, &self.id),
        }
    }

    /// Full detail for `net show`. `body=false` drops the (possibly large)
    /// text/base64 payload but keeps every metadata field.
    pub fn detail(&self, body: bool) -> serde_json::Value {
        let mut value = serde_json::json!({
            "type": "ws_msg",
            "id": self.id,
            "session_id": self.session_id,
            "flow_sequence": self.flow_sequence,
            "capture_session_id": self.capture_session_id,
            "ts": self.ts,
            "host": self.host,
            "dir": self.dir,
            "seq": self.seq,
            "opcode": self.opcode,
            "payload_len": self.payload_len,
            "wire_len": self.wire_len,
            "retained_len": self.retained_len,
            "frame_count": self.frame_count,
            "truncated": self.truncated,
            "compressed": self.compressed,
            "decompressed": self.decompressed,
            "preview": self.preview,
            "close_code": self.close_code,
            "close_reason": self.close_reason,
            "body_redacted": self.body_redacted,
            "redaction_policy": self.redaction_policy,
            "redaction_policy_version": self.redaction_policy_version,
        });
        if body {
            value["text"] = serde_json::json!(self.text);
            value["data_b64"] = serde_json::json!(self.data_b64);
        }
        value
    }

    /// Redact the textual payload, close reason, and preview — used by
    /// `net export` when the proxy captured without `--redact` but a policy is
    /// active at export time (matching the HTTP flow re-redaction path).
    pub fn redact(&mut self, policy: &crate::redaction::Policy) {
        if let Some(text) = self.text.as_mut() {
            let (redacted, changed) = policy.redact_body(text);
            *text = redacted;
            self.body_redacted |= changed;
        }
        if let Some(reason) = self.close_reason.as_mut() {
            let (redacted, changed) = policy.redact_body(reason);
            *reason = redacted;
            self.body_redacted |= changed;
        }
        if let Some(preview) = self.preview.as_mut() {
            *preview = policy.redact_text(preview);
        }
        self.redaction_policy = Some(policy.label().to_string());
        self.redaction_policy_version = Some(crate::redaction::POLICY_VERSION);
    }

    /// The retained payload bytes, for `net show --body-file` (decodes base64 or
    /// returns the UTF-8 text bytes).
    pub fn raw_payload(&self) -> Option<Vec<u8>> {
        if let Some(b64) = &self.data_b64 {
            return b64_decode(b64);
        }
        self.text.as_ref().map(|t| t.as_bytes().to_vec())
    }
}

/// The `ws_close` line: session teardown + per-direction totals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsCloseRecord {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    pub session_id: String,
    #[serde(default)]
    pub flow_sequence: u64,
    #[serde(default)]
    pub capture_session_id: String,
    pub ts: f64,
    pub started_ts: f64,
    pub dur_ms: u64,
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_code: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_initiator: Option<String>,
    pub c2s_msgs: u64,
    pub s2c_msgs: u64,
    pub c2s_bytes: u64,
    pub s2c_bytes: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub dropped: u64,
}

impl WsCloseRecord {
    /// Redact the app-supplied close reason (for `net export` re-redaction).
    pub fn redact(&mut self, policy: &crate::redaction::Policy) {
        if let Some(reason) = self.close_reason.as_mut() {
            *reason = policy.redact_text(reason);
        }
    }

    pub fn close_event(&self, serial: &Serial) -> Event {
        Event::WsClose {
            ts: self.ts,
            id: self.id.clone(),
            session_id: self.session_id.clone(),
            capture_session_id: self.capture_session_id.clone(),
            host: self.host.clone(),
            dur_ms: self.dur_ms,
            close_code: self.close_code,
            close_reason: self.close_reason.clone(),
            close_initiator: self.close_initiator.clone(),
            c2s_msgs: self.c2s_msgs,
            s2c_msgs: self.s2c_msgs,
            c2s_bytes: self.c2s_bytes,
            s2c_bytes: self.s2c_bytes,
            dropped: self.dropped,
            next_actions: crate::net::ws_session_next_actions(serial, &self.session_id),
        }
    }

    pub fn detail(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "ws_close",
            "id": self.id,
            "session_id": self.session_id,
            "flow_sequence": self.flow_sequence,
            "capture_session_id": self.capture_session_id,
            "ts": self.ts,
            "started_ts": self.started_ts,
            "dur_ms": self.dur_ms,
            "host": self.host,
            "close_code": self.close_code,
            "close_reason": self.close_reason,
            "close_initiator": self.close_initiator,
            "c2s_msgs": self.c2s_msgs,
            "s2c_msgs": self.s2c_msgs,
            "c2s_bytes": self.c2s_bytes,
            "s2c_bytes": self.s2c_bytes,
            "dropped": self.dropped,
        })
    }
}

/// Apply the active capture policy before a terminal record is persisted or
/// broadcast. The close reason originates on the wire just like a text-frame
/// payload and may contain tokens, email addresses, or other private data.
fn redact_close_for_live_capture(
    record: &mut WsCloseRecord,
    policy: Option<&crate::redaction::Policy>,
) {
    if let Some(policy) = policy {
        record.redact(policy);
    }
}

/// Headers as a JSON object (repeated headers collect into an array), matching
/// the HTTP flow detail shape.
fn headers_to_map(headers: &[(String, String)]) -> serde_json::Value {
    use serde_json::Value;
    let mut map = serde_json::Map::new();
    for (key, value) in headers {
        let canonical = map
            .keys()
            .find(|existing| existing.eq_ignore_ascii_case(key))
            .cloned()
            .unwrap_or_else(|| key.clone());
        let val = Value::String(value.clone());
        match map.get_mut(&canonical) {
            None => {
                map.insert(canonical, val);
            }
            Some(Value::Array(items)) => items.push(val),
            Some(prev) => {
                let first = prev.take();
                *prev = Value::Array(vec![first, val]);
            }
        }
    }
    Value::Object(map)
}

/// Encode a reassembled message into `(text, data_b64, close_code, close_reason)`
/// for the durable record. Text messages and UTF-8-valid binary/control payloads
/// become `text`; non-UTF-8 binary becomes base64 for binary-safe access.
fn encode_payload(
    message: &Message,
) -> (Option<String>, Option<String>, Option<u16>, Option<String>) {
    if message.opcode == Opcode::Close {
        let (code, reason) = message.close_code_reason().unwrap_or((1005, String::new()));
        let text = (!reason.is_empty()).then(|| reason.clone());
        return (text, None, Some(code), Some(reason));
    }
    if message.payload.is_empty() {
        return (None, None, None, None);
    }
    // A compressed message we could not inflate holds raw DEFLATE bytes — never
    // present those as text (that was mojibake); keep a binary-safe artifact.
    if message.compressed && !message.decompressed {
        return (None, Some(b64_encode(&message.payload)), None, None);
    }
    if message.opcode == Opcode::Text {
        return (
            Some(String::from_utf8_lossy(&message.payload).into_owned()),
            None,
            None,
            None,
        );
    }
    // Binary / ping / pong: a UTF-8 payload round-trips as text (JSON-over-binary
    // is common); otherwise keep a base64 artifact.
    match std::str::from_utf8(&message.payload) {
        Ok(text) => (Some(text.to_string()), None, None, None),
        Err(_) => (None, Some(b64_encode(&message.payload)), None, None),
    }
}

/// A one-line preview: collapsed leading text, or a byte-count note for binary.
fn build_preview(
    text: Option<&str>,
    payload_len: u64,
    close_code: Option<u16>,
    close_reason: Option<&str>,
) -> Option<String> {
    if let Some(code) = close_code {
        let reason = close_reason.unwrap_or("");
        return Some(if reason.is_empty() {
            format!("close {code}")
        } else {
            format!("close {code} {}", collapse_ws(reason, PREVIEW_CAP))
        });
    }
    if let Some(text) = text {
        return Some(collapse_ws(text, PREVIEW_CAP));
    }
    if payload_len == 0 {
        None
    } else {
        Some(format!("<{payload_len} binary bytes>"))
    }
}

/// Collapse runs of whitespace to single spaces and cap length — a compact,
/// single-line preview that stays cheap for an agent to read.
fn collapse_ws(input: &str, cap: usize) -> String {
    let mut out = String::new();
    let mut in_ws = false;
    for ch in input.chars() {
        if ch.is_whitespace() {
            in_ws = true;
            continue;
        }
        if in_ws && !out.is_empty() {
            out.push(' ');
        }
        in_ws = false;
        out.push(ch);
        if out.chars().count() >= cap {
            out.push('…');
            break;
        }
    }
    out
}

/// Build a durable message record from a reassembled [Message], applying
/// redaction to textual payloads.
#[allow(clippy::too_many_arguments)]
fn build_message_record(
    meta: &WsSessionMeta,
    direction: Direction,
    seq: u64,
    message: &Message,
    redaction: Option<&crate::redaction::Policy>,
    ts: f64,
) -> WsMessageRecord {
    let (mut text, data_b64, close_code, mut close_reason) = encode_payload(message);
    let mut body_redacted = false;
    let mut redaction_policy = None;
    let mut redaction_policy_version = None;
    if let Some(policy) = redaction {
        if let Some(payload) = text.as_mut() {
            let (redacted, changed) = policy.redact_body(payload);
            *payload = redacted;
            body_redacted |= changed;
        }
        // The close reason is app-supplied text too, and feeds the preview; it
        // must be redacted alongside `text` (which for a close frame is a copy).
        if let Some(reason) = close_reason.as_mut() {
            let (redacted, changed) = policy.redact_body(reason);
            *reason = redacted;
            body_redacted |= changed;
        }
        redaction_policy = Some(policy.label().to_string());
        redaction_policy_version = Some(crate::redaction::POLICY_VERSION);
    }
    let preview = build_preview(
        text.as_deref(),
        message.payload_len,
        close_code,
        close_reason.as_deref(),
    );
    let retained_len = message.payload.len() as u64;
    WsMessageRecord {
        kind: "ws_msg".to_string(),
        id: format!("{}.{}", meta.id, seq),
        session_id: meta.id.clone(),
        flow_sequence: crate::net::flow::next_sequence(),
        capture_session_id: meta.capture_session_id.clone(),
        ts,
        host: meta.host.clone(),
        dir: direction.as_str().to_string(),
        seq,
        opcode: message.opcode.as_str().to_string(),
        payload_len: message.payload_len,
        wire_len: message.wire_len,
        retained_len,
        frame_count: message.frame_count,
        truncated: message.truncated,
        compressed: message.compressed,
        decompressed: message.decompressed,
        text,
        data_b64,
        preview,
        close_code,
        close_reason,
        body_redacted,
        redaction_policy,
        redaction_policy_version,
    }
}

// ── Async bidirectional tap driver ────────────────────────────────────────────

/// The identity a running tap needs to stamp on each message/close record. The
/// full session parameters (scheme, path, headers) already live in the persisted
/// `ws_open` record.
#[derive(Debug, Clone)]
pub struct WsSessionMeta {
    pub id: String,
    pub capture_session_id: String,
    pub host: String,
    pub started_ts: f64,
    pub deflate: DeflateParams,
}

/// The first close frame observed on a session (whichever side sent it).
struct CloseInfo {
    code: Option<u16>,
    reason: Option<String>,
    initiator: Direction,
}

#[derive(Default)]
struct WsStats {
    c2s_msgs: AtomicU64,
    s2c_msgs: AtomicU64,
    c2s_bytes: AtomicU64,
    s2c_bytes: AtomicU64,
    dropped: AtomicU64,
    close: Mutex<Option<CloseInfo>>,
}

impl WsStats {
    fn record(&self, direction: Direction, message: &Message) {
        match direction {
            Direction::ClientToServer => {
                self.c2s_msgs.fetch_add(1, Ordering::Relaxed);
                self.c2s_bytes
                    .fetch_add(message.payload_len, Ordering::Relaxed);
            }
            Direction::ServerToClient => {
                self.s2c_msgs.fetch_add(1, Ordering::Relaxed);
                self.s2c_bytes
                    .fetch_add(message.payload_len, Ordering::Relaxed);
            }
        }
        if message.opcode == Opcode::Close {
            let mut close = self.close.lock().unwrap();
            if close.is_none() {
                let (code, reason) = message
                    .close_code_reason()
                    .map(|(code, reason)| (Some(code), Some(reason)))
                    .unwrap_or((None, None));
                *close = Some(CloseInfo {
                    code,
                    reason,
                    initiator: direction,
                });
            }
        }
    }
}

/// Splice the two upgraded WebSocket streams, forwarding every byte verbatim
/// while decoding a copy of each direction into durable records + live events.
/// Never alters or delays forwarded traffic beyond one buffered read.
pub async fn tap<Device, Upstream>(
    ctx: Arc<ProxyContext>,
    meta: WsSessionMeta,
    device: Device,
    upstream: Upstream,
) where
    Device: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    Upstream: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let meta = Arc::new(meta);
    let stats = Arc::new(WsStats::default());
    let counter = Arc::new(AtomicU64::new(1));
    let (device_reader, device_writer) = tokio::io::split(device);
    let (upstream_reader, upstream_writer) = tokio::io::split(upstream);
    let (tx, rx) = mpsc::channel::<WsMessageRecord>(256);

    let drain = {
        let ctx_drain = ctx.clone();
        let meta = meta.clone();
        let stats = stats.clone();
        // Clone the tracker so `spawn` doesn't borrow the `ctx` the task moves.
        ctx.tasks
            .clone()
            .spawn(async move { drain_loop(ctx_drain, meta, stats, rx).await })
    };

    let client = pump(
        ctx.clone(),
        meta.clone(),
        Direction::ClientToServer,
        device_reader,
        upstream_writer,
        MessageAssembler::new(Direction::ClientToServer, meta.deflate),
        stats.clone(),
        counter.clone(),
        tx.clone(),
    );
    let server = pump(
        ctx.clone(),
        meta.clone(),
        Direction::ServerToClient,
        upstream_reader,
        device_writer,
        MessageAssembler::new(Direction::ServerToClient, meta.deflate),
        stats.clone(),
        counter.clone(),
        tx,
    );

    tokio::select! {
        _ = async { tokio::join!(client, server); } => {}
        _ = ctx.shutdown.cancelled() => {}
    }
    // Both pumps have returned (or been cancelled), dropping every sender; the
    // drain loop now finalizes the session with a `ws_close` record.
    let _ = drain.await;
}

/// Forward one direction verbatim while decoding a copy into message records.
/// Bytes are written to `dst` *before* any parsing so decoding can never stall
/// or corrupt the tunnel.
#[allow(clippy::too_many_arguments)]
async fn pump<Reader, Writer>(
    ctx: Arc<ProxyContext>,
    meta: Arc<WsSessionMeta>,
    direction: Direction,
    mut reader: Reader,
    mut writer: Writer,
    mut assembler: MessageAssembler,
    stats: Arc<WsStats>,
    counter: Arc<AtomicU64>,
    tx: mpsc::Sender<WsMessageRecord>,
) where
    Reader: AsyncRead + Unpin,
    Writer: AsyncWrite + Unpin,
{
    let mut decoder = FrameDecoder::new();
    let mut buf = vec![0u8; 16 * 1024];
    let mut desync_logged = false;
    loop {
        let read = match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => break,
        };
        if writer.write_all(&buf[..read]).await.is_err() {
            break;
        }
        for frame in decoder.push(&buf[..read]) {
            if let Some(message) = assembler.accept(frame) {
                stats.record(direction, &message);
                let seq = counter.fetch_add(1, Ordering::Relaxed);
                let record = build_message_record(
                    &meta,
                    direction,
                    seq,
                    &message,
                    ctx.shared.redaction.as_ref(),
                    events_now(),
                );
                if tx.try_send(record).is_err() {
                    stats.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        // If framing desyncs, the tunnel keeps flowing (bytes are already
        // forwarded) but decoding stops; note it once so a debugging agent can
        // see why frames went quiet.
        if decoder.desynced() && !desync_logged {
            desync_logged = true;
            tracing::debug!(
                session = %meta.id,
                dir = direction.as_str(),
                "WebSocket frame decode desynced; forwarding continues untapped"
            );
        }
    }
    let _ = writer.shutdown().await;
}

/// Persist message records + broadcast their events; on channel close (both
/// pumps done) write the terminal `ws_close` record.
async fn drain_loop(
    ctx: Arc<ProxyContext>,
    meta: Arc<WsSessionMeta>,
    stats: Arc<WsStats>,
    mut rx: mpsc::Receiver<WsMessageRecord>,
) {
    while let Some(record) = rx.recv().await {
        let event = record.msg_event(&ctx.serial);
        if let Err(error) = store::append_ws_message(&ctx.serial, &record) {
            ctx.shared.record_persistence_error("ws_msg", &error);
        }
        let _ = ctx.shared.events.send(Arc::new(event));
    }

    let ended = events_now();
    let (close_code, close_reason, close_initiator) = match stats.close.lock().unwrap().as_ref() {
        Some(close) => (
            close.code,
            close.reason.clone(),
            Some(close.initiator.as_str().to_string()),
        ),
        None => (None, None, None),
    };
    let mut record = WsCloseRecord {
        kind: "ws_close".to_string(),
        id: meta.id.clone(),
        session_id: meta.id.clone(),
        flow_sequence: crate::net::flow::next_sequence(),
        capture_session_id: meta.capture_session_id.clone(),
        ts: ended,
        started_ts: meta.started_ts,
        dur_ms: ((ended - meta.started_ts).max(0.0) * 1000.0) as u64,
        host: meta.host.clone(),
        close_code,
        close_reason,
        close_initiator,
        c2s_msgs: stats.c2s_msgs.load(Ordering::Relaxed),
        s2c_msgs: stats.s2c_msgs.load(Ordering::Relaxed),
        c2s_bytes: stats.c2s_bytes.load(Ordering::Relaxed),
        s2c_bytes: stats.s2c_bytes.load(Ordering::Relaxed),
        dropped: stats.dropped.load(Ordering::Relaxed),
    };
    redact_close_for_live_capture(&mut record, ctx.shared.redaction.as_ref());
    let event = record.close_event(&ctx.serial);
    if let Err(error) = store::append_ws_close(&ctx.serial, &record) {
        ctx.shared.record_persistence_error("ws_close", &error);
    }
    let _ = ctx.shared.events.send(Arc::new(event));
}

fn events_now() -> f64 {
    crate::events::now_ts()
}

// ── base64 (standard alphabet) — no dependency, keeps the single binary ──────

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_ALPHABET[(triple >> 18) as usize & 0x3F] as char);
        out.push(B64_ALPHABET[(triple >> 12) as usize & 0x3F] as char);
        out.push(if chunk.len() > 1 {
            B64_ALPHABET[(triple >> 6) as usize & 0x3F] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64_ALPHABET[triple as usize & 0x3F] as char
        } else {
            '='
        });
    }
    out
}

pub fn b64_decode(input: &str) -> Option<Vec<u8>> {
    fn val(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some((byte - b'A') as u32),
            b'a'..=b'z' => Some((byte - b'a' + 26) as u32),
            b'0'..=b'9' => Some((byte - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        if chunk.len() < 2 {
            return None;
        }
        let mut acc = 0u32;
        let mut count = 0;
        for &byte in chunk {
            if byte == b'=' {
                break;
            }
            acc = (acc << 6) | val(byte)?;
            count += 1;
        }
        acc <<= 6 * (4 - count);
        if count >= 2 {
            out.push((acc >> 16) as u8);
        }
        if count >= 3 {
            out.push((acc >> 8) as u8);
        }
        if count >= 4 {
            out.push(acc as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a client (masked) or server (unmasked) frame for decoder tests.
    fn frame(fin: bool, rsv1: bool, opcode: u8, payload: &[u8], mask: Option<[u8; 4]>) -> Vec<u8> {
        let mut out = Vec::new();
        let mut b0 = opcode & 0x0F;
        if fin {
            b0 |= 0x80;
        }
        if rsv1 {
            b0 |= 0x40;
        }
        out.push(b0);
        let masked_bit = if mask.is_some() { 0x80 } else { 0 };
        let len = payload.len();
        if len < 126 {
            out.push(masked_bit | len as u8);
        } else if len < 65536 {
            out.push(masked_bit | 126);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            out.push(masked_bit | 127);
            out.extend_from_slice(&(len as u64).to_be_bytes());
        }
        if let Some(key) = mask {
            out.extend_from_slice(&key);
            for (i, &byte) in payload.iter().enumerate() {
                out.push(byte ^ key[i & 3]);
            }
        } else {
            out.extend_from_slice(payload);
        }
        out
    }

    #[test]
    fn decodes_unmasked_text_frame() {
        let mut decoder = FrameDecoder::new();
        let frames = decoder.push(&frame(true, false, 0x1, b"hello", None));
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].opcode, Opcode::Text);
        assert!(frames[0].fin);
        assert_eq!(frames[0].payload, b"hello");
        assert_eq!(frames[0].payload_len, 5);
        assert!(!frames[0].truncated);
    }

    #[test]
    fn unmasks_client_frame() {
        let mut decoder = FrameDecoder::new();
        let frames = decoder.push(&frame(
            true,
            false,
            0x1,
            b"secret payload",
            Some([0x11, 0x22, 0x33, 0x44]),
        ));
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].payload, b"secret payload");
    }

    #[test]
    fn reassembles_across_byte_boundaries() {
        // Split one frame across arbitrary chunk boundaries.
        let wire = frame(true, false, 0x1, b"fragmented over the wire", None);
        let mut decoder = FrameDecoder::new();
        let mut frames = Vec::new();
        for byte in &wire {
            frames.extend(decoder.push(&[*byte]));
        }
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].payload, b"fragmented over the wire");
    }

    #[test]
    fn reassembles_fragmented_message() {
        let mut assembler =
            MessageAssembler::new(Direction::ServerToClient, DeflateParams::default());
        let mut decoder = FrameDecoder::new();
        // Text (fin=0) + continuation (fin=0) + continuation (fin=1).
        let mut wire = frame(false, false, 0x1, b"one ", None);
        wire.extend(frame(false, false, 0x0, b"two ", None));
        wire.extend(frame(true, false, 0x0, b"three", None));
        let mut message = None;
        for f in decoder.push(&wire) {
            if let Some(m) = assembler.accept(f) {
                message = Some(m);
            }
        }
        let message = message.expect("message completes on FIN continuation");
        assert_eq!(message.payload, b"one two three");
        assert_eq!(message.frame_count, 3);
        assert_eq!(message.opcode, Opcode::Text);
    }

    #[test]
    fn control_frame_interleaved_with_fragments() {
        let mut assembler =
            MessageAssembler::new(Direction::ServerToClient, DeflateParams::default());
        let mut decoder = FrameDecoder::new();
        let mut wire = frame(false, false, 0x2, b"AB", None); // binary start
        wire.extend(frame(true, false, 0x9, b"ping", None)); // ping interleaved
        wire.extend(frame(true, false, 0x0, b"CD", None)); // finish binary
        let mut messages = Vec::new();
        for f in decoder.push(&wire) {
            if let Some(m) = assembler.accept(f) {
                messages.push(m);
            }
        }
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].opcode, Opcode::Ping);
        assert_eq!(messages[0].payload, b"ping");
        assert_eq!(messages[1].opcode, Opcode::Binary);
        assert_eq!(messages[1].payload, b"ABCD");
    }

    #[test]
    fn close_frame_carries_code_and_reason() {
        let mut assembler =
            MessageAssembler::new(Direction::ClientToServer, DeflateParams::default());
        let mut decoder = FrameDecoder::new();
        let mut payload = 1000u16.to_be_bytes().to_vec();
        payload.extend_from_slice(b"bye");
        let mut message = None;
        for f in decoder.push(&frame(true, false, 0x8, &payload, None)) {
            message = assembler.accept(f);
        }
        let message = message.unwrap();
        assert_eq!(message.opcode, Opcode::Close);
        assert_eq!(message.close_code_reason(), Some((1000, "bye".to_string())));
    }

    #[test]
    fn oversized_frame_is_truncated_not_buffered() {
        let mut decoder = FrameDecoder::new();
        decoder.retain_cap = 8;
        let payload = vec![b'x'; 5000];
        let frames = decoder.push(&frame(true, false, 0x2, &payload, None));
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].payload.len(), 8);
        assert_eq!(frames[0].payload_len, 5000);
        assert!(frames[0].truncated);
    }

    #[test]
    fn malformed_header_desyncs_without_panicking() {
        let mut decoder = FrameDecoder::new();
        // Control frame claiming >125 bytes is illegal.
        let bad = vec![0x89, 0x7E, 0x01, 0x00]; // ping, len=256
        let frames = decoder.push(&bad);
        assert!(frames.is_empty());
        assert!(decoder.desynced());
        // Further bytes are ignored (tunnel keeps flowing elsewhere).
        assert!(decoder.push(b"anything").is_empty());
    }

    #[test]
    fn permessage_deflate_roundtrip() {
        // Compress "the quick brown fox" as a raw deflate block (no zlib header),
        // strip the trailing empty block per RFC 7692, and decode.
        use flate2::Compress;
        let plain = b"the quick brown fox jumps over the lazy dog";
        let mut compressor = Compress::new(flate2::Compression::default(), false);
        let mut compressed = vec![0u8; 256];
        compressor
            .compress(plain, &mut compressed, flate2::FlushCompress::Sync)
            .unwrap();
        let produced = compressor.total_out() as usize;
        compressed.truncate(produced);
        // Strip the 00 00 FF FF sync trailer the framing layer would add back.
        if compressed.ends_with(&[0x00, 0x00, 0xFF, 0xFF]) {
            compressed.truncate(compressed.len() - 4);
        }

        let params = DeflateParams {
            enabled: true,
            ..Default::default()
        };
        let mut assembler = MessageAssembler::new(Direction::ServerToClient, params);
        let mut decoder = FrameDecoder::new();
        let mut message = None;
        for f in decoder.push(&frame(true, true, 0x1, &compressed, None)) {
            message = assembler.accept(f);
        }
        let message = message.unwrap();
        assert!(message.compressed);
        assert!(message.decompressed);
        assert_eq!(message.payload, plain);
    }

    #[test]
    fn parse_deflate_params_reads_context_takeover() {
        let params = parse_deflate_params(Some(
            "permessage-deflate; client_no_context_takeover; server_max_window_bits=15",
        ));
        assert!(params.enabled);
        assert!(params.client_no_context_takeover);
        assert!(!params.server_no_context_takeover);
        assert!(!parse_deflate_params(None).enabled);
        assert!(!parse_deflate_params(Some("x-webkit-deflate-frame")).enabled);
    }

    #[test]
    fn base64_roundtrips_including_padding() {
        for sample in [
            &b""[..],
            &b"f"[..],
            &b"fo"[..],
            &b"foo"[..],
            &b"foob"[..],
            &b"fooba"[..],
            &b"foobar"[..],
            &[0u8, 255, 16, 128, 3][..],
        ] {
            let encoded = b64_encode(sample);
            assert_eq!(b64_decode(&encoded).as_deref(), Some(sample), "{encoded}");
        }
    }

    /// Drive raw wire bytes through the same decode → reassemble → record
    /// pipeline the live tap uses, returning the durable records.
    fn pipeline(
        direction: Direction,
        deflate: DeflateParams,
        redaction: Option<&crate::redaction::Policy>,
        wire: &[u8],
    ) -> Vec<WsMessageRecord> {
        let meta = WsSessionMeta {
            id: "w1".to_string(),
            capture_session_id: "n-test".to_string(),
            host: "ws.example.com".to_string(),
            started_ts: 100.0,
            deflate,
        };
        let mut decoder = FrameDecoder::new();
        let mut assembler = MessageAssembler::new(direction, deflate);
        let mut records = Vec::new();
        for frame in decoder.push(wire) {
            if let Some(message) = assembler.accept(frame) {
                let seq = records.len() as u64 + 1;
                records.push(build_message_record(
                    &meta, direction, seq, &message, redaction, 200.0,
                ));
            }
        }
        records
    }

    #[test]
    fn record_pipeline_covers_every_message_shape() {
        // A server→client exchange: text (JSON), fragmented text, binary, ping.
        let mut wire = frame(true, false, 0x1, br#"{"event":"tick"}"#, None);
        wire.extend(frame(false, false, 0x1, b"frag ", None));
        wire.extend(frame(true, false, 0x0, b"mented", None));
        wire.extend(frame(true, false, 0x2, &[0x00, 0x01, 0xFF, 0x10], None));
        wire.extend(frame(true, false, 0x9, b"hb", None));

        let records = pipeline(
            Direction::ServerToClient,
            DeflateParams::default(),
            None,
            &wire,
        );
        assert_eq!(records.len(), 4);

        let text = &records[0];
        assert_eq!(text.opcode, "text");
        assert_eq!(text.dir, "s2c");
        assert_eq!(text.id, "w1.1");
        assert_eq!(text.session_id, "w1");
        assert_eq!(text.text.as_deref(), Some(r#"{"event":"tick"}"#));
        assert!(text.data_b64.is_none());

        let fragmented = &records[1];
        assert_eq!(fragmented.opcode, "text");
        assert_eq!(fragmented.text.as_deref(), Some("frag mented"));
        assert_eq!(fragmented.frame_count, 2);

        let binary = &records[2];
        assert_eq!(binary.opcode, "binary");
        assert!(binary.text.is_none());
        assert_eq!(
            b64_decode(binary.data_b64.as_deref().unwrap()).unwrap(),
            vec![0x00, 0x01, 0xFF, 0x10]
        );

        let ping = &records[3];
        assert_eq!(ping.opcode, "ping");
        assert_eq!(ping.text.as_deref(), Some("hb"));
    }

    #[test]
    fn record_pipeline_marks_decompressed_messages() {
        use flate2::Compress;
        let plain = br#"{"large":"repeated repeated repeated repeated payload"}"#;
        let mut compressor = Compress::new(flate2::Compression::default(), false);
        let mut compressed = vec![0u8; 512];
        compressor
            .compress(plain, &mut compressed, flate2::FlushCompress::Sync)
            .unwrap();
        compressed.truncate(compressor.total_out() as usize);
        if compressed.ends_with(&[0x00, 0x00, 0xFF, 0xFF]) {
            compressed.truncate(compressed.len() - 4);
        }
        let params = DeflateParams {
            enabled: true,
            ..Default::default()
        };
        let records = pipeline(
            Direction::ServerToClient,
            params,
            None,
            &frame(true, true, 0x1, &compressed, None),
        );
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert!(record.compressed);
        assert!(record.decompressed);
        assert_eq!(
            record.text.as_deref(),
            Some(std::str::from_utf8(plain).unwrap())
        );
        // wire_len is the compressed size; payload_len the decompressed size.
        assert!(record.wire_len < record.payload_len);
    }

    /// Compress `plain` as one permessage-deflate message on a shared context
    /// (context takeover), stripping the sync trailer the framing layer re-adds.
    fn deflate_message(compressor: &mut flate2::Compress, plain: &[u8]) -> Vec<u8> {
        let before = compressor.total_out();
        let mut out = vec![0u8; plain.len() + 512];
        compressor
            .compress(plain, &mut out, flate2::FlushCompress::Sync)
            .unwrap();
        let produced = (compressor.total_out() - before) as usize;
        out.truncate(produced);
        if out.ends_with(&[0x00, 0x00, 0xFF, 0xFF]) {
            out.truncate(out.len() - 4);
        }
        out
    }

    #[test]
    fn context_takeover_survives_an_output_truncated_message() {
        // Regression: a compressed message whose OUTPUT exceeds RETAIN_CAP must
        // still fully advance the shared inflate window, so the next message
        // (which back-references it) decompresses correctly.
        use flate2::Compress;
        let mut compressor = Compress::new(flate2::Compression::default(), false);
        let big = "lorem ipsum ".repeat(8 * 1024); // ~96 KiB > RETAIN_CAP
        let small = "lorem ipsum dolor sit amet"; // reuses the shared window
        let frame_big = frame(
            true,
            true,
            0x1,
            &deflate_message(&mut compressor, big.as_bytes()),
            None,
        );
        let frame_small = frame(
            true,
            true,
            0x1,
            &deflate_message(&mut compressor, small.as_bytes()),
            None,
        );

        let params = DeflateParams {
            enabled: true,
            ..Default::default()
        };
        let records = {
            let mut wire = frame_big;
            wire.extend(frame_small);
            pipeline(Direction::ServerToClient, params, None, &wire)
        };
        assert_eq!(records.len(), 2);

        let first = &records[0];
        assert!(first.decompressed, "big message must still decode");
        assert_eq!(
            first.payload_len,
            big.len() as u64,
            "full decompressed length reported"
        );
        assert!(first.truncated, "output beyond RETAIN_CAP is truncated");
        assert_eq!(first.retained_len, RETAIN_CAP as u64);

        let second = &records[1];
        assert!(
            second.decompressed,
            "window stayed in sync after truncation"
        );
        assert!(!second.truncated);
        assert_eq!(
            second.text.as_deref(),
            Some(small),
            "back-references resolve"
        );
    }

    #[test]
    fn final_deflate_block_desyncs_later_messages_instead_of_faking_empty() {
        // A peer that ends a message with BFINAL=1 (RFC 7692 violation) leaves the
        // context-takeover stream dead. The BFINAL message itself decodes, but
        // later ones must be reported un-decoded (binary), never as clean-empty.
        use flate2::{Compress, Compression};
        let mut finished = Compress::new(Compression::default(), false);
        let mut buf = vec![0u8; 512];
        finished
            .compress(b"hello", &mut buf, flate2::FlushCompress::Finish)
            .unwrap();
        buf.truncate(finished.total_out() as usize); // ends in a final block

        let mut sync = Compress::new(Compression::default(), false);
        let later = deflate_message(&mut sync, b"world");

        let params = DeflateParams {
            enabled: true,
            ..Default::default()
        };
        let mut wire = frame(true, true, 0x1, &buf, None);
        wire.extend(frame(true, true, 0x1, &later, None));
        let records = pipeline(Direction::ServerToClient, params, None, &wire);
        assert_eq!(records.len(), 2);

        assert!(records[0].decompressed);
        assert_eq!(records[0].text.as_deref(), Some("hello"));

        let second = &records[1];
        assert!(!second.decompressed, "must not report a fake clean decode");
        assert!(second.compressed);
        assert!(second.truncated, "flagged undecodable");
        assert!(
            second.data_b64.is_some(),
            "raw bytes preserved, not shown as empty"
        );
        assert!(second.text.is_none());
    }

    #[test]
    fn no_context_takeover_recovers_after_a_compressed_message_error() {
        use flate2::{Compress, Compression};

        let cases = [
            (
                Direction::ServerToClient,
                DeflateParams {
                    enabled: true,
                    server_no_context_takeover: true,
                    ..Default::default()
                },
                None,
            ),
            (
                Direction::ClientToServer,
                DeflateParams {
                    enabled: true,
                    client_no_context_takeover: true,
                    ..Default::default()
                },
                Some([0x11, 0x22, 0x33, 0x44]),
            ),
        ];

        for (direction, params, mask) in cases {
            // BTYPE=3 is reserved, so this compressed message deterministically
            // fails raw-DEFLATE decoding. With no context takeover, the failure
            // must not poison the next independently compressed message.
            let mut wire = frame(true, true, 0x1, &[0x07], mask);
            let mut compressor = Compress::new(Compression::default(), false);
            let recovered = deflate_message(&mut compressor, b"after error");
            wire.extend(frame(true, true, 0x1, &recovered, mask));

            let records = pipeline(direction, params, None, &wire);
            assert_eq!(records.len(), 2, "{direction:?}");
            assert!(!records[0].decompressed, "{direction:?}");
            assert!(records[0].truncated, "{direction:?}");
            assert!(records[1].decompressed, "{direction:?}");
            assert!(!records[1].truncated, "{direction:?}");
            assert_eq!(
                records[1].text.as_deref(),
                Some("after error"),
                "{direction:?}"
            );
        }
    }

    #[test]
    fn oversized_compressed_message_is_binary_not_mojibake() {
        // A compressed message we can't buffer whole is stored as a binary
        // artifact, never as lossy-UTF-8 garbage.
        let params = DeflateParams {
            enabled: true,
            ..Default::default()
        };
        // rsv1 text frame whose compressed payload exceeds COMPRESSED_ACCUM_CAP.
        let huge = vec![0x42u8; COMPRESSED_ACCUM_CAP + 4096];
        let records = pipeline(
            Direction::ServerToClient,
            params,
            None,
            &frame(true, true, 0x1, &huge, None),
        );
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert!(record.compressed);
        assert!(!record.decompressed);
        assert!(record.truncated);
        assert!(
            record.text.is_none(),
            "raw DEFLATE must not be shown as text"
        );
        assert!(record.data_b64.is_some(), "kept as a binary-safe artifact");
    }

    #[test]
    fn session_record_redacts_handshake_headers() {
        let mut session = WsSessionRecord {
            kind: "ws_open".to_string(),
            id: "w1".to_string(),
            flow_sequence: 1,
            capture_session_id: "n-test".to_string(),
            ts: 1.0,
            scheme: "wss".to_string(),
            host: "chat.example.com".to_string(),
            path: "/socket".to_string(),
            status: 101,
            subprotocol: None,
            permessage_deflate: false,
            req_headers: vec![
                (
                    "Cookie".to_string(),
                    "session=super-secret-token".to_string(),
                ),
                ("Authorization".to_string(), "Bearer sk-abc123".to_string()),
            ],
            resp_headers: vec![("Set-Cookie".to_string(), "sid=leak-me".to_string())],
            redaction_policy: None,
            redaction_policy_version: None,
        };
        session.redact_headers(&crate::redaction::Policy::builtin());
        let joined: String = session
            .req_headers
            .iter()
            .chain(&session.resp_headers)
            .map(|(_, value)| value.clone())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            !joined.contains("super-secret-token"),
            "cookie leaked: {joined}"
        );
        assert!(!joined.contains("sk-abc123"), "bearer leaked: {joined}");
        assert!(!joined.contains("leak-me"), "set-cookie leaked: {joined}");
        assert!(session.redaction_policy.is_some());
    }

    #[test]
    fn record_pipeline_redacts_textual_payloads() {
        let policy = crate::redaction::Policy::builtin();
        let records = pipeline(
            Direction::ClientToServer,
            DeflateParams::default(),
            Some(&policy),
            &frame(
                true,
                false,
                0x1,
                br#"{"authorization":"Bearer sk-secret-value"}"#,
                Some([0x01, 0x02, 0x03, 0x04]),
            ),
        );
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert!(record.body_redacted);
        assert!(record.redaction_policy.is_some());
        assert!(
            !record.text.as_deref().unwrap().contains("sk-secret-value"),
            "the bearer token must be redacted: {:?}",
            record.text
        );
    }

    #[test]
    fn close_reason_and_preview_are_redacted() {
        // Finding #4: a close frame's reason (and the preview built from it) must
        // be redacted, not just the `text` copy.
        let mut payload = 1008u16.to_be_bytes().to_vec();
        payload.extend_from_slice(b"blocked user alice@example.com from 10.2.3.4");
        let records = pipeline(
            Direction::ServerToClient,
            DeflateParams::default(),
            Some(&crate::redaction::Policy::builtin()),
            &frame(true, false, 0x8, &payload, None),
        );
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.opcode, "close");
        assert_eq!(record.close_code, Some(1008));
        let reason = record.close_reason.as_deref().unwrap();
        assert!(
            !reason.contains("alice@example.com"),
            "reason leaked: {reason}"
        );
        assert!(!reason.contains("10.2.3.4"), "reason leaked ip: {reason}");
        let preview = record.preview.as_deref().unwrap();
        assert!(
            !preview.contains("alice@example.com"),
            "preview leaked: {preview}"
        );
        assert!(record.body_redacted);
    }

    #[test]
    fn live_close_record_and_event_are_redacted_before_delivery() {
        let mut record = WsCloseRecord {
            kind: "ws_close".to_string(),
            id: "w1".to_string(),
            session_id: "w1".to_string(),
            flow_sequence: 3,
            capture_session_id: "n-test".to_string(),
            ts: 2.0,
            started_ts: 1.0,
            dur_ms: 1000,
            host: "chat.example.com".to_string(),
            close_code: Some(1008),
            close_reason: Some(
                "Bearer sk-live-secret for alice@example.com from 10.2.3.4".to_string(),
            ),
            close_initiator: Some("s2c".to_string()),
            c2s_msgs: 2,
            s2c_msgs: 2,
            c2s_bytes: 20,
            s2c_bytes: 20,
            dropped: 0,
        };
        redact_close_for_live_capture(&mut record, Some(&crate::redaction::Policy::builtin()));

        let persisted = serde_json::to_string(&record).unwrap();
        let broadcast =
            serde_json::to_string(&record.close_event(&Serial::new("emulator-5554"))).unwrap();
        for secret in ["sk-live-secret", "alice@example.com", "10.2.3.4"] {
            assert!(
                !persisted.contains(secret),
                "persisted close leaked {secret}"
            );
            assert!(!broadcast.contains(secret), "close event leaked {secret}");
        }
        assert!(
            record
                .close_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("<redacted:"))
        );
    }

    #[test]
    fn message_re_redaction_covers_text_reason_and_preview() {
        // Finding #5: `net export` re-redaction (typed) redacts a record captured
        // without --redact.
        let mut record = WsMessageRecord {
            kind: "ws_msg".to_string(),
            id: "w1.1".to_string(),
            session_id: "w1".to_string(),
            flow_sequence: 1,
            capture_session_id: "n".to_string(),
            ts: 1.0,
            host: "h".to_string(),
            dir: "c2s".to_string(),
            seq: 1,
            opcode: "text".to_string(),
            payload_len: 10,
            wire_len: 10,
            retained_len: 10,
            frame_count: 1,
            truncated: false,
            compressed: false,
            decompressed: false,
            text: Some("token=Bearer sk-live-abc from bob@example.com".to_string()),
            data_b64: None,
            preview: Some("token=Bearer sk-live-abc from bob@example.com".to_string()),
            close_code: None,
            close_reason: None,
            body_redacted: false,
            redaction_policy: None,
            redaction_policy_version: None,
        };
        record.redact(&crate::redaction::Policy::builtin());
        assert!(record.body_redacted);
        assert!(record.redaction_policy.is_some());
        assert!(!record.text.as_deref().unwrap().contains("bob@example.com"));
        assert!(
            !record
                .preview
                .as_deref()
                .unwrap()
                .contains("bob@example.com")
        );
    }

    #[test]
    fn encode_payload_prefers_text_for_utf8() {
        let text = Message {
            opcode: Opcode::Text,
            payload: b"{\"a\":1}".to_vec(),
            payload_len: 7,
            wire_len: 7,
            truncated: false,
            frame_count: 1,
            compressed: false,
            decompressed: false,
        };
        let (t, b, _, _) = encode_payload(&text);
        assert_eq!(t.as_deref(), Some("{\"a\":1}"));
        assert!(b.is_none());

        let binary = Message {
            opcode: Opcode::Binary,
            payload: vec![0x00, 0x01, 0xFF],
            payload_len: 3,
            wire_len: 3,
            truncated: false,
            frame_count: 1,
            compressed: false,
            decompressed: false,
        };
        let (t, b, _, _) = encode_payload(&binary);
        assert!(t.is_none());
        assert_eq!(b64_decode(&b.unwrap()).unwrap(), vec![0x00, 0x01, 0xFF]);
    }
}
