#!/usr/bin/env python3
"""Render a checked-in LazyAgents aggregator-installer template.

Reads .github/installers/lazyagents-installer.<ext>.in, substitutes the
release tag + version placeholders with pure Python string replacement
(no regex), writes the result to target/distrib/lazyagents-installer.<ext>.

We deliberately do NOT use `sed` for this: sed's replacement DSL would
interpret `&` (backref to the match) and break on the delimiter
character. SemVer tags don't contain those characters today, but a
workflow_dispatch run with a stray tag value should still produce a
sane error rather than emit a half-substituted script. Python's
`str.replace()` has neither footgun.

Usage:
  render-aggregator-installer.py <ext>

  ext: shell extension to render — `sh` or `ps1`.

Environment:
  RELEASE_TAG       full release tag, e.g. `v0.1.0-rc.1` (required)
  RELEASE_VERSION   version sans leading `v`, e.g. `0.1.0-rc.1`
                    (derived from RELEASE_TAG if unset)
  SRC_DIR           override template source directory (default
                    `.github/installers`)
  DEST_DIR          override output directory (default
                    `target/distrib`)
"""
from __future__ import annotations
import os
import pathlib
import sys

PLACEHOLDERS = ("__RELEASE_TAG__", "__RELEASE_VERSION__")


def main(argv: list[str]) -> int:
    if len(argv) != 2 or argv[1] not in ("sh", "ps1"):
        print(f"usage: {argv[0]} <sh|ps1>", file=sys.stderr)
        return 2

    ext = argv[1]
    tag = os.environ.get("RELEASE_TAG", "").strip()
    if not tag:
        print("RELEASE_TAG is required", file=sys.stderr)
        return 2
    version = os.environ.get("RELEASE_VERSION", "").strip() or tag.lstrip("v")

    src_dir = pathlib.Path(os.environ.get("SRC_DIR", ".github/installers"))
    dst_dir = pathlib.Path(os.environ.get("DEST_DIR", "target/distrib"))

    src = src_dir / f"lazyagents-installer.{ext}.in"
    dst = dst_dir / f"lazyagents-installer.{ext}"

    body = src.read_text()
    body = body.replace("__RELEASE_TAG__", tag).replace("__RELEASE_VERSION__", version)

    for marker in PLACEHOLDERS:
        if marker in body:
            print(f"::error::unsubstituted placeholder {marker} in {dst}", file=sys.stderr)
            return 1

    dst_dir.mkdir(parents=True, exist_ok=True)
    dst.write_text(body)
    print(f"rendered {src} -> {dst} (tag={tag!r}, version={version!r})")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
