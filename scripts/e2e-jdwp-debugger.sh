#!/usr/bin/env bash
set -euo pipefail

# Real-device contract test for the standalone debugger (`debug --backend jdwp`)
# against the Field Lab sample. The caller supplies artifacts built from the
# same checkout; every ShadowDroid response is kept as JSON evidence. Line
# numbers come from the sample source, so editing MainActivity.kt does not
# silently break the breakpoints.
#
# Environment:
#   SHADOWDROID_CLI_BIN          CLI binary (default: cli/target/debug/shadowdroid)
#   SHADOWDROID_DEVICE           device serial (default: emulator-5554)
#   SHADOWDROID_SERVER_TEST_APK  server test APK for `connect --apk` (optional)
#   SHADOWDROID_SAMPLE_APK       Field Lab APK to (re)install (optional)
#   SHADOWDROID_E2E_EVIDENCE     evidence directory (default: a fresh temp dir)

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
sd_bin="${SHADOWDROID_CLI_BIN:-$repo_root/cli/target/debug/shadowdroid}"
serial="${SHADOWDROID_DEVICE:-emulator-5554}"
server_test_apk="${SHADOWDROID_SERVER_TEST_APK:-}"
sample_apk="${SHADOWDROID_SAMPLE_APK:-}"
evidence_dir="${SHADOWDROID_E2E_EVIDENCE:-$(mktemp -d "${TMPDIR:-/tmp}/shadowdroid-jdwp-debugger.XXXXXX")}"
package_name="io.github.andriyo.shadowdroid.sample"
project_root="$repo_root/samples/shadowdroid-test-app"
main_activity_kt="$project_root/app/src/main/kotlin/io/github/andriyo/shadowdroid/sample/MainActivity.kt"
unreachable_studio="http://127.0.0.1:1"

for required in "$sd_bin" "$main_activity_kt" ${server_test_apk:+"$server_test_apk"} ${sample_apk:+"$sample_apk"}; do
    if [[ ! -f "$required" ]]; then
        printf 'required artifact does not exist: %s\n' "$required" >&2
        exit 2
    fi
done
command -v jq >/dev/null || { printf 'jq is required\n' >&2; exit 2; }
mkdir -p "$evidence_dir"

# A private ownership authority: the reservation step never touches the
# developer's or another job's sessions.
export SHADOWDROID_AUTHORITY_DIR="$evidence_dir/authority"
mkdir -p "$SHADOWDROID_AUTHORITY_DIR"
unset SHADOWDROID_SESSION

# ── line numbers from the sample source ────────────────────────────────────
source_line() {
    local pattern="$1" offset="${2:-0}" found
    found="$(grep -nF -- "$pattern" "$main_activity_kt" | head -n 1 | cut -d: -f1)"
    if [[ -z "$found" ]]; then
        printf 'pattern not found in %s: %s\n' "$main_activity_kt" "$pattern" >&2
        exit 2
    fi
    printf '%s' "$((found + offset))"
}
line_on_create="$(source_line 'override fun onCreate(savedInstanceState: Bundle?) {' 1)"
line_new_intent="$(source_line 'super.onNewIntent(intent)')"
line_crash="$(source_line 'throw RuntimeException("Deliberate ShadowDroid sample crash")')"
printf 'lines: onCreate=%s onNewIntent=%s crashNow=%s\n' "$line_on_create" "$line_new_intent" "$line_crash"

# ── helpers ────────────────────────────────────────────────────────────────
step_count=0
current_step=""

sd() {
    SHADOWDROID_QUIET=1 "$sd_bin" --device "$serial" "$@"
}

# Run a CLI command, keep its last JSON line as evidence, never abort on its
# exit code (assertions decide).
record() {
    local label="$1"
    shift
    local out="$evidence_dir/$label.json"
    { sd "$@" 2>"$evidence_dir/$label.stderr" || true; } | grep -v '^[[:space:]]*$' | tail -n 1 >"$out" || true
    if ! jq -e . "$out" >/dev/null 2>&1; then
        fail "$label: no JSON result (stderr: $(tail -n 3 "$evidence_dir/$label.stderr" 2>/dev/null | tr '\n' ' '))"
    fi
}

step() {
    current_step="$1"
    step_count=$((step_count + 1))
    printf '\n[%02d] %s (t+%ss)\n' "$step_count" "$current_step" "$SECONDS"
}

