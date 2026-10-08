"""Error-code probe: frames on running thread, stale frame ids, bogus ids, breakpoint on bogus index."""
import os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import jdwp
from jdwp import *
from spike import adb, fire, loc_str, find_line_locations, SER, PKG
def tryit(label, fn):
    try: r = fn(); print(f"   {label}: OK {str(r)[:120]}")
    except JdwpError as e: print(f"   {label}: JDWP error {e.code}")
pid = int(adb("shell","pidof",PKG).split()[0])
s = jdwp.open_adb_jdwp(SER, pid); jdwp.handshake(s); c = jdwp.Jdwp(s); c.id_sizes()
try:
    locs, _ = find_line_locations(c, 100); loc = locs[0][4]
    main = next(t for t in c.all_threads() if c.thread_name(t) == "main")
    tryit("Frames on running main thread", lambda: c.frames(main))
    tryit("FrameCount on running main", lambda: c.cmd(11, 7, c.w().obj(main).b).i32())
    tryit("ObjectReference.ReferenceType bogus id 0xdeadbeef", lambda: c.obj_type(0xdeadbeef))
    tryit("ReferenceType.Signature bogus ref", lambda: c.signature(0xdeadbeef))
    tryit("Breakpoint at codeIndex 9999", lambda: c.set_request(EV_BREAKPOINT, SUSPEND_THREAD, [m_location((loc[0], loc[1], loc[2], 9999))]))
    tryit("Breakpoint at odd codeIndex 6 (mid-instruction?)", lambda: c.set_request(EV_BREAKPOINT, SUSPEND_THREAD, [m_location((loc[0], loc[1], loc[2], 6))]))
    tryit("Clear unknown request id 4242", lambda: c.clear_request(EV_BREAKPOINT, 4242))
    tryit("Thread resume on running (not suspended) thread", lambda: c.thread_resume(main))
    c.clear_all_breakpoints()
    rid = c.set_request(EV_BREAKPOINT, SUSPEND_THREAD, [m_location(loc)])
    p = fire("err"); ev = c.wait_event(lambda e: e["kind"] == EV_BREAKPOINT, timeout=15)
    th = ev["thread"]; fr = c.frames(th); fid = fr[0][0]
    print(f"   frame ids at hit: {[f for f,_ in fr[:5]]} (thread {th}, main {main} same={th==main})")
    c.thread_suspend(th); print("   extra suspend -> suspendCount", c.suspend_count(th))
    c.thread_resume(th); print("   one resume -> suspendCount", c.suspend_count(th))
    c.clear_request(EV_BREAKPOINT, rid)
    c.thread_resume(th); time.sleep(0.5)
    print("   resumed -> suspendCount", c.suspend_count(th))
    tryit("StackFrame.ThisObject with stale frame id after resume", lambda: c.this_object(th, fid))
    c.thread_suspend(th)
    fr2 = c.frames(th, 0, 2); print(f"   re-suspended; new frame ids {[f for f,_ in fr2]} top={loc_str(c, fr2[0][1])}")
    tryit("StackFrame.ThisObject with pre-resume frame id while re-suspended", lambda: c.this_object(th, fid))
    c.thread_resume(th)
    p.wait(10)
finally:
    c.dispose(); s.close(); print("disposed; pid", adb("shell","pidof",PKG,check=False).strip())
