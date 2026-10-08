//! A fake JDWP VM on loopback TCP for device-free tests of the standalone
//! debugger. Shared by `tests/jdwp_backend.rs` (driving the real binary) and
//! the in-crate session tests (`#[path]`-included), so it deliberately has its
//! own minimal codec instead of reusing the one under test.
//!
//! The synthetic world (all id sizes 8):
//!
//! | id  | what                                                     |
//! |-----|----------------------------------------------------------|
//! | 100 | `io.example.app.MainActivity` (MainActivity.kt)          |
//! |     |   1000 `onCreate` lines 20–23, 1001 `onNewIntent` 30–32  |
//! | 101 | `io.example.app.MainActivity$onCreate$1` (lambda, 25)    |
//! | 102 | `io.example.app.R$id` (no SourceFile: error 101)          |
//! | 103 | `java.lang.String`, 104 `java.lang.Object`               |
//! | 105 | `io.example.app.Late` (Late.kt, line 7) — not loaded     |
//! |     |   until [`FakeVm::load_late_class`]                      |
//! | 300 | thread `main`, 301 thread `worker`                       |
//! | 500 | MainActivity instance: counter=7, label→501, numbers→502 |
//! | 501 | String "hello", 502 int[] {1, 2, 3}                      |
//!
//! Frames of `main` while suspended: 900 = onNewIntent@5 (locals: this
//! slot 0, `tag` slot 1 = 501, `count` slot 2 = 42, `$i$f$inline` slot 3),
//! 901 = onCreate@4.
#![allow(dead_code)]

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

pub const MAIN_THREAD: u64 = 300;
pub const ACTIVITY_CLASS: u64 = 100;
pub const ON_NEW_INTENT: u64 = 1001;
pub const ACTIVITY_OBJECT: u64 = 500;

#[derive(Clone, Debug)]
pub struct Request {
    pub id: i32,
    pub kind: u8,
    pub policy: u8,
    /// `(modifier kind, raw payload summary)`
    pub location: Option<(u64, u64, u64)>,
    pub source_name: Option<String>,
    pub class_match: Option<String>,
    pub step_thread: Option<u64>,
    /// ExceptionOnly `(caught, uncaught)` flags.
    pub exception_flags: Option<(bool, bool)>,
    pub modifier_kinds: Vec<u8>,
}

#[derive(Default)]
pub struct State {
    pub requests: Vec<Request>,
    pub cleared: Vec<i32>,
    pub next_request: i32,
    pub suspend_count: i32,
    pub resumes: u32,
    pub thread_resumes: u32,
    pub disposed: bool,
    pub pinned: BTreeSet<u64>,
    pub commands: Vec<(u8, u8)>,
    pub late_loaded: bool,
    /// Commands that never get a reply (timeout tests).
    pub silent: Vec<(u8, u8)>,
    pub reject_handshake: bool,
    /// Accept the handshake bytes and never answer (a wedged endpoint).
    pub silent_handshake: bool,
    /// Load `Late` while answering AllClassesWithGeneric: its ClassPrepare
    /// goes out before the reply, which already lists it (the bind race).
    pub prepare_late_during_scan: bool,
    pub connections: u32,
    pub step_line_index: u64,
}

struct Shared {
    state: Mutex<State>,
    writer: Mutex<Option<TcpStream>>,
    changed: Condvar,
}

/// Handle to a running fake VM.
#[derive(Clone)]
pub struct FakeVm {
    pub addr: SocketAddr,
    shared: Arc<Shared>,
}

