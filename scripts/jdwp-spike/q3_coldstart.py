"""Q3 + deferred binding: cold start under `am set-debug-app -w`, ClassPrepare with SourceNameMatch vs ClassMatch."""
import os, sys, time, subprocess, collections
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import jdwp
from jdwp import *
from spike import adb, loc_str, SER, PKG, ACT, describe_value

adb("shell","am","force-stop",PKG)
adb("shell","am","set-debug-app","-w",PKG)
c = s = None
try:
    t0 = time.monotonic()
    subprocess.Popen(["adb","-s",SER,"shell","am","start","-n",ACT], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    pid = None
    while time.monotonic()-t0 < 20:
        out = adb("shell","pidof",PKG,check=False).split()
        if out:
            cand = int(out[0])
            if cand in jdwp.jdwp_pids(SER, 0.3): pid = cand; break
        time.sleep(0.1)
    print(f"pid {pid} visible in jdwp after {(time.monotonic()-t0)*1000:.0f}ms")
    s = jdwp.open_adb_jdwp(SER, pid); jdwp.handshake(s); c = jdwp.Jdwp(s); c.id_sizes()
    t_att = time.monotonic()
    reqs = {}
    for label, kind, pol, mods in [
        ("SNM MainActivity.kt", EV_CLASS_PREPARE, SUSPEND_NONE, [m_source_name("MainActivity.kt")]),
        ("SNM *Activity.kt", EV_CLASS_PREPARE, SUSPEND_NONE, [m_source_name("*Activity.kt")]),
        ("ClassMatch sample.*", EV_CLASS_PREPARE, SUSPEND_NONE, [m_class_match("io.github.andriyo.shadowdroid.sample.*")]),
        ("ClassMatch MainActivity exact (suspend)", EV_CLASS_PREPARE, SUSPEND_THREAD, [m_class_match("io.github.andriyo.shadowdroid.sample.MainActivity")]),
    ]:
        try:
            reqs[c.set_request(kind, pol, mods)] = label
        except JdwpError as e:
            print(f"  {label}: JDWP error {e.code}")
    print("requests:", reqs)
    threads = c.all_threads()
    main = next((t for t in threads if c.thread_name(t) == "main"), None)
    print(f"threads={len(threads)} main status={c.thread_status(main)} suspendCount={c.suspend_count(main)}")
    fr = c.frames(main, 0, 4) if c.thread_status(main)[1] else []
    if not fr:
        c.thread_suspend(main); fr = c.frames(main, 0, 6); 
        for i,(f,l) in enumerate(fr): print(f"   main #{i} {loc_str(c,l)}")
        c.thread_resume(main)
    by_req = collections.defaultdict(list)
    bp_req = None; bp_hits = []
    deadline = time.monotonic() + 12
    while time.monotonic() < deadline:
        try: e = c.wait_event(timeout=deadline - time.monotonic())
        except TimeoutError: break
        if e["kind"] == EV_CLASS_PREPARE:
            by_req[e["req"]].append(e["sig"])
            if reqs.get(e["req"], "").startswith("ClassMatch MainActivity exact"):
                print(f"  MainActivity prepared {(time.monotonic()-t_att)*1000:.0f}ms after attach; binding deferred bp at line 61 (thread suspended)")
                ref = e["type"]
                for m in c.methods(ref):
                    if m["name"] == "onCreate":
                        _,_,lt = c.line_table(ref, m["id"])
                        ci = min(ci for ci,ln in lt if ln == 61)
                        bp_req = c.set_request(EV_BREAKPOINT, SUSPEND_THREAD, [m_location((e["tag"], ref, m["id"], ci))])
                c.thread_resume(e["thread"])
        elif e["kind"] == EV_BREAKPOINT:
            bp_hits.append(e)
            print(f"  deferred BREAKPOINT hit {(time.monotonic()-t_att)*1000:.0f}ms after attach at {loc_str(c, e['loc'])}")
            fid = c.frames(e["thread"],0,1)[0][0]
            print("    this:", describe_value(c, c.this_object(e["thread"], fid)))
            c.thread_resume(e["thread"])
    for rid, label in reqs.items():
        sigs = by_req.get(rid, [])
        print(f"== req {rid} {label}: {len(sigs)} ClassPrepare events")
        for sg in sigs[:12]: print("     ", sg)
        if len(sigs) > 12: print("      ...")
    # did SNM filter correctly? any SNM event outside expected?
    snm = [r for r,l in reqs.items() if l.startswith("SNM MainActivity")]
    if snm:
        bad = [sg for sg in by_req.get(snm[0], []) if "MainActivity" not in sg]
        print("SNM MainActivity.kt non-MainActivity sigs:", bad[:10])
finally:
    if c:
        try: c.dispose(); print("disposed")
        except Exception as e: print("dispose err", e)
    if s: s.close()
    adb("shell","am","clear-debug-app")
    time.sleep(1)
    print("pid after:", adb("shell","pidof",PKG,check=False).strip())
    print(adb("shell","dumpsys activity activities | grep topResumed", check=False).strip())
