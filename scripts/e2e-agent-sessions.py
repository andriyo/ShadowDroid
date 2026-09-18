#!/usr/bin/env python3
"""Real-device concurrency contracts. Requires a dedicated, rebootable emulator.

The caller explicitly selects a device and optional independent peer. This suite
reboots the selected device to prove recovery from an interrupted CLI operation.
Every invocation and response is retained; no default device is guessed.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--device", required=True)
    parser.add_argument("--peer")
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--cli", type=Path, default=Path(__file__).resolve().parents[1] / "cli/target/debug/shadowdroid")
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=False)
    authority = args.out / "authority"
    env = dict(os.environ, SHADOWDROID_QUIET="1")
    env.pop("SHADOWDROID_SESSION", None)
    sequence = 0

    def command(*words, token=None, serial=None, root=None):
        prefix = [str(args.cli.resolve()), "--device", serial or args.device,
                  "--authority-dir", str(root or authority), "--lock-timeout-ms", "0"]
        if token:
            prefix += ["--session", token]
        return prefix + list(words)

    def call(*words, token=None, serial=None, error=None, root=None):
        nonlocal sequence
        argv = command(*words, token=token, serial=serial, root=root)
        result = subprocess.run(argv, env=env, text=True, capture_output=True, timeout=90)
        sequence += 1
        evidence = {"argv": argv, "code": result.returncode, "stdout": result.stdout, "stderr": result.stderr}
        (args.out / f"{sequence:03d}.json").write_text(json.dumps(evidence, indent=2))
        lines = result.stdout.strip().splitlines()
        assert len(lines) == 1, evidence
        value = json.loads(lines[0])
        if error:
            assert result.returncode != 0 and value["code"] == error, evidence
        else:
            assert result.returncode == 0, evidence
        return value

    token = call("session", "open", "--agent", "driver-a")["session"]
    peer_token = None
    try:
        call("session", "open", "--agent", "driver-b", error="device_reserved")
        call("ui", "key", "HOME", error="device_reserved")
        call("session", "open", "--agent", "private-root", root=args.out / "private-authority", error="authority_conflict")
        call("ui", "dump", token=token, error="session_observation_required")
        call("session", "observe", "--subscription", "driver-a", token=token)
        call("connect", token=token)
        call("ui", "dump", token=token)

        # Long command retains driver ownership; independent devices remain free.
        worker = subprocess.Popen(command("device", "shell", "sleep 4", token=token), env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        time.sleep(0.5)
        call("ui", "key", "HOME", token=token, error="device_operation_busy")
        call("session", "observe", "--subscription", "passive-reader")
        if args.peer:
            start = time.monotonic()
            peer_token = call("session", "open", "--agent", "peer-driver", serial=args.peer)["session"]
            call("session", "observe", "--subscription", "peer", serial=args.peer, token=peer_token)
            assert time.monotonic() - start < 4, "independent device was blocked by first device"
            call("session", "close", serial=args.peer, token=peer_token)
            peer_token = None
        stdout, stderr = worker.communicate(timeout=20)
        assert worker.returncode == 0, (stdout, stderr)
        (args.out / "long-command.json").write_text(stdout)

        # Independent observers both receive a real induced crash of the fixture app.
        package = "io.github.andriyo.shadowdroid.sample"
        call("app", "start", package, "--activity", ".MainActivity", token=token)
        for observer in ("a", "b"):
            call("session", "observe", "--subscription", observer)
        pid = subprocess.run(["adb", "-s", args.device, "shell", "pidof", package], check=True, capture_output=True, text=True, timeout=10).stdout.strip().split()[0]
        call("device", "shell", f"am crash {pid}", token=token)
        time.sleep(1)
        for observer in ("a", "b"):
            evidence = call("session", "observe", "--subscription", observer)
            assert any(c.get("package") == package for c in evidence["crashes"]), evidence
        call("app", "stop", package, token=token)

        # Explicit handoff invalidates the earlier token and requires a fresh observation.
        context = args.out / "handoff.json"
        context.write_text(json.dumps({"candidate": "sample-fixture", "plan": "session-e2e", "backend": "none", "state": "sample stopped; services idle"}))
        previous = token
        token = call("session", "handoff", "--agent", "driver-b", "--context", str(context), token=token)["session"]
        call("ui", "key", "HOME", token=previous, error="device_reserved")
        call("ui", "dump", token=token, error="session_observation_required")
        call("session", "observe", "--subscription", "driver-b", token=token)

        # Test handoff leaves UiAutomation free even when an adviser observes it.
        handoff = subprocess.Popen(command("test", "--no-reconnect", "--", "python3", "-c", "import time; time.sleep(3)", token=token), env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        time.sleep(1)
        evidence = call("session", "observe", "--subscription", "during-tests")
        assert evidence["screen"]["status"] == "unavailable", evidence
        call("connect", token=token, error="device_operation_busy")
        stdout, stderr = handoff.communicate(timeout=30)
        assert handoff.returncode == 0, (stdout, stderr)
        (args.out / "test-handoff.json").write_text(stdout)
        call("connect", token=token)

        # A killed caller cannot release the reservation or silently retry an unknown action.
        interrupted = subprocess.Popen(command("device", "shell", "sleep 20", token=token), env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        time.sleep(1)
        status = call("session", "status")
        assert status["in_flight"], status
        interrupted.kill()
        interrupted.communicate(timeout=5)
        call("ui", "key", "HOME", token=token, error="session_recovery_required")
        call("session", "close", token=token, error="session_recovery_required")
        # Reboot is deliberate: it fences remote shell/server work before journal recovery.
        subprocess.run(["adb", "-s", args.device, "reboot"], check=True, timeout=20)
        subprocess.run(["adb", "-s", args.device, "wait-for-device"], check=True, timeout=90)
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            ready = subprocess.run(["adb", "-s", args.device, "shell", "getprop", "sys.boot_completed"], capture_output=True, text=True, timeout=10)
            if ready.stdout.strip() == "1":
                break
            time.sleep(1)
        else:
            raise AssertionError("emulator did not finish rebooting")
        recovered = call("session", "recover", "--external-workers-stopped", token=token)
        assert recovered["last_completion"]["outcome"] == "unknown", recovered
        call("session", "observe", "--subscription", "recovery", token=token)
        call("connect", token=token)
        call("ui", "dump", token=token)
        call("disconnect", token=token)
    finally:
        # Leave failed recovery reservations intact for diagnosis; never remove lock files.
        if peer_token:
            call("session", "close", serial=args.peer, token=peer_token)
        call("session", "close", token=token)
    summary = {"suite": "agent-sessions", "passed": True, "device": args.device, "peer": args.peer,
               "covered": ["exclusive-driver", "private-authority-rejection", "passive-observer", "independent-crash-cursors", "handoff-fencing", "instrumentation-slot", "interrupted-recovery"] + (["independent-device"] if args.peer else [])}
    (args.out / "summary.json").write_text(json.dumps(summary, indent=2))
    print(json.dumps(summary))


if __name__ == "__main__":
    main()
