// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Scale smoke client for [`nvnmosd`].
//!
//! Drives one scenario (node / session / resource counts), records per-RPC
//! latencies (including explicit `RemoveResource`, separate from `CloseSession`),
//! optional daemon RSS samples, and per-phase CPU utilization (Linux `/proc`
//! background sampling), and prints one JSON line.
//!
//! See `doc/designs/nvnmosd/scale-smoke.md` for the full matrix and presets.
//! Usually invoked by `scripts/run-nvnmosd-scale-smoke.sh`.

use std::collections::HashMap;
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Parser;
use nvnmos_rpc::v1::nvnmos_daemon_client::NvnmosDaemonClient;
use nvnmos_rpc::v1::{
    AckActivationRequest, ActivationEvent, AddReceiverRequest, AddSenderRequest,
    CloseSessionRequest, NetworkServicesConfig, NodeConfig, OpenSessionRequest,
    RemoveResourceRequest, Side as ProtoSide, SubscribeActivationsRequest,
    SyncResourceStateRequest, Transport as ProtoTransport,
};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UnixStream};
use tokio::sync::{mpsc as tokio_mpsc, Mutex, Semaphore};
use tokio::task::JoinSet;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

/// Typo guards for scale axes (not product limits).
const MAX_NODES: usize = 10_000;
const MAX_SENDERS: usize = 100_000;
const MAX_RECEIVERS: usize = 100_000;
const MAX_SESSIONS: usize = 10_000;
const MAX_CLIENTS: usize = 1_000;
const MAX_SYNCS: usize = 100_000;
const MAX_PATCHES: usize = 100_000;

#[derive(Parser, Debug)]
#[command(version, about = "nvnmosd scale smoke / benchmark client")]
struct Args {
    #[arg(long, env = "NVNMOSD_UDS", default_value = "/tmp/nvnmosd.sock")]
    uds: PathBuf,

    /// Optional label echoed in JSON output (preset name).
    #[arg(long)]
    label: Option<String>,

    /// Daemon PID for VmRSS and CPU sampling via /proc (Linux).
    #[arg(long, env = "NVNMOSD_PID")]
    daemon_pid: Option<u32>,

    /// Background CPU sample interval during each bench phase (milliseconds).
    #[arg(long, default_value_t = 100)]
    cpu_sample_ms: u64,

    #[arg(long, default_value_t = 1)]
    nodes: usize,

    /// Senders to register via `AddSender` (`--senders` alias).
    #[arg(long = "add-senders", alias = "senders", default_value_t = 5)]
    add_senders: usize,

    /// Receivers to register via `AddReceiver` (`--receivers` alias).
    #[arg(long = "add-receivers", alias = "receivers", default_value_t = 5)]
    add_receivers: usize,

    /// Explicit `RemoveResource` for senders before `CloseSession` (`0` = skip; default).
    /// Pass the add count to remove all senders on that side before session close.
    #[arg(long = "remove-senders", default_value_t = 0)]
    remove_senders: usize,

    /// Explicit `RemoveResource` for receivers before `CloseSession` (`0` = skip; default).
    /// Pass the add count to remove all receivers on that side before session close.
    #[arg(long = "remove-receivers", default_value_t = 0)]
    remove_receivers: usize,

    /// Concurrent gRPC sessions; resources round-robin across sessions (`resource_index %
    /// sessions`). When equal to `nodes`, one session per node; when equal to add-senders +
    /// add-receivers, one session per resource.
    #[arg(long, default_value_t = 1)]
    sessions: usize,

    #[arg(long, default_value_t = 18080)]
    base_http_port: u16,

    #[arg(long, env = "NVNMOSD_BENCH_INTERFACE_IP")]
    interface_ip: Option<String>,

    /// Out-of-band `SyncResourceState` workflows (`0` = none). Evenly spaced targets when
    /// not more than registered senders; otherwise round-robin through them.
    #[arg(long, default_value_t = 0)]
    syncs: usize,

    /// In-band GET/PATCH activation workflows (`0` = none). Evenly spaced targets when not
    /// more than registered senders; otherwise round-robin through them.
    #[arg(long, default_value_t = 0)]
    patches: usize,

    /// Max concurrent controller-side GET/PATCH HTTP workflows (`0` when `patches == 0`).
    #[arg(long, default_value_t = 0)]
    clients: usize,
}

#[derive(Debug, Serialize)]
struct ScenarioMeta {
    label: Option<String>,
    nodes: usize,
    add_senders: usize,
    add_receivers: usize,
    remove_senders: usize,
    remove_receivers: usize,
    sessions: usize,
    clients: usize,
    syncs: usize,
    patches: usize,
}

#[derive(Debug, Serialize, Default)]
struct MemoryKb {
    baseline: Option<u64>,
    after_open: Option<u64>,
    after_add: Option<u64>,
    after_activate: Option<u64>,
    after_remove: Option<u64>,
    after_close: Option<u64>,
}

/// Latency stats in **milliseconds** (floating point) for JSON output.
#[derive(Debug, Serialize)]
struct LatencyMs {
    count: usize,
    total: f64,
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
}

impl LatencyMs {
    fn from_samples_us(samples_us: Vec<u64>) -> Self {
        if samples_us.is_empty() {
            return Self::zero();
        }
        let mut ms: Vec<f64> = samples_us.iter().map(|&us| us_to_ms(us)).collect();
        ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let count = ms.len();
        let total: f64 = ms.iter().sum();
        Self {
            count,
            total,
            p50: percentile_ms(&ms, 50),
            p95: percentile_ms(&ms, 95),
            p99: percentile_ms(&ms, 99),
            max: *ms.last().unwrap_or(&0.0),
        }
    }

    fn zero() -> Self {
        Self {
            count: 0,
            total: 0.0,
            p50: 0.0,
            p95: 0.0,
            p99: 0.0,
            max: 0.0,
        }
    }
}

fn us_to_ms(us: u64) -> f64 {
    us as f64 / 1000.0
}