pass() {
    printf '  PASS %s\n' "$1"
}

fail() {
    printf '  FAIL [%s] %s\n' "$current_step" "$1" >&2
    printf '  evidence: %s\n' "$evidence_dir" >&2
    exit 1
}

# expect LABEL JQ_FILTER MESSAGE [ACTUAL_JQ]: ACTUAL_JQ picks what to print
# when the filter does not hold (default: the response envelope).
expect() {
    local envelope='{ok, code, msg, backend, backend_reason}'
    local label="$1" filter="$2" message="$3" actual="${4:-$envelope}"
    if jq -e "$filter" "$evidence_dir/$label.json" >/dev/null 2>&1; then
        pass "$message"
    else
        fail "$message
       expected: $filter
       actual:   $(jq -c "$actual" "$evidence_dir/$label.json" 2>/dev/null)
       response: $evidence_dir/$label.json"
    fi
}

adb_shell() {
    adb -s "$serial" shell "$@" | tr -d '\r'
}

fire_intent() {
    adb_shell am start -n "$package_name/.MainActivity" -f 0x20000000 --es tag "$1" >/dev/null 2>&1
}

start_main() {
    adb_shell am start -W -n "$package_name/.MainActivity" >/dev/null
}

# wait_status LABEL JQ_PREDICATE TIMEOUT_S: poll `debug status` until the
# first session matches.
wait_status() {
    local label="$1" predicate="$2" timeout_s="$3" deadline=$((SECONDS + $3))
    while true; do
        record "$label" debug status --backend jdwp
        if jq -e "(.sessions[0] // {}) | $predicate" "$evidence_dir/$label.json" >/dev/null 2>&1; then
            return 0
        fi
        if ((SECONDS >= deadline)); then
            fail "$label: session did not reach '$predicate' within ${timeout_s}s"
        fi
        sleep 0.5
    done
}

# Still running after TIMEOUT_S (a breakpoint that must not fire).
expect_running_for() {
    local label="$1" timeout_s="$2" deadline=$((SECONDS + $2))
    while ((SECONDS < deadline)); do
        record "$label" debug status --backend jdwp
        if jq -e '.sessions[0].suspended == true' "$evidence_dir/$label.json" >/dev/null 2>&1; then
            fail "$label: stopped although it must not"
        fi
        sleep 0.5
    done
    pass "still running after ${timeout_s}s"
}

dismiss_system_dialogs() {
    local focus
    for _ in 1 2 3; do
        focus="$(adb_shell dumpsys window | grep mCurrentFocus || true)"
        case "$focus" in
            *"Not Responding"* | *"has stopped"* | *"keeps stopping"* | *"isn't responding"*)
                sd ui tap --rid android:id/aerr_close --exact >/dev/null 2>&1 || true
                sleep 1
                ;;
            *) return 0 ;;
        esac
    done
}

remove_all_breakpoints() {
    local ids
    ids="$(sd debug breakpoints --backend jdwp 2>/dev/null | jq -r '.breakpoints[]?.id' 2>/dev/null || true)"
    for id in $ids; do
        sd debug break remove --backend jdwp --id "$id" >/dev/null 2>&1 || true
    done
}

registry_live() {
    find "$HOME/.shadowdroid/debug" -path "*/$serial-*" \( -name '*.json' -o -name '*.sock' \) 2>/dev/null | grep -c . || true
}

original_debug_app="$(adb_shell settings get global debug_app)"
owner_token=""

cleanup() {
    local status=$?
    set +e
    local ids
    ids="$(sd debug sessions --backend jdwp 2>/dev/null | jq -r '.sessions[]?.id' 2>/dev/null)"
    for id in $ids; do
        if [[ -n "$owner_token" ]]; then
            SHADOWDROID_SESSION="$owner_token" sd debug detach --backend jdwp --session "$id" >/dev/null 2>&1
        else
            sd debug detach --backend jdwp --session "$id" >/dev/null 2>&1
        fi
    done
    pkill -f "__debugd --serial $serial" >/dev/null 2>&1
    if [[ -n "$owner_token" ]]; then
        SHADOWDROID_SESSION="$owner_token" sd session close >/dev/null 2>&1
    fi
    if [[ "$(adb_shell settings get global debug_app)" != "$original_debug_app" && "$original_debug_app" == "null" ]]; then
        adb_shell am clear-debug-app >/dev/null
    fi
    dismiss_system_dialogs
    if ((status != 0)); then
        printf '\nFAILED after %s step(s); evidence in %s\n' "$step_count" "$evidence_dir" >&2
    fi
    exit "$status"
}
trap cleanup EXIT

