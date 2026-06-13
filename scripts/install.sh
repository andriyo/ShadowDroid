#!/usr/bin/env sh
set -eu

REPO="${SHADOWDROID_REPO:-andriyo/ShadowDroid}"
VERSION="${SHADOWDROID_VERSION:-latest}"
INSTALL_DIR="${SHADOWDROID_INSTALL_DIR:-$HOME/.local/bin}"
BIN_NAME="shadowdroid"
UNINSTALL=0

usage() {
  cat <<'EOF'
ShadowDroid installer for macOS and Linux.

Usage:
  sh install.sh [options]

Options:
  --version <tag>       Install a specific release tag, e.g. v0.1.3.
                        Values without a leading "v" are normalized.
  --install-dir <dir>   Install directory. Default: ~/.local/bin.
  --repo <owner/repo>   GitHub repo. Default: andriyo/ShadowDroid.
  --uninstall           Remove shadowdroid from the install directory.
  -h, --help            Show this help.

Environment overrides:
  SHADOWDROID_VERSION
  SHADOWDROID_INSTALL_DIR
  SHADOWDROID_REPO

Examples:
  curl --proto '=https' --tlsv1.2 -LsSf \
    https://github.com/andriyo/ShadowDroid/releases/latest/download/shadowdroid-installer.sh | sh

  curl --proto '=https' --tlsv1.2 -LsSf \
    https://github.com/andriyo/ShadowDroid/releases/latest/download/shadowdroid-installer.sh \
    | sh -s -- --version v0.1.3 --install-dir "$HOME/bin"
EOF
}

die() {
  echo "shadowdroid installer: $*" >&2
  exit 1
}

need() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

normalize_version() {
  case "$1" in
    latest | v*) printf '%s\n' "$1" ;;
    *) printf 'v%s\n' "$1" ;;
  esac
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      [ "$#" -ge 2 ] || die "--version requires a value"
      VERSION="$2"
      shift 2
      ;;
    --version=*)
      VERSION="${1#*=}"
      shift
      ;;
    --install-dir)
      [ "$#" -ge 2 ] || die "--install-dir requires a value"
      INSTALL_DIR="$2"
      shift 2
      ;;
    --install-dir=*)
      INSTALL_DIR="${1#*=}"
      shift
      ;;
    --repo)
      [ "$#" -ge 2 ] || die "--repo requires a value"
      REPO="$2"
      shift 2
      ;;
    --repo=*)
      REPO="${1#*=}"
      shift
      ;;
    --uninstall)
      UNINSTALL=1
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      die "unknown option: $1"
      ;;
  esac
done

VERSION="$(normalize_version "$VERSION")"

if [ "$UNINSTALL" -eq 1 ]; then
  rm -f "$INSTALL_DIR/$BIN_NAME"
  echo "removed $INSTALL_DIR/$BIN_NAME"
  exit 0
fi

download() {
  url="$1"
  out="$2"
  if command -v curl >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -fsSL "$url" -o "$out"
  elif command -v wget >/dev/null 2>&1; then
    wget -q "$url" -O "$out"
  else
    die "install curl or wget first"
  fi
}

sha256_file() {
  file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{print $1}'
  else
    die "install sha256sum or shasum first"
  fi
}

path_hint() {
  case ":$PATH:" in
    *":$INSTALL_DIR:"*) return 0 ;;
  esac

  shell_name="$(basename "${SHELL:-sh}")"
  case "$shell_name" in
    zsh) profile="$HOME/.zshrc" ;;
    bash) profile="$HOME/.bashrc" ;;
    fish) profile="$HOME/.config/fish/config.fish" ;;
    *) profile="your shell profile" ;;
  esac

  cat <<EOF

$INSTALL_DIR is not on PATH.
Add this to $profile:
  export PATH="$INSTALL_DIR:\$PATH"
EOF
}

adb_hint() {
  if command -v adb >/dev/null 2>&1; then
    return 0
  fi

  cat <<'EOF'

adb was not found on PATH.
Install Android Platform Tools before running `shadowdroid connect`:
  macOS: brew install --cask android-platform-tools
  Linux: install android-sdk-platform-tools with your package manager
EOF
}

case "$(uname -s)" in
  Darwin) os="apple-darwin" ;;
  Linux) os="unknown-linux-gnu" ;;
  *) die "unsupported OS: $(uname -s)" ;;
esac

case "$(uname -m)" in
  x86_64 | amd64) arch="x86_64" ;;
  arm64 | aarch64) arch="aarch64" ;;
  *) die "unsupported architecture: $(uname -m)" ;;
esac

target="${arch}-${os}"
asset="shadowdroid-${target}.tar.gz"
if [ "$VERSION" = "latest" ]; then
  base_url="https://github.com/${REPO}/releases/latest/download"
else
  base_url="https://github.com/${REPO}/releases/download/${VERSION}"
fi

need awk
need tar
tmp="$(mktemp -d "${TMPDIR:-/tmp}/shadowdroid-install.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "Installing shadowdroid ${VERSION} for ${target}..."
download "${base_url}/${asset}" "$tmp/$asset"
download "${base_url}/SHA256SUMS" "$tmp/SHA256SUMS"

expected="$(awk -v asset="$asset" '$2 == asset || $2 == "*" asset {print $1}' "$tmp/SHA256SUMS" | head -n 1)"
[ -n "$expected" ] || die "checksum for $asset not found in SHA256SUMS"

actual="$(sha256_file "$tmp/$asset")"
if [ "$expected" != "$actual" ]; then
  echo "expected: $expected" >&2
  echo "actual:   $actual" >&2
  die "checksum mismatch for $asset"
fi

tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$INSTALL_DIR"
cp "$tmp/$BIN_NAME" "$INSTALL_DIR/$BIN_NAME"
chmod 755 "$INSTALL_DIR/$BIN_NAME"

if "$INSTALL_DIR/$BIN_NAME" init --no-studio-plugin >/dev/null 2>&1; then
  skills_msg="Agent skills installed/updated."
else
  skills_msg="Agent skill install skipped. Run: shadowdroid init"
fi

cat <<EOF
shadowdroid installed to $INSTALL_DIR/$BIN_NAME
$skills_msg
Run:
  shadowdroid connect
EOF
path_hint
adb_hint