fn percentile_ms(sorted_ms: &[f64], pct: u8) -> f64 {
    if sorted_ms.is_empty() {
        return 0.0;
    }
    let idx = ((sorted_ms.len() as f64) * (f64::from(pct) / 100.0)).ceil() as usize;
    sorted_ms[idx.saturating_sub(1).min(sorted_ms.len() - 1)]
}

fn read_rss_kb(pid: u32) -> anyhow::Result<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
        .with_context(|| format!("reading /proc/{pid}/status"))?;
    for line in status.lines() {
        if let Some(kb) = line.strip_prefix("VmRSS:") {
            let kb: u64 = kb.trim().trim_end_matches(" kB").parse()?;
            return Ok(kb);
        }
    }
    anyhow::bail!("VmRSS not found in /proc/{pid}/status");
}

fn sample_memory(pid: Option<u32>) -> Option<u64> {
    pid.and_then(|p| read_rss_kb(p).ok())
}

/// Linux `CLK_TCK` for `/proc/<pid>/stat` jiffies (typically 100).
const LINUX_CLK_TCK: f64 = 100.0;

/// Poll interval for per-phase daemon CPU sampling.
fn cpu_sample_interval(ms: u64) -> Duration {
    Duration::from_millis(ms.max(10))
}

/// Core-equivalent CPU percent between two `/proc/<pid>/stat` samples (may exceed 100).
fn cpu_pct_between(
    prev_utime: u64,
    prev_stime: u64,
    prev_wall: Instant,
    utime: u64,
    stime: u64,
    wall: Instant,
) -> f64 {
    let delta_jiffies = utime.saturating_sub(prev_utime) + stime.saturating_sub(prev_stime);
    let delta_wall_secs = wall.duration_since(prev_wall).as_secs_f64();
    if delta_wall_secs <= 0.0 {
        return 0.0;
    }
    100.0 * delta_jiffies as f64 / (delta_wall_secs * LINUX_CLK_TCK)
}

fn read_proc_cpu_jiffies(pid: u32) -> anyhow::Result<(u64, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .with_context(|| format!("reading /proc/{pid}/stat"))?;
    let rparen = stat
        .rfind(')')
        .context("parsing /proc/stat comm field")?;
    let fields: Vec<&str> = stat[rparen + 2..].split_whitespace().collect();
    anyhow::ensure!(
        fields.len() > 12,
        "short /proc/{pid}/stat (expected utime/stime)"
    );
    let utime: u64 = fields[11].parse().context("utime")?;
    let stime: u64 = fields[12].parse().context("stime")?;
    Ok((utime, stime))
}

#[derive(Debug, Serialize, Default, Clone)]
struct CpuPhasePct {
    avg: Option<f64>,
    max: Option<f64>,
    p95: Option<f64>,
    samples: usize,
}

impl CpuPhasePct {
    fn from_samples(mut samples: Vec<f64>) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let count = samples.len();
        let sum: f64 = samples.iter().sum();
        Self {
            avg: Some(sum / count as f64),
            max: Some(*samples.last().unwrap_or(&0.0)),
            p95: Some(percentile_ms(&samples, 95)),
            samples: count,
        }
    }
}

#[derive(Debug, Default)]
struct CpuPhases {
    open: CpuPhasePct,
    subscribe: Option<CpuPhasePct>,
    add: CpuPhasePct,
    activate_sync: Option<CpuPhasePct>,
    activate_patch: Option<CpuPhasePct>,
    remove: Option<CpuPhasePct>,
    close: CpuPhasePct,
}

#[derive(Debug, Serialize, Default)]
struct CpuPct {
    open: CpuPhasePct,
    #[serde(skip_serializing_if = "Option::is_none")]
    subscribe: Option<CpuPhasePct>,
    add: CpuPhasePct,
    #[serde(skip_serializing_if = "Option::is_none")]
    activate_sync: Option<CpuPhasePct>,
    #[serde(skip_serializing_if = "Option::is_none")]
    activate_patch: Option<CpuPhasePct>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remove: Option<CpuPhasePct>,
    close: CpuPhasePct,
    overall: CpuPhasePct,
}

struct CpuPhaseSampler {
    stop: std::sync::mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<Vec<f64>>>,
}

impl CpuPhaseSampler {
    fn start(pid: u32, interval: Duration) -> Self {
        let (stop, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || cpu_sampler_loop(pid, interval, rx));
        Self {
            stop,
            thread: Some(thread),
        }
    }

    fn stop(mut self) -> Vec<f64> {
        let _ = self.stop.send(());
        self.thread
            .take()
            .and_then(|t| t.join().ok())
            .unwrap_or_default()
    }
}

