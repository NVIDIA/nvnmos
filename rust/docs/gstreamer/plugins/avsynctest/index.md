---
short-description: Phase-locked A/V test sources GStreamer plugin
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
