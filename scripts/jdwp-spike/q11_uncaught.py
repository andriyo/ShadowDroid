"""Q11: where does ART report the catch location of a main-thread crash?"""
import os, sys, time, subprocess
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import jdwp
from jdwp import *
from spike import adb, loc_str, SER, PKG
x, y = sys.argv[1], sys.argv[2]
pid = int(adb("shell","pidof",PKG).split()[0])
s = jdwp.open_adb_jdwp(SER, pid); jdwp.handshake(s); c = jdwp.Jdwp(s); c.id_sizes()
try:
    rt = c.classes_by_signature("Ljava/lang/RuntimeException;")[0]["id"]
    def exc_only(ref, caught, uncaught): return (8, lambda w: w.ref(ref).bool(caught).bool(uncaught))
    r_unc = c.set_request(EV_EXCEPTION, SUSPEND_ALL, [exc_only(rt, False, True)])
    r_all = c.set_request(EV_EXCEPTION, SUSPEND_ALL, [exc_only(rt, True, True)])
    print("requests uncaught-only", r_unc, "caught+uncaught", r_all)
    adb("shell","input","tap",x,y)
    e = c.wait_event(lambda e: e["kind"] == EV_EXCEPTION, timeout=10)
    print("event req", e["req"], "composite_size", e.get("composite_size"))
    print("  throw at", loc_str(c, e["loc"]))
    tag, cls, meth, idx = e["catch"]
    print("  catch location raw", e["catch"], "->", loc_str(c, e["catch"]) if cls else "NONE (uncaught)")
    _, ref = c.obj_type(e["exc"][1]); print("  exception type", c.signature(ref))
    other = [x for x in c._backlog if x["kind"] == EV_EXCEPTION]
    print("  other exception events in composite:", [(o["req"]) for o in other])
finally:
    c.dispose(); s.close()
time.sleep(1); print("pid after:", adb("shell","pidof",PKG,check=False).strip() or "dead")