fn cpu_sampler_loop(
    pid: u32,
    interval: Duration,
    stop: std::sync::mpsc::Receiver<()>,
) -> Vec<f64> {
    let mut samples = Vec::new();
    let Ok((mut prev_utime, mut prev_stime)) = read_proc_cpu_jiffies(pid) else {
        return samples;
    };
    let mut prev_wall = Instant::now();
    loop {
        match stop.recv_timeout(interval) {
            Ok(()) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
        let wall = Instant::now();
        let Ok((utime, stime)) = read_proc_cpu_jiffies(pid) else {
            break;
        };
        let pct = cpu_pct_between(prev_utime, prev_stime, prev_wall, utime, stime, wall);
        if pct.is_finite() && pct >= 0.0 {
            samples.push(pct);
        }
        prev_utime = utime;
        prev_stime = stime;
        prev_wall = wall;
    }
    samples
}

struct CpuMonitor {
    pid: Option<u32>,
    interval: Duration,
    overall_samples: Vec<f64>,
    active: Option<CpuPhaseSampler>,
}

impl CpuMonitor {
    fn new(pid: Option<u32>, interval: Duration) -> Self {
        Self {
            pid,
            interval,
            overall_samples: Vec::new(),
            active: None,
        }
    }

    fn begin_phase(&mut self) {
        if let Some(pid) = self.pid {
            self.active = Some(CpuPhaseSampler::start(pid, self.interval));
        }
    }

    fn end_phase(&mut self) -> CpuPhasePct {
        let samples = self
            .active
            .take()
            .map(CpuPhaseSampler::stop)
            .unwrap_or_default();
        self.overall_samples.extend(samples.iter().copied());
        CpuPhasePct::from_samples(samples)
    }

    fn into_report(self, phases: CpuPhases) -> Option<CpuPct> {
        self.pid?;
        let overall = CpuPhasePct::from_samples(self.overall_samples);
        Some(CpuPct {
            open: phases.open,
            subscribe: phases.subscribe,
            add: phases.add,
            activate_sync: phases.activate_sync,
            activate_patch: phases.activate_patch,
            remove: phases.remove,
            close: phases.close,
            overall,
        })
    }
}

#[derive(Debug, Serialize)]
struct BenchReport {
    scenario: ScenarioMeta,
    memory_kb: MemoryKb,
    #[serde(skip_serializing_if = "Option::is_none")]
    cpu_pct: Option<CpuPct>,
    open_session_ms: LatencyMs,
    subscribe_activations_ms: LatencyMs,
    add_sender_ms: LatencyMs,
    add_receiver_ms: LatencyMs,
    sync_activate_ms: LatencyMs,
    sync_deactivate_ms: LatencyMs,
    connection_get_ms: LatencyMs,
    patch_activate_ms: LatencyMs,
    patch_deactivate_ms: LatencyMs,
    remove_sender_ms: LatencyMs,
    remove_receiver_ms: LatencyMs,
    close_session_ms: LatencyMs,
    wall_ms: f64,
}

struct SessionSlot {
    session_handle: String,
    http_port: u16,
}

struct SessionContext {
    index: usize,
    slot: SessionSlot,
    client: NvnmosDaemonClient<Channel>,
    activation_hub: Option<Arc<ActivationHub>>,
    ack_task: Option<tokio::task::JoinHandle<()>>,
}

/// Node-side demux: match `ActivationEvent`s by `(resource_handle, active)` so
/// concurrent controller PATCH workflows targeting the same session do not steal
/// each other's subscription events.
struct ActivationHub {
    state: Mutex<ActivationHubState>,
}

#[derive(Default)]
struct ActivationHubState {
    /// Events that arrived before `wait` registered.
    pending: HashMap<ActivationWaitKey, u32>,
    waiters: HashMap<ActivationWaitKey, Vec<tokio::sync::oneshot::Sender<()>>>,
}

#[derive(Clone, Hash, Eq, PartialEq)]
struct ActivationWaitKey {
    resource_handle: String,
    active: bool,
}

impl ActivationHub {
    fn new() -> (Arc<Self>, tokio_mpsc::Sender<ActivationEvent>) {
        let (event_tx, mut event_rx) = tokio_mpsc::channel(256);
        let hub = Arc::new(Self {
            state: Mutex::new(ActivationHubState::default()),
        });
        let dispatch_hub = hub.clone();
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                dispatch_hub.deliver(event).await;
            }
        });
        (hub, event_tx)
    }

    async fn deliver(&self, event: ActivationEvent) {
        let side = ProtoSide::try_from(event.side).unwrap_or(ProtoSide::Unspecified);
        if side != ProtoSide::Sender {
            return;
        }
        let key = ActivationWaitKey {
            resource_handle: event.resource_handle,
            active: event.transport_file.is_some(),
        };
        let mut st = self.state.lock().await;
        if let Some(waiters) = st.waiters.remove(&key) {
            for tx in waiters {
                let _ = tx.send(());
            }
        } else {
            *st.pending.entry(key).or_insert(0) += 1;
        }
    }

    async fn wait(
        &self,
        resource_handle: &str,
        active: bool,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        let key = ActivationWaitKey {
            resource_handle: resource_handle.to_owned(),
            active,
        };
        let rx = {
            let mut st = self.state.lock().await;
            if let Some(count) = st.pending.get_mut(&key) {
                if *count > 0 {
                    *count -= 1;
                    if *count == 0 {
                        st.pending.remove(&key);
                    }
                    return Ok(());
                }
            }
            let (tx, rx) = tokio::sync::oneshot::channel();
            st.waiters.entry(key).or_default().push(tx);
            rx
        };
        tokio::time::timeout(timeout, rx)
            .await
            .context("timed out waiting for ActivationEvent")?
            .context("activation waiter dropped")?;
        Ok(())
    }
}

#[derive(Clone)]
struct ResourceSlot {
    session_index: usize,
    resource_handle: String,
    resource_id: String,
    sdp: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let args = Args::parse();
    anyhow::ensure!(
        (1..=MAX_NODES).contains(&args.nodes),
        "nodes must be 1..={MAX_NODES}"
    );
    anyhow::ensure!(
        args.add_senders <= MAX_SENDERS,
        "add-senders must be <= {MAX_SENDERS}"
    );
    anyhow::ensure!(
        args.add_receivers <= MAX_RECEIVERS,
        "add-receivers must be <= {MAX_RECEIVERS}"
    );
    anyhow::ensure!(
        args.remove_senders <= MAX_SENDERS,
        "remove-senders must be <= {MAX_SENDERS}"
    );
    anyhow::ensure!(
        args.remove_receivers <= MAX_RECEIVERS,
        "remove-receivers must be <= {MAX_RECEIVERS}"
    );
    anyhow::ensure!(
        args.remove_senders <= args.add_senders,
        "remove-senders ({}) cannot exceed add-senders ({})",
        args.remove_senders,
        args.add_senders,
    );
    anyhow::ensure!(
        args.remove_receivers <= args.add_receivers,
        "remove-receivers ({}) cannot exceed add-receivers ({})",
        args.remove_receivers,
        args.add_receivers,
    );
    anyhow::ensure!(
        args.syncs <= MAX_SYNCS,
        "syncs must be <= {MAX_SYNCS}"
    );
    anyhow::ensure!(
        args.patches <= MAX_PATCHES,
        "patches must be <= {MAX_PATCHES}"
    );
    if args.syncs > 0 && args.add_senders == 0 {
        anyhow::bail!("syncs > 0 requires at least one registered sender");
    }
    if args.patches > 0 && args.add_senders == 0 {
        anyhow::bail!("patches > 0 requires at least one registered sender");
    }
    anyhow::ensure!(
        args.clients <= MAX_CLIENTS,
        "clients must be <= {MAX_CLIENTS}"
    );
    if args.patches > 0 {
        anyhow::ensure!(args.clients >= 1, "patches > 0 requires clients >= 1");
    }
    anyhow::ensure!(
        args.sessions <= MAX_SESSIONS,
        "sessions must be <= {MAX_SESSIONS}"
    );
    if args.add_senders > 0 || args.add_receivers > 0 {
        anyhow::ensure!(args.sessions >= 1, "sessions must be >= 1 when registering resources");
    }
    anyhow::ensure!(
        u32::from(args.base_http_port) + args.nodes as u32 <= u16::MAX as u32,
        "base_http_port + nodes overflows port range",
    );

