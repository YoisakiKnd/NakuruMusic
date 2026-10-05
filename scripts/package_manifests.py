#!/usr/bin/env python3
"""Generate Scoop and Homebrew Cask manifests from verified release archives."""

import argparse
import hashlib
import json
import tarfile
import zipfile
from pathlib import Path


TARGETS = {
    "windows": ("x86_64-pc-windows-msvc", "zip", "nakuru-music.exe"),
    "arm": ("aarch64-apple-darwin", "tar.gz", "nakuru-music"),
    "intel": ("x86_64-apple-darwin", "tar.gz", "nakuru-music"),
}


def read_archive(dist: Path, tag: str, target: str) -> tuple[str, str]:
    triple, extension, binary = TARGETS[target]
    root = f"nakuru-music-{tag}-{triple}"
    filename = f"{root}.{extension}"
    path = dist / filename
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    expected = f"{root}/{binary}"
    if extension == "zip":
        with zipfile.ZipFile(path) as archive:
            assert expected in archive.namelist(), f"missing {expected} in {filename}"
    else:
        with tarfile.open(path, "r:gz") as archive:
            assert expected in archive.getnames(), f"missing {expected} in {filename}"
    return filename, digest


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag", help="release tag such as v0.2.0")
    parser.add_argument("dist", type=Path, help="directory containing release archives")
    parser.add_argument("output", type=Path)
    parser.add_argument("--repo", default="YoisakiKnd/NakuruMusic")
    args = parser.parse_args()
    if not args.tag.startswith("v"):
        parser.error("tag must start with v")
    version = args.tag[1:]
    archives = {target: read_archive(args.dist, args.tag, target) for target in TARGETS}
    base = f"https://github.com/{args.repo}"
    release = f"{base}/releases/download/{args.tag}"
    win_file, win_hash = archives["windows"]
    manifest = {
        "version": version,
        "description": "NakuruMusic YouTube Music terminal client with built-in playback",
        "homepage": base,
        "license": "GPL-3.0-only",
        "architecture": {
            "64bit": {
                "url": f"{release}/{win_file}",
                "hash": win_hash,
                "extract_dir": win_file.removesuffix(".zip"),
            }
        },
        "bin": "nakuru-music.exe",
        "checkver": "github",
        "autoupdate": {
            "architecture": {
                "64bit": {
                    "url": f"{base}/releases/download/v$version/nakuru-music-v$version-x86_64-pc-windows-msvc.zip",
                    "hash": {
                        "url": f"{base}/releases/download/v$version/SHA256SUMS",
                        "regex": "$sha256\\s+$basename",
                    },
                    "extract_dir": "nakuru-music-v$version-x86_64-pc-windows-msvc",
                }
            }
        },
    }
    cask = f'''cask "nakuru-music" do
  arch arm: "aarch64", intel: "x86_64"

  version "{version}"
  sha256 arm: "{archives['arm'][1]}", intel: "{archives['intel'][1]}"

  url "{base}/releases/download/v#{{version}}/nakuru-music-v#{{version}}-#{{arch}}-apple-darwin.tar.gz"
  name "NakuruMusic"
  desc "YouTube Music terminal client with built-in audio playback"
  homepage "{base}"

  binary "nakuru-music-v#{{version}}-#{{arch}}-apple-darwin/nakuru-music"
end
'''
    (args.output / "bucket").mkdir(parents=True, exist_ok=True)
    (args.output / "Casks").mkdir(parents=True, exist_ok=True)
    (args.output / "bucket" / "nakuru-music.json").write_text(
        json.dumps(manifest, ensure_ascii=False, indent=4) + "\n", encoding="utf-8"
    )
    (args.output / "Casks" / "nakuru-music.rb").write_text(cask, encoding="utf-8")


if __name__ == "__main__":
    main()
