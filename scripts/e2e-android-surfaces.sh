#!/usr/bin/env bash
set -euo pipefail

# Real-device contract test for the agent-facing Android surfaces. The caller
# supplies artifacts built from the same checkout; every ShadowDroid response
# is retained as JSON evidence for CI or local diagnosis.

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
sd_bin="${SHADOWDROID_CLI_BIN:-$repo_root/cli/target/debug/shadowdroid}"
serial="${SHADOWDROID_DEVICE:-emulator-5554}"
server_test_apk="${SHADOWDROID_SERVER_TEST_APK:-$repo_root/server/app/build/outputs/apk/androidTest/debug/app-debug-androidTest.apk}"
sample_apk="${SHADOWDROID_SAMPLE_APK:-$repo_root/samples/shadowdroid-test-app/app/build/outputs/apk/debug/app-debug.apk}"
expected_form_factor="${SHADOWDROID_EXPECT_FORM_FACTOR:-phone}"
evidence_dir="${SHADOWDROID_E2E_EVIDENCE:-$(mktemp -d "${TMPDIR:-/tmp}/shadowdroid-android-surfaces.XXXXXX")}"
package_name="io.github.andriyo.shadowdroid.sample"

for required in "$sd_bin" "$server_test_apk" "$sample_apk"; do
    if [[ ! -f "$required" ]]; then
        printf 'required artifact does not exist: %s\n' "$required" >&2
        exit 2
    fi
done
if [[ "$expected_form_factor" != "phone" && "$expected_form_factor" != "tv" ]]; then
    printf 'SHADOWDROID_EXPECT_FORM_FACTOR must be phone or tv\n' >&2
    exit 2
fi
mkdir -p "$evidence_dir"

sd() {
    SHADOWDROID_QUIET=1 "$sd_bin" --device "$serial" "$@"
}

record() {
    local label="$1"
    shift
    sd "$@" >"$evidence_dir/$label.json"
}

if adb -s "$serial" shell pm has-feature android.software.leanback \
    | tr -d '\r' \
    | grep -q 'true$'; then
    actual_form_factor="tv"
else
    actual_form_factor="phone"
fi
if [[ "$actual_form_factor" != "$expected_form_factor" ]]; then
    printf 'expected %s emulator, detected %s\n' "$expected_form_factor" "$actual_form_factor" >&2
    exit 1
fi

record connect --apk "$server_test_apk" connect
record reinstall app reinstall "$sample_apk" --grant-all

# Native focus contract: the origin explicitly owns focus and declares the
# target as its right-hand successor. The same path runs on touch phones and TV.
record start-focus-fixture app start "$package_name" --activity .FocusFixtureActivity
record focus-origin-ready ui wait --rid focus_origin_button --timeout-ms 10000
record focus-origin-dump ui dump --full
jq -e '
    any(
        .elements[];
        (((.rid // "") == "focus_origin_button") or
         ((.rid // "") | endswith(":id/focus_origin_button"))) and
        .focused == true
    )
' "$evidence_dir/focus-origin-dump.json" >/dev/null
record focus-target ui focus --rid focus_target_button --center --max-steps 6
record focus-activated ui wait --text 'Focus target activated' --timeout-ms 5000

record start app start "$package_name" --activity .MainActivity
record root-ready ui wait --package "$package_name" --rid status_text --timeout-ms 15000

# Compose selector: the navigation item is a Compose test tag exported as a
# resource id. Reaching the catalog proves selector resolution and input.
record open-lab ui tap --rid nav_lab --expect-text 'Challenge catalog' --timeout-ms 8000
record lab-dump ui dump --full
jq -e '
    .snapshot_state == "consistent" and
    any(.elements[]; (.rid // "") == "fixture_lab_scroll")
' "$evidence_dir/lab-dump.json" >/dev/null
jq -e '
    .action_delivered == true and
    .matched_element.rid == "nav_lab" and
    .postcondition_satisfied == true
' "$evidence_dir/open-lab.json" >/dev/null

# Platform View selector: scroll to a native android.widget.Button, activate it,
# and observe the counter postcondition.
record reveal-counter ui scroll-to --rid counter_button --max-swipes 12
record tap-counter ui tap --rid counter_button --expect-text 'Counter incremented to 1' --timeout-ms 8000
record counter-dump ui dump --full
jq -e '
    any(
        .elements[];
        ((.rid // "") | endswith(":id/counter_button")) and
        (.klass | endswith("Button"))
    )
' "$evidence_dir/counter-dump.json" >/dev/null

# Lifecycle/accessibility convergence: a delayed Activity transition must settle
# into a consistent tree and the original Compose screen must settle after Back.
record filter-lifecycle ui text lifecycle --rid lab_search_input --clear
record hide-lifecycle-keyboard ui hide-keyboard
record lifecycle-card-ready ui wait --text 'Lifecycle & state' --timeout-ms 5000
record reveal-lifecycle ui scroll-to --rid lab_lifecycle_section_toggle --max-swipes 20 --tap
record open-delayed-detail ui scroll-to --rid delayed_detail_button --max-swipes 12 --tap
record detail-ready ui wait --activity DetailActivity --timeout-ms 10000
record detail-dump ui dump --full
jq -e '.snapshot_state == "consistent"' "$evidence_dir/detail-dump.json" >/dev/null
record return-from-detail ui back
record lab-restored ui wait --activity MainActivity --rid delayed_detail_button --timeout-ms 10000

# WebView selector: the platform WebView is created from a Compose action. The
# test only requires accessibility convergence, not an external network reply.
record open-signals ui tap --rid nav_signals --expect-rid url_input --timeout-ms 8000
viewport_width="$(jq -er '.screen.viewport.w' "$evidence_dir/open-signals.json")"
viewport_height="$(jq -er '.screen.viewport.h' "$evidence_dir/open-signals.json")"
swipe_x=$((viewport_width / 2))
swipe_from_y=$((viewport_height * 3 / 4))
swipe_to_y=$((viewport_height / 4))
record reveal-webview-1 ui swipe \
    "$swipe_x" "$swipe_from_y" "$swipe_x" "$swipe_to_y" \
    --duration-ms 350 --observe
record reveal-webview-2 ui swipe \
    "$swipe_x" "$swipe_from_y" "$swipe_x" "$swipe_to_y" \
    --duration-ms 350 --expect-rid webview_button --timeout-ms 8000
record load-webview ui tap --rid webview_button
record webview-ready ui wait --klass WebView --timeout-ms 10000
record webview-dump ui dump --full
jq -e '
    .ok == true and
    .matched == true and
    (.element.klass | endswith("WebView"))
' "$evidence_dir/webview-ready.json" >/dev/null
jq -e '
    .snapshot_state == "consistent"
' "$evidence_dir/webview-dump.json" >/dev/null

jq -n \
    --arg device "$serial" \
    --arg form_factor "$actual_form_factor" \
    --arg evidence "$evidence_dir" \
    '{
        ok: true,
        suite: "android-surfaces",
        device: $device,
        form_factor: $form_factor,
        covered: [
            "compose-selector",
            "view-selector",
            "webview-selector",
            "dpad-focus",
            "lifecycle-accessibility-convergence"
        ],
        evidence: $evidence
    }'
