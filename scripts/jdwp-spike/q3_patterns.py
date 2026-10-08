"""Q3 deep-dive: which SourceNameMatch patterns match which classes on ART (cold start)."""
import os, sys, time, subprocess, collections
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import jdwp
from jdwp import *
from spike import adb, SER, PKG, ACT
PATS = ["MainActivity.kt", "*MainActivity.kt", "MainActivity*", "*.kt", "*.java", "ComponentActivity.kt",
        "*Activity.kt", "LabTheme.kt", "LabThemeKt", "*Kt", "io.github.andriyo.shadowdroid.sample.*", "*sample*"]
adb("shell","am","force-stop",PKG); adb("shell","am","set-debug-app","-w",PKG)
c = s = None
try:
    subprocess.Popen(["adb","-s",SER,"shell","am","start","-n",ACT], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    t0=time.monotonic(); pid=None
    while time.monotonic()-t0 < 20 and not pid:
        out = adb("shell","pidof",PKG,check=False).split()
        if out and int(out[0]) in jdwp.jdwp_pids(SER, 0.3): pid=int(out[0])
        else: time.sleep(0.1)
    s = jdwp.open_adb_jdwp(SER, pid); jdwp.handshake(s); c = jdwp.Jdwp(s); c.id_sizes()
    reqs = {}
    for p in PATS:
        try: reqs[c.set_request(EV_CLASS_PREPARE, SUSPEND_NONE, [m_source_name(p)])] = p
        except JdwpError as e: print(f"{p!r}: err {e.code}")
    allreq = c.set_request(EV_CLASS_PREPARE, SUSPEND_NONE, [])
    hits = collections.defaultdict(set); sigs = {}
    deadline = time.monotonic()+10
    while time.monotonic() < deadline:
        try: e = c.wait_event(lambda e: e["kind"]==EV_CLASS_PREPARE, timeout=deadline-time.monotonic())
        except TimeoutError: break
        sigs[e["sig"]] = e["type"]
        hits[e["sig"]].add(e["req"])
    print(f"total prepared after attach: {len(sigs)}")
    c.suspend_vm()
    def sf(sig):
        try: return c.source_file(sigs[sig])
        except JdwpError as e: return f"<err {e.code}>"
    for rid, p in reqs.items():
        m = [sg for sg, r in hits.items() if rid in r]
        print(f"== {p!r}: {len(m)} matches")
        for sg in m[:6]: print(f"     {sg}  SourceFile={sf(sg)}")
    print("== sample-package classes and which patterns matched them (first 15)")
    for sg in [x for x in sigs if "andriyo/shadowdroid/sample" in x][:15]:
        print(f"   {sg} SourceFile={sf(sg)} matched={[reqs[r] for r in hits[sg] if r in reqs]}")
    print("== *.kt matches by package root:", collections.Counter("/".join(sg[1:].split("/")[:2]) for sg,r in hits.items() if any(reqs.get(x)=="*.kt" for x in r)).most_common(8))
    print("== all prepared by package root:", collections.Counter("/".join(sg[1:].split("/")[:2]) for sg in sigs).most_common(8))
    c.resume_vm()
finally:
    if c:
        try: c.dispose()
        except Exception as e: print("dispose err", e)
    if s: s.close()
    adb("shell","am","clear-debug-app"); time.sleep(1)
    print("pid after:", adb("shell","pidof",PKG,check=False).strip())
