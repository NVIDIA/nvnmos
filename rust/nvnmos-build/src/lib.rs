// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Build-script helper that sets the NvNmos version of the crate being built.

use std::env;
use std::path::Path;
use std::process::Command;

/// Sets the `NVNMOS_VERSION` environment variable for the crate being built.
///
/// The value is the crate version. When the crate is built from a Git
/// checkout, the version ends with the commit as Semantic Versioning build
/// metadata, such as `0.1.0-alpha+g0123abc`. A build without a Git checkout,
/// such as a Docker build, can set the build metadata in the
/// `NVNMOS_VERSION_BUILD` environment variable instead.
pub fn set_version_env() {
    let mut version = env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION not set");
    println!("cargo:rerun-if-env-changed=NVNMOS_VERSION_BUILD");
    if let Some(build) = env::var("NVNMOS_VERSION_BUILD")
        .ok()
        .filter(|build| !build.is_empty())
    {
        assert!(
            build.split('.').all(|id| {
                !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            }),
            "NVNMOS_VERSION_BUILD is not Semantic Versioning build metadata: {build}"
        );
        version.push('+');
        version.push_str(&build);
    } else if let Some(commit) = git(&["rev-parse", "--short=7", "HEAD"]) {
        version.push_str("+g");
        version.push_str(&commit);

        // Rerun when HEAD moves, so the commit stays current.
        let mut git_files = vec![git(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "HEAD",
        ])];
        if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
            git_files.push(git(&[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                &branch,
            ]));
        }
        for path in git_files.into_iter().flatten() {
            if Path::new(&path).exists() {
                println!("cargo:rerun-if-changed={path}");
            }
        }
    }
    println!("cargo:rustc-env=NVNMOS_VERSION={version}");
}

fn git(args: &[&str]) -> Option<String> {
    let dir = env::var("CARGO_MANIFEST_DIR").ok()?;
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let output = String::from_utf8(output.stdout).ok()?;
    let output = output.trim();
    (!output.is_empty()).then(|| output.to_owned())
}
