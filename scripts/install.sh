#!/usr/bin/env sh
set -eu

REPO="${SHADOWDROID_REPO:-andriyo/ShadowDroid}"
VERSION="${SHADOWDROID_VERSION:-latest}"
INSTALL_DIR="${SHADOWDROID_INSTALL_DIR:-$HOME/.local/bin}"
BIN_NAME="shadowdroid"

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "shadowdroid installer: missing required command: $1" >&2
    exit 1
  fi
}

download() {
  url="$1"
  out="$2"
  if command -v curl >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -fsSL "$url" -o "$out"
  elif command -v wget >/dev/null 2>&1; then
    wget -q "$url" -O "$out"
  else
    echo "shadowdroid installer: install curl or wget first" >&2
    exit 1
  fi
}

sha256_file() {
  file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{print $1}'
  else
    echo "shadowdroid installer: install sha256sum or shasum first" >&2
    exit 1
  fi
}

case "$(uname -s)" in
  Darwin) os="apple-darwin" ;;
  Linux) os="unknown-linux-gnu" ;;
  *)
    echo "shadowdroid installer: unsupported OS: $(uname -s)" >&2
    exit 1
    ;;
esac

case "$(uname -m)" in
  x86_64 | amd64) arch="x86_64" ;;
  arm64 | aarch64) arch="aarch64" ;;
  *)
    echo "shadowdroid installer: unsupported architecture: $(uname -m)" >&2
    exit 1
    ;;
esac

target="${arch}-${os}"
asset="shadowdroid-${target}.tar.gz"
if [ "$VERSION" = "latest" ]; then
  base_url="https://github.com/${REPO}/releases/latest/download"
else
  base_url="https://github.com/${REPO}/releases/download/${VERSION}"
fi

need tar
tmp="${TMPDIR:-/tmp}/shadowdroid-install.$$"
mkdir -p "$tmp"
trap 'rm -rf "$tmp"' EXIT INT TERM

download "${base_url}/${asset}" "$tmp/$asset"
download "${base_url}/SHA256SUMS" "$tmp/SHA256SUMS"

expected="$(awk -v asset="$asset" '$2 == asset || $2 == "*" asset {print $1}' "$tmp/SHA256SUMS" | head -n 1)"
if [ -z "$expected" ]; then
  echo "shadowdroid installer: checksum for $asset not found in SHA256SUMS" >&2
  exit 1
fi
actual="$(sha256_file "$tmp/$asset")"
if [ "$expected" != "$actual" ]; then
  echo "shadowdroid installer: checksum mismatch for $asset" >&2
  echo "expected: $expected" >&2
  echo "actual:   $actual" >&2
  exit 1
fi

tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$INSTALL_DIR"
install -m 755 "$tmp/$BIN_NAME" "$INSTALL_DIR/$BIN_NAME"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) path_msg="" ;;
  *) path_msg="
Add this to your shell profile if needed:
  export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac

cat <<EOF
shadowdroid installed to $INSTALL_DIR/$BIN_NAME
Run:
  shadowdroid connect
$path_msg
EOF
