"""Throwaway stdlib-only JDWP client for the ShadowDroid JDWP spike.

Transport: ADB server protocol on localhost:5037 -> host:transport:<serial> -> jdwp:<pid>
(or a plain TCP socket after `adb forward tcp:N jdwp:<pid>`).
"""
import queue
import socket
import struct
import threading
import time

ADB_HOST, ADB_PORT = "127.0.0.1", 5037

# Tags
TAG_NAMES = {91: "array", 66: "byte", 67: "char", 76: "object", 70: "float", 68: "double",
             73: "int", 74: "long", 83: "short", 86: "void", 90: "boolean", 115: "string",
             116: "thread", 103: "threadgroup", 108: "classloader", 99: "classobject"}
OBJ_TAGS = {91, 76, 115, 116, 103, 108, 99}

EV_STEP, EV_BREAKPOINT, EV_EXCEPTION = 1, 2, 4
EV_THREAD_START, EV_THREAD_DEATH, EV_CLASS_PREPARE, EV_CLASS_UNLOAD = 6, 7, 8, 9
EV_FIELD_ACCESS, EV_FIELD_MOD = 20, 21
EV_METHOD_ENTRY, EV_METHOD_EXIT, EV_METHOD_EXIT_RV = 40, 41, 42
EV_VM_START, EV_VM_DEATH = 90, 99
SUSPEND_NONE, SUSPEND_THREAD, SUSPEND_ALL = 0, 1, 2

CAP_NAMES = [
    "canWatchFieldModification", "canWatchFieldAccess", "canGetBytecodes",
    "canGetSyntheticAttribute", "canGetOwnedMonitorInfo", "canGetCurrentContendedMonitor",
    "canGetMonitorInfo", "canRedefineClasses", "canAddMethod",
    "canUnrestrictedlyRedefineClasses", "canPopFrames", "canUseInstanceFilters",
    "canGetSourceDebugExtension", "canRequestVMDeathEvent", "canSetDefaultStratum",
    "canGetInstanceInfo", "canRequestMonitorEvents", "canGetMonitorFrameInfo",
    "canUseSourceNameFilters", "canGetConstantPool", "canForceEarlyReturn",
] + [f"reserved{i}" for i in range(22, 33)]


class JdwpError(Exception):
    def __init__(self, code, cmd):
        super().__init__(f"JDWP error {code} for cmd {cmd}")
        self.code = code
        self.cmd = cmd


class AdbFail(Exception):
    pass


def _adb_send(sock, msg):
    data = msg.encode()
    sock.sendall(b"%04x" % len(data) + data)
    status = _recv_exact(sock, 4)
    if status == b"OKAY":
        return
    if status == b"FAIL":
        n = int(_recv_exact(sock, 4), 16)
        raise AdbFail(_recv_exact(sock, n).decode(errors="replace"))
    raise AdbFail(f"unexpected status {status!r}")


def _recv_exact(sock, n):
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise EOFError(f"EOF after {len(buf)}/{n} bytes")
        buf += chunk
    return buf


def adb_host_query(msg, timeout=5):
    s = socket.create_connection((ADB_HOST, ADB_PORT), timeout=timeout)
    try:
        _adb_send(s, msg)
        n = int(_recv_exact(s, 4), 16)
        return _recv_exact(s, n).decode()
    finally:
        s.close()


def jdwp_pids(serial, wait=1.0):
    """Read the device `jdwp` service (streams pids) for `wait` seconds."""
    s = socket.create_connection((ADB_HOST, ADB_PORT), timeout=5)
    try:
        _adb_send(s, f"host:transport:{serial}")
        _adb_send(s, "jdwp")
        s.settimeout(wait)
        buf = b""
        try:
            while True:
                c = s.recv(4096)
                if not c:
                    break
                buf += c
        except socket.timeout:
            pass
        # jdwp service writes "%04x" + newline-separated pids
        out = []
        while len(buf) >= 4:
            n = int(buf[:4], 16)
            out = buf[4:4 + n].decode().split()
            buf = buf[4 + n:]
        return [int(p) for p in out]
    finally:
        s.close()


