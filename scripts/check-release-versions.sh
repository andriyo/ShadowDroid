#!/usr/bin/env bash
set -euo pipefail

# One executable source-version contract used both before creating a tag and
# inside the tag-triggered release workflow. It deliberately checks the lockfile
# and every runtime marker that can label a shipped artifact.

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

die() {
  echo "error: $*" >&2
  exit 1
}

expected="${1:-}"
cargo_version="$(awk -F'"' '/^version = "/ {print $2; exit}' cli/Cargo.toml)"
lock_version="$(awk '
  $0 == "name = \"shadowdroid\"" { in_package = 1; next }
  in_package && /^version = "/ { gsub(/"/, "", $3); print $3; exit }
' cli/Cargo.lock)"
server_fallback="$(sed -nE 's/^[[:space:]]*\?: "([^"]+)"/\1/p' server/app/build.gradle.kts | head -1)"
server_marker="$(sed -nE 's/^[[:space:]]*const val SERVER_VERSION: String = "([^"]+)"/\1/p' server/app/src/androidTest/java/io/github/andriyo/shadowdroid/BuildInfo.kt)"
agent_marker="$(sed -nE 's/^[[:space:]]*const val VERSION = "([^"]+)"/\1/p' agent/shadowdroid-agent/src/main/kotlin/io/github/andriyo/shadowdroid/agent/BuildInfo.kt)"

[ -n "$cargo_version" ] || die "could not read cli/Cargo.toml package version"
[ -n "$expected" ] || expected="$cargo_version"

for observed in \
  "Cargo.toml:$cargo_version" \
  "Cargo.lock:$lock_version" \
  "server fallback:$server_fallback" \
  "server marker:$server_marker" \
  "agent marker:$agent_marker"
do
  label="${observed%%:*}"
  value="${observed#*:}"
  [ "$value" = "$expected" ] \
    || die "$label version is ${value:-missing}; expected $expected"
done

printf 'release versions synchronized: %s\n' "$expected"