    let iface_ip = match args.interface_ip.clone() {
        Some(ip) => ip,
        None => autodetect_interface_ip().context("interface IP (set --interface-ip)")?,
    };
    let wall_start = Instant::now();
    let mut memory = MemoryKb {
        baseline: sample_memory(args.daemon_pid),
        ..Default::default()
    };
    let mut cpu = CpuMonitor::new(args.daemon_pid, cpu_sample_interval(args.cpu_sample_ms));

    let node_seeds: Vec<String> = (0..args.nodes)
        .map(|i| format!("bench-node-{i}"))
        .collect();

    // -------- Open sessions (one UDS/gRPC client per session, in parallel) --------
    cpu.begin_phase();
    let mut open_set = JoinSet::new();
    for s in 0..args.sessions {
        let uds = args.uds.clone();
        let node_index = session_node_index(s, args.nodes);
        let http_port = args.base_http_port + node_index as u16;
        let node_seed = node_seeds[node_index].clone();
        open_set.spawn(async move {
            let t0 = Instant::now();
            let channel = connect_uds(&uds).await?;
            let mut client = NvnmosDaemonClient::new(channel);
            let resp = client
                .open_session(OpenSessionRequest {
                    node_config: Some(bench_node_config(&node_seed, http_port)),
                })
                .await
                .with_context(|| format!("OpenSession node={node_index} session={s}"))?
                .into_inner();
            Ok::<_, anyhow::Error>((
                s,
                t0.elapsed().as_micros() as u64,
                SessionContext {
                    index: s,
                    slot: SessionSlot {
                        session_handle: resp.session_handle,
                        http_port,
                    },
                    client,
                    activation_hub: None,
                    ack_task: None,
                },
            ))
        });
    }

    let mut open_samples = Vec::with_capacity(args.sessions);
    let mut open_results: Vec<(usize, u64, SessionContext)> = Vec::with_capacity(args.sessions);
    while let Some(res) = open_set.join_next().await {
        open_results.push(res.context("OpenSession task panicked")??);
    }
    open_results.sort_by_key(|(index, _, _)| *index);
    let mut sessions = Vec::with_capacity(args.sessions);
    for (index, sample, ctx) in open_results {
        anyhow::ensure!(index == sessions.len(), "unexpected OpenSession index");
        open_samples.push(sample);
        sessions.push(ctx);
    }
    let cpu_open = cpu.end_phase();
    memory.after_open = sample_memory(args.daemon_pid);

    // -------- Subscribe (PATCH path, parallel per session) --------
    let mut subscribe_samples = Vec::new();
    let mut cpu_subscribe = None;
    if args.patches > 0 {
        cpu.begin_phase();
        let mut sub_set = JoinSet::new();
        for ctx in &mut sessions {
            let client = ctx.client.clone();
            let session_handle = ctx.slot.session_handle.clone();
            let index = ctx.index;
            sub_set.spawn(async move {
                let t0 = Instant::now();
                let (task, hub) = spawn_auto_ack_task(client, session_handle).await?;
                Ok::<_, anyhow::Error>((
                    index,
                    t0.elapsed().as_micros() as u64,
                    hub,
                    task,
                ))
            });
        }
        while let Some(res) = sub_set.join_next().await {
            let (index, sample, hub, task) = res.context("SubscribeActivations task panicked")??;
            subscribe_samples.push(sample);
            let ctx = &mut sessions[index];
            ctx.activation_hub = Some(hub);
            ctx.ack_task = Some(task);
        }
        cpu_subscribe = Some(cpu.end_phase());
    }

    // -------- Add resources (parallel per session, sequential within session) --------
    cpu.begin_phase();
    let mut add_sender_samples = Vec::with_capacity(args.add_senders);
    let mut add_receiver_samples = Vec::with_capacity(args.add_receivers);
    let mut senders: Vec<Option<ResourceSlot>> = vec![None; args.add_senders];
    let mut receivers: Vec<Option<ResourceSlot>> = vec![None; args.add_receivers];

