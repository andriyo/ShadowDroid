# Samples

This directory contains apps and fixtures used to exercise ShadowDroid against
real Android packages.

## shadowdroid-test-app

`shadowdroid-test-app` is a kitchen-sink Android application for local manual
and agent-driven testing. It includes selector-rich UI, multiple launcher
activities, deep links, dialogs, toasts, runtime permissions, notifications,
files, clipboard, logs, crashes, ANR triggers, WebView, HTTP/HTTPS request
buttons, and a native WebSocket chat client. Its `chat-server` submodule runs a
local Ktor WS/WSS room for real bidirectional proxy verification.