impl FakeVm {
    pub fn start() -> FakeVm {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                next_request: 1,
                step_line_index: 10,
                ..State::default()
            }),
            writer: Mutex::new(None),
            changed: Condvar::new(),
        });
        let accept_shared = shared.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let shared = accept_shared.clone();
                std::thread::spawn(move || serve(stream, shared));
            }
        });
        FakeVm { addr, shared }
    }

    pub fn address(&self) -> String {
        self.addr.to_string()
    }

    pub fn with_state<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        f(&mut self.shared.state.lock().unwrap())
    }

    /// Wait until `predicate` holds (or panic after `timeout`).
    pub fn wait_for(&self, timeout: Duration, what: &str, predicate: impl Fn(&State) -> bool) {
        let deadline = std::time::Instant::now() + timeout;
        let mut state = self.shared.state.lock().unwrap();
        while !predicate(&state) {
            let now = std::time::Instant::now();
            if now >= deadline {
                panic!("fake VM: timed out waiting for {what}");
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap()
                .0;
        }
    }

    fn send(&self, bytes: &[u8]) {
        if let Some(stream) = self.shared.writer.lock().unwrap().as_mut() {
            let _ = stream.write_all(bytes);
        }
    }

    /// Fire every breakpoint request at `onNewIntent` code index `index` as
    /// one composite (suspend policy ALL), as ART does.
    pub fn hit_breakpoint(&self, index: u64) -> usize {
        let matching: Vec<i32> = self.with_state(|state| {
            state
                .requests
                .iter()
                .filter(|r| r.kind == 2 && !state.cleared.contains(&r.id))
                .filter(|r| {
                    r.location
                        .is_some_and(|(_, method, at)| method == ON_NEW_INTENT && at == index)
                })
                .map(|r| r.id)
                .collect()
        });
        if matching.is_empty() {
            return 0;
        }
        self.with_state(|state| state.suspend_count += 1);
        let mut body = vec![2_u8];
        put_i32(&mut body, matching.len() as i32);
        for request in &matching {
            body.push(2);
            put_i32(&mut body, *request);
            put_u64(&mut body, MAIN_THREAD);
            put_location(&mut body, ACTIVITY_CLASS, ON_NEW_INTENT, index);
        }
        self.send(&command_packet(0x4000_0001, 64, 100, &body));
        matching.len()
    }

    /// Load `io.example.app.Late`: one ClassPrepare composite carrying an
    /// event for every matching ClassPrepare request (policy EVENT_THREAD).
    pub fn load_late_class(&self) -> usize {
        let (count, packet) = self.with_state(late_prepare_packet);
        if let Some(packet) = packet {
            self.send(&packet);
        }
        count
    }

    /// Throw from `onNewIntent` index 5, caught in `catch_class` (`None`:
    /// uncaught). Fires every exception request whose flags match as one
    /// composite (suspend policy ALL), as ART does.
    pub fn throw_exception(&self, catch_class: Option<u64>) -> usize {
        let matching: Vec<i32> = self.with_state(|state| {
            state
                .requests
                .iter()
                .filter(|r| r.kind == 4 && !state.cleared.contains(&r.id))
                .filter(|r| {
                    r.exception_flags.is_some_and(|(caught, uncaught)| {
                        if catch_class.is_some() {
                            caught
                        } else {
                            uncaught
                        }
                    })
                })
                .map(|r| r.id)
                .collect()
        });
        if matching.is_empty() {
            return 0;
        }
        self.with_state(|state| state.suspend_count += 1);
        let mut body = vec![2_u8];
        put_i32(&mut body, matching.len() as i32);
        for request in &matching {
            body.push(4);
            put_i32(&mut body, *request);
            put_u64(&mut body, MAIN_THREAD);
            put_location(&mut body, ACTIVITY_CLASS, ON_NEW_INTENT, 5);
            body.push(b'L');
            put_u64(&mut body, ACTIVITY_OBJECT);
            match catch_class {
                Some(class) => put_location(&mut body, class, 0, 0),
                None => {
                    body.push(0);
                    put_u64(&mut body, 0);
                    put_u64(&mut body, 0);
                    put_u64(&mut body, 0);
                }
            }
        }
        self.send(&command_packet(0x4000_0004, 64, 100, &body));
        matching.len()
    }

    /// Drop the connection (the app process died).
    pub fn kill_connection(&self) {
        if let Some(stream) = self.shared.writer.lock().unwrap().take() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// Mark `io.example.app.Late` loaded and build the ClassPrepare composite
/// for every matching request (policy EVENT_THREAD).
fn late_prepare_packet(state: &mut State) -> (usize, Option<Vec<u8>>) {
    state.late_loaded = true;
    let matching: Vec<i32> = state
        .requests
        .iter()
        .filter(|r| r.kind == 8 && !state.cleared.contains(&r.id))
        .filter(|r| {
            r.source_name
                .as_deref()
                .is_none_or(|name| name == "Late.kt")
                && r.class_match
                    .as_deref()
                    .is_none_or(|pattern| class_matches("io.example.app.Late", pattern))
        })
        .map(|r| r.id)
        .collect();
    if matching.is_empty() {
        return (0, None);
    }
    let mut body = vec![1_u8];
    put_i32(&mut body, matching.len() as i32);
    for request in &matching {
        body.push(8);
        put_i32(&mut body, *request);
        put_u64(&mut body, MAIN_THREAD);
        body.push(1);
        put_u64(&mut body, 105);
        put_str(&mut body, "Lio/example/app/Late;");
        put_i32(&mut body, 7);
    }
    (
        matching.len(),
        Some(command_packet(0x4000_0002, 64, 100, &body)),
    )
}

fn class_matches(name: &str, pattern: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        name.starts_with(prefix)
    } else if let Some(suffix) = pattern.strip_prefix('*') {
        name.ends_with(suffix)
    } else {
        name == pattern
    }
}

// ── wire helpers ──────────────────────────────────────────────────────

fn put_i32(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_str(out: &mut Vec<u8>, value: &str) {
    put_i32(out, value.len() as i32);
    out.extend_from_slice(value.as_bytes());
}

fn put_location(out: &mut Vec<u8>, class: u64, method: u64, index: u64) {
    out.push(1);
    put_u64(out, class);
    put_u64(out, method);
    put_u64(out, index);
}

fn command_packet(id: u32, set: u8, cmd: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(11 + body.len());
    out.extend_from_slice(&((11 + body.len()) as u32).to_be_bytes());
    out.extend_from_slice(&id.to_be_bytes());
    out.push(0);
    out.push(set);
    out.push(cmd);
    out.extend_from_slice(body);
    out
}

fn reply_packet(id: u32, error: u16, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(11 + body.len());
    out.extend_from_slice(&((11 + body.len()) as u32).to_be_bytes());
    out.extend_from_slice(&id.to_be_bytes());
    out.push(0x80);
    out.extend_from_slice(&error.to_be_bytes());
    out.extend_from_slice(body);
    out
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn u8(&mut self) -> u8 {
        let v = self.data[self.pos];
        self.pos += 1;
        v
    }
    fn i32(&mut self) -> i32 {
        let v = i32::from_be_bytes(self.data[self.pos..self.pos + 4].try_into().unwrap());
        self.pos += 4;
        v
    }
    fn u64(&mut self) -> u64 {
        let v = u64::from_be_bytes(self.data[self.pos..self.pos + 8].try_into().unwrap());
        self.pos += 8;
        v
    }
    fn string(&mut self) -> String {
        let len = self.i32() as usize;
        let v = String::from_utf8_lossy(&self.data[self.pos..self.pos + len]).into_owned();
        self.pos += len;
        v
    }
}

// ── the synthetic world ───────────────────────────────────────────────

/// `(method id, name, signature, line table)`
type Method = (u64, &'static str, &'static str, &'static [(u64, i32)]);

struct Class {
    id: u64,
    signature: &'static str,
    source: Option<&'static str>,
    methods: &'static [Method],
}

const CLASSES: &[Class] = &[
    Class {
        id: 100,
        signature: "Lio/example/app/MainActivity;",
        source: Some("MainActivity.kt"),
        methods: &[
            (
                1000,
                "onCreate",
                "(Landroid/os/Bundle;)V",
                &[(0, 20), (4, 21), (8, 22), (12, 23)],
            ),
            (
                1001,
                "onNewIntent",
                "(Landroid/content/Intent;)V",
                &[(0, 30), (5, 31), (10, 32)],
            ),
        ],
    },
    Class {
        id: 101,
        signature: "Lio/example/app/MainActivity$onCreate$1;",
        source: Some("MainActivity.kt"),
        methods: &[(1010, "invoke", "()V", &[(0, 25)])],
    },
    Class {
        id: 102,
        signature: "Lio/example/app/R$id;",
        source: None,
        methods: &[],
    },
    Class {
        id: 103,
        signature: "Ljava/lang/String;",
        source: Some("String.java"),
        methods: &[],
    },
    Class {
        id: 104,
        signature: "Ljava/lang/Object;",
        source: Some("Object.java"),
        methods: &[],
    },
    Class {
        id: 105,
        signature: "Lio/example/app/Late;",
        source: Some("Late.kt"),
        methods: &[(1050, "run", "()V", &[(0, 7)])],
    },
    Class {
        id: 106,
        signature: "[I",
        source: None,
        methods: &[],
    },
];

fn class(id: u64) -> Option<&'static Class> {
    CLASSES.iter().find(|c| c.id == id)
}

fn loaded(state: &State) -> impl Iterator<Item = &'static Class> + '_ {
    CLASSES
        .iter()
        .filter(move |c| c.id != 105 || state.late_loaded)
}

fn serve(mut stream: TcpStream, shared: Arc<Shared>) {
    let mut hello = [0_u8; 14];
    if stream.read_exact(&mut hello).is_err() || &hello != b"JDWP-Handshake" {
        return;
    }
    if shared.state.lock().unwrap().reject_handshake {
        // What a second debugger sees: adb OKAY, then EOF before the echo.
        return;
    }
    if shared.state.lock().unwrap().silent_handshake {
        // Hold the stream open, never echo, until the client gives up.
        let mut sink = [0_u8; 64];
        while matches!(stream.read(&mut sink), Ok(n) if n > 0) {}
        return;
    }
    stream.write_all(b"JDWP-Handshake").unwrap();
    *shared.writer.lock().unwrap() = Some(stream.try_clone().unwrap());
    shared.state.lock().unwrap().connections += 1;
    loop {
        let mut header = [0_u8; 11];
        if stream.read_exact(&mut header).is_err() {
            break;
        }
        let length = u32::from_be_bytes(header[0..4].try_into().unwrap()) as usize;
        let id = u32::from_be_bytes(header[4..8].try_into().unwrap());
        let mut body = vec![0_u8; length - 11];
        if stream.read_exact(&mut body).is_err() {
            break;
        }
        if header[8] & 0x80 != 0 {
            continue;
        }
        let (set, cmd) = (header[9], header[10]);
        let silent = {
            let mut state = shared.state.lock().unwrap();
            state.commands.push((set, cmd));
            state.silent.contains(&(set, cmd))
        };
        shared.changed.notify_all();
        if silent {
            continue;
        }
        if (set, cmd) == (1, 20) {
            let packet = {
                let mut state = shared.state.lock().unwrap();
                if state.prepare_late_during_scan && !state.late_loaded {
                    late_prepare_packet(&mut state).1
                } else {
                    None
                }
            };
            if let (Some(packet), Some(writer)) = (packet, shared.writer.lock().unwrap().as_mut()) {
                let _ = writer.write_all(&packet);
            }
        }
        let (error, reply, after) = handle(&shared, set, cmd, &body);
        if let Some(writer) = shared.writer.lock().unwrap().as_mut() {
            let _ = writer.write_all(&reply_packet(id, error, &reply));
        }
        shared.changed.notify_all();
        match after {
            After::Nothing => {}
            After::Close => {
                if let Some(writer) = shared.writer.lock().unwrap().take() {
                    let _ = writer.shutdown(std::net::Shutdown::Both);
                }
                break;
            }
            After::Step(request) => {
                // The thread runs one line, then the step request fires.
                std::thread::sleep(Duration::from_millis(20));
                let index = shared.state.lock().unwrap().step_line_index;
                shared.state.lock().unwrap().suspend_count += 1;
                let mut event = vec![2_u8];
                put_i32(&mut event, 1);
                event.push(1);
                put_i32(&mut event, request);
                put_u64(&mut event, MAIN_THREAD);
                put_location(&mut event, ACTIVITY_CLASS, ON_NEW_INTENT, index);
                if let Some(writer) = shared.writer.lock().unwrap().as_mut() {
                    let _ = writer.write_all(&command_packet(0x4000_0003, 64, 100, &event));
                }
            }
        }
    }
    shared.writer.lock().unwrap().take();
    shared.changed.notify_all();
}

enum After {
    Nothing,
    Close,
    Step(i32),
}

fn handle(shared: &Shared, set: u8, cmd: u8, body: &[u8]) -> (u16, Vec<u8>, After) {
    let mut c = Cursor { data: body, pos: 0 };
    let mut out = Vec::new();
    let mut state = shared.state.lock().unwrap();
    let suspended = state.suspend_count > 0;
    match (set, cmd) {
        // VirtualMachine
        (1, 7) => {
            for _ in 0..5 {
                put_i32(&mut out, 8);
            }
        }
        (1, 1) => {
            put_str(&mut out, "Fake ART JDWP");
            put_i32(&mut out, 1);
            put_i32(&mut out, 8);
            put_str(&mut out, "fake-0.1");
            put_str(&mut out, "Dalvik");
        }
        (1, 17) => {
            // 32 booleans with the undocumented reserved32 bit set, as ART does.
            for index in 0..32 {
                out.push(u8::from(index < 2 || index == 31));
            }
        }
        (1, 20) => {
            let classes: Vec<_> = loaded(&state).collect();
            put_i32(&mut out, classes.len() as i32);
            for class in classes {
                out.push(1);
                put_u64(&mut out, class.id);
                put_str(&mut out, class.signature);
                put_str(&mut out, "");
                put_i32(&mut out, 7);
            }
        }
        (1, 2) => {
            let signature = c.string();
            let found: Vec<_> = loaded(&state)
                .filter(|class| class.signature == signature)
                .collect();
            put_i32(&mut out, found.len() as i32);
            for class in found {
                out.push(1);
                put_u64(&mut out, class.id);
                put_i32(&mut out, 7);
            }
        }
        (1, 4) => {
            put_i32(&mut out, 2);
            put_u64(&mut out, 300);
            put_u64(&mut out, 301);
        }
        (1, 8) => state.suspend_count += 1,
        (1, 9) => {
            state.resumes += 1;
            state.suspend_count = (state.suspend_count - 1).max(0);
            let step = state
                .requests
                .iter()
                .rev()
                .find(|r| r.kind == 1 && !state.cleared.contains(&r.id))
                .map(|r| r.id);
            if let Some(step) = step {
                return (0, out, After::Step(step));
            }
        }
        (1, 6) => {
            state.disposed = true;
            state.requests.clear();
            state.suspend_count = 0;
            return (0, out, After::Close);
        }
        // ReferenceType
        (2, 1) => match class(c.u64()) {
            Some(class) => put_str(&mut out, class.signature),
            None => return (21, out, After::Nothing),
        },
        (2, 7) => match class(c.u64()).map(|class| class.source) {
            Some(Some(source)) => put_str(&mut out, source),
            Some(None) => return (101, out, After::Nothing),
            None => return (21, out, After::Nothing),
        },
        (2, 15) => {
            let Some(class) = class(c.u64()) else {
                return (21, out, After::Nothing);
            };
            put_i32(&mut out, class.methods.len() as i32);
            for (id, name, signature, _) in class.methods {
                put_u64(&mut out, *id);
                put_str(&mut out, name);
                put_str(&mut out, signature);
                put_str(&mut out, "");
                put_i32(&mut out, 1);
            }
        }
        (2, 14) => {
            let type_id = c.u64();
            let fields: &[(u64, &str, &str)] = if type_id == ACTIVITY_CLASS {
                &[
                    (2000, "counter", "I"),
                    (2001, "label", "Ljava/lang/String;"),
                    (2002, "numbers", "[I"),
                    (2003, "shadow$_klass_", "Ljava/lang/Class;"),
                ]
            } else {
                &[]
            };
            put_i32(&mut out, fields.len() as i32);
            for (id, name, signature) in fields {
                put_u64(&mut out, *id);
                put_str(&mut out, name);
                put_str(&mut out, signature);
                put_str(&mut out, "");
                put_i32(&mut out, 2);
            }
        }
        (2, 12) => return (101, out, After::Nothing),
        // ClassType.Superclass
        (3, 1) => {
            let type_id = c.u64();
            put_u64(&mut out, if type_id == 104 { 0 } else { 104 });
        }
        // Method
        (6, 1) => {
            let type_id = c.u64();
            let method_id = c.u64();
            let Some(lines) = class(type_id).and_then(|class| {
                class
                    .methods
                    .iter()
                    .find(|(id, ..)| *id == method_id)
                    .map(|(.., lines)| *lines)
            }) else {
                return (23, out, After::Nothing);
            };
            put_u64(&mut out, 0);
            put_u64(&mut out, 20);
            put_i32(&mut out, lines.len() as i32);
            for (index, line) in lines {
                put_u64(&mut out, *index);
                put_i32(&mut out, *line);
            }
        }
        (6, 5) => {
            let _type_id = c.u64();
            let method_id = c.u64();
            let vars: &[(u64, &str, &str, i32, i32)] = if method_id == ON_NEW_INTENT {
                &[
                    (0, "this", "Lio/example/app/MainActivity;", 20, 0),
                    (0, "tag", "Ljava/lang/String;", 20, 1),
                    (5, "count", "I", 15, 2),
                    (5, "$i$f$inline", "I", 15, 3),
                ]
            } else {
                &[]
            };
            put_i32(&mut out, 1);
            put_i32(&mut out, vars.len() as i32);
            for (start, name, signature, length, slot) in vars {
                put_u64(&mut out, *start);
                put_str(&mut out, name);
                put_str(&mut out, signature);
                put_str(&mut out, "");
                put_i32(&mut out, *length);
                put_i32(&mut out, *slot);
            }
        }
        // ObjectReference
        (9, 1) => {
            let object = c.u64();
            let type_id = match object {
                500 => 100,
                501 => 103,
                502 => 106,
                _ => return (20, out, After::Nothing),
            };
            out.push(if object == 502 { 3 } else { 1 });
            put_u64(&mut out, type_id);
        }
        (9, 2) => {
            let object = c.u64();
            if object != ACTIVITY_OBJECT {
                return (20, out, After::Nothing);
            }
            let count = c.i32();
            put_i32(&mut out, count);
            for _ in 0..count {
                match c.u64() {
                    2000 => {
                        out.push(b'I');
                        put_i32(&mut out, 7);
                    }
                    2001 => {
                        out.push(b's');
                        put_u64(&mut out, 501);
                    }
                    2002 => {
                        out.push(b'[');
                        put_u64(&mut out, 502);
                    }
                    _ => {
                        out.push(b'L');
                        put_u64(&mut out, 0);
                    }
                }
            }
        }
        (9, 7) => {
            let object = c.u64();
            state.pinned.insert(object);
        }
        (9, 8) => {
            let object = c.u64();
            state.pinned.remove(&object);
        }
        (9, 9) => {
            let object = c.u64();
            out.push(u8::from(!matches!(object, 500..=502)));
        }
        (10, 1) => match c.u64() {
            501 => put_str(&mut out, "hello"),
            _ => return (20, out, After::Nothing),
        },
        // ThreadReference
        (11, 1) => match c.u64() {
            300 => put_str(&mut out, "main"),
            301 => put_str(&mut out, "worker"),
            _ => return (10, out, After::Nothing),
        },
        (11, 3) => {
            state.thread_resumes += 1;
        }
        (11, 4) => {
            let _thread = c.u64();
            put_i32(&mut out, if suspended { 2 } else { 1 });
            put_i32(&mut out, i32::from(suspended));
        }
        (11, 12) => {
            let _thread = c.u64();
            put_i32(&mut out, state.suspend_count);
        }
        (11, 7) => {
            let thread = c.u64();
            if !suspended {
                return (13, out, After::Nothing);
            }
            put_i32(&mut out, if thread == MAIN_THREAD { 2 } else { 0 });
        }
        (11, 6) => {
            let thread = c.u64();
            let start = c.i32();
            let length = c.i32();
            if !suspended {
                return (13, out, After::Nothing);
            }
            let all: Vec<(u64, u64, u64)> = if thread == MAIN_THREAD {
                let index = if state.resumes > 0 && state.suspend_count > 0 {
                    state.step_line_index
                } else {
                    5
                };
                vec![(900, ON_NEW_INTENT, index), (901, 1000, 4)]
            } else {
                Vec::new()
            };
            let start = start.max(0) as usize;
            let end = if length < 0 {
                all.len()
            } else {
                (start + length as usize).min(all.len())
            };
            let frames = all.get(start..end).unwrap_or(&[]);
            put_i32(&mut out, frames.len() as i32);
            for (frame, method, index) in frames {
                put_u64(&mut out, *frame);
                put_location(&mut out, ACTIVITY_CLASS, *method, *index);
            }
        }
        // ArrayReference
        (13, 1) => {
            let _array = c.u64();
            put_i32(&mut out, 3);
        }
        (13, 2) => {
            let _array = c.u64();
            let first = c.i32();
            let length = c.i32();
            out.push(b'I');
            put_i32(&mut out, length);
            for value in first..first + length {
                put_i32(&mut out, value + 1);
            }
        }
        // EventRequest
        (15, 1) => {
            let kind = c.u8();
            let policy = c.u8();
            let count = c.i32();
            let mut request = Request {
                id: state.next_request,
                kind,
                policy,
                location: None,
                source_name: None,
                class_match: None,
                step_thread: None,
                exception_flags: None,
                modifier_kinds: Vec::new(),
            };
            for _ in 0..count {
                let modifier = c.u8();
                request.modifier_kinds.push(modifier);
                match modifier {
                    1 => {
                        c.i32();
                    }
                    3 => {
                        c.u64();
                    }
                    4 => {
                        c.u64();
                    }
                    5 => request.class_match = Some(c.string()),
                    6 => {
                        c.string();
                    }
                    7 => {
                        c.u8();
                        request.location = Some((c.u64(), c.u64(), c.u64()));
                    }
                    8 => {
                        c.u64();
                        let caught = c.u8() != 0;
                        let uncaught = c.u8() != 0;
                        request.exception_flags = Some((caught, uncaught));
                    }
                    10 => {
                        request.step_thread = Some(c.u64());
                        c.i32();
                        c.i32();
                    }
                    12 => request.source_name = Some(c.string()),
                    _ => return (103, out, After::Nothing),
                }
            }
            state.next_request += 1;
            put_i32(&mut out, request.id);
            state.requests.push(request);
        }
        (15, 2) => {
            let _kind = c.u8();
            let id = c.i32();
            state.cleared.push(id);
        }
        (15, 3) => {
            state.requests.retain(|r| r.kind != 2);
        }
        // StackFrame
        (16, 1) => {
            let _thread = c.u64();
            let frame = c.u64();
            if frame != 900 {
                return (30, out, After::Nothing);
            }
            let count = c.i32();
            put_i32(&mut out, count);
            for _ in 0..count {
                let slot = c.i32();
                let _tag = c.u8();
                match slot {
                    0 => {
                        out.push(b'L');
                        put_u64(&mut out, ACTIVITY_OBJECT);
                    }
                    1 => {
                        out.push(b's');
                        put_u64(&mut out, 501);
                    }
                    2 | 3 => {
                        out.push(b'I');
                        put_i32(&mut out, 42);
                    }
                    _ => return (35, Vec::new(), After::Nothing),
                }
            }
        }
        (16, 3) => {
            let _thread = c.u64();
            let frame = c.u64();
            out.push(b'L');
            put_u64(&mut out, if frame == 900 { ACTIVITY_OBJECT } else { 0 });
        }
        _ => return (99, out, After::Nothing),
    }
    (0, out, After::Nothing)
}
