#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
"""Fetch the devhelp index of the upstream GStreamer documentation.

Hotdoc resolves names it does not document itself (`GstElement`, `GstPad`, …)
through devhelp indexes found under `$XDG_DATA_DIRS/devhelp/books`. Each
gstreamer-docs release tarball contains such an index, whose `online` attribute
points at gstreamer.freedesktop.org. This script streams the tarball for the
given GStreamer version and extracts only that index, as
`<data-dir>/devhelp/books/GStreamer/GStreamer.devhelp2`.

Exits 0 when the index is present (including from an earlier run) and 1 when
it could not be fetched, for example offline or for a version that has no
docs tarball.
"""

from __future__ import annotations

import argparse
import sys
import tarfile
import urllib.error
import urllib.request
from pathlib import Path

URL = "https://gstreamer.freedesktop.org/src/gstreamer-docs/gstreamer-docs-{version}.tar.xz"
MEMBER = "gstreamer-docs-{version}/devhelp/books/GStreamer/GStreamer.devhelp2"
TIMEOUT_SECONDS = 60


def fetch(version: str, data_dir: Path) -> int:
    dest = data_dir / "devhelp" / "books" / "GStreamer" / "GStreamer.devhelp2"
    if dest.is_file():
        print(f"{dest}: already present")
        return 0

    url = URL.format(version=version)
    member = MEMBER.format(version=version)
    tmp = dest.with_suffix(".tmp")
    try:
        with urllib.request.urlopen(url, timeout=TIMEOUT_SECONDS) as response:
            with tarfile.open(fileobj=response, mode="r|xz") as tar:
                for info in tar:
                    if info.name == member and info.isfile():
                        dest.parent.mkdir(parents=True, exist_ok=True)
                        with tar.extractfile(info) as src, open(tmp, "wb") as out:
                            out.write(src.read())
                        break
                else:
                    print(f"{url}: no {member}", file=sys.stderr)
                    return 1
    except (urllib.error.URLError, OSError, tarfile.TarError) as e:
        tmp.unlink(missing_ok=True)
        print(f"{url}: {e}", file=sys.stderr)
        return 1

    tmp.replace(dest)
    print(f"{dest}: fetched from {url}")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version", help="GStreamer version, for example 1.24.2")
    parser.add_argument(
        "data_dir",
        type=Path,
        help="Directory to add to XDG_DATA_DIRS for Hotdoc",
    )
    args = parser.parse_args()
    return fetch(args.version, args.data_dir)


if __name__ == "__main__":
    sys.exit(main())