    let mut add_set = JoinSet::new();
    for ctx in &mut sessions {
        let session_index = ctx.index;
        let session_handle = ctx.slot.session_handle.clone();
        let mut client = ctx.client.clone();
        let sender_indices: Vec<usize> = (0..args.add_senders)
            .filter(|&i| {
                session_for_resource(i, args.sessions) == session_index
            })
            .collect();
        let receiver_indices: Vec<usize> = (0..args.add_receivers)
            .filter(|&i| {
                session_for_resource(args.add_senders + i, args.sessions) == session_index
            })
            .collect();
        if sender_indices.is_empty() && receiver_indices.is_empty() {
            continue;
        }
        let iface_ip = iface_ip.clone();
        add_set.spawn(async move {
            let mut local_sender_samples = Vec::new();
            let mut local_receiver_samples = Vec::new();
            let mut local_senders = Vec::new();
            let mut local_receivers = Vec::new();

            for i in sender_indices {
                let name = format!("sender-{i}");
                let sdp = build_video_sdp(&name, true, &iface_ip);
                let t0 = Instant::now();
                let resp = client
                    .add_sender(AddSenderRequest {
                        session_handle: session_handle.clone(),
                        transport: ProtoTransport::Rtp as i32,
                        transport_file: sdp.clone(),
                        name: name.clone(),
                    })
                    .await
                    .with_context(|| format!("AddSender {name}"))?
                    .into_inner();
                local_sender_samples.push((i, t0.elapsed().as_micros() as u64));
                local_senders.push((
                    i,
                    ResourceSlot {
                        session_index,
                        resource_handle: resp.resource_handle,
                        resource_id: resp.resource_id,
                        sdp,
                    },
                ));
            }

            for i in receiver_indices {
                let name = format!("receiver-{i}");
                let sdp = build_video_sdp(&name, false, &iface_ip);
                let t0 = Instant::now();
                let resp = client
                    .add_receiver(AddReceiverRequest {
                        session_handle: session_handle.clone(),
                        transport: ProtoTransport::Rtp as i32,
                        transport_file: sdp.clone(),
                        name: name.clone(),
                    })
                    .await
                    .with_context(|| format!("AddReceiver {name}"))?
                    .into_inner();
                local_receiver_samples.push((i, t0.elapsed().as_micros() as u64));
                local_receivers.push((
                    i,
                    ResourceSlot {
                        session_index,
                        resource_handle: resp.resource_handle,
                        resource_id: resp.resource_id,
                        sdp,
                    },
                ));
            }

            Ok::<_, anyhow::Error>((
                local_sender_samples,
                local_receiver_samples,
                local_senders,
                local_receivers,
            ))
        });
    }

    while let Some(res) = add_set.join_next().await {
        let (s_samples, r_samples, s_slots, r_slots) =
            res.context("AddResource task panicked")??;
        for (_i, sample) in s_samples {
            add_sender_samples.push(sample);
        }
        for (i, slot) in s_slots {
            senders[i] = Some(slot);
        }
        for (_i, sample) in r_samples {
            add_receiver_samples.push(sample);
        }
        for (i, slot) in r_slots {
            receivers[i] = Some(slot);
        }
    }
    let senders: Vec<ResourceSlot> = senders
        .into_iter()
        .map(|s| s.context("missing sender slot"))
        .collect::<Result<_, _>>()?;
    let receivers: Vec<ResourceSlot> = receivers
        .into_iter()
        .map(|r| r.context("missing receiver slot"))
        .collect::<Result<_, _>>()?;
    let cpu_add = cpu.end_phase();
    memory.after_add = sample_memory(args.daemon_pid);

    // -------- Sync activations (parallel per session) --------
    let mut sync_activate_samples = Vec::new();
    let mut sync_deactivate_samples = Vec::new();
    let mut cpu_activate_sync = None;

    if args.syncs > 0 {
        cpu.begin_phase();
        let sync_indices = sender_activation_indices(senders.len(), args.syncs);
        let mut sync_set = JoinSet::new();
        for ctx in &mut sessions {
            let session_index = ctx.index;
            let session_handle = ctx.slot.session_handle.clone();
            let mut client = ctx.client.clone();
            let session_senders: Vec<ResourceSlot> = sync_indices
                .iter()
                .map(|&i| senders[i].clone())
                .filter(|s| s.session_index == session_index)
                .collect();
            if session_senders.is_empty() {
                continue;
            }
            sync_set.spawn(async move {
                let mut activate = Vec::new();
                let mut deactivate = Vec::new();
                for s in &session_senders {
                    let updated = s.sdp.replacen("o=- 0 0", "o=- 0 1", 1);
                    let t0 = Instant::now();
                    client
                        .sync_resource_state(SyncResourceStateRequest {
                            session_handle: session_handle.clone(),
                            resource_handle: s.resource_handle.clone(),
                            transport_file: Some(updated),
                        })
                        .await
                        .context("SyncResourceState activate")?;
                    activate.push(t0.elapsed().as_micros() as u64);

                    let t0 = Instant::now();
                    client
                        .sync_resource_state(SyncResourceStateRequest {
                            session_handle: session_handle.clone(),
                            resource_handle: s.resource_handle.clone(),
                            transport_file: None,
                        })
                        .await
                        .context("SyncResourceState deactivate")?;
                    deactivate.push(t0.elapsed().as_micros() as u64);
                }
                Ok::<_, anyhow::Error>((activate, deactivate))
            });
        }
        while let Some(res) = sync_set.join_next().await {
            let (activate, deactivate) = res.context("SyncResourceState task panicked")??;
            sync_activate_samples.extend(activate);
            sync_deactivate_samples.extend(deactivate);
        }
        cpu_activate_sync = Some(cpu.end_phase());
    }

    // -------- PATCH activations (GET then PATCH; HTTP parallelism via clients) --------
    let mut connection_get_samples = Vec::new();
    let mut patch_activate_samples = Vec::new();
    let mut patch_deactivate_samples = Vec::new();
    let mut cpu_activate_patch = None;

