#!/usr/bin/env python3
# Vendored from GStreamer gst-docs 1.26.0 (scripts/generate_sitemap.py).
# The upstream file carries no per-file license header; the tags below record
# the contributors per upstream git history and the module's license.
#
# SPDX-FileCopyrightText: Copyright (c) 2021, 2023 Thibault Saunier <tsaunier@igalia.com>
# SPDX-FileCopyrightText: Copyright (c) 2025 Mathieu Duponchelle <mathieu@centricular.com>
# SPDX-License-Identifier: LGPL-2.1-or-later

import json
import os
from argparse import ArgumentParser
from pathlib import Path as P

if __name__ == "__main__":
    parser = ArgumentParser()
    parser.add_argument('--input-sitemap', type=P)
    parser.add_argument('--output-sitemap', type=P)
    parser.add_argument('--markdown-index', type=P)
    parser.add_argument('--libs', type=str)
    parser.add_argument('--plugins', type=str)
    parser.add_argument('--plugin-configs', nargs='*', default=[])
    parser.add_argument('--lib-configs', nargs='*', default=[])

    args = parser.parse_args()

    in_ = args.input_sitemap
    out = args.output_sitemap
    index_md = args.markdown_index
    plugin_configs = args.plugin_configs
    lib_configs = args.lib_configs

    with open(in_) as f:
        index = f.read()
        index = '\n'.join(line for line in index.splitlines())

        if args.libs is None:
            libs = []
        else:
            libs = args.libs.split(os.pathsep)
        for config in lib_configs:
            with open(config) as f:
                libs += json.load(f)

        if args.plugins is None:
            plugins = []
        else:
            plugins = args.plugins.replace('\n', '').split(os.pathsep)
        for config in plugin_configs:
            with open(config) as f:
                plugins += json.load(f)
        plugins = sorted(plugins, key=lambda x: os.path.basename(x))

        if libs:
            index += '\n\tlibs.md'
            for lib in libs:
                if not lib:
                    continue
                name = lib
                if not name.endswith('.json'):
                    name += '.json'
                index += "\n\t\t" + name
        if plugins:
            plugin_index = '\tgst-index'
            for plugin in plugins:
                if not plugin:
                    continue
                fname = plugin
                if not fname.endswith('.json'):
                    fname += '.json'
                plugin_index += "\n\t\t" + fname
            if '\tgst-index' in index:
                index = index.replace('\tgst-index', plugin_index, 1)
            else:
                index += '\n' + plugin_index

        index = '%s\n%s' % (index_md, index)

        with open(out, 'w') as fw:
            fw.write(index)
