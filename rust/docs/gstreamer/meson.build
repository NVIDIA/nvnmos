# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

project(
  'nvnmos-gstreamer-docs',
  'c',
  version: '0.1.0',
  meson_version: '>= 1.2',
  default_options: ['buildtype=plain'],
)

if get_option('doc').disabled()
  subdir_done()
endif

if meson.is_cross_build()
  error('GStreamer plugin documentation cannot be built while cross-compiling')
endif

build_hotdoc = false

gst_dep = dependency('gstreamer-1.0', version: '>= 1.20')
gio_dep = dependency('gio-2.0')

cdir = meson.current_source_dir()
rust_root = cdir / '..' / '..'
plugins_cache = cdir / 'plugins' / 'gst_plugins_cache.json'
build_plugins_cache = meson.current_build_dir() / 'gst_plugins_cache.json'
if host_machine.system() == 'windows'
  pathsep = ';'
else
  pathsep = ':'
endif
tools_dir = cdir / 'tools'
build_plugins = find_program(cdir / 'scripts' / 'build-plugins.sh')

hotdoc_plugin_scanner = executable(
  'gst-hotdoc-plugins-scanner',
  tools_dir / 'gst-hotdoc-plugins-scanner.c',
  dependencies: [gst_dep, gio_dep],
  install: false,
)

plugins_cache_generator = find_program(tools_dir / 'gst-plugins-doc-cache-generator.py')

plugin_build = custom_target(
  'gst-plugins-build',
  command: [build_plugins],
  output: 'plugins-built.stamp',
  build_always_stale: true,
)

gstnmos_plugin = custom_target(
  'libgstnmos.so',
  command: [
    'cp',
    rust_root / 'target' / 'release' / 'libgstnmos.so',
    '@OUTPUT@',
  ],
  output: 'libgstnmos.so',
  depends: plugin_build,
  build_always_stale: true,
)

gstavsynctest_plugin = custom_target(
  'libgstavsynctest.so',
  command: [
    'cp',
    rust_root / 'target' / 'release' / 'libgstavsynctest.so',
    '@OUTPUT@',
  ],
  output: 'libgstavsynctest.so',
  depends: plugin_build,
  build_always_stale: true,
)

gst_plugins_doc_dep = custom_target(
  'nvnmos-plugins-doc-cache',
  command: [
    plugins_cache_generator,
    plugins_cache,
    '@OUTPUT@',
    gstnmos_plugin,
    gstavsynctest_plugin,
  ],
  input: [gstnmos_plugin, gstavsynctest_plugin],
  output: 'gst_plugins_cache.json',
  env: {
    'GST_HOTDOC_PLUGINS_SCANNER': hotdoc_plugin_scanner.full_path(),
    'MESON_BUILD_ROOT': meson.current_build_dir(),
    'GST_PLUGIN_PATH': meson.current_build_dir(),
  },
  depends: hotdoc_plugin_scanner,
  build_always_stale: true,
)

hotdoc_p = find_program('hotdoc', required: get_option('doc'))
if not hotdoc_p.found()
  message('Hotdoc not found, not building GStreamer plugin documentation')
  subdir_done()
endif

hotdoc_req_min = '>=0.18.0'
hotdoc_req_max = '<0.19.0'
hotdoc_version = run_command(hotdoc_p, '--version', check: false).stdout().strip()
if not hotdoc_version.version_compare(hotdoc_req_min) or not hotdoc_version.version_compare(hotdoc_req_max)
  error('Hotdoc @0@, <0.19.0 required, found @1@'.format(hotdoc_req_min, hotdoc_version))
endif

hotdoc = import('hotdoc')
required_hotdoc_extensions = ['gst-extension']
foreach extension: required_hotdoc_extensions
  if not hotdoc.has_extensions(extension)
    error('Hotdoc gst-extension missing (install hotdoc with system json-glib dev packages)')
  endif
endforeach

build_hotdoc = true