    if args.patches > 0 {
        cpu.begin_phase();
        let patch_indices = sender_activation_indices(senders.len(), args.patches);

        let patch_work: Vec<(ResourceSlot, String)> = patch_indices
            .iter()
            .map(|&i| {
                let s = &senders[i];
                (
                    s.clone(),
                    format!(
                        "/x-nmos/connection/v1.1/single/senders/{}/staged",
                        s.resource_id
                    ),
                )
            })
            .collect();

        let http_sem = Arc::new(Semaphore::new(args.clients));
        let iface_ip = Arc::new(iface_ip);
        let mut patch_set = JoinSet::new();

        for (sender, staged_path) in patch_work {
            let sem = http_sem.clone();
            let iface_ip = iface_ip.clone();
            let session_index = sender.session_index;
            let http_port = sessions[session_index].slot.http_port;
            let resource_handle = sender.resource_handle.clone();
            let staged_path = staged_path.clone();
            let activation_hub = sessions[session_index]
                .activation_hub
                .as_ref()
                .context("activation hub for session")?
                .clone();

            patch_set.spawn(async move {
                let _permit = sem
                    .acquire()
                    .await
                    .context("HTTP client semaphore closed")?;

                let mut get_samples = Vec::with_capacity(2);
                let mut activate_samples = Vec::with_capacity(1);
                let mut deactivate_samples = Vec::with_capacity(1);

                let t0 = Instant::now();
                let (status, _) =
                    connection_get(&iface_ip, http_port, &staged_path).await?;
                anyhow::ensure!(status == 200, "GET staged returned HTTP {status}");
                get_samples.push(t0.elapsed().as_micros() as u64);

                let t0 = Instant::now();
                let body = r#"{"master_enable":true,"activation":{"mode":"activate_immediate"}}"#;
                let (status, _) =
                    connection_patch(&iface_ip, http_port, &staged_path, body).await?;
                anyhow::ensure!(status == 200, "PATCH activate returned HTTP {status}");
                activate_samples.push(t0.elapsed().as_micros() as u64);
                activation_hub
                    .wait(&resource_handle, true, Duration::from_secs(30))
                    .await?;

                let t0 = Instant::now();
                let (status, _) =
                    connection_get(&iface_ip, http_port, &staged_path).await?;
                anyhow::ensure!(status == 200, "GET staged returned HTTP {status}");
                get_samples.push(t0.elapsed().as_micros() as u64);

                let t0 = Instant::now();
                let body = r#"{"master_enable":false,"activation":{"mode":"activate_immediate"}}"#;
                let (status, _) =
                    connection_patch(&iface_ip, http_port, &staged_path, body).await?;
                anyhow::ensure!(status == 200, "PATCH deactivate returned HTTP {status}");
                deactivate_samples.push(t0.elapsed().as_micros() as u64);
                activation_hub
                    .wait(&resource_handle, false, Duration::from_secs(30))
                    .await?;

                Ok::<_, anyhow::Error>((get_samples, activate_samples, deactivate_samples))
            });
        }

        while let Some(res) = patch_set.join_next().await {
            let (gets, activates, deactivates) =
                res.context("Connection API task panicked")??;
            connection_get_samples.extend(gets);
            patch_activate_samples.extend(activates);
            patch_deactivate_samples.extend(deactivates);
        }
        cpu_activate_patch = Some(cpu.end_phase());
    }

    memory.after_activate = sample_memory(args.daemon_pid);

    // -------- Remove resources (parallel per session; distinct from CloseSession) --------
    let mut remove_sender_samples = Vec::new();
    let mut remove_receiver_samples = Vec::new();
    let mut cpu_remove = None;

    if args.remove_senders > 0 || args.remove_receivers > 0 {
        cpu.begin_phase();
        let (sender_removals, receiver_removals) =
            resources_to_remove(
                &senders,
                &receivers,
                args.remove_senders,
                args.remove_receivers,
            );
        let mut remove_set = JoinSet::new();
        for ctx in &mut sessions {
            let session_index = ctx.index;
            let session_handle = ctx.slot.session_handle.clone();
            let mut client = ctx.client.clone();
            let session_sender_removals: Vec<ResourceSlot> = sender_removals
                .iter()
                .filter(|r| r.session_index == session_index)
                .cloned()
                .collect();
            let session_receiver_removals: Vec<ResourceSlot> = receiver_removals
                .iter()
                .filter(|r| r.session_index == session_index)
                .cloned()
                .collect();
            if session_sender_removals.is_empty() && session_receiver_removals.is_empty() {
                continue;
            }
            remove_set.spawn(async move {
                let mut sender_samples = Vec::new();
                let mut receiver_samples = Vec::new();
                for resource in session_sender_removals {
                    let t0 = Instant::now();
                    client
                        .remove_resource(RemoveResourceRequest {
                            session_handle: session_handle.clone(),
                            resource_handle: resource.resource_handle.clone(),
                        })
                        .await
                        .with_context(|| {
                            format!(
                                "RemoveResource sender resource_id={}",
                                resource.resource_id
                            )
                        })?;
                    sender_samples.push(t0.elapsed().as_micros() as u64);
                }
                for resource in session_receiver_removals {
                    let t0 = Instant::now();
                    client
                        .remove_resource(RemoveResourceRequest {
                            session_handle: session_handle.clone(),
                            resource_handle: resource.resource_handle.clone(),
                        })
                        .await
                        .with_context(|| {
                            format!(
                                "RemoveResource receiver resource_id={}",
                                resource.resource_id
                            )
                        })?;
                    receiver_samples.push(t0.elapsed().as_micros() as u64);
                }
                Ok::<_, anyhow::Error>((sender_samples, receiver_samples))
            });
        }
        while let Some(res) = remove_set.join_next().await {
            let (sender_samples, receiver_samples) =
                res.context("RemoveResource task panicked")??;
            remove_sender_samples.extend(sender_samples);
            remove_receiver_samples.extend(receiver_samples);
        }
        cpu_remove = Some(cpu.end_phase());
    }
    memory.after_remove = sample_memory(args.daemon_pid);

    // -------- Close sessions (parallel, one client per session) --------
    cpu.begin_phase();
    let mut close_samples = Vec::with_capacity(sessions.len());
    let mut close_set = JoinSet::new();
    for ctx in &mut sessions {
        let session_handle = ctx.slot.session_handle.clone();
        let mut client = ctx.client.clone();
        close_set.spawn(async move {
            let t0 = Instant::now();
            client
                .close_session(CloseSessionRequest {
                    session_handle,
                })
                .await
                .context("CloseSession")?;
            Ok::<_, anyhow::Error>(t0.elapsed().as_micros() as u64)
        });
    }
    while let Some(res) = close_set.join_next().await {
        close_samples.push(res.context("CloseSession task panicked")??);
    }
    let cpu_close = cpu.end_phase();
    memory.after_close = sample_memory(args.daemon_pid);

