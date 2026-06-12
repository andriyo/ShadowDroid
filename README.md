# ShadowDroid

Drive Android apps with **structured output** and **agent-friendly latency**.

ShadowDroid is a two-piece system:

- **`shadowdroid`** — a single static **Rust** binary on the laptop that gives you a streaming JSON-line view of the device UI and a set of CLI verbs: flat interaction primitives (`tap`, `swipe`, `text`, `find`, `scroll-to`, `screen`, `watch`, …) plus nested resource namespaces (`app`, `perm`, `appops`, `profile`, `device`, `files`).
- **`io.github.andriyo.shadowdroid`** — a tiny **Kotlin Instrumentation APK** on the device that wraps **AndroidX UI Automator 2.3.0** behind a localhost HTTP service.

The two talk over `adb forward` + HTTP. No Python anywhere. No Appium server. No `uiautomator2` Python package.

ShadowDroid is intentionally self-contained: no Python dependency tree, current UI Automator, and a single binary distribution.

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

## Install

Homebrew:

```bash
brew install andriyo/tap/shadowdroid
```

macOS / Linux:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/andriyo/ShadowDroid/releases/latest/download/shadowdroid-installer.sh | sh
```

Scoop:

```powershell
scoop bucket add andriyo https://github.com/andriyo/scoop-bucket
scoop install shadowdroid
```

Windows PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/andriyo/ShadowDroid/releases/latest/download/shadowdroid-installer.ps1 | iex"
```

ShadowDroid requires Android Platform Tools (`adb`) on PATH. The installers
print a hint if `adb` is missing; on macOS you can install it with
`brew install --cask android-platform-tools`, and on Windows with
`scoop install adb`.

Initialize optional host integrations:

```bash
shadowdroid init                         # detect Android Studio + plugin state
shadowdroid init --install-studio-plugin # install/update the Android Studio plugin
```

Then connect to an attached Android device or emulator:

```bash
shadowdroid connect
```

Check for CLI updates:

```bash
shadowdroid update --check
```

The installer only installs the host CLI. On first `connect`, ShadowDroid
downloads the matching instrumentation APKs from the same GitHub Release,
verifies them with SHA-256, caches them under `~/.shadowdroid/apks/<version>/`,
and installs them on the device.

The Android Studio plugin is shipped as `shadowdroid-studio-plugin.zip` in the
same GitHub Release. `shadowdroid studio install` detects Android Studio,
downloads/verifies/caches the plugin under `~/.shadowdroid/plugins/<version>/`,
unpacks it into Studio's user plugin directory, and tells you when a restart is
required.

See [docs/getting-started.md](docs/getting-started.md) for manual downloads
and pinned versions. Maintainers can use [docs/release.md](docs/release.md) to
cut a release.

## Agent integration

ShadowDroid is self-describing. `shadowdroid commands --json` emits the full
command catalog (names, nesting, args, help) straight from the CLI definition —
the machine-readable counterpart to `--help` that an agent can read once to
discover the whole tool.

`shadowdroid skill <agent>` generates a ready-to-drop integration file for a
coding agent, with driving guidance and an auto-generated command reference.
Supported agents: `claude-code`, `cursor`, `codex`, `gemini`, `antigravity`
(the last four match the set Android's own CLI installs skills for).

```bash
shadowdroid skill claude-code --install   # → ~/.claude/skills/shadowdroid/SKILL.md
shadowdroid skill cursor      --install   # → ~/.cursor/skills/shadowdroid/SKILL.md
shadowdroid skill gemini      --install   # → ~/.gemini/skills/shadowdroid/SKILL.md
shadowdroid skill antigravity --install   # → ~/.gemini/antigravity*/skills/shadowdroid/SKILL.md
shadowdroid skill codex                   # → prints an AGENTS.md section to stdout
```

Cursor `--install` creates a personal skill that is available across projects.
To write a project-scoped Cursor rule instead:

```bash
shadowdroid skill cursor --out /path/to/project/.cursor/rules/shadowdroid.mdc
```

Each installed skill is stamped with a version marker. After you upgrade the
CLI, refresh them in one shot — unmodified skills are rewritten in place, and
any you've hand-edited are left alone (pass `--force` to overwrite those too):

```bash
shadowdroid skill --sync          # refresh every installed skill to this version
```

`connect` also runs this refresh automatically (pristine skills only), so an
upgraded CLI keeps its installed skills current with no extra step.

## Agent debugging

ShadowDroid also exposes an agent-first debugging surface:
`debug snapshot` for one-shot state, `debug record` / `debug replay` for JSONL
timelines, `debugger` for Android Studio-backed attach/breakpoints/stack/vars/eval,
and `layout` for UI-tree snapshots, diffs, and Android Studio Layout
Inspector-backed Compose/source/recomposition enrichment. See
[docs/agent-debugging.md](docs/agent-debugging.md).

## Status

M5 distribution wiring is implemented. See [docs/architecture.md](docs/architecture.md), [docs/protocol.md](docs/protocol.md), and [docs/delivery-plan.md](docs/delivery-plan.md).

## License

Apache-2.0. See [LICENSE](LICENSE).
