"""Q7b: step INTO from line 100 (super.onNewIntent) - excludes vs none; plus static var-table survey."""
import os, sys, time, subprocess
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import jdwp
from jdwp import *
from spike import adb, fire, loc_str, find_line_locations, SER, PKG, PREFIX, describe_value

def bp_hit(c, loc, tag):
    rid = c.set_request(EV_BREAKPOINT, SUSPEND_THREAD, [m_location(loc)])
    p = fire(tag)
    ev = c.wait_event(lambda e: e["kind"] == EV_BREAKPOINT, timeout=15)
    c.clear_request(EV_BREAKPOINT, rid)
    return ev["thread"], p

def step(c, thread, depth, excludes=(), timeout=20, extra=()):
    mods = [m_step(thread, 1, depth), m_count(1)] + [m_class_exclude(x) for x in excludes] + list(extra)
    rid = c.set_request(EV_STEP, SUSPEND_THREAD, mods)
    t0 = time.monotonic(); c.thread_resume(thread)
    try:
        e = c.wait_event(lambda e: e["kind"] in (EV_STEP, EV_BREAKPOINT), timeout=timeout)
        print(f"   depth={depth} excl={list(excludes)} -> kind={e['kind']} {(time.monotonic()-t0)*1000:.0f}ms {loc_str(c, e['loc'])}")
        r = e
    except TimeoutError:
        print(f"   depth={depth} excl={list(excludes)} -> NO STEP EVENT in {timeout}s; thread status={c.thread_status(thread)} suspendCount={c.suspend_count(thread)}")
        c.thread_suspend(thread)
        for i,(fid,loc) in enumerate(c.frames(thread,0,6)):
            print(f"      #{i} {loc_str(c, loc)}")
        r = None
    try: c.clear_request(EV_STEP, rid)
    except JdwpError as ex: print("   clear err", ex.code)
    return r

pid = int(adb("shell","pidof",PKG).split()[0])
s = jdwp.open_adb_jdwp(SER, pid); jdwp.handshake(s); c = jdwp.Jdwp(s); c.id_sizes()
try:
    locs, cands = find_line_locations(c, 100)
    loc = locs[0][4]
    print("== A: INTO with excludes from line 100")
    th, p = bp_hit(c, loc, "a")
    r = step(c, th, 0, ["kotlin.*","java.*","android.*","androidx.*"])
    c.thread_resume(th); p.wait(10)
    print("== B: INTO, no excludes, from line 100 (x4)")
    th, p = bp_hit(c, loc, "b")
    for _ in range(4):
        if not step(c, th, 0): break
    c.thread_resume(th); p.wait(10)
    print("== C: INTO with only androidx.* excluded")
    th, p = bp_hit(c, loc, "c")
    step(c, th, 0, ["androidx.*"])
    c.thread_resume(th); p.wait(10)
    print("== D: INTO with only android.* excluded")
    th, p = bp_hit(c, loc, "d")
    step(c, th, 0, ["android.*"])
    c.thread_resume(th); p.wait(10)
    print("== E: OVER from line 100 then INTO with all excludes at 102")
    th, p = bp_hit(c, loc, "e")
    step(c, th, 1); step(c, th, 1)
    step(c, th, 0, ["kotlin.*","java.*","android.*","androidx.*"])
    c.thread_resume(th); p.wait(10)

    print("== SourceFile ABSENT_INFORMATION classes under package")
    for k in c.all_classes_generic():
        if k["sig"].startswith(PREFIX):
            try: c.source_file(k["id"])
            except JdwpError as e: print(f"   {k['sig']} err {e.code}")
    print("== Kotlin synthetic locals survey (MainActivity* methods)")
    seen = {}
    for k in cands:
        for m in c.methods(k["id"]):
            try: _, sl = c.variable_table(k["id"], m["id"])
            except JdwpError as e: seen.setdefault(f"<err {e.code}>", m["name"]); continue
            for v in sl:
                if "$" in v["name"] or v["name"].startswith("<"):
                    seen.setdefault(v["name"], f"{k['sig'].split('/')[-1]}.{m['name']}")
    for n, where in list(seen.items())[:40]: print(f"   {n!r} in {where}")
    print("== SourceDebugExtension (SMAP) on MainActivity")
    mc = next(k for k in cands if k["sig"].endswith("/MainActivity;"))
    try: print("  ", c.source_debug_extension(mc["id"])[:600].replace("\n"," | "))
    except JdwpError as e: print("   err", e.code)
finally:
    c.dispose(); s.close(); print("disposed; pid alive:", adb("shell","pidof",PKG,check=False).strip())