    for ctx in &mut sessions {
        if let Some(task) = ctx.ack_task.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
        }
    }

    let report = BenchReport {
        scenario: ScenarioMeta {
            label: args.label,
            nodes: args.nodes,
            add_senders: args.add_senders,
            add_receivers: args.add_receivers,
            remove_senders: args.remove_senders,
            remove_receivers: args.remove_receivers,
            sessions: args.sessions,
            clients: args.clients,
            syncs: args.syncs,
            patches: args.patches,
        },
        memory_kb: memory,
        cpu_pct: cpu.into_report(CpuPhases {
            open: cpu_open,
            subscribe: cpu_subscribe,
            add: cpu_add,
            activate_sync: cpu_activate_sync,
            activate_patch: cpu_activate_patch,
            remove: cpu_remove,
            close: cpu_close,
        }),
        open_session_ms: LatencyMs::from_samples_us(open_samples),
        subscribe_activations_ms: LatencyMs::from_samples_us(subscribe_samples),
        add_sender_ms: LatencyMs::from_samples_us(add_sender_samples),
        add_receiver_ms: LatencyMs::from_samples_us(add_receiver_samples),
        sync_activate_ms: LatencyMs::from_samples_us(sync_activate_samples),
        sync_deactivate_ms: LatencyMs::from_samples_us(sync_deactivate_samples),
        connection_get_ms: LatencyMs::from_samples_us(connection_get_samples),
        patch_activate_ms: LatencyMs::from_samples_us(patch_activate_samples),
        patch_deactivate_ms: LatencyMs::from_samples_us(patch_deactivate_samples),
        remove_sender_ms: LatencyMs::from_samples_us(remove_sender_samples),
        remove_receiver_ms: LatencyMs::from_samples_us(remove_receiver_samples),
        close_session_ms: LatencyMs::from_samples_us(close_samples),
        wall_ms: us_to_ms(wall_start.elapsed().as_micros() as u64),
    };

    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

fn session_node_index(session_index: usize, nodes: usize) -> usize {
    session_index % nodes
}

fn session_for_resource(resource_index: usize, session_count: usize) -> usize {
    resource_index % session_count
}

/// Per-workflow index for sync, PATCH, or RemoveResource. `workflow_count == 0` → none.
/// When `workflow_count <= registered_count`, targets are evenly spaced; when larger,
/// round-robin through `[0, registered_count)`.
fn sender_activation_indices(registered_count: usize, workflow_count: usize) -> Vec<usize> {
    match (registered_count, workflow_count) {
        (0, _) | (_, 0) => Vec::new(),
        (n, c) if c <= n => (0..c).map(|i| i * n / c).collect(),
        (n, c) => (0..c).map(|i| i % n).collect(),
    }
}

fn resources_to_remove(
    senders: &[ResourceSlot],
    receivers: &[ResourceSlot],
    remove_senders: usize,
    remove_receivers: usize,
) -> (Vec<ResourceSlot>, Vec<ResourceSlot>) {
    let sender_out: Vec<ResourceSlot> = sender_activation_indices(senders.len(), remove_senders)
        .iter()
        .map(|&i| senders[i].clone())
        .collect();
    let receiver_out: Vec<ResourceSlot> =
        sender_activation_indices(receivers.len(), remove_receivers)
            .iter()
            .map(|&i| receivers[i].clone())
            .collect();
    (sender_out, receiver_out)
}

