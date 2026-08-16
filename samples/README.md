# Samples

This directory contains apps and fixtures used to exercise ShadowDroid against
real Android packages.

## ShadowDroid Field Lab

[`shadowdroid-test-app`](shadowdroid-test-app) is a stateful operator experience
for local manual and agent-driven testing:

- **Overview** presents mission progress, workspace shortcuts, live posture,
  and recent events.
- **Mission** is a deterministic three-gate “Night Shift: Signal Recovery”
  scenario.
- **Signals** contains the HTTP request composer, live WS/WSS channel, and
  WebView.
- **Lab** keeps the searchable raw fixtures for selectors, native and Compose
  ranges, windows, permissions, lifecycle, storage, coroutines, logs, crash,
  and ANR paths.

The app package is `io.github.andriyo.shadowdroid.sample`. Build and launch it
from the repository root:

```bash
./server/gradlew -p samples/shadowdroid-test-app :app:assembleDebug
shadowdroid app reinstall \
  samples/shadowdroid-test-app/app/build/outputs/apk/debug/app-debug.apk \
  --grant-all --wait-front
shadowdroid app start io.github.andriyo.shadowdroid.sample \
  --activity .MainActivity
```

The walkthroughs below assume the app is foregrounded and ShadowDroid is
connected.

### Night Shift mission

Open **Mission**, claim the incident with a non-empty callsign and the exact run
code `NIGHT-42`, tune the semantic slider to `71`, enable telemetry, arm the
relay, choose **Relay East**, and long-press the acknowledgement surface:

```bash
shadowdroid ui tap --rid nav_mission --expect-text "Night Shift: Signal Recovery"
shadowdroid ui text "operator-7" --rid name_input --clear
shadowdroid ui text "NIGHT-42" --rid mission_code_input --clear
shadowdroid ui hide-keyboard
shadowdroid ui tap --rid mission_claim_button \
  --expect-text "Incident claimed by operator-7"
shadowdroid ui scroll-to --rid mission_signal_slider
shadowdroid ui set-progress --rid mission_signal_slider --value 71 --observe
shadowdroid ui tap --rid mission_telemetry_switch --observe
shadowdroid ui scroll-to --rid mission_arm_relay_button --tap
shadowdroid ui wait --text "Relay calibrated at 71%"
shadowdroid ui scroll-to --rid mission_relay_east --tap
shadowdroid ui wait --text "Relay East selected"
shadowdroid ui scroll-to --rid mission_hold_acknowledge
shadowdroid ui find --rid mission_hold_acknowledge
```

Take the returned `element.tap` X/Y coordinates from the final `ui find`, then
finish the gesture-only gate:

```bash
shadowdroid ui long-tap <x> <y> --duration-ms 800 \
  --expect-text "Signal recovered"
```

### Selector gauntlet

The **Interaction gauntlet** is expanded when **Lab** first opens. It preserves
the ambiguous labels, native clickable-ancestor behavior, disabled state,
delivery-only action, and unstable-update fixture:

```bash
shadowdroid ui tap --rid nav_lab --expect-text "Challenge catalog"
shadowdroid ui tap --text "Duplicate action" --exact
# Expected: ambiguous_match. Disambiguate with a stable resource ID:
shadowdroid ui tap --rid duplicate_two_button \
  --expect-text "Second duplicate action tapped"
shadowdroid ui tap --rid nested_inner_label \
  --expect-text "Nearest clickable ancestor activated"
shadowdroid ui tap --rid disabled_button
# Expected: element_disabled.
shadowdroid ui tap --rid noop_button --observe
# Expected: input_delivered=true with no visible mutation.
shadowdroid ui tap --rid unstable_updates_button --observe
```

### Counter logpoint

The counter mutation is intentionally simple enough for a deterministic
non-suspending logpoint check. This walkthrough requires the sample project to
be open in Android Studio with the ShadowDroid plugin running, and the debug APK
to be attached to Studio's debugger. From the repository root, resolve the
fixture line from its stable statement instead of pinning a line number that
will drift as the sample changes:

```bash
SAMPLE_PROJECT="$PWD/samples/shadowdroid-test-app"
COUNTER_FILE="$SAMPLE_PROJECT/app/src/main/kotlin/io/github/andriyo/shadowdroid/sample/MainActivity.kt"
COUNTER_LINE="$(rg -n -F 'setStatus("Counter incremented to $counter")' "$COUNTER_FILE" | cut -d: -f1)"

shadowdroid debug attach \
  --project "$SAMPLE_PROJECT" \
  --package io.github.andriyo.shadowdroid.sample \
  --configuration app
shadowdroid debug logpoint add \
  --file "$COUNTER_FILE" \
  --line "$COUNTER_LINE" \
  --expression '"counter=" + counter' \
  --project "$SAMPLE_PROJECT" \
  --owner sample-counter-e2e
```

In one terminal, follow exactly the three new hits. `follow` starts at the live
tail, so start it before tapping the fixture:

```bash
SAMPLE_PROJECT="$PWD/samples/shadowdroid-test-app"
shadowdroid debug logpoint follow \
  --project "$SAMPLE_PROJECT" \
  --owner sample-counter-e2e \
  --max-events 3
```

In another terminal, open **Lab**, reveal the native counter, and tap it three
times:

```bash
shadowdroid ui tap --rid nav_lab --expect-text "Challenge catalog"
shadowdroid ui scroll-to --rid counter_button
shadowdroid ui tap --rid counter_button --expect-text "Counter incremented to 1"
shadowdroid ui tap --rid counter_button --expect-text "Counter incremented to 2"
shadowdroid ui tap --rid counter_button --expect-text "Counter incremented to 3"
```

The follower should emit three ordered JSONL records with `type:"logpoint"`;
their composite JetBrains-rendered messages contain `counter=1`, `counter=2`,
and `counter=3`, and the app remains responsive throughout. Clean up by owner
and confirm that only that test instrumentation is gone—human Studio
breakpoints are preserved:

```bash
SAMPLE_PROJECT="$PWD/samples/shadowdroid-test-app"
shadowdroid debug logpoint clear \
  --project "$SAMPLE_PROJECT" \
  --owner sample-counter-e2e
shadowdroid debug logpoint list \
  --project "$SAMPLE_PROJECT" \
  --owner sample-counter-e2e
```

### Network channel

Run the local Ktor WS/WSS room in one terminal, then start the proxy:

```bash
./server/gradlew -p samples/shadowdroid-test-app :chat-server:run
```

```bash
shadowdroid net trust --auto # rootable emulator; use the documented manual flow otherwise
shadowdroid net start --host shadowdroid.localhost
```

In another terminal, open **Signals**, enter the live channel, establish WSS,
send a text response, expose the advanced controls, then send binary and 4KB
text frames before closing normally:

```bash
shadowdroid ui tap --rid nav_signals --expect-text "Traffic observatory"
shadowdroid ui scroll-to --rid websocket_chat_button --tap
shadowdroid ui wait --activity WebSocketChatActivity
shadowdroid ui tap --rid websocket_use_wss_button
shadowdroid ui tap --rid websocket_connect_button
shadowdroid ui wait --text "Connected" --timeout-ms 10000
shadowdroid ui text "field-check-42" --rid websocket_message_input --clear
shadowdroid ui hide-keyboard
shadowdroid ui tap --rid websocket_send_button --expect-text "Message sent"
shadowdroid ui tap --rid websocket_advanced_toggle
shadowdroid ui tap --rid websocket_send_binary_button --observe
shadowdroid ui tap --rid websocket_send_large_button --observe
shadowdroid ui tap --rid websocket_disconnect_button
shadowdroid net ws
```

The Field Lab adds navigation and realistic state, but the deterministic
fixtures and their stable resource IDs, Compose test tags, and content
descriptions remain directly available under **Lab**.

Fault injection exposes confirmation-first `prepare_crash_button` and
`prepare_anr_button` actions for exploratory use. The legacy `crash_button`
and `anr_button` IDs remain one-tap fixtures so existing recovery recipes keep
their original contract.
