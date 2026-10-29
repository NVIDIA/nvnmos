#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
"""Title GStreamer enum sections with their GType name in Hotdoc's gst_extension.py.

Hotdoc 0.18.x derives an enum section title from the GType name by dropping
`Gst`, hyphenating and capitalising it (`GstNmosCapsMode` becomes
`Nmos-caps-mode`). A property that uses the enum links to it with the GType
name, and whichever title Hotdoc resolves last wins, so the same plugin can
show both forms. There is no config hook for the title, so this script patches
the installed module before documentation generation to use the GType name,
as gstreamer.freedesktop.org shows.

Idempotent: safe to run repeatedly against an already-patched install.
"""

from __future__ import annotations

import argparse
import importlib.util
import sys
from pathlib import Path

# Exact substrings as shipped in hotdoc 0.18.x, both in __create_enum_symbol.
REPLACEMENTS = (
    (
        "        display_name = re.sub(\n"
        '            r"([a-z])([A-Z])", r"\\g<1>-\\g<2>", '
        "type_name.replace('Gst', ''))\n",
        "        display_name = type_name\n",
    ),
    (
        "display_name=display_name.capitalize(),",
        "display_name=display_name,",
    ),
)


def find_gst_extension_path() -> Path:
    spec = importlib.util.find_spec("hotdoc.extensions.gst.gst_extension")
    if spec is None or not spec.origin:
        raise SystemExit(
            "hotdoc.extensions.gst.gst_extension not found; is hotdoc installed?"
        )
    return Path(spec.origin)


def patch_file(path: Path, *, dry_run: bool = False) -> int:
    original = path.read_text(encoding="utf-8")
    updated = original
    for old, new in REPLACEMENTS:
        updated = updated.replace(old, new)

    if updated == original:
        if not all(new in original for _, new in REPLACEMENTS):
            print(
                f"{path}: enum title code not found; "
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
        help="Override path to hotdoc/extensions/gst/gst_extension.py "
        "(default: import from the active Python environment)",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Report whether a patch is needed without writing",
    )
    args = parser.parse_args()
    path = args.path if args.path is not None else find_gst_extension_path()
    return patch_file(path, dry_run=args.dry_run)


if __name__ == "__main__":
    sys.exit(main())
