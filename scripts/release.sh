#!/usr/bin/env bash
#
# Cut a ShadowDroid release.
#
# cli/Cargo.toml's version is the source of truth for `shadowdroid --version`,
# and the release tag MUST match it (the workflow builds with --locked and the
# installer smoke-test asserts `shadowdroid --version` == tag minus the "v").
# This script keeps the three in lockstep: it bumps Cargo.toml, refreshes
# Cargo.lock, commits, tags vX.Y.Z, and pushes.
#
# Pushing the tag triggers .github/workflows/release.yml, which builds every
# artifact (APKs, Studio plugin, agent AAR, CLI for 5 targets), publishes the
# GitHub Release, smoke-tests the installers, and updates the Homebrew/Scoop repos.
#
# Usage:
#   scripts/release.sh 0.4.0            # bump -> commit -> tag -> push
#   scripts/release.sh v0.4.0           # a leading "v" is fine
#   scripts/release.sh 0.4.0 --dry-run  # print the plan, change nothing
#
set -euo pipefail

cargo_toml="cli/Cargo.toml"

die() { echo "error: $*" >&2; exit 1; }

# --- locate repo root ---------------------------------------------------
repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || die "not inside a git repository"
cd "$repo_root"
[ -f "$cargo_toml" ] || die "$cargo_toml not found (run from the ShadowDroid repo)"

# --- parse args ---------------------------------------------------------
dry_run=0
version=""
for arg in "$@"; do
  case "$arg" in
    --dry-run) dry_run=1 ;;
    -h|--help) sed -n '2,28p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*) die "unknown flag: $arg" ;;
    *) [ -z "$version" ] || die "unexpected extra argument: $arg"; version="$arg" ;;
  esac
done
[ -n "$version" ] || { echo "usage: scripts/release.sh <version> [--dry-run]" >&2; exit 1; }

version="${version#v}"          # normalize: strip a leading v
tag="v$version"

# --- validate version format (X.Y.Z, optional -prerelease) --------------
printf '%s' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$' \
  || die "'$version' is not a valid semver (expected X.Y.Z)"

# --- read current state -------------------------------------------------
crate="$(awk -F' = ' '/^name = "/ {gsub(/"/,"",$2); print $2; exit}' "$cargo_toml")"
current="$(awk -F'"' '/^version = "/ {print $2; exit}' "$cargo_toml")"
[ -n "$current" ] || die "could not read current version from $cargo_toml"

echo "crate:   $crate"
echo "current: $current"
echo "new:     $version  (tag $tag)"

[ "$version" != "$current" ] || die "$cargo_toml is already at $version"

# --- preflight ----------------------------------------------------------
# On a real run these are fatal; under --dry-run they only warn, so the plan
# still prints (e.g. the untracked release.sh itself dirties the tree).
block() { if [ "$dry_run" = 1 ]; then echo "warning (blocks a real run): $*" >&2; else die "$*"; fi; }

[ -z "$(git status --porcelain)" ] || { git status --short >&2; block "working tree is not clean; commit or stash first"; }

branch="$(git rev-parse --abbrev-ref HEAD)"
[ "$branch" != "HEAD" ] || die "detached HEAD; check out a branch first"
[ "$branch" = "main" ] || echo "warning: on '$branch', not 'main'" >&2

git rev-parse -q --verify "refs/tags/$tag" >/dev/null && block "tag $tag already exists locally"
git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null 2>&1 && block "tag $tag already exists on origin"

if [ "$dry_run" = 1 ]; then
  cat <<EOF

[dry-run] would:
  1. set version in $cargo_toml -> $version
  2. cargo update -p $crate --precise $version --offline   (refresh Cargo.lock)
  3. git commit cli/Cargo.toml cli/Cargo.lock -m "Release $tag"
  4. git tag -a $tag -m "ShadowDroid $tag"
  5. git push origin $branch $tag
EOF
  exit 0
fi

# --- 1. bump Cargo.toml (first version line == [package] version) -------
awk -v new="$version" '!done && /^version = "/ { sub(/"[^"]*"/, "\"" new "\""); done=1 } { print }' \
  "$cargo_toml" > "$cargo_toml.tmp" && mv "$cargo_toml.tmp" "$cargo_toml"

# --- 2. refresh Cargo.lock (offline; touches only the local crate entry) -
( cd cli && cargo update -p "$crate" --precise "$version" --offline )

# --- 3. commit ----------------------------------------------------------
git add cli/Cargo.toml cli/Cargo.lock
git commit -m "Release $tag"

# --- 4. tag -------------------------------------------------------------
git tag -a "$tag" -m "ShadowDroid $tag"

# --- 5. push ------------------------------------------------------------
echo "pushing $branch and $tag to origin..."
git push origin "$branch" "$tag"

echo
echo "done. release build triggered for $tag."
remote_url="$(git remote get-url origin 2>/dev/null || true)"
slug="$(printf '%s' "$remote_url" | sed -E 's#(git@github.com:|https://github.com/)##; s#\.git$##')"
if [ -n "$slug" ]; then
  echo "  actions:  https://github.com/$slug/actions"
  echo "  release:  https://github.com/$slug/releases/tag/$tag"
fi
echo "  watch:    gh run watch \$(gh run list --workflow=release.yml -L1 --json databaseId -q '.[0].databaseId')"
