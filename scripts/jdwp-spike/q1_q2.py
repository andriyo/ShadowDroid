import os, sys, json, time, subprocess
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import jdwp
SER = "emulator-5554"; PKG = "io.github.andriyo.shadowdroid.sample"
print("adb host:version", jdwp.adb_host_query("host:version"))
t=time.monotonic(); pids = jdwp.jdwp_pids(SER, 1.0); print("jdwp pids (1s read):", pids)
pid = int(subprocess.check_output(["adb","-s",SER,"shell","pidof",PKG]).split()[0])
print("app pid", pid, "in jdwp list:", pid in pids)

def session(sock, label):
    t0=time.monotonic(); jdwp.handshake(sock); t_hs=(time.monotonic()-t0)*1000
    c = jdwp.Jdwp(sock)
    t1=time.monotonic(); sz=c.id_sizes(); t_ids=(time.monotonic()-t1)*1000
    rtts=[]
    for _ in range(20):
        t2=time.monotonic(); c.version(); rtts.append((time.monotonic()-t2)*1000)
    v=c.version(); caps=c.capabilities_new()
    print(f"[{label}] handshake {t_hs:.1f}ms idsizes {t_ids:.1f}ms version rtt min/med/max {min(rtts):.2f}/{sorted(rtts)[10]:.2f}/{max(rtts):.2f}ms")
    print(f"[{label}] IDSizes", sz); print(f"[{label}] Version", json.dumps(v))
    time.sleep(0.3)
    print(f"[{label}] unsolicited VM commands:", [(cs,cm,b[:16].hex()) for cs,cm,b in c.vm_cmds][:10])
    return c, caps

# A: adb device service
t0=time.monotonic(); s=jdwp.open_adb_jdwp(SER,pid); t_open=(time.monotonic()-t0)*1000
print(f"[adb-service] open host:transport+jdwp:{pid} {t_open:.1f}ms")
c,caps=session(s,"adb-service")
print("CAPS", json.dumps({k:v for k,v in caps.items() if not k.startswith('reserved')}, indent=1))
print("reserved true:", [k for k,v in caps.items() if k.startswith('reserved') and v])
c.dispose(); s.close(); time.sleep(0.5)

# B: adb forward tcp:0
port = int(subprocess.check_output(["adb","-s",SER,"forward","tcp:0",f"jdwp:{pid}"]).strip())
print("forward port", port)
try:
    t0=time.monotonic(); s=jdwp.open_tcp(port); t_open=(time.monotonic()-t0)*1000
    print(f"[tcp-forward] connect {t_open:.1f}ms")
    c,_=session(s,"tcp-forward"); c.dispose(); s.close()
finally:
    subprocess.call(["adb","-s",SER,"forward","--remove",f"tcp:{port}"])