fn bench_node_config(node_seed: &str, http_port: u16) -> NodeConfig {
    NodeConfig {
        seed: node_seed.to_string(),
        http_port: u32::from(http_port),
        network_services: Some(NetworkServicesConfig {
            // registration_port 65535 without registration_address puts libnvnmos in
            // registry-less mode (DNS-SD disabled, no Registration API requests)
            registration_port: 65535,
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn autodetect_interface_ip() -> anyhow::Result<String> {
    let sock = UdpSocket::bind("0.0.0.0:0").context("UdpSocket::bind")?;
    sock.connect("8.8.8.8:80").context("UdpSocket::connect")?;
    Ok(sock.local_addr()?.ip().to_string())
}

async fn spawn_auto_ack_task(
    mut client: NvnmosDaemonClient<Channel>,
    session_handle: String,
) -> anyhow::Result<(tokio::task::JoinHandle<()>, Arc<ActivationHub>)> {
    let mut stream = client
        .subscribe_activations(SubscribeActivationsRequest {
            session_handle: session_handle.clone(),
        })
        .await?
        .into_inner();

    let (hub, event_tx) = ActivationHub::new();

    let handle = tokio::spawn(async move {
        while let Ok(Some(event)) = stream.message().await {
            let _ = client
                .ack_activation(AckActivationRequest {
                    session_handle: session_handle.clone(),
                    activation_handle: event.activation_handle.clone(),
                    success: true,
                    failure_reason: String::new(),
                })
                .await;
            let _ = event_tx.send(event).await;
        }
    });
    Ok((handle, hub))
}

async fn connection_get(host: &str, port: u16, path: &str) -> anyhow::Result<(u16, String)> {
    connection_http("GET", host, port, path, None).await
}

async fn connection_patch(
    host: &str,
    port: u16,
    path: &str,
    body: &str,
) -> anyhow::Result<(u16, String)> {
    connection_http("PATCH", host, port, path, Some(body)).await
}

async fn connection_http(
    method: &str,
    host: &str,
    port: u16,
    path: &str,
    body: Option<&str>,
) -> anyhow::Result<(u16, String)> {
    let addr = format!("{host}:{port}");
    let request = match body {
        Some(body) => format!(
            "{method} {path} HTTP/1.1\r\n\
             Host: {addr}\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {len}\r\n\
             Connection: close\r\n\
             \r\n\
             {body}",
            len = body.len(),
        ),
        None => format!(
            "{method} {path} HTTP/1.1\r\n\
             Host: {addr}\r\n\
             Connection: close\r\n\
             \r\n",
        ),
    };

    tokio::time::timeout(Duration::from_secs(30), async {
        let mut sock = TcpStream::connect(&addr).await?;
        sock.write_all(request.as_bytes()).await?;
        let mut buf = Vec::with_capacity(4096);
        sock.read_to_end(&mut buf).await?;
        let resp = String::from_utf8_lossy(&buf).into_owned();
        let status_line = resp.lines().next().unwrap_or("");
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| anyhow::anyhow!("bad HTTP status"))?;
        Ok((status, resp))
    })
    .await
    .with_context(|| format!("{method} timed out"))?
}

fn build_video_sdp(name: &str, sender: bool, iface_ip: &str) -> String {
    const MULTICAST_IP: &str = "233.252.0.0";
    const SOURCE_IP: &str = "192.0.2.0";
    const DESTINATION_PORT: u16 = 5020;
    const SOURCE_PORT: u16 = 5004;
    const PAYLOAD_TYPE: u8 = 96;
    const ENCODING: &str = "raw/90000";
    const FMTP: &str = "sampling=YCbCr-4:2:2; width=1920; height=1080; \
        exactframerate=50; depth=10; TCS=SDR; colorimetry=BT709; \
        PM=2110GPM; SSN=ST2110-20:2017; TP=2110TPN; ";

    let mut out = format!(
        "v=0\r\n\
         o=- 0 0 IN IP4 {iface_ip}\r\n\
         s=nvnmosd-bench {name}\r\n\
         t=0 0\r\n\
         a=x-nvnmos-name:{name}\r\n\
         m=video {DESTINATION_PORT} RTP/AVP {PAYLOAD_TYPE}\r\n\
         c=IN IP4 {MULTICAST_IP}/64\r\n\
         a=source-filter: incl IN IP4 {MULTICAST_IP} {}\r\n\
         a=x-nvnmos-iface-ip:{iface_ip}\r\n\
         a=rtpmap:{PAYLOAD_TYPE} {ENCODING}\r\n\
         a=fmtp:{PAYLOAD_TYPE} {FMTP}\r\n\
         a=mediaclk:direct=0\r\n",
        if sender { iface_ip } else { SOURCE_IP },
    );
    if sender {
        out.push_str(&format!("a=x-nvnmos-src-port:{SOURCE_PORT}\r\n"));
        out.push_str("a=ts-refclk:localmac=CA-FE-01-CA-FE-02\r\n");
    }
    out
}

async fn connect_uds(uds: &Path) -> anyhow::Result<Channel> {
    let uds = uds.to_path_buf();
    let endpoint = Endpoint::try_from("http://[::1]:50051")?;
    endpoint
        .connect_with_connector(service_fn(move |_: Uri| {
            let uds = uds.clone();
            async move {
                let stream = UnixStream::connect(uds).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await
        .context("UDS connect")
}

#[cfg(test)]
mod tests {
    use super::sender_activation_indices;

    #[test]
    fn activation_indices_zero_means_none() {
        assert!(sender_activation_indices(10, 0).is_empty());
        assert!(sender_activation_indices(0, 0).is_empty());
        assert!(sender_activation_indices(0, 5).is_empty());
    }

    #[test]
    fn activation_indices_full_coverage() {
        assert_eq!(sender_activation_indices(10, 10), (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn activation_indices_evenly_spaced_sample() {
        assert_eq!(sender_activation_indices(1000, 32).len(), 32);
        assert_eq!(sender_activation_indices(1000, 32)[0], 0);
        assert_eq!(sender_activation_indices(1000, 32)[31], 31 * 1000 / 32);
    }

    #[test]
    fn activation_indices_round_robin_when_exceeding_registered() {
        let indices = sender_activation_indices(1000, 5000);
        assert_eq!(indices.len(), 5000);
        assert_eq!(indices[0], 0);
        assert_eq!(indices[999], 999);
        assert_eq!(indices[1000], 0);
        assert_eq!(indices[1001], 1);
    }

    #[test]
    fn resources_to_remove_respects_per_side_counts() {
        use super::{resources_to_remove, ResourceSlot};

        let senders: Vec<ResourceSlot> = (0..4)
            .map(|i| ResourceSlot {
                session_index: i % 2,
                resource_id: format!("sender-{i}"),
                resource_handle: format!("res-s-{i}"),
                sdp: String::new(),
            })
            .collect();
        let receivers: Vec<ResourceSlot> = (0..2)
            .map(|i| ResourceSlot {
                session_index: 0,
                resource_id: format!("receiver-{i}"),
                resource_handle: format!("res-r-{i}"),
                sdp: String::new(),
            })
            .collect();

        let (removed_senders, removed_receivers) =
            resources_to_remove(&senders, &receivers, 2, 1);
        assert_eq!(removed_senders.len(), 2);
        assert_eq!(removed_receivers.len(), 1);
        assert_eq!(removed_senders[0].resource_id, "sender-0");
        assert_eq!(removed_senders[1].resource_id, "sender-2");
        assert_eq!(removed_receivers[0].resource_id, "receiver-0");
    }

    #[test]
    fn cpu_pct_between_one_core_second() {
        use std::time::{Duration, Instant};

        let t0 = Instant::now();
        // 100 jiffies at CLK_TCK=100 == 1.0 core-second over 1.0 wall second => 100%
        let pct = super::cpu_pct_between(0, 0, t0, 100, 0, t0 + Duration::from_secs(1));
        assert!((pct - 100.0).abs() < 0.01);
    }

    #[test]
    fn cpu_phase_pct_stats() {
        let stats = super::CpuPhasePct::from_samples(vec![10.0, 20.0, 30.0, 40.0]);
        assert_eq!(stats.samples, 4);
        assert!((stats.avg.unwrap() - 25.0).abs() < 0.01);
        assert!((stats.max.unwrap() - 40.0).abs() < 0.01);
        assert!((stats.p95.unwrap() - 40.0).abs() < 0.01);
    }
}
