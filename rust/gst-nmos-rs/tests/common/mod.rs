// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared helpers for gst-nmos-rs integration tests that spawn `nvnmosd`.

// Each integration test binary pulls in this module but uses a different
// subset, so items unused by a given binary are expected, not dead.
#![allow(dead_code)]

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Once;
use std::thread;
use std::time::{Duration, Instant};

use gstreamer as gst;

static REGISTER: Once = Once::new();

pub fn init() {
    REGISTER.call_once(|| {
        ensure_gst_plugin_path();
        // Smoke scripts clear this to avoid a stale system `nmos` plugin; core
        // elements (audiotestsrc, audiomixer, …) still need the system registry.
        if std::env::var("GST_PLUGIN_SYSTEM_PATH")
            .map(|s| s.is_empty())
            .unwrap_or(false)
        {
            // SAFETY: called once before any other threads exist (Once).
            unsafe { std::env::remove_var("GST_PLUGIN_SYSTEM_PATH") };
        }
        gst::init().expect("gst::init");
    });
}

fn ensure_gst_plugin_path() {
    if std::env::var("NVNMOS_SKIP_PLUGIN_PATH_AUTO").is_ok() {
        return;
    }
    let target_dir = std::env::var("CARGO_TARGET_DIR").ok().unwrap_or_else(|| {
        let manifest =
            std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo");
        PathBuf::from(manifest)
            .parent()
            .expect("manifest parent")
            .join("target")
            .to_string_lossy()
            .into_owned()
    });
    for profile in ["debug", "release"] {
        let profile_dir = PathBuf::from(&target_dir).join(profile);
        let deps_so = profile_dir.join("deps").join("libgstnmos.so");
        let profile_so = profile_dir.join("libgstnmos.so");
        let mut plugin_dirs = Vec::new();
        if deps_so.exists() {
            plugin_dirs.push(profile_dir.join("deps"));
        } else if profile_so.exists() {
            plugin_dirs.push(profile_dir);
        }
        if plugin_dirs.is_empty() {
            continue;
        }
        let mut cur = std::env::var("GST_PLUGIN_PATH").unwrap_or_default();
        for plugin_dir in plugin_dirs {
            let prefix = plugin_dir.to_string_lossy();
            if cur.split(':').any(|p| p == prefix.as_ref()) {
                continue;
            }
            cur = if cur.is_empty() {
                prefix.into_owned()
            } else {
                format!("{prefix}:{cur}")
            };
        }
        // SAFETY: called once before gst::init / other threads (Once).
        unsafe { std::env::set_var("GST_PLUGIN_PATH", cur) };
        break;
    }
}

pub fn require_factories(names: &[&str]) {
    let missing: Vec<&str> = names
        .iter()
        .filter(|n| gst::ElementFactory::find(n).is_none())
        .copied()
        .collect();
    assert!(
        missing.is_empty(),
        "missing element factories: {missing:?}; set `GST_PLUGIN_PATH` to include \
         `libgstnmos.so` (built in this workspace's `target/debug` or `target/release`)",
    );
}

/// Locate the `nvnmosd` binary. Prefer `NVNMOSD_BIN`, else
/// `${CARGO_TARGET_DIR:-<manifest>/../target}/{debug,release}/nvnmosd`.
pub fn nvnmosd_bin() -> PathBuf {
    if let Ok(p) = std::env::var("NVNMOSD_BIN") {
        return PathBuf::from(p);
    }
    let target_dir = std::env::var("CARGO_TARGET_DIR").ok().unwrap_or_else(|| {
        let manifest =
            std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo");
        PathBuf::from(manifest)
            .parent()
            .expect("manifest parent")
            .join("target")
            .to_string_lossy()
            .into_owned()
    });
    let debug = PathBuf::from(&target_dir).join("debug").join("nvnmosd");
    if debug.exists() {
        return debug;
    }
    PathBuf::from(target_dir).join("release").join("nvnmosd")
}

/// Directory holding `libnvnmos.so`, searched on `LD_LIBRARY_PATH` then
/// `NVNMOS_LIB_DIR`. `None` when the C library has not been built/placed.
pub fn libnvnmos_dir() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(paths) = std::env::var("LD_LIBRARY_PATH") {
        dirs.extend(
            paths
                .split(':')
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
        );
    }
    if let Ok(dir) = std::env::var("NVNMOS_LIB_DIR") {
        dirs.push(PathBuf::from(dir));
    }
    dirs.into_iter().find(|d| d.join("libnvnmos.so").exists())
}

