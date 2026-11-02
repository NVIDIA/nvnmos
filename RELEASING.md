<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Releasing NvNmos

The C library, the daemon and its gRPC API, and the GStreamer plugins share one version number, which follows [Semantic Versioning](https://semver.org/).
The version is declared in two places, which must match:

- `NVNMOS_VERSION_MAJOR`, `NVNMOS_VERSION_MINOR`, `NVNMOS_VERSION_PATCH`, `NVNMOS_VERSION_PRERELEASE` and `NVNMOS_VERSION_STRING` in `src/nvnmos.h`.
- `version` in the `[workspace.package]` section of `rust/Cargo.toml`.

The CMake configure step fails when `NVNMOS_VERSION_STRING` does not match the other macros.
The `nvnmos-sys` build and the `version-check` CI job fail when `src/nvnmos.h` and `rust/Cargo.toml` declare different versions.
For a `v*` tag, the `version-check` job also fails when the tag does not match the declared version.

## Branches and Versions

`main` always carries a pre-release identifier, such as `0.2.0-alpha`.

Each minor release has a release branch, such as `release/v0.1.x`.
A release commit on that branch removes the pre-release identifier, and the release tag, such as `v0.1.0`, points to that commit.

## Changing the Version

1. In `src/nvnmos.h`, set the `NVNMOS_VERSION_*` macros. `NVNMOS_VERSION_PRERELEASE` is `""` for a release, and `NVNMOS_VERSION_STRING` is the full version, such as `"0.1.0"` or `"0.2.0-alpha"`.
2. In `rust/Cargo.toml`, set `version` in `[workspace.package]` to the same string.
3. Run `cargo update --workspace` in `rust/` so that `rust/Cargo.lock` records the new version. CI builds with `--locked`, so it fails if the lockfile is out of date.

## Making a Minor Release

These steps use 0.1.0 as an example.

1. Check that `doc/user/migration.md` has a "Migrating to 0.1.0" section that describes the changes applications need to make.
2. Create `release/v0.1.x` from `main`.
3. On `release/v0.1.x`, commit the version change from `0.1.0-alpha` to `0.1.0`.
4. When CI passes on that commit, tag it `v0.1.0`, and push the tag.
5. Create a GitHub Release from the tag.
6. On `main`, commit the version change from `0.1.0-alpha` to `0.2.0-alpha`.

## Making a Patch Release

The release branch keeps the released version until a fix needs to be added.

1. Before adding the first fix after a release, commit the version change on the release branch to the next patch pre-release, such as `0.1.1-alpha`. Builds from the release branch then sort after `0.1.0` and are marked as pre-release.
2. Add the fixes, usually by cherry-picking them from `main`.
3. Commit the version change to `0.1.1`, then tag, push and create the GitHub Release as for a minor release.
