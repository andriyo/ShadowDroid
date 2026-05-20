# ShadowDroid

Drive Android apps with **structured output** and **agent-friendly latency**.

ShadowDroid is a two-piece system:

- **`shadowdroid`** — a single static **Rust** binary on the laptop that gives you a streaming JSON-line view of the device UI and a small set of CLI verbs (`tap`, `swipe`, `screenshot`, `xpath`, `watch`, …).
- **`io.github.andriyo.shadowdroid`** — a tiny **Kotlin Instrumentation APK** on the device that wraps **AndroidX UI Automator 2.3.0** behind a localhost HTTP service.

The two talk over `adb forward` + HTTP. No Python anywhere. No Appium server. No `uiautomator2` Python package.

> Successor to `movi` (MobiVision). Same agent-driving philosophy, no Python in the dependency tree, latest UI Automator, single binary distribution.

## Why it exists

The state of the art for "have an LLM drive an Android app" looks like one of these:

| Tool                                   | Problem                                                                     |
| -------------------------------------- | --------------------------------------------------------------------------- |
| Raw `adb shell uiautomator dump`       | ~500ms-1s per dump. Agents stall, the loop feels dead.                      |
| Appium                                 | Heavy. Java/Node server on the laptop. Selenium-WebDriver API mismatch.     |
| openatx + uiautomator2 (Python)        | Fast, but Python toolchain + Go binary + bundled jars. Hard to package.     |
| `adb shell input tap`                  | Stateless. No knowledge of what's on screen. Fragile to layout changes.     |

ShadowDroid keeps the speed of openatx (persistent on-device HTTP service, ~25ms dumps) without the Python deps and without the maintenance opacity. The on-device side is small Kotlin we own; the laptop side is one Rust binary.

## Repo layout

```
ShadowDroid/
├── cli/                  # Rust workspace — the `shadowdroid` binary
├── server/               # Gradle project — io.github.andriyo.shadowdroid Instrumentation APK
├── docs/                 # Architecture + HTTP protocol spec + design notes
├── proto/                # Shared schema (OpenAPI today; could codegen later)
├── scripts/              # Dev helpers (boot emulator, install APK, release builder)
├── examples/             # Sample flows + watcher rule files
└── README.md
```

## Status

Design phase. See [docs/architecture.md](docs/architecture.md), [docs/protocol.md](docs/protocol.md), and [docs/delivery-plan.md](docs/delivery-plan.md).

## License

TBD — proposed Apache 2.0.
