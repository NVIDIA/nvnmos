// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=NVNMOS_LIB_DIR");
    println!("cargo:rerun-if-env-changed=NVNMOS_INCLUDE_DIR");
    println!("cargo:rerun-if-changed=wrapper.h");

    // Header search path.
    //
    // `NVNMOS_INCLUDE_DIR` overrides the default. When unset we use `../src/`
    // relative to the crate root — i.e. the in-tree location of `nvnmos.h`.
    let include_dir = env::var("NVNMOS_INCLUDE_DIR").unwrap_or_else(|_| {
        let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
        PathBuf::from(manifest_dir)
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace layout: expected ../../src/ relative to crate")
            .join("src")
            .to_string_lossy()
            .into_owned()
    });
    println!("cargo:rerun-if-changed={}/nvnmos.h", include_dir);
    println!("cargo:include={}", include_dir);

    // Link strategy.
    //
    // `NVNMOS_LIB_DIR`, when set, is prepended to the linker search path.
    // Otherwise we rely on the system default paths (`/usr/local/lib`,
    // `/usr/lib`, etc.).
    if let Ok(lib_dir) = env::var("NVNMOS_LIB_DIR") {
        println!("cargo:rustc-link-search=native={}", lib_dir);
    }
    println!("cargo:rustc-link-lib=dylib=nvnmos");

    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg(format!("-I{}", include_dir))
        .derive_default(true)
        .derive_debug(true)
        .generate_comments(true)
        .allowlist_function("nmos_.*")
        .allowlist_function("(create|destroy)_nmos_.*")
        .allowlist_function("(add|remove)_nmos_.*")
        .allowlist_type("NvNmos.*")
        .allowlist_var("NVNMOS.*")
        .generate()
        .expect("failed to generate libnvnmos bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("failed to write bindings.rs");
}
