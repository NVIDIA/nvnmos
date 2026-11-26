---
short-description: Phase-locked A/V test sources and sync analyzer
...

<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# avsynctest

Phase-locked audio and video test sources for measuring end-to-end A/V
synchronisation. Both sources derive their content purely from running time, so
they are aligned by construction and any skew observed downstream was introduced
by the pipeline under test.


## Measuring A/V sync

Link received video, audio and optional ST 2038 ANC to `avsyncanalyzer`'s
`video`, `audio` and `anc` pads. It detects the bar, beep and caption in the
media itself and reports their offsets in a printed summary and bus messages.
All inputs must share one media timeline. See the `gst-avsynctest-rs` README
for usage and properties.
