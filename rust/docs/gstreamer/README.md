<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# GStreamer plugin documentation (Hotdoc)

Builds official-style Hotdoc HTML for the `nmos` and `avsynctest` plugins
(`gst-nmos-rs`, `gst-avsynctest-rs`).

Published at [nvidia.github.io/nvnmos/gstreamer/](https://nvidia.github.io/nvnmos/gstreamer/)
alongside the Doxygen C API.

## Toolchain pins

| Component | Pin | Notes |
| --------- | --- | ----- |
| Hotdoc | `~=0.18.0` | Enforced in `meson.build` and CI |
| Lumen theme | 0.16 (sha256 in `meson.build`) | Downloaded at build time |
| Plugin scanner | GStreamer **1.24.2** (`tools/`) | Vendored; newer scanner needs newer GStreamer API |
| Sitemap generator | gst-docs **1.26.0** (`scripts/generate_sitemap.py`) | See below |

`scripts/generate_sitemap.py` is from gst-docs 1.26 because that release uses
`argparse` (`--input-sitemap`, …), which matches how our Meson `custom_target`
invokes it. The 1.24 script expects positional arguments only and fails with
Meson's flag-style command line.

## Prerequisites

Ubuntu / Debian packages:

```sh
sudo apt-get install -y \
  meson ninja-build \
  libgstreamer1.0-dev \
  python3-dev libxml2-dev libxslt1-dev cmake libyaml-dev \
  libjson-glib-dev flex
```

Hotdoc (virtualenv recommended):

```sh
python3 -m venv .venv-hotdoc
. .venv-hotdoc/bin/activate
pip install "hotdoc~=0.18.0"
```

Meson then runs `scripts/patch_hotdoc_gtk_doc_hrefs.py` against the installed
Hotdoc to rewrite leftover `web.mit.edu/barnowl` gtk-doc fallbacks (for example
`gchararray`) to `docs.gtk.org`. The patch is idempotent and re-applied on every
configure.

Rust toolchain plus workspace dependencies (same as `rust/` CI).

## Build

```sh
export CARGO_TARGET_DIR=/path/to/nvnmos/rust/target
cd rust/docs/gstreamer
meson setup build -Ddoc=enabled
ninja -C build gstreamer-doc
```

HTML output: `build/NvNmos-GStreamer-doc/html/`

## Refresh committed plugin cache

When element properties or pad templates change, regenerate
`plugins/gst_plugins_cache.json`:

```sh
./scripts/refresh-plugins-cache.sh
git add plugins/gst_plugins_cache.json
```

The introspection tools under `tools/` are vendored from GStreamer 1.24.2
(`gst-hotdoc-plugins-scanner.c`, `gst-plugins-doc-cache-generator.py`).

## Site customisation (`theme/extra/`)

Portal chrome (light-mode default, “View on GitHub” with octocat icon) lives in
`theme/extra/` as Hotdoc `html_extra_theme` overlays — a small CSS/JS pair rather
than a fork of the upstream navbar template. The octocat SVG uses `currentColor`
so it follows navbar link colour in both light and dark mode.

## Upgrading Hotdoc or the Lumen theme

After bumping Hotdoc or `html_theme` in `meson.build`:

1. Rebuild: `ninja -C build gstreamer-doc` and open `index.html` locally.
2. Check the navbar: home icon, search, theme toggle, and **View on GitHub**
   (right-aligned, octocat + label).
3. Toggle dark/light mode; confirm the octocat remains visible and the sidebar
   iframe tracks the theme.
4. Spot-check portal Subpages (C API, gst-nmos-rs, Plugins) and one element
   page per plugin.
5. If the upstream navbar markup changed, adjust `theme/extra/js/extra_frontend.js`
   (injection target / classes) and `theme/extra/css/extra_frontend.css` — do
   **not** copy the whole `navbar.html` unless unavoidable.

After bumping the vendored GStreamer scanner (`tools/`):

1. Rebuild the scanner: `ninja -C build`.
2. Regenerate the cache: `./scripts/refresh-plugins-cache.sh`.
3. Re-run `ninja -C build gstreamer-doc` and diff element property tables.