gst_nmos_rs_globs = [
  rust_root / 'gst-nmos-rs' / 'src' / '*.rs',
  rust_root / 'gst-nmos-rs' / 'src' / '*/*.rs',
  rust_root / 'gst-nmos-rs' / 'src' / '*/*/*.rs',
  rust_root / 'gst-nmos-rs' / 'src' / '*/*/*/*.rs',
]

gst_avsynctest_rs_globs = [
  rust_root / 'gst-avsynctest-rs' / 'src' / '*.rs',
  rust_root / 'gst-avsynctest-rs' / 'src' / '*/*.rs',
  rust_root / 'gst-avsynctest-rs' / 'src' / '*/*/*.rs',
]

nmos_doc = hotdoc.generate_doc(
  'nmos',
  project_version: meson.project_version(),
  sitemap: cdir / 'plugins' / 'sitemap.txt',
  index: cdir / 'plugins' / 'nmos' / 'index.md',
  gst_index: cdir / 'plugins' / 'nmos' / 'index.md',
  gst_smart_index: true,
  gst_c_sources: gst_nmos_rs_globs,
  gst_cache_file: build_plugins_cache,
  gst_plugin_name: 'nmos',
  extra_assets: [rust_root / 'gst-nmos-rs' / 'images'],
  dependencies: gst_dep,
  depends: [gst_plugins_doc_dep, gstnmos_plugin],
  install: false,
)

avsynctest_doc = hotdoc.generate_doc(
  'avsynctest',
  project_version: meson.project_version(),
  sitemap: cdir / 'plugins' / 'sitemap.txt',
  index: cdir / 'plugins' / 'avsynctest' / 'index.md',
  gst_index: cdir / 'plugins' / 'avsynctest' / 'index.md',
  gst_smart_index: true,
  gst_c_sources: gst_avsynctest_rs_globs,
  gst_cache_file: build_plugins_cache,
  gst_plugin_name: 'avsynctest',
  dependencies: gst_dep,
  depends: [gst_plugins_doc_dep, gstavsynctest_plugin],
  install: false,
)

sitemap_gen = find_program(cdir / 'scripts' / 'generate_sitemap.py')

# Publish the repository usage guide directly instead of maintaining a
# Hotdoc-only copy.
configure_file(
  input: rust_root / 'gst-nmos-rs' / 'README.md',
  output: 'gst-nmos-rs.md',
  copy: true,
)

portal_sitemap = custom_target(
  'portal-sitemap',
  command: [
    sitemap_gen,
    '--input-sitemap', cdir / 'portal' / 'sitemap.txt',
    '--output-sitemap', '@OUTPUT@',
    '--markdown-index', 'index.md',
    '--plugins', nmos_doc.full_path() + pathsep + avsynctest_doc.full_path(),
  ],
  depends: [nmos_doc, avsynctest_doc],
  output: 'portal-sitemap.txt',
)

html_theme = 'https://github.com/hotdoc/hotdoc_lumen_theme/releases/download/0.16/hotdoc_lumen_theme-0.16.tar.xz?sha256=b7d7dde51285d1c90836c44ae298754e4cfa957e9a6d14ee5844b8a2cac04b5a'

nvnmos_gstreamer_doc = hotdoc.generate_doc(
  'NvNmos-GStreamer',
  project_version: meson.project_version(),
  sitemap: portal_sitemap,
  index: cdir / 'portal' / 'index.md',
  gst_index: cdir / 'portal' / 'plugins_doc.md',
  html_theme: html_theme,
  html_extra_theme: cdir / 'theme' / 'extra',
  syntax_highlighting_activate: true,
  gst_list_plugins_page: 'gst-index',
  subprojects: [nmos_doc, avsynctest_doc],
  depends: [portal_sitemap, nmos_doc, avsynctest_doc],
  install: false,
)

alias_target('gstreamer-doc', nvnmos_gstreamer_doc)
