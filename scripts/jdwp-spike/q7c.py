"""Q7c: is the INTO-with-excludes escape caused by modifier order (Count before ClassExclude)?"""
import os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import jdwp
from jdwp import *
from spike import adb, loc_str, find_line_locations, SER, PKG
from q7b_lib import bp_hit
EX = ["kotlin.*","java.*","android.*","androidx.*"]
def step(c, th, mods, label, timeout=8):
    rid = c.set_request(EV_STEP, SUSPEND_THREAD, mods)
    t0=time.monotonic(); c.thread_resume(th)
    try:
        e = c.wait_event(lambda e: e["kind"] in (EV_STEP, EV_BREAKPOINT), timeout=timeout)
        print(f"   {label}: kind={e['kind']} {(time.monotonic()-t0)*1000:.0f}ms {loc_str(c, e['loc'])}"); ok=True
    except TimeoutError:
        print(f"   {label}: NO EVENT in {timeout}s (thread suspendCount={c.suspend_count(th)})"); ok=False
    try: c.clear_request(EV_STEP, rid)
    except JdwpError as ex: print(f"   {label}: clear err {ex.code}")
    return ok
pid = int(adb("shell","pidof",PKG).split()[0])
s = jdwp.open_adb_jdwp(SER, pid); jdwp.handshake(s); c = jdwp.Jdwp(s); c.id_sizes()
try:
    locs, _ = find_line_locations(c, 100); loc = locs[0][4]
    for label, build in [
        ("Step,ClassExclude*4,Count(1)", lambda th: [m_step(th,1,0)]+[m_class_exclude(x) for x in EX]+[m_count(1)]),
        ("Step,ClassExclude*4 (no Count)", lambda th: [m_step(th,1,0)]+[m_class_exclude(x) for x in EX]),
        ("Step,Count(1),ClassExclude*4", lambda th: [m_step(th,1,0), m_count(1)]+[m_class_exclude(x) for x in EX]),
    ]:
        th, p = bp_hit(c, loc, "x")
        ok = step(c, th, build(th), "INTO@100 " + label)
        if ok:
            step(c, th, build(th), "INTO again " + label)
            c.thread_resume(th)
        p.wait(10)
    # Count(1) on a Breakpoint modifier: does it behave (fires once)?
finally:
    c.dispose(); s.close(); print("disposed; pid", adb("shell","pidof",PKG,check=False).strip())
