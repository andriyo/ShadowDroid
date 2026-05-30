#!/usr/bin/env python3
"""Rewrite ShadowDroid Homebrew and Scoop package metadata for a release."""

from __future__ import annotations

import argparse
import json
from pathlib import Path


ASSETS = {
    "mac_arm": "shadowdroid-aarch64-apple-darwin.tar.gz",
    "mac_intel": "shadowdroid-x86_64-apple-darwin.tar.gz",
    "linux_arm": "shadowdroid-aarch64-unknown-linux-gnu.tar.gz",
    "linux_intel": "shadowdroid-x86_64-unknown-linux-gnu.tar.gz",
    "windows": "shadowdroid-x86_64-pc-windows-msvc.zip",
}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True, help="Release tag, e.g. v0.1.3")
    parser.add_argument("--checksums", required=True, type=Path, help="Path to SHA256SUMS")
    parser.add_argument("--homebrew-path", required=True, type=Path)
    parser.add_argument("--scoop-path", required=True, type=Path)
    args = parser.parse_args()

    tag = normalize_tag(args.version)
    version = tag.removeprefix("v")
    checksums = read_checksums(args.checksums)

    write_homebrew_formula(args.homebrew_path, tag, checksums)
    write_scoop_manifest(args.scoop_path, tag, version, checksums)


def normalize_tag(value: str) -> str:
    value = value.strip()
    return value if value.startswith("v") else f"v{value}"


def read_checksums(path: Path) -> dict[str, str]:
    checksums: dict[str, str] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        parts = line.split()
        if len(parts) < 2:
            continue
        sha, name = parts[0], parts[1].lstrip("*")
        checksums[name] = sha.lower()

    missing = sorted(set(ASSETS.values()) - checksums.keys())
    if missing:
        raise SystemExit(f"missing checksums for: {', '.join(missing)}")
    return checksums


def write_homebrew_formula(root: Path, tag: str, checksums: dict[str, str]) -> None:
    formula = root / "Formula" / "shadowdroid.rb"
    formula.parent.mkdir(parents=True, exist_ok=True)
    formula.write_text(
        f"""class Shadowdroid < Formula
  desc "Drive Android apps from the command-line with streaming JSON events"
  homepage "https://github.com/andriyo/ShadowDroid"
  license "Apache-2.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/andriyo/ShadowDroid/releases/download/{tag}/{ASSETS['mac_arm']}"
      sha256 "{checksums[ASSETS['mac_arm']]}"
    else
      url "https://github.com/andriyo/ShadowDroid/releases/download/{tag}/{ASSETS['mac_intel']}"
      sha256 "{checksums[ASSETS['mac_intel']]}"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/andriyo/ShadowDroid/releases/download/{tag}/{ASSETS['linux_arm']}"
      sha256 "{checksums[ASSETS['linux_arm']]}"
    else
      url "https://github.com/andriyo/ShadowDroid/releases/download/{tag}/{ASSETS['linux_intel']}"
      sha256 "{checksums[ASSETS['linux_intel']]}"
    end
  end

  def install
    bin.install "shadowdroid"
  end

  test do
    assert_match "shadowdroid #{{version}}", shell_output("#{{bin}}/shadowdroid --version")
  end
end
""",
        encoding="utf-8",
    )


def write_scoop_manifest(root: Path, tag: str, version: str, checksums: dict[str, str]) -> None:
    manifest = root / "bucket" / "shadowdroid.json"
    manifest.parent.mkdir(parents=True, exist_ok=True)
    data = {
        "version": version,
        "description": "Drive Android apps from the command line with streaming JSON events.",
        "homepage": "https://github.com/andriyo/ShadowDroid",
        "license": "Apache-2.0",
        "architecture": {
            "64bit": {
                "url": f"https://github.com/andriyo/ShadowDroid/releases/download/{tag}/{ASSETS['windows']}",
                "hash": checksums[ASSETS["windows"]],
            }
        },
        "bin": "shadowdroid.exe",
        "checkver": {"github": "https://github.com/andriyo/ShadowDroid"},
        "autoupdate": {
            "architecture": {
                "64bit": {
                    "url": "https://github.com/andriyo/ShadowDroid/releases/download/v$version/shadowdroid-x86_64-pc-windows-msvc.zip"
                }
            }
        },
    }
    manifest.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
