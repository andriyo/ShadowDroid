"""Q8: MethodEntry(ClassMatch sample.*, suspend NONE) event rate and app cost; baseline vs enabled."""
import os, sys, time, subprocess, collections, threading, re
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import jdwp
from jdwp import *
from spike import adb, SER, PKG, ACT

def cpu_ticks(pid):
    f = adb("shell", "cat", f"/proc/{pid}/stat").split(")")[-1].split()
    return int(f[11]) + int(f[12])   # utime + stime (fields 14,15)

def gfx(reset=False):
    out = adb("shell","dumpsys","gfxinfo",PKG,*(["reset"] if reset else []))
    if reset: return None
    g = lambda k: (re.search(k + r": *([^\n]+)", out) or [None, "?"])[1].strip()
    return {"frames": g("Total frames rendered"), "janky": g("Janky frames"), "p50": g("50th percentile"), "p90": g("90th percentile"), "p99": g("99th percentile")}

def interact(seconds):
    t0 = time.monotonic(); n = 0; lat = []
    while time.monotonic() - t0 < seconds:
        y1, y2 = (1700, 900) if n % 2 == 0 else (900, 1700)
        t = time.monotonic()
        adb("shell","input","swipe","540",str(y1),"540",str(y2),"150")
        lat.append((time.monotonic()-t)*1000)
        if n % 3 == 2:
            adb("shell","am","start","-n",ACT,"-f","0x20000000","--es","tag",f"me{n}")
        n += 1
    return n, sum(lat)/len(lat)

def window(c, label, seconds, active, me_req):
    if c: 
        while True:
            try: c.events.get_nowait()
            except Exception: break
    gfx(reset=True); ct0 = cpu_ticks(pid); t0 = time.monotonic()
    if active: n, swipe_ms = interact(seconds)
    else: time.sleep(seconds); n, swipe_ms = 0, 0
    dt = time.monotonic() - t0; ct1 = cpu_ticks(pid)
    evs = 0; meths = collections.Counter()
    if c:
        time.sleep(0.3)
        while True:
            try: _, batch = c.events.get_nowait()
            except Exception: break
            if not batch: continue
            for e in batch:
                if e["kind"] == EV_METHOD_ENTRY and e["req"] == me_req:
                    evs += 1; meths[(e["loc"][1], e["loc"][2])] += 1
    g = gfx()
    print(f"[{label}] {dt:.1f}s interactions={n} avg_swipe_cmd={swipe_ms:.0f}ms app_cpu={(ct1-ct0)/dt:.0f} ticks/s "
          f"events={evs} ({evs/dt:.0f}/s) gfx={g}")
    return meths

pid = int(adb("shell","pidof",PKG).split()[0])
adb("shell","am","start","-n",ACT,"-f","0x20000000"); time.sleep(1)
print("pid", pid)
window(None, "baseline-detached idle", 3, False, None)
window(None, "baseline-detached interact", 3, True, None)
s = jdwp.open_adb_jdwp(SER, pid); jdwp.handshake(s); c = jdwp.Jdwp(s); c.id_sizes()
try:
    window(c, "attached-no-requests idle", 3, False, None)
    window(c, "attached-no-requests interact", 3, True, None)
    t = time.monotonic()
    me = c.set_request(EV_METHOD_ENTRY, SUSPEND_NONE, [m_class_match("io.github.andriyo.shadowdroid.sample.*")])
    print(f"MethodEntry set id={me} in {(time.monotonic()-t)*1000:.0f}ms")
    window(c, "MethodEntry idle", 3, False, me)
    meths = window(c, "MethodEntry interact", 3, True, me)
    names = {}
    for (cls, mid), n in meths.most_common(8):
        sig = c.signature(cls); nm = next((m["name"] for m in c.methods(cls) if m["id"] == mid), "?")
        print(f"   {n:6d}  {sig.split('/')[-1]}{nm}")
    t = time.monotonic(); c.clear_request(EV_METHOD_ENTRY, me)
    print(f"MethodEntry cleared in {(time.monotonic()-t)*1000:.0f}ms")
    window(c, "after-clear interact", 3, True, None)
    # unfiltered MethodEntry for 1s idle, just to see the raw JVMTI firehose the filter sits on
    me2 = c.set_request(EV_METHOD_ENTRY, SUSPEND_NONE, [m_class_match("io.github.andriyo.shadowdroid.sample.*"), m_count(1000000)])
    c.clear_request(EV_METHOD_ENTRY, me2)
finally:
    c.dispose(); s.close()
    time.sleep(0.5)
    window(None, "after-dispose interact", 3, True, None)
    print("pid after:", adb("shell","pidof",PKG,check=False).strip())