const SHM_MXL_HINT: &str = "MXL uses mkdtemp(3) under /dev/shm; run integration tests \
    on Linux with tmpfs (/dev/shm)";

/// `Some(reason)` when MXL's `mkdtemp(3)` domains under `/dev/shm` are
/// unavailable (non-Linux, or a sandbox blocking tmpfs); `None` when usable.
pub fn dev_shm_mkdtemp_skip_reason() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CStr;

        let mut path = b"/dev/shm/nvnmos_mxl_shm_probeXXXXXX\0".to_vec();
        let created = unsafe {
            unsafe extern "C" {
                fn mkdtemp(template: *mut i8) -> *mut i8;
            }
            mkdtemp(path.as_mut_ptr() as *mut i8)
        };
        if created.is_null() {
            let err = std::io::Error::last_os_error();
            return Some(format!(
                "mkdtemp(3) on /dev/shm failed: {err}; {SHM_MXL_HINT}"
            ));
        }
        let dir = unsafe { CStr::from_ptr(created) };
        std::fs::remove_dir_all(dir.to_str().expect("mkdtemp path")).ok();
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        Some(format!("not Linux; {SHM_MXL_HINT}"))
    }
}

/// Search `LD_LIBRARY_PATH` (and `NVNMOS_LIB_DIR` when set) for `libmxl.so`.
fn libmxl_so_path() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(paths) = std::env::var("LD_LIBRARY_PATH") {
        dirs.extend(
            paths
                .split(':')
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
        );
    }
    if let Ok(dir) = std::env::var("NVNMOS_LIB_DIR") {
        dirs.push(PathBuf::from(dir));
    }
    dirs.into_iter()
        .map(|d| d.join("libmxl.so"))
        .find(|p| p.exists())
}

/// `Some(reason)` when `libmxl.so` cannot be loaded (missing from the loader
/// search path, or present but not dlopen-able — e.g. `internal/` libraries not
/// found). `None` when the MXL runtime is loadable.
pub fn libmxl_skip_reason() -> Option<String> {
    let Some(so_path) = libmxl_so_path() else {
        return Some(
            "libmxl.so not found via LD_LIBRARY_PATH or NVNMOS_LIB_DIR; \
             build MXL and export its lib dir (and lib/internal) on LD_LIBRARY_PATH"
                .into(),
        );
    };
    let dir = so_path.parent().expect("libmxl.so parent");
    let internal = dir.join("internal");
    let mut ld_path = format!("{}:{}", internal.display(), dir.display());
    if let Ok(existing) = std::env::var("LD_LIBRARY_PATH") {
        if !existing.is_empty() {
            ld_path = format!("{ld_path}:{existing}");
        }
    }
    // SAFETY: single-threaded skip probe before any MXL threads exist.
    unsafe { std::env::set_var("LD_LIBRARY_PATH", &ld_path) };

    let cpath = match std::ffi::CString::new(so_path.to_string_lossy().into_owned()) {
        Ok(p) => p,
        Err(_) => return Some("libmxl.so path contains an interior NUL byte".into()),
    };
    unsafe {
        unsafe extern "C" {
            fn dlopen(filename: *const i8, flag: i32) -> *mut std::ffi::c_void;
            fn dlclose(handle: *mut std::ffi::c_void) -> i32;
        }
        const RTLD_LAZY: i32 = 1;
        let handle = dlopen(cpath.as_ptr(), RTLD_LAZY);
        if handle.is_null() {
            let err = std::io::Error::last_os_error();
            return Some(format!(
                "dlopen `{}` failed: {err}; ensure LD_LIBRARY_PATH includes the libmxl \
                 directory and its internal/ backend dir",
                so_path.display()
            ));
        }
        dlclose(handle);
    }
    None
}

/// Combined MXL runtime prerequisites for integration tests that reach PLAYING
/// with `mxlsink` / `mxlsrc` (not just element-factory registration).
pub fn mxl_runtime_skip_reason() -> Option<String> {
    dev_shm_mkdtemp_skip_reason().or_else(libmxl_skip_reason)
}

