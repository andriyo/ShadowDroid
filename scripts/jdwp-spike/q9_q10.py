"""Q9 clean detach + Studio visibility; Q10 exclusivity over TCP forward and vs a Studio-held pid."""
import os, sys, time, subprocess, json
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import jdwp
from jdwp import *
from spike import adb, SER, PKG, ACT

def clients(pid):
    out = subprocess.run(["shadowdroid","debug","clients"], capture_output=True, text=True).stdout
    try:
        for cl in json.loads(out)["clients"]:
            if cl["pid"] == pid: return {k: cl[k] for k in ("pid","process","debugger_attached","debugger_status")}
    except Exception as e: return f"parse err {e}: {out[:200]}"
    return "pid not listed"

def responsive(tag):
    adb("logcat","-c")
    t = time.monotonic(); adb("shell","am","start","-n",ACT,"-f","0x20000000","--es","tag",tag)
    for _ in range(50):
        if "New intent" in adb("logcat","-d","-s","ShadowLab:I","*:S", check=False) or "New intent" in adb("logcat","-d", check=False)[-20000:]:
            return f"onNewIntent ran {(time.monotonic()-t)*1000:.0f}ms after am start"
        time.sleep(0.1)
    return "NO onNewIntent log within 5s"

pid = int(adb("shell","pidof",PKG).split()[0]); print("pid", pid)
print("clients before attach:", clients(pid))
s = jdwp.open_adb_jdwp(SER, pid); jdwp.handshake(s); c = jdwp.Jdwp(s); c.id_sizes()
try:
    time.sleep(1.0)
    print("clients while attached (raw jdwp):", clients(pid))
    # Q10 over TCP forward
    port = int(adb("forward","tcp:0",f"jdwp:{pid}").strip())
    try:
        t0 = time.monotonic(); s2 = jdwp.open_tcp(port)
        try:
            jdwp.handshake(s2, timeout=4); print("Q10 tcp-forward second handshake: SUCCEEDED")
        except Exception as e:
            print(f"Q10 tcp-forward second handshake: {type(e).__name__}: {e} after {(time.monotonic()-t0)*1000:.0f}ms")
        s2.close()
    finally:
        adb("forward","--remove",f"tcp:{port}")
    # Q10 via adb service, repeated, and check first conn survives
    for i in range(3):
        t0 = time.monotonic()
        try:
            s2 = jdwp.open_adb_jdwp(SER, pid)
            try: jdwp.handshake(s2, timeout=4); r = "handshake OK"
            except Exception as e: r = f"{type(e).__name__}: {e}"
            s2.close()
        except jdwp.AdbFail as e: r = f"ADB FAIL {e}"
        print(f"Q10 adb-service attempt {i}: {r} ({(time.monotonic()-t0)*1000:.0f}ms)")
    print("first connection alive:", c.version()["vmName"])
    # an EventRequest to prove Dispose clears it: breakpoint-free MethodEntry would be noisy; use ClassPrepare
    c.set_request(EV_CLASS_PREPARE, SUSPEND_ALL, [m_class_match("io.github.andriyo.shadowdroid.sample.*")])
    c.suspend_vm(); print("VM suspended (count 1) before Dispose")
finally:
    t = time.monotonic(); c.dispose(); print(f"Dispose OK in {(time.monotonic()-t)*1000:.0f}ms")
    time.sleep(0.2); print("reader saw EOF after dispose:", c.closed, c.eof_reason)
    s.close()
time.sleep(1.0)
print("pid alive:", adb("shell","pidof",PKG,check=False).strip())
print("responsive:", responsive("q9"))
print("clients after dispose:", clients(pid))
# reattach immediately works?
s = jdwp.open_adb_jdwp(SER, pid); jdwp.handshake(s); c = jdwp.Jdwp(s); c.id_sizes()
print("re-attach after dispose OK:", c.version()["vmName"]); c.dispose(); s.close()
