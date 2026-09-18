#!/usr/bin/env python3
"""Reproduce and verify agent reliability on a dedicated, already-booted AVD.

Build/install the current server and sample APK first. This suite changes proxy
settings and drives the sample app. It refuses other AVDs and restores the proxy
in finally. --emulator-pid explicitly enables two real 20-second ADB timeouts.
Every subprocess result, including failures, is saved without projection.
"""
import argparse
import concurrent.futures
import fcntl
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import time
import threading


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", type=Path, required=True)
    parser.add_argument("--serial", default="emulator-5580")
    parser.add_argument("--avd", default="ShadowDroid_Reliability_API36")
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--emulator-pid", type=int)
    args = parser.parse_args()
    args.cli = args.cli.resolve()
    args.evidence.mkdir(parents=True, exist_ok=True)
    package = "io.github.andriyo.shadowdroid.sample"

    def adb(*words):
        return subprocess.check_output(["adb", "-s", args.serial, *words], text=True).strip()

    assert args.avd.startswith("ShadowDroid_Reliability_"), "use a dedicated reliability AVD"
    assert adb("emu", "avd", "name").splitlines()[0] == args.avd
    original_proxy = adb("shell", "settings get global http_proxy")
    assert original_proxy in ("null", ":0", ""), "start with an inactive proxy"
    if args.emulator_pid:
        process = subprocess.check_output(["ps", "-p", str(args.emulator_pid), "-o", "args="], text=True)
        assert "qemu-system" in process and f"-avd {args.avd} " in process

    def record(label, argv, expected=0, cwd=None):
        started = time.monotonic()
        child = subprocess.run([str(x) for x in argv], capture_output=True, text=True, cwd=cwd, timeout=90)
        result = dict(argv=[str(x) for x in argv], exit=child.returncode,
                      seconds=time.monotonic() - started, stdout=child.stdout, stderr=child.stderr)
        (args.evidence / f"{label}.json").write_text(json.dumps(result, indent=2))
        if expected is not None:
            assert child.returncode == expected, f"{label}: {result}"
        print(f"{label}: exit={child.returncode} elapsed={result['seconds']:.2f}s", flush=True)
        return child

    def sd(label, *words, expected=0, cwd=None, target=False):
        scope = ["--target", "mobile"] if target else ["-d", args.serial]
        child = record(label, [args.cli, "--quiet", *scope, *words], expected, cwd)
        return json.loads(child.stdout)

    initial = sd("initial-status", "net", "status", "--json")
    assert initial["complete"] and initial["running"] is False
    assert initial["http_proxy_state"] == "known"
    started_proxy = False
    try:
        # Exact report case: absent daemon/snapshot and an unowned local proxy.
        adb("shell", "settings put global http_proxy localhost:9090")
        stopped = sd("dangling-proxy-stop", "net", "stop")
        assert stopped["already_stopped"] and not stopped["cleanup_complete"]
        assert stopped["proxy_restoration"] == "unresolved" and stopped["warnings"]
        assert stopped["connectivity_restored"] is None
        assert stopped["application_connectivity"] == "not_checked"
        assert adb("shell", "settings get global http_proxy") == "localhost:9090"

        # A saved custom proxy must be restored, not deleted or called unowned.
        adb("shell", "settings put global http_proxy proxy.example:3128")
        sd("custom-proxy-start", "net", "start", "--port", "19090")
        started_proxy = True
        assert sd("active-status", "net", "status", "--json")["pointed_at_proxy"] is True
        stopped = sd("custom-proxy-stop", "net", "stop")
        started_proxy = False
        assert stopped["proxy_restoration"] == "restored" and stopped["cleanup_complete"]
        assert adb("shell", "settings get global http_proxy") == "proxy.example:3128"
        adb("shell", "settings delete global http_proxy")

        # Another tool changes both settings while our proxy is running.
        # Keep those values and the recovery snapshot until ownership is clear.
        sd("changed-wiring-start", "net", "start", "--port", "19090")
        started_proxy = True
        adb("shell", "settings put global http_proxy other.example:3128")
        adb("reverse", "tcp:19090", "tcp:19093")
        try:
            changed = sd("changed-wiring-stop", "net", "stop")
            assert not changed["cleanup_complete"]
            assert changed["proxy_restoration"] == "unresolved"
            assert changed["adb_reverse_restoration"] == "unresolved"
            assert adb("shell", "settings get global http_proxy") == "other.example:3128"
            assert "tcp:19090 tcp:19093" in adb("reverse", "--list")
        finally:
            adb("shell", "settings delete global http_proxy")
            adb("reverse", "--remove", "tcp:19090")
            recovered = sd("changed-wiring-recovery", "net", "stop")
            started_proxy = False
        assert recovered["proxy_restoration"] == "restored" and recovered["cleanup_complete"]

        missing = record("missing-device", [args.cli, "-d", "reliability-missing", "net", "status"], 1)
        error = json.loads(missing.stdout)
        assert error["code"] == "net_status_incomplete"
        assert error["detail"]["http_proxy_state"] == "unknown"
        assert error["detail"]["http_proxy_matches"] is None
        if args.emulator_pid:
            for operation in ("status", "stop"):
                os.kill(args.emulator_pid, signal.SIGSTOP)
                try:
                    error = sd(f"adb-timeout-{operation}", "net", operation, expected=1)
                    if operation == "status":
                        assert error["code"] == "net_status_incomplete"
                        assert error["detail"]["http_proxy_error"]["code"] == "adb_timeout"
                        assert error["detail"]["http_proxy_state"] == "unknown"
                    else:
                        assert error["code"] == "adb_timeout"
                        assert error["detail"]["phase"] == "observe_initial_state"
                        assert error["detail"]["cause"]["detail"]["timeout_ms"] == 20000
                        assert error["detail"]["elapsed_ms"] >= 20000
                finally:
                    os.kill(args.emulator_pid, signal.SIGCONT)

        # Bound target resolution contention while retaining project isolation.
        project = args.evidence.resolve() / "target-project"
        (project / ".shadowdroid").mkdir(parents=True, exist_ok=True)
        config = json.dumps({"targets": {"mobile": {"avd": args.avd, "start": "never"}}})
        (project / ".shadowdroid/config.json").write_text(config)
        key = "avd:" + args.avd
        prefix = re.sub(r"[^A-Za-z0-9_.-]", "_", key[:40])
        component = prefix + "-" + hashlib.sha256(key.encode()).hexdigest()[:16]
        lock = Path.home() / ".shadowdroid/locks" / f"device-{component}.lock"
        lock.parent.mkdir(parents=True, exist_ok=True)
        with lock.open("a+") as owner, concurrent.futures.ThreadPoolExecutor() as pool:
            fcntl.flock(owner, fcntl.LOCK_EX)
            # Do not rewrite metadata of a real lock owner. We own this lock.
            owner.seek(0); owner.truncate(); owner.write(str(os.getpid())); owner.flush()
            error = sd("fail-fast-lock", "--lock-timeout-ms", "0", "net", "status", expected=1, cwd=project, target=True)
            assert error["detail"]["lock_scope"] == "target_resolution"
            calls = [pool.submit(sd, "parallel-status", "net", "status", cwd=project, target=True),
                     pool.submit(sd, "parallel-info", "app", "info", package, cwd=project, target=True)]
            time.sleep(0.35)
            fcntl.flock(owner, fcntl.LOCK_UN)
            for call in calls:
                assert call.result()["ok"]
        other = args.evidence.resolve() / "other-project"
        (other / ".shadowdroid").mkdir(parents=True, exist_ok=True)
        (other / ".shadowdroid/config.json").write_text(config)
        error = sd("ownership-protected", "net", "status", expected=1, cwd=other, target=True)
        assert error["code"] == "target_avd_owned_by_other_project"

        sd("devices-json", "devices", "--json")
        sd("app-info-json", "app", "info", package, "--json")
        sd("reset-sample", "app", "stop", package)
        sd("app-launch-alias", "app", "launch", package, "--activity", ".MainActivity", "--json")
        sd("app-wait-front", "app", "wait", package, "--front")
        before = sd("before-ui", "ui", "dump")
        assert before["snapshot_state"] == "consistent"
        sd("open-lab", "ui", "tap", "--rid", "nav_lab", "--expect-text", "Challenge catalog")
        rejected = sd("stale-screen-guard", "ui", "tap", "--rid", "nav_signals", "--if-screen", before["screen_hash"], expected=1)
        assert rejected["code"] == "screen_changed"
        rejected = sd("stale-interaction-guard", "ui", "tap", "--rid", "nav_signals", "--if-interaction", before["interaction_hash"], expected=1)
        assert rejected["code"] == "interaction_changed"
        old_handle = next(e["handle"] for e in before["elements"] if e.get("rid") == "nav_signals")
        rejected = sd("stale-element-handle", "ui", "tap", "--handle", old_handle, expected=1)
        assert rejected["code"] == "stale_element"
        fresh = sd("fresh-guard-observation", "ui", "dump")
        assert fresh["snapshot_state"] == "consistent"
        handle = next(e["handle"] for e in fresh["elements"] if e.get("rid") == "nav_signals")
        sd("guarded-recovery", "ui", "tap", "--handle", handle, "--if-screen", fresh["screen_hash"], "--if-interaction", fresh["interaction_hash"], "--expect-rid", "url_input")
        sd("ui-type-alias", "ui", "type", "http://reliability.invalid/prepared", "--rid", "url_input", "--clear", "--json")
        sd("hide-keyboard", "ui", "hide-keyboard")

        # The app's actual HTTP stack receives a preinstalled, narrowly matched
        # response. No upstream request or human response to a live hold needed.
        sd("prepared-rule-start", "net", "start", "--port", "19090")
        started_proxy = True
        rule = sd("prepared-rule", "net", "rule", "add", "respond", "--host", "reliability.invalid", "--path", "/prepared", "--method", "GET", "--status", "200", "--header", "content-type=text/plain", "--body", "reliability-prepared-response")
        rule_id = rule.get("id") or rule.get("rule", {}).get("id")
        assert rule_id, rule
        try:
            sd("prepared-request", "ui", "tap", "--rid", "https_get_button", "--expect-text", "reliability-prepared-response", "--timeout-ms", "10000")
        finally:
            sd("remove-prepared-rule", "net", "rule", "rm", str(rule_id))
        flows = record("prepared-flows", [args.cli, "-d", args.serial, "net", "log", "--rule-id", rule_id, "--limit", "5"])
        assert any(item.get("upstream_bypassed") and rule_id in item.get("rule_ids", [])
                   for item in map(json.loads, flows.stdout.splitlines()))
        stopped = sd("final-stop", "net", "stop")
        started_proxy = False
        assert stopped["proxy_restoration"] == "restored" and stopped["cleanup_complete"]
        assert sd("final-status", "net", "status")["pointed_at_proxy"] is False

        # Verify application HTTP after stop through the real Android stack,
        # independently of ShadowDroid's deliberately limited ping/DNS result.
        class Canary(BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(200)
                self.send_header("Content-Type", "text/plain")
                self.end_headers()
                self.wfile.write(b"reliability-direct-http-restored")

            def log_message(self, *_):
                pass

        with ThreadingHTTPServer(("127.0.0.1", 0), Canary) as server:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                url = f"http://10.0.2.2:{server.server_port}/after-stop"
                sd("direct-http-url", "ui", "text", url, "--rid", "url_input", "--clear")
                sd("direct-http-keyboard", "ui", "hide-keyboard")
                sd("direct-http-after-stop", "ui", "tap", "--rid", "https_get_button", "--expect-text", "reliability-direct-http-restored", "--timeout-ms", "10000")
            finally:
                server.shutdown()
                thread.join()

        # Exercise the literal generated Python recipe, preserving error JSON.
        skill = record("generated-skill", [args.cli, "skill", "codex"]).stdout
        snippet = skill.split("```python\n", 1)[1].split("```", 1)[0]
        wrapper = args.evidence / "checked.py"
        wrapper.write_text(snippet + "\nchecked_run(sys.argv[1:])\n")
        child = record("checked-failure", ["python3", wrapper, args.cli, "not-a-command"], 2)
        assert json.loads(child.stdout)["ok"] is False
        record("checked-success", ["python3", wrapper, args.cli, "devices", "--json"])
        pipeline = '\"$1\" not-a-command | python3 -m json.tool'
        masked = record("masked-pipeline-repro", ["bash", "-c", pipeline, "bash", args.cli])
        assert json.loads(masked.stdout)["ok"] is False
        record("checked-pipeline", ["bash", "-c", "set -o pipefail; " + pipeline, "bash", args.cli], 2)
        dependent = record("checked-dependent-batch", ["bash", "-c", '\"$1\" not-a-command && \"$1\" --version', "bash", args.cli], 2)
        assert json.loads(dependent.stdout)["ok"] is False
        for path in ("ui tap", "ui text", "app start", "devices"):
            full = record(f"full-{path.replace(' ', '-')}", [args.cli, "commands", "--describe", path, "--json"])
            lean = record(f"lean-{path.replace(' ', '-')}", [args.cli, "commands", "--describe", path, "--json", "--compact"])
            assert len(lean.stdout) < len(full.stdout) * .65 and len(lean.stdout) < 12 * 1024
        (args.evidence / "summary.json").write_text(json.dumps({"ok": True, "suite": "reliability", "serial": args.serial, "real_adb_timeouts": bool(args.emulator_pid)}, indent=2))
    finally:
        if started_proxy:
            sd("cleanup-stop", "net", "stop", expected=None)
        command = "settings delete global http_proxy" if original_proxy in ("null", "") else "settings put global http_proxy :0"
        adb("shell", command)
        assert adb("shell", "settings get global http_proxy") in (original_proxy, "null")


if __name__ == "__main__":
    main()