/// Reason the `nvnmosd`-backed integration tests cannot run, or `None` when the
/// prerequisites are present. `nvnmosd` itself is built by `cargo test`, but it
/// links `libnvnmos.so` from the C build, which CI exposes on `LD_LIBRARY_PATH`.
/// On a Rust-only checkout the caller skips rather than fails.
pub fn nvnmosd_skip_reason() -> Option<String> {
    let bin = nvnmosd_bin();
    if !bin.exists() {
        return Some(format!("nvnmosd not built at `{}`", bin.display()));
    }
    if libnvnmos_dir().is_none() {
        return Some("libnvnmos.so not found via LD_LIBRARY_PATH or NVNMOS_LIB_DIR".into());
    }
    None
}

/// Spawns `nvnmosd` on a UDS socket and kills it on drop.
pub struct DaemonGuard {
    child: Child,
    socket: PathBuf,
}

impl DaemonGuard {
    pub fn new(socket: PathBuf) -> Self {
        let bin = nvnmosd_bin();
        assert!(
            bin.exists(),
            "nvnmosd binary not found at `{}`; build with `cargo build -p nvnmosd` \
             or set NVNMOSD_BIN",
            bin.display(),
        );

        let _ = std::fs::remove_file(&socket);
        let mut command = Command::new(&bin);
        command
            .arg("--uds")
            .arg(&socket)
            .env(
                "RUST_LOG",
                std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
            )
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        // nvnmosd links libnvnmos.so; surface NVNMOS_LIB_DIR to the loader even
        // when the caller set only it (not LD_LIBRARY_PATH).
        if let Ok(lib_dir) = std::env::var("NVNMOS_LIB_DIR") {
            let existing = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
            let value = if existing.is_empty() {
                lib_dir
            } else {
                format!("{lib_dir}:{existing}")
            };
            command.env("LD_LIBRARY_PATH", value);
        }
        let mut child = command
            .spawn()
            .unwrap_or_else(|e| panic!("spawn `{}`: {e}", bin.display()));

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if socket.exists() {
                return Self { child, socket };
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!(
            "nvnmosd UDS `{}` did not appear within 5s; check LD_LIBRARY_PATH includes libnvnmos",
            socket.display(),
        );
    }

    pub fn uri(&self) -> String {
        format!("unix:{}", self.socket.display())
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Goertzel magnitude-squared at `target_hz` for mono `samples`.
pub fn goertzel_power(samples: &[f32], sample_rate: f32, target_hz: f32) -> f64 {
    let n = samples.len();
    if n == 0 {
        return 0.0;
    }
    let k = (0.5 + (n as f64) * f64::from(target_hz) / f64::from(sample_rate)).floor() as usize;
    let omega = 2.0 * std::f64::consts::PI * k as f64 / n as f64;
    let cosine = omega.cos();
    let coeff = 2.0 * cosine;
    let mut s0 = 0.0f64;
    let mut s1 = 0.0f64;
    for &sample in samples {
        let x = f64::from(sample);
        let s2 = x + coeff * s1 - s0;
        s0 = s1;
        s1 = s2;
    }
    let real = s0 - s1 * cosine;
    let imag = s1 * omega.sin();
    real * real + imag * imag
}

/// De-interleave F32LE stereo to mono (average L/R).
pub fn stereo_f32le_to_mono(data: &[u8]) -> Vec<f32> {
    let mut mono = Vec::with_capacity(data.len() / 8);
    let mut chunks = data.chunks_exact(8);
    for frame in chunks.by_ref() {
        let l = f32::from_le_bytes(frame[0..4].try_into().unwrap());
        let r = f32::from_le_bytes(frame[4..8].try_into().unwrap());
        mono.push(0.5 * (l + r));
    }
    mono
}

/// A4 concert pitch (440 Hz).
pub const A4_HZ: f32 = 440.0;

/// Perfect fifth above `base_hz` (seven semitones, equal temperament).
pub fn perfect_fifth_hz(base_hz: f32) -> f32 {
    (f64::from(base_hz) * 2.0_f64.powf(7.0 / 12.0)) as f32
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Low,  // A4
    High, // E5 (perfect fifth above Low)
}

impl Tone {
    pub fn hz(self) -> f32 {
        match self {
            Tone::Low => A4_HZ,
            Tone::High => perfect_fifth_hz(A4_HZ),
        }
    }

    pub fn dominant_in(self, samples: &[f32], sample_rate: f32) -> bool {
        let p_low = goertzel_power(samples, sample_rate, Self::Low.hz());
        let p_high = goertzel_power(samples, sample_rate, Self::High.hz());
        match self {
            Tone::Low => p_low > p_high * 4.0,
            Tone::High => p_high > p_low * 4.0,
        }
    }
}