# ── 0. setup ───────────────────────────────────────────────────────────────
step "setup: server, sample app"
if [[ -n "$server_test_apk" ]]; then
    record connect connect --apk "$server_test_apk"
else
    record connect connect
fi
expect connect '.ok == true' "device server connected"
if [[ -n "$sample_apk" ]]; then
    record reinstall app reinstall "$sample_apk" --grant-all
    expect reinstall '.ok == true' "sample reinstalled"
fi
start_main
pass "sample started"

# ── 1. P0 journey ──────────────────────────────────────────────────────────
step "P0 journey: attach, break line, hit, inspect, step, pause, detach"
record p0-attach debug attach --backend jdwp --package "$package_name"
expect p0-attach '.ok == true and .backend == "jdwp" and (.session.id | startswith("jdwp:"))' "attached to the running app"
record p0-break --project-root "$project_root" debug break line --backend jdwp --file MainActivity.kt --line "$line_new_intent"
expect p0-break ".breakpoint.bound == true and .breakpoint.locations[0].method == \"onNewIntent\"" "line $line_new_intent bound in onNewIntent"
fire_intent p0
wait_status p0-hit '.suspended == true and .suspend_reason == "breakpoint"' 10
expect p0-hit ".sessions[0].position.line == $line_new_intent and .sessions[0].thread == \"main\"" "stopped at line $line_new_intent on main" \
    '.sessions[0] | {position, thread}'
record p0-stack debug stack --backend jdwp --limit 3
expect p0-stack ".frames[0].method == \"onNewIntent\" and .frames[0].line == $line_new_intent" "stack top is onNewIntent"
record p0-variables debug variables --backend jdwp --depth 0
expect p0-variables '[.variables[].name] | index("intent") != null' "variables list intent"
expect p0-variables '.this.type == "io.github.andriyo.shadowdroid.sample.MainActivity"' "this is the activity"
record p0-eval debug eval --backend jdwp intent.mFlags
expect p0-eval '.ok == true and .result.type == "int"' "eval intent.mFlags"
record p0-step-over debug step-over --backend jdwp
expect p0-step-over ".session.position.line == $((line_new_intent + 1))" "step-over lands on line $((line_new_intent + 1))" \
    '{ok, code, msg, position: .session.position}'
record p0-step-in debug step-in --backend jdwp
expect p0-step-in '.ok == true and .session.suspend_reason == "step" and (.session.position.class | startswith("io.github.andriyo") or startswith("android."))' "step-in stops on a line"
record p0-step-out debug step-out --backend jdwp
expect p0-step-out '.ok == true and .session.suspend_reason == "step"' "step-out stops in the caller"
record p0-resume debug resume --backend jdwp
expect p0-resume '.ok == true and .session.suspended == false' "resumed"
record p0-remove debug break remove --backend jdwp --id "$(jq -r '.breakpoint.id' "$evidence_dir/p0-break.json")"
expect p0-remove '.ok == true' "breakpoint removed"
record p0-pause debug pause --backend jdwp
expect p0-pause '.ok == true' "paused"
record p0-paused-stack debug stack --backend jdwp --limit 1
expect p0-paused-stack '.frames[0].thread == "main"' "pause selects the main thread"
record p0-resume-2 debug resume --backend jdwp
record p0-detach debug detach --backend jdwp
expect p0-detach '.ok == true and .result.detached == true' "detached"
sleep 1
[[ "$(registry_live)" == "0" ]] || fail "registry entries left after detach"
pass "registry cleaned up"

# ── 2. launch-time attach ──────────────────────────────────────────────────
step "launch-time attach: --wait-for-launch --break onCreate"
record launch-attach --project-root "$project_root" debug attach --backend jdwp --package "$package_name" \
    --wait-for-launch --launch-activity .MainActivity --break "MainActivity.kt:$line_on_create"
