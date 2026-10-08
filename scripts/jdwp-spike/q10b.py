import os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import jdwp
pid = int(sys.argv[1])
for label, opener in [("adb-service", lambda: jdwp.open_adb_jdwp("emulator-5554", pid))]:
    t0 = time.monotonic()
    try:
        s = opener()
        print(f"[{label}] ADB OKAY in {(time.monotonic()-t0)*1000:.0f}ms")
        try:
            jdwp.handshake(s, timeout=5); print(f"[{label}] handshake OK (!)")
            c = jdwp.Jdwp(s); print(c.version()); c.dispose()
        except Exception as e:
            print(f"[{label}] handshake failed: {type(e).__name__}: {e} after {(time.monotonic()-t0)*1000:.0f}ms")
        s.close()
    except jdwp.AdbFail as e:
        print(f"[{label}] ADB FAIL {e!r}")