def open_adb_jdwp(serial, pid, timeout=10):
    s = socket.create_connection((ADB_HOST, ADB_PORT), timeout=timeout)
    _adb_send(s, f"host:transport:{serial}")
    _adb_send(s, f"jdwp:{pid}")
    return s


def open_tcp(port, timeout=10):
    return socket.create_connection(("127.0.0.1", port), timeout=timeout)


def handshake(sock, timeout=10):
    sock.settimeout(timeout)
    sock.sendall(b"JDWP-Handshake")
    got = _recv_exact(sock, 14)
    if got != b"JDWP-Handshake":
        raise AdbFail(f"bad handshake {got!r}")


class W:
    """Packet data writer."""

    def __init__(self, c):
        self.c = c
        self.b = bytearray()

    def u8(self, v):
        self.b += struct.pack(">B", v); return self

    def i32(self, v):
        self.b += struct.pack(">i", v); return self

    def i64(self, v):
        self.b += struct.pack(">q", v); return self

    def bool(self, v):
        return self.u8(1 if v else 0)

    def str(self, s):
        d = s.encode()
        self.i32(len(d)); self.b += d; return self

    def id(self, v, size):
        self.b += v.to_bytes(size, "big"); return self

    def obj(self, v):
        return self.id(v, self.c.sz["object"])

    def ref(self, v):
        return self.id(v, self.c.sz["ref"])

    def meth(self, v):
        return self.id(v, self.c.sz["method"])

    def field(self, v):
        return self.id(v, self.c.sz["field"])

    def frame(self, v):
        return self.id(v, self.c.sz["frame"])

    def loc(self, loc):
        tag, cls, meth, idx = loc
        return self.u8(tag).ref(cls).meth(meth).i64(idx)


class R:
    """Packet data reader."""

    def __init__(self, c, data):
        self.c = c
        self.d = data
        self.p = 0

    def take(self, n):
        v = self.d[self.p:self.p + n]
        if len(v) < n:
            raise EOFError("short packet")
        self.p += n
        return v

    def u8(self):
        return self.take(1)[0]

    def bool(self):
        return self.u8() != 0

    def i32(self):
        return struct.unpack(">i", self.take(4))[0]

    def i64(self):
        return struct.unpack(">q", self.take(8))[0]

    def str(self):
        return self.take(self.i32()).decode(errors="replace")

    def id(self, size):
        return int.from_bytes(self.take(size), "big")

    def obj(self):
        return self.id(self.c.sz["object"])

    def ref(self):
        return self.id(self.c.sz["ref"])

    def meth(self):
        return self.id(self.c.sz["method"])

    def field(self):
        return self.id(self.c.sz["field"])

    def frame(self):
        return self.id(self.c.sz["frame"])

    def loc(self):
        return (self.u8(), self.ref(), self.meth(), self.i64())

    def value(self, tag=None):
        if tag is None:
            tag = self.u8()
        if tag in OBJ_TAGS:
            return (TAG_NAMES[tag], self.obj())
        if tag == 66: return ("byte", struct.unpack(">b", self.take(1))[0])
        if tag == 90: return ("boolean", self.bool())
        if tag == 67: return ("char", chr(struct.unpack(">H", self.take(2))[0]))
        if tag == 83: return ("short", struct.unpack(">h", self.take(2))[0])
        if tag == 73: return ("int", self.i32())
        if tag == 74: return ("long", self.i64())
        if tag == 70: return ("float", struct.unpack(">f", self.take(4))[0])
        if tag == 68: return ("double", struct.unpack(">d", self.take(8))[0])
        if tag == 86: return ("void", None)
        raise ValueError(f"unknown tag {tag}")

    def rest(self):
        return self.d[self.p:]