expect launch-attach '.ok == true and .session.launched_under_debugger == true' "launched under the debugger"
expect launch-attach '[.launch.steps[].step] | (index("set_debug_app") != null and index("process_started") != null)' "launch steps recorded"
wait_status launch-hit ".suspended == true and .position.line == $line_on_create" 15
pass "stopped at onCreate line $line_on_create before the activity ran"
record launch-resume debug resume --backend jdwp
record launch-debug-app-check debug status --backend jdwp
[[ "$(adb_shell settings get global debug_app)" == "$original_debug_app" ]] || fail "debug_app setting changed: $(adb_shell settings get global debug_app)"
pass "debug_app setting unchanged ($original_debug_app)"
remove_all_breakpoints

# ── 3. conditional breakpoint + break update ───────────────────────────────
step "conditional breakpoint and break update"
record cond-break --project-root "$project_root" debug break line --backend jdwp --file MainActivity.kt \
    --line "$line_new_intent" --condition 'intent == null'
expect cond-break '.breakpoint.condition == "intent == null"' "condition set"
sleep 2
fire_intent cond-false
expect_running_for cond-false-status 3
record cond-update debug break update --backend jdwp --id "$(jq -r '.breakpoint.id' "$evidence_dir/cond-break.json")" \
    --condition 'intent != null'
expect cond-update '.ok == true and .breakpoint.condition == "intent != null"' "condition updated"
fire_intent cond-true
wait_status cond-hit '.suspended == true and .suspend_reason == "breakpoint"' 10
pass "the true condition stops"
record cond-resume debug resume --backend jdwp
remove_all_breakpoints

# ── 4. logpoint ────────────────────────────────────────────────────────────
step "logpoint with an expression"
record logpoint-add --project-root "$project_root" debug logpoint add --backend jdwp --file MainActivity.kt \
    --line "$line_new_intent" --expression intent.mFlags --owner e2e
expect logpoint-add '.ok == true' "logpoint added"
fire_intent log-1
fire_intent log-2
sleep 2
record logpoint-events debug logpoint events --backend jdwp --owner e2e
expect logpoint-events '([.events[] | select(.message | test("^[0-9]+$"))] | length) >= 2' "two evaluated events"
expect_running_for logpoint-running 1
record logpoint-clear debug logpoint clear --backend jdwp --owner e2e
expect logpoint-clear '.removed >= 1' "logpoint cleared"

# ── 5. --invoke eval ───────────────────────────────────────────────────────
step "eval with --invoke, including a thrown exception"
record invoke-break --project-root "$project_root" debug break line --backend jdwp --file MainActivity.kt --line "$line_new_intent"
fire_intent invoke
wait_status invoke-hit '.suspended == true' 10
record invoke-denied debug eval --backend jdwp 'intent.toString()'
expect invoke-denied '.ok == false and .code == "invoke_not_allowed"' "a call without --invoke is refused"
record invoke-ok debug eval --backend jdwp --invoke 'intent.toString()'
expect invoke-ok '.ok == true and (.result.value | tostring | startswith("Intent {"))' "intent.toString() runs with --invoke"
record invoke-thrown debug eval --backend jdwp --invoke 'this.getString(0)'
expect invoke-thrown '.ok == true and .result.thrown == true' "a throwing call is reported as thrown"
record invoke-resume debug resume --backend jdwp
remove_all_breakpoints

# ── 6. method breakpoint ───────────────────────────────────────────────────
step "method breakpoint"
record method-break debug break method --backend jdwp --class io.github.andriyo.shadowdroid.sample.MainActivity \
    --method onNewIntent
expect method-break '.ok == true and (.breakpoint.locations | length) >= 1' "method breakpoint bound"
fire_intent method
wait_status method-hit '.suspended == true and .suspend_reason == "method_breakpoint" and .position.method == "onNewIntent"' 10
pass "stopped on method entry"
record method-resume debug resume --backend jdwp
remove_all_breakpoints
record method-detach debug detach --backend jdwp

# ── 7. run-until-crash ─────────────────────────────────────────────────────
step "run-until-crash through the fault controls"
start_main
record crash-attach debug attach --backend jdwp --package "$package_name"
record crash-nav-wait ui wait --rid nav_lab --timeout-ms 15000
record crash-nav-tap ui tap --rid nav_lab --exact
record crash-search-wait ui wait --rid lab_search_input --timeout-ms 8000
record crash-filter ui text crash --rid lab_search_input --clear
record crash-keyboard ui hide-keyboard
record crash-expand ui tap --rid lab_faults_section_toggle --exact
record crash-reveal ui scroll-to --rid crash_button --exact --max-swipes 8
expect crash-reveal '.ok == true' "crash button on screen"
crash_out="$evidence_dir/run-until-crash.json"
(sd debug run-until-crash --backend jdwp --timeout-ms 60000 --bundle "$evidence_dir/crash-bundle" \
    2>"$evidence_dir/run-until-crash.stderr" | grep -v '^[[:space:]]*$' | tail -n 1 >"$crash_out") &
