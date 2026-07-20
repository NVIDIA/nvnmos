#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
"""Rewrite leftover MIT gtk-doc fallback URLs in Hotdoc's gtk_doc.py.

Hotdoc 0.18.x already points many GLib/GObject symbols at docs.gtk.org, but a
handful of hardcoded fallbacks still use web.mit.edu/barnowl mirrors. There is
no config hook for those URLs, so this script patches the installed module
before documentation generation.

Idempotent: safe to run repeatedly against an already-patched install.
"""

from __future__ import annotations

import argparse
import importlib.util
import sys
from pathlib import Path

# Exact substrings as shipped in hotdoc 0.18.x. Keep replacements conservative:
# only rewrite the known MIT templates, leave the rest of the file alone.
REPLACEMENTS = (
    (
        "https://web.mit.edu/barnowl/share/gtk-doc/html/glib/"
        "glib-Limits-of-Basic-Types.html#G-MIN{numerical_type}:CAPS",
        "https://docs.gtk.org/glib/types.html",
    ),
    (
        "https://web.mit.edu/barnowl/share/gtk-doc/html/glib/"
        "glib-Limits-of-Basic-Types.html#G-MAX{numerical_type}:CAPS",
        "https://docs.gtk.org/glib/types.html",
    ),
    (
        "https://docs.gtk.org/glib/const.MIN{numerical_type}.html",
        "https://docs.gtk.org/glib/types.html",
    ),
    (
        "https://docs.gtk.org/glib/const.MAX{numerical_type}.html",
        "https://docs.gtk.org/glib/types.html",
    ),
    (
        "https://web.mit.edu/barnowl/share/gtk-doc/html/gobject/"
        "gobject-Type-Information.html#G-TYPE-{gtype}:CAPS",
        "https://docs.gtk.org/gobject/types.html",
    ),
    (
        "https://web.mit.edu/barnowl/share/gtk-doc/html/gobject/"
        "gobject-Standard-Parameter-and-Value-Types.html#gchararray",
        # gchararray is the introspection name for G_TYPE_STRING properties.
        "https://docs.gtk.org/gobject/types.html",
    ),
    (
        "https://web.mit.edu/barnowl/share/gtk-doc/html/glib/"
        "glib-Miscellaneous-Macros.html#G-GNUC-NO-INSTRUMENT:CAPS",
        "https://docs.gtk.org/glib/macros.html",
    ),
    (
        "https://web.mit.edu/barnowl/share/gtk-doc/html/glib/"
        "glib-Standard-Macros.html#{define}:CAPS",
        "https://docs.gtk.org/glib/macros.html",
    ),
)


def find_gtk_doc_path() -> Path:
    spec = importlib.util.find_spec("hotdoc.parsers.gtk_doc")
    if spec is None or not spec.origin:
        raise SystemExit("hotdoc.parsers.gtk_doc not found; is hotdoc installed?")
    return Path(spec.origin)


def patch_file(path: Path, *, dry_run: bool = False) -> int:
    original = path.read_text(encoding="utf-8")
    updated = original
    for old, new in REPLACEMENTS:
        updated = updated.replace(old, new)

    if updated == original:
        leftover = original.count("web.mit.edu/barnowl")
        if leftover:
            print(
                f"{path}: still contains {leftover} barnowl URL(s); "
                "Hotdoc may have changed — update this script",
                file=sys.stderr,
            )
            return 1
        print(f"{path}: already patched")
        return 0

    if dry_run:
        print(f"{path}: would patch")
        return 0

    path.write_text(updated, encoding="utf-8")
    print(f"{path}: patched")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--path",
        type=Path,
        help="Override path to hotdoc/parsers/gtk_doc.py "
        "(default: import from the active Python environment)",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Report whether a patch is needed without writing",
    )
    args = parser.parse_args()
    path = args.path if args.path is not None else find_gtk_doc_path()
    return patch_file(path, dry_run=args.dry_run)


if __name__ == "__main__":
    sys.exit(main())