class Jdwp:
    def __init__(self, sock, log=print):
        self.sock = sock
        self.log = log
        self.sz = {"object": 8, "ref": 8, "method": 8, "field": 8, "frame": 8}
        self.next_id = 1
        self.lock = threading.Lock()
        self.pending = {}
        self.events = queue.Queue()
        self.vm_cmds = []  # non-event commands from VM (e.g. DDM chunks)
        self.closed = False
        self.eof_reason = None
        sock.settimeout(None)
        self.reader = threading.Thread(target=self._read_loop, daemon=True)
        self.reader.start()

    # -- wire -------------------------------------------------------------
    def _read_loop(self):
        try:
            while True:
                hdr = _recv_exact(self.sock, 11)
                length, pid, flags = struct.unpack(">IIB", hdr[:9])
                body = _recv_exact(self.sock, length - 11)
                if flags & 0x80:
                    err = struct.unpack(">H", hdr[9:11])[0]
                    with self.lock:
                        slot = self.pending.get(pid)
                    if slot:
                        slot["err"], slot["data"] = err, body
                        slot["t"] = time.monotonic()
                        slot["ev"].set()
                else:
                    cs, cmd = hdr[9], hdr[10]
                    if cs == 64 and cmd == 100:
                        self.events.put((time.monotonic(), self._parse_composite(body)))
                    else:
                        self.vm_cmds.append((cs, cmd, body))
        except Exception as e:  # EOF or socket close
            self.eof_reason = repr(e)
            self.closed = True
            with self.lock:
                for slot in self.pending.values():
                    slot["ev"].set()
            self.events.put((time.monotonic(), None))

    def cmd(self, cs, c, data=b"", timeout=10, raw=False):
        with self.lock:
            pid = self.next_id
            self.next_id += 1
            slot = {"ev": threading.Event(), "err": None, "data": None}
            self.pending[pid] = slot
        pkt = struct.pack(">IIBBB", 11 + len(data), pid, 0, cs, c) + bytes(data)
        t0 = time.monotonic()
        self.sock.sendall(pkt)
        if not slot["ev"].wait(timeout):
            raise TimeoutError(f"no reply to {cs}/{c} in {timeout}s")
        with self.lock:
            del self.pending[pid]
        if slot["err"] is None:
            raise EOFError(f"connection closed during {cs}/{c}: {self.eof_reason}")
        slot["rtt_ms"] = (slot["t"] - t0) * 1000
        if slot["err"] != 0:
            raise JdwpError(slot["err"], f"{cs}/{c}")
        return slot["data"] if raw else R(self, slot["data"])

    def w(self):
        return W(self)

    # -- events -----------------------------------------------------------
    def _parse_composite(self, body):
        r = R(self, body)
        policy = r.u8()
        n = r.i32()
        evs = []
        for _ in range(n):
            kind = r.u8()
            req = r.i32()
            e = {"kind": kind, "req": req, "policy": policy}
            if kind in (EV_STEP, EV_BREAKPOINT, EV_METHOD_ENTRY, EV_METHOD_EXIT):
                e["thread"] = r.obj(); e["loc"] = r.loc()
            elif kind == EV_METHOD_EXIT_RV:
                e["thread"] = r.obj(); e["loc"] = r.loc(); e["value"] = r.value()
            elif kind == EV_EXCEPTION:
                e["thread"] = r.obj(); e["loc"] = r.loc(); e["exc"] = r.value(); e["catch"] = r.loc()
            elif kind in (EV_THREAD_START, EV_THREAD_DEATH, EV_VM_START):
                e["thread"] = r.obj()
            elif kind == EV_CLASS_PREPARE:
                e["thread"] = r.obj(); e["tag"] = r.u8(); e["type"] = r.ref()
                e["sig"] = r.str(); e["status"] = r.i32()
            elif kind == EV_CLASS_UNLOAD:
                e["sig"] = r.str()
            elif kind == EV_VM_DEATH:
                pass
            else:
                e["unparsed"] = r.rest().hex()
                evs.append(e)
                break
            evs.append(e)
        return evs

    def wait_event(self, pred=lambda e: True, timeout=10):
        """Return the next event matching pred. Events of a Composite are flattened and
        kept in order (a single ClassPrepare can carry one event per matching request)."""
        if not hasattr(self, "_backlog"):
            self._backlog = []
        for i, e in enumerate(self._backlog):
            if pred(e):
                return self._backlog.pop(i)
        deadline = time.monotonic() + timeout
        while True:
            left = deadline - time.monotonic()
            if left <= 0:
                raise TimeoutError("no matching event")
            try:
                t, evs = self.events.get(timeout=left)
            except queue.Empty:
                raise TimeoutError("no matching event")
            if evs is None:
                raise EOFError(f"VM connection closed: {self.eof_reason}")
            found = None
            for e in evs:
                e["composite_size"] = len(evs)
                if found is None and pred(e):
                    found = e
                else:
                    self._backlog.append(e)
            if found is not None:
                return found

    def drain_events(self):
        out = []
        while True:
            try:
                out.append(self.events.get_nowait())
            except queue.Empty:
                return out

    # -- VirtualMachine (1) --------------------------------------------------
    def id_sizes(self):
        r = self.cmd(1, 7)
        f, m, o, ref, fr = (r.i32() for _ in range(5))
        self.sz = {"field": f, "method": m, "object": o, "ref": ref, "frame": fr}
        return self.sz

    def version(self):
        r = self.cmd(1, 1)
        return {"description": r.str(), "jdwpMajor": r.i32(), "jdwpMinor": r.i32(),
                "vmVersion": r.str(), "vmName": r.str()}

    def capabilities_new(self):
        r = self.cmd(1, 17)
        return {CAP_NAMES[i]: r.bool() for i in range(32)}

    def all_classes_generic(self):
        r = self.cmd(1, 20, timeout=30)
        out = []
        for _ in range(r.i32()):
            out.append({"tag": r.u8(), "id": r.ref(), "sig": r.str(), "gsig": r.str(),
                        "status": r.i32()})
        return out

    def classes_by_signature(self, sig):
        r = self.cmd(1, 2, self.w().str(sig).b)
        return [{"tag": r.u8(), "id": r.ref(), "status": r.i32()} for _ in range(r.i32())]

    def all_threads(self):
        r = self.cmd(1, 4)
        return [r.obj() for _ in range(r.i32())]

    def suspend_vm(self):
        self.cmd(1, 8)

    def resume_vm(self):
        self.cmd(1, 9)

    def dispose(self):
        self.cmd(1, 6, timeout=5)

    # -- ReferenceType (2) -------------------------------------------------
    def signature(self, ref):
        return self.cmd(2, 1, self.w().ref(ref).b).str()

    def source_file(self, ref):
        return self.cmd(2, 7, self.w().ref(ref).b).str()

    def source_debug_extension(self, ref):
        return self.cmd(2, 12, self.w().ref(ref).b).str()

    def methods(self, ref):
        r = self.cmd(2, 15, self.w().ref(ref).b)
        return [{"id": r.meth(), "name": r.str(), "sig": r.str(), "gsig": r.str(),
                 "mod": r.i32()} for _ in range(r.i32())]

    def fields(self, ref):
        r = self.cmd(2, 14, self.w().ref(ref).b)
        return [{"id": r.field(), "name": r.str(), "sig": r.str(), "gsig": r.str(),
                 "mod": r.i32()} for _ in range(r.i32())]

    def static_values(self, ref, field_ids):
        w = self.w().ref(ref).i32(len(field_ids))
        for f in field_ids:
            w.field(f)
        r = self.cmd(2, 6, w.b)
        return [r.value() for _ in range(r.i32())]

    def superclass(self, ref):
        return self.cmd(3, 1, self.w().ref(ref).b).ref()

    # -- Method (6) ----------------------------------------------------------
    def line_table(self, ref, meth):
        r = self.cmd(6, 1, self.w().ref(ref).meth(meth).b)
        start, end = r.i64(), r.i64()
        return start, end, [(r.i64(), r.i32()) for _ in range(r.i32())]

    def variable_table(self, ref, meth):
        r = self.cmd(6, 5, self.w().ref(ref).meth(meth).b)
        argc = r.i32()
        slots = [{"code_index": r.i64(), "name": r.str(), "sig": r.str(), "gsig": r.str(),
                  "length": r.i32(), "slot": r.i32()} for _ in range(r.i32())]
        return argc, slots

    # -- ObjectReference (9) / String (10) -----------------------------------
    def obj_type(self, obj):
        r = self.cmd(9, 1, self.w().obj(obj).b)
        return r.u8(), r.ref()

    def obj_values(self, obj, field_ids):
        w = self.w().obj(obj).i32(len(field_ids))
        for f in field_ids:
            w.field(f)
        r = self.cmd(9, 2, w.b)
        return [r.value() for _ in range(r.i32())]

    def disable_collection(self, obj):
        self.cmd(9, 7, self.w().obj(obj).b)

    def enable_collection(self, obj):
        self.cmd(9, 8, self.w().obj(obj).b)

    def is_collected(self, obj):
        return self.cmd(9, 9, self.w().obj(obj).b).bool()

    def string_value(self, obj):
        return self.cmd(10, 1, self.w().obj(obj).b).str()

    # -- ThreadReference (11) ------------------------------------------------
    def thread_name(self, t):
        return self.cmd(11, 1, self.w().obj(t).b).str()

    def thread_resume(self, t):
        self.cmd(11, 3, self.w().obj(t).b)

    def thread_suspend(self, t):
        self.cmd(11, 2, self.w().obj(t).b)

    def thread_status(self, t):
        r = self.cmd(11, 4, self.w().obj(t).b)
        return r.i32(), r.i32()

    def suspend_count(self, t):
        return self.cmd(11, 12, self.w().obj(t).b).i32()

    def frames(self, t, start=0, length=-1):
        r = self.cmd(11, 6, self.w().obj(t).i32(start).i32(length).b)
        return [(r.frame(), r.loc()) for _ in range(r.i32())]

    # -- StackFrame (16) -----------------------------------------------------
    def frame_values(self, t, frame, slots):
        w = self.w().obj(t).frame(frame).i32(len(slots))
        for slot, sig in slots:
            w.i32(slot).u8(ord(sig[0]))
        r = self.cmd(16, 1, w.b)
        return [r.value() for _ in range(r.i32())]

    def this_object(self, t, frame):
        return self.cmd(16, 3, self.w().obj(t).frame(frame).b).value()

    # -- EventRequest (15) ---------------------------------------------------
    def set_request(self, kind, policy, mods=()):
        """mods: list of (modKind, writer-callback(W))"""
        w = self.w().u8(kind).u8(policy).i32(len(mods))
        for mk, fn in mods:
            w.u8(mk)
            fn(w)
        return self.cmd(15, 1, w.b).i32()

    def clear_request(self, kind, req):
        self.cmd(15, 2, self.w().u8(kind).i32(req).b)

    def clear_all_breakpoints(self):
        self.cmd(15, 3)


# modifier helpers
def m_count(n): return (1, lambda w: w.i32(n))
def m_thread_only(t): return (3, lambda w: w.obj(t))
def m_class_only(ref): return (4, lambda w: w.ref(ref))
def m_class_match(p): return (5, lambda w: w.str(p))
def m_class_exclude(p): return (6, lambda w: w.str(p))
def m_location(loc): return (7, lambda w: w.loc(loc))
def m_step(t, size, depth): return (10, lambda w: w.obj(t).i32(size).i32(depth))
def m_source_name(p): return (12, lambda w: w.str(p))