crash_wait=$!
# run-until-crash arms its exception request before it resumes and waits; a
# tap that lands earlier would crash the app with nobody listening.
sleep 5
record crash-tap ui tap --rid crash_button --exact
wait "$crash_wait" || true
jq -e . "$crash_out" >/dev/null 2>&1 || fail "run-until-crash printed no JSON"
expect run-until-crash ".ok == true and .stop.throwing_frame.line == $line_crash" "stopped at the throw on line $line_crash" \
    '{ok, code, msg, throwing_frame: .stop.throwing_frame}'
expect run-until-crash '.crash.exception == "java.lang.RuntimeException" and (.crash.message | test("Deliberate"))' "exception and message captured"
expect run-until-crash '.jdwp.resume.needed == false' "the app was not resumed into the crash"
record crash-resume debug resume --backend jdwp
deadline=$((SECONDS + 30))
until [[ "$(registry_live)" == "0" ]]; do
    ((SECONDS < deadline)) || fail "the daemon did not deregister after the crash"
    sleep 1
done
pass "the daemon deregistered after the process died"
dismiss_system_dialogs

# ── 8. --backend auto routing ──────────────────────────────────────────────
step "--backend auto with Studio unreachable"
start_main
record auto-attach debug attach --package "$package_name" --studio-url "$unreachable_studio"
expect auto-attach '.ok == true and .backend == "jdwp" and .backend_reason == "studio_bridge_unreachable"' "auto attach falls back to jdwp"
record auto-status debug status --studio-url "$unreachable_studio"
expect auto-status '.backend == "jdwp" and .backend_reason == "jdwp_session_holds_target"' "a live session keeps the next verb on jdwp"
record auto-detach debug detach --studio-url "$unreachable_studio"
expect auto-detach '.ok == true and .backend == "jdwp"' "auto detach"

# ── 9. device ownership ────────────────────────────────────────────────────
step "device reservation refuses another driver"
record own-open session open --agent e2e-owner
expect own-open '.ok == true and (.session | type == "string")' "reservation opened"
owner_token="$(jq -r '.session' "$evidence_dir/own-open.json")"
record own-refused debug attach --backend jdwp --package "$package_name"
expect own-refused '.ok == false and .code == "device_reserved"' "attach without the token is refused"
SHADOWDROID_SESSION="$owner_token" record own-observe session observe --subscription e2e-owner
SHADOWDROID_SESSION="$owner_token" record own-attach debug attach --backend jdwp --package "$package_name"
expect own-attach '.ok == true and .backend == "jdwp"' "the owner attaches"
record own-pause-refused debug pause --backend jdwp
expect own-pause-refused '.ok == false and .code == "device_reserved"' "pause without the token is refused"
SHADOWDROID_SESSION="$owner_token" record own-detach debug detach --backend jdwp
expect own-detach '.ok == true' "the owner detaches"
SHADOWDROID_SESSION="$owner_token" record own-close session close
expect own-close '.ok == true' "reservation closed"
owner_token=""

# ── 10. leftovers ──────────────────────────────────────────────────────────
step "nothing left behind"
sleep 1
[[ "$(registry_live)" == "0" ]] || fail "registry entries left"
pass "no registry entries"
# The daemon removes its registry entry before it exits; give it a moment.
deadline=$((SECONDS + 10))
while pgrep -f "__debugd --serial $serial" >/dev/null; do
    ((SECONDS < deadline)) || fail "a __debugd process is still running: $(pgrep -fl "__debugd --serial $serial" | tr '\n' ' ')"
    sleep 0.5
done
pass "no __debugd processes"
[[ "$(adb_shell settings get global debug_app)" == "$original_debug_app" ]] || fail "debug_app setting changed"
pass "debug_app setting unchanged"
adb_shell pidof "$package_name" >/dev/null || fail "the sample is not running"
pass "sample running"

jq -n \
    --arg device "$serial" \
    --arg evidence "$evidence_dir" \
    --argjson steps "$step_count" \
    '{ok: true, suite: "jdwp-debugger", device: $device, steps: $steps, evidence: $evidence}'
