"""Live spike for design questions 3-7 and 10 (attach to the running sample app).

Run:  python3 -I spike.py
Always disposes the VM in finally (which clears requests and resumes everything).
"""
import json
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import jdwp  # noqa: E402
from jdwp import (EV_BREAKPOINT, EV_CLASS_PREPARE, EV_STEP, SUSPEND_NONE, SUSPEND_THREAD,
                  JdwpError, m_class_exclude, m_class_match, m_count, m_location,
                  m_source_name, m_step)

SER = "emulator-5554"
PKG = "io.github.andriyo.shadowdroid.sample"
ACT = f"{PKG}/.MainActivity"
PREFIX = "Lio/github/andriyo/shadowdroid/sample/"
SRC = "MainActivity.kt"
LINE = 100


def adb(*a, check=True):
    return subprocess.run(["adb", "-s", SER, *a], capture_output=True, text=True, check=check).stdout


def fire(tag):
    # non-blocking: the main thread will be suspended when the intent lands
    return subprocess.Popen(["adb", "-s", SER, "shell", "am", "start", "-n", ACT, "-f",
                             "0x20000000", "--es", "tag", tag],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def section(t):
    print(f"\n===== {t} =====", flush=True)


def loc_str(c, loc, cache={}):
    tag, cls, meth, idx = loc
    key = (cls, meth)
    if key not in cache:
        sig = c.signature(cls)
        name = next((m["name"] for m in c.methods(cls) if m["id"] == meth), f"m{meth}")
        line = None
        try:
            _, _, lt = c.line_table(cls, meth)
            cache[key] = (sig, name, lt)
        except JdwpError as e:
            cache[key] = (sig, name, [])
    sig, name, lt = cache[key]
    line = None
    for ci, ln in sorted(lt):
        if ci <= idx:
            line = ln
    return f"{sig}.{name}@{idx} line={line}"


def describe_value(c, v):
    tag, val = v
    if tag == "string" and val:
        try:
            return f'string#{val}="{c.string_value(val)}"'
        except JdwpError as e:
            return f"string#{val} <err {e.code}>"
    if tag in ("object", "array", "thread", "classobject", "classloader") and val:
        try:
            _, ref = c.obj_type(val)
            return f"{tag}#{val}:{c.signature(ref)}"
        except JdwpError as e:
            return f"{tag}#{val} <err {e.code}>"
    return f"{tag}:{val}"


def main():
    pid = int(adb("shell", "pidof", PKG).split()[0])
    print("pid", pid)
    sock = jdwp.open_adb_jdwp(SER, pid)
    jdwp.handshake(sock)
    c = jdwp.Jdwp(sock)
    c.id_sizes()
    second = None
    try:
        run(c, pid)
    finally:
        section("cleanup")
        try:
            c.dispose()
            print("Dispose OK")
        except Exception as e:
            print("Dispose failed:", e)
        sock.close()
        time.sleep(0.5)
        print("pidof after:", adb("shell", "pidof", PKG, check=False).strip())


def find_line_locations(c, line):
    t0 = time.monotonic()
    classes = c.all_classes_generic()
    t_all = (time.monotonic() - t0) * 1000
    pkg_classes = [k for k in classes if k["sig"].startswith(PREFIX)]
    print(f"AllClassesWithGeneric: {len(classes)} classes in {t_all:.0f}ms; "
          f"{len(pkg_classes)} under {PREFIX}")
    cands = []
    srcfile_errs = {}
    for k in pkg_classes:
        try:
            sf = c.source_file(k["id"])
        except JdwpError as e:
            srcfile_errs[e.code] = srcfile_errs.get(e.code, 0) + 1
            continue
        if sf == SRC:
            cands.append(k)
    print("SourceFile errors by code:", srcfile_errs)
    print(f"classes with SourceFile=={SRC}:")
    for k in cands:
        print(f"   tag={k['tag']} status={k['status']} {k['sig']}")
    locs = []
    for k in cands:
        for m in c.methods(k["id"]):
            try:
                start, end, lt = c.line_table(k["id"], m["id"])
            except JdwpError as e:
                continue
            hits = sorted(ci for ci, ln in lt if ln == line)
            if hits:
                loc = (k["tag"], k["id"], m["id"], hits[0])
                locs.append((k, m, hits, lt, loc))
                print(f"   LINE {line} in {k['sig']}.{m['name']}{m['sig']} codeIndex={hits} "
                      f"(method range {start}..{end}, {len(lt)} line entries)")
    return locs, cands


def dump_frame(c, thread, frames, idx=0):
    fid, loc = frames[idx]
    tag, cls, meth, ci = loc
    argc, slots = c.variable_table(cls, meth)
    print(f"VariableTableWithGeneric argCnt={argc} entries={len(slots)}")
    for s in slots:
        vis = s["code_index"] <= ci < s["code_index"] + s["length"]
        print(f"   slot={s['slot']} name={s['name']!r} sig={s['sig']} gsig={s['gsig']!r} "
              f"range={s['code_index']}+{s['length']} visible@{ci}={vis}")
    visible = [s for s in slots if s["code_index"] <= ci < s["code_index"] + s["length"]]
    vals = c.frame_values(thread, fid, [(s["slot"], s["sig"]) for s in visible])
    out = {}
    for s, v in zip(visible, vals):
        print(f"   GetValues {s['name']} = {describe_value(c, v)}")
        out[s["name"]] = v
    # an invisible slot, to record the error code
    invisible = [s for s in slots if s not in visible]
    if invisible:
        s = invisible[0]
        try:
            v = c.frame_values(thread, fid, [(s["slot"], s["sig"])])
            print(f"   GetValues invisible {s['name']} -> {v}")
        except JdwpError as e:
            print(f"   GetValues invisible {s['name']} -> JDWP error {e.code}")
    try:
        c.frame_values(thread, fid, [(99, "I")])
    except JdwpError as e:
        print(f"   GetValues bogus slot 99 -> JDWP error {e.code}")
    return out


def run(c, pid):
    section("Q10 exclusivity: second jdwp:<pid> connection while attached")
    t0 = time.monotonic()
    try:
        s2 = jdwp.open_adb_jdwp(SER, pid)
        print(f"   ADB-level: OKAY after {(time.monotonic()-t0)*1000:.0f}ms")
        try:
            jdwp.handshake(s2, timeout=4)
            print("   handshake: SUCCEEDED (not exclusive!)")
        except Exception as e:
            print(f"   handshake: {type(e).__name__}: {e} after {(time.monotonic()-t0)*1000:.0f}ms")
        s2.close()
    except jdwp.AdbFail as e:
        print(f"   ADB-level FAIL: {e!r} after {(time.monotonic()-t0)*1000:.0f}ms")
    time.sleep(0.3)
    print("   first connection still alive:", c.version()["vmName"], "closed=", c.closed)

    section("Q3 SourceNameMatch ClassPrepare")
    for pat in (SRC, "*.kt", "Main*"):
        try:
            rid = c.set_request(EV_CLASS_PREPARE, SUSPEND_NONE, [m_source_name(pat)])
            print(f"   SourceNameMatch {pat!r}: accepted request id {rid}")
            c.clear_request(EV_CLASS_PREPARE, rid)
        except JdwpError as e:
            print(f"   SourceNameMatch {pat!r}: JDWP error {e.code}")
    rid = c.set_request(EV_CLASS_PREPARE, SUSPEND_NONE, [m_class_match("io.github.andriyo.shadowdroid.sample.*")])
    print(f"   ClassMatch 'io.github.andriyo.shadowdroid.sample.*' ClassPrepare accepted id {rid} (kept for later)")
    cp_req = rid

    section("Q4 breakpoint resolution")
    locs, cands = find_line_locations(c, LINE)
    bp_reqs = []
    for k, m, hits, lt, loc in locs:
        rid = c.set_request(EV_BREAKPOINT, SUSPEND_THREAD, [m_location(loc)])
        bp_reqs.append(rid)
        print(f"   Breakpoint set id={rid} at {k['sig']}.{m['name']}@{loc[3]}")

    # make sure activity is in front
    t_fire = time.monotonic()
    p = fire("bp1")
    ev = c.wait_event(lambda e: e["kind"] == EV_BREAKPOINT, timeout=15)
    print(f"   Breakpoint event after {(time.monotonic()-t_fire)*1000:.0f}ms: {ev}")
    thread = ev["thread"]
    print(f"   thread id={thread} name={c.thread_name(thread)!r} status={c.thread_status(thread)} "
          f"suspendCount={c.suspend_count(thread)}")
    print("   location:", loc_str(c, ev["loc"]))
    other = c.drain_events()
    print("   other queued events:", [(round(t, 3), e) for t, e in other][:5])

    section("Q5 frames while suspended")
    frames = c.frames(thread)
    print(f"   Frames: {len(frames)}")
    for i, (fid, loc) in enumerate(frames[:8]):
        print(f"   #{i} frame={fid} {loc_str(c, loc)}")
    this = c.this_object(thread, frames[0][0])
    print("   ThisObject:", describe_value(c, this))
    vals = dump_frame(c, thread, frames)
    intent = vals.get("intent", (None, None))[1]
    if intent:
        tag, ref = c.obj_type(intent)
        print(f"   intent ReferenceType tag={tag} sig={c.signature(ref)}")
        fl = c.fields(ref)
        print(f"   Intent declared fields ({len(fl)}):", [f["name"] for f in fl])
        want = [f for f in fl if f["name"] in ("mAction", "mComponent", "mFlags", "mExtras", "mData")]
        fv = c.obj_values(intent, [f["id"] for f in want])
        for f, v in zip(want, fv):
            print(f"      {f['name']} ({f['sig']}) = {describe_value(c, v)}")
        comp = next((v for f, v in zip(want, fv) if f["name"] == "mComponent"), None)
        if comp and comp[1]:
            _, cref = c.obj_type(comp[1])
            cf = c.fields(cref)
            cv = c.obj_values(comp[1], [f["id"] for f in cf if not f["mod"] & 0x8])
            print("      mComponent fields:", {f["name"]: describe_value(c, v)
                                              for f, v in zip([f for f in cf if not f["mod"] & 0x8], cv)})
    # this fields (Kotlin delegated props)
    _, tref = c.obj_type(this[1])
    tf = c.fields(tref)
    print("   MainActivity declared fields:", [(f["name"], f["sig"]) for f in tf])
    inst = [f for f in tf if not f["mod"] & 0x8]
    tv = c.obj_values(this[1], [f["id"] for f in inst])
    for f, v in zip(inst, tv):
        print(f"      this.{f['name']} = {describe_value(c, v)}")

    section("Q7 stepping: OVER from line 100")
    def step(depth, excludes=()):
        # Count MUST be last: modifiers are applied in order (see RESULTS.md Q7)
        mods = [m_step(thread, 1, depth)] + [m_class_exclude(x) for x in excludes] + [m_count(1)]
        rid = c.set_request(EV_STEP, SUSPEND_THREAD, mods)
        t0 = time.monotonic()
        c.thread_resume(thread)
        e = c.wait_event(lambda e: e["kind"] in (EV_STEP, EV_BREAKPOINT), timeout=10)
        dt = (time.monotonic() - t0) * 1000
        try:
            c.clear_request(EV_STEP, rid)
            cleared = "cleared"
        except JdwpError as ex:
            cleared = f"clear err {ex.code}"
        print(f"   step depth={depth} req={rid} -> kind={e['kind']} after {dt:.0f}ms "
              f"{loc_str(c, e['loc'])} thread_same={e['thread']==thread} ({cleared})")
        return e
    step(1)   # OVER: 100 -> 101
    step(1)   # OVER: 101 -> 102
    excl = ["kotlin.*", "java.*", "android.*", "androidx.*"]
    e = step(0, excl)  # INTO at 102 -> intentSummary?
    fr = c.frames(thread, 0, 3)
    for i, (fid, loc) in enumerate(fr):
        print(f"      after INTO #{i} {loc_str(c, loc)}")
    print("   frame values after INTO:")
    dump_frame(c, thread, c.frames(thread))
    e = step(2)  # OUT
    c.thread_resume(thread)
    p.wait(timeout=10)

    section("Q6 DisableCollection")
    # hit #1 already happened (intent A = `intent`); pin A and this
    A = intent
    c.disable_collection(A)
    c.disable_collection(this[1])
    print(f"   pinned A={A} this={this[1]}")
    ids = {"A": A}
    for tag in ("bp2", "bp3", "bp4"):
        p = fire(tag)
        ev = c.wait_event(lambda e: e["kind"] == EV_BREAKPOINT, timeout=15)
        fr = c.frames(ev["thread"], 0, 1)
        _, sl = c.variable_table(fr[0][1][1], fr[0][1][2])
        s = next(s for s in sl if s["name"] == "intent")
        v = c.frame_values(ev["thread"], fr[0][0], [(s["slot"], s["sig"])])[0]
        ids[tag] = v[1]
        print(f"   hit {tag}: intent id {v[1]}")
        c.thread_resume(ev["thread"])
        p.wait(timeout=10)
    for rid in bp_reqs:
        c.clear_request(EV_BREAKPOINT, rid)
    # GC pressure: SIGUSR1 via run-as forces a GC in ART; also am dumpheap fallback
    for i in range(3):
        r = subprocess.run(["adb", "-s", SER, "shell", "run-as", PKG, "kill", "-10", str(pid)],
                           capture_output=True, text=True)
        time.sleep(1.0)
    print("   SIGUSR1 x3 rc", r.returncode, r.stderr.strip())
    for name, oid in ids.items():
        try:
            col = c.is_collected(oid)
        except JdwpError as e:
            col = f"err {e.code}"
        try:
            _, ref = c.obj_type(oid)
            fl = {f["name"]: f for f in c.fields(ref)}
            vv = c.obj_values(oid, [fl["mAction"]["id"], fl["mFlags"]["id"]])
            got = [describe_value(c, x) for x in vv]
        except JdwpError as e:
            got = f"GetValues err {e.code}"
        print(f"   {name} id={oid} pinned={name=='A'} IsCollected={col} values={got}")
    print("   this IsCollected:", c.is_collected(this[1]))
    c.enable_collection(A)
    c.enable_collection(this[1])
    print("   EnableCollection OK")
    c.clear_request(EV_CLASS_PREPARE, cp_req)
    cps = [e for t, evs in c.drain_events() if evs for e in evs if e["kind"] == EV_CLASS_PREPARE]
    print("   ClassPrepare (ClassMatch) events seen during run:", [e["sig"] for e in cps][:20])

    section("Q7b step INTO from line 100 (super.onNewIntent) with excludes")
    rid = c.set_request(EV_BREAKPOINT, SUSPEND_THREAD, [m_location(locs[0][4])])
    p = fire("bp5")
    ev = c.wait_event(lambda e: e["kind"] == EV_BREAKPOINT, timeout=15)
    thread = ev["thread"]
    c.clear_request(EV_BREAKPOINT, rid)
    step(0, excl)
    step(0)  # INTO without excludes from wherever we landed
    fr = c.frames(thread, 0, 4)
    for i, (fid, loc) in enumerate(fr):
        print(f"      #{i} {loc_str(c, loc)}")
    c.thread_resume(thread)
    p.wait(timeout=10)


if __name__ == "__main__":
    main()
