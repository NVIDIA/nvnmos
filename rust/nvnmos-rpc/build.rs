// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

fn main() {
    // Use the protoc that `protobuf-src` ships, so the build doesn't depend
    // on a system-installed `protoc`.
    unsafe {
        std::env::set_var("PROTOC", protobuf_src::protoc());
    }

    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&["proto/nvnmosd.proto"], &["proto"])
        .expect("failed to compile nvnmosd.proto");

    println!("cargo:rerun-if-changed=proto/nvnmosd.proto");
}
