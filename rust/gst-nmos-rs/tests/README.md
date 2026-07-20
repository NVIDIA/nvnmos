<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# gst-nmos-rs Integration Testing

## Sync Testing

[`av_sync.rs`](av_sync.rs) is an end-to-end integration test that drives
[`gst-avsynctest-rs`](../../gst-avsynctest-rs)'s `avsyncvideotestsrc` and
`avsyncaudiotestsrc` through real NMOS Senders and Receivers. It verifies that
video, audio pip, and CEA-708 caption alignment survive the round trip.

The video's ancillary data contains a frame index and a phase-locked TICK/TOCK
caption. The test splits it into its own Sender with `st2038extractor` and
re-attaches it on the Receiver side with `st2038combiner`.

The test runs once per transport:

- `av_sync_via_mxl`
- `av_sync_via_udp`
- `av_sync_via_nvdsudp`

Each case negotiates the essence format expected by its transport: `v210` and
`F32LE` for MXL, or `UYVP` and `S24BE` for RTP/UDP.

Run the complete test target from `rust/`:

```sh
cargo test -p gst-nmos-rs --test av_sync -- --test-threads=1
```

Each case self-skips when a prerequisite is missing:

- `libnvnmos` and `nvnmosd` configured as described in the
  [workspace quick start](../../README.md)
- the transport's element factories on `GST_PLUGIN_PATH`
- `/dev/shm` for MXL
- current `st2038combiner` (`drop-late-st2038`) and `rtpsmpte291depay` builds
  for the MXL and UDP cases
- the DeepStream and Rivermax stack for `nvdsudp`
