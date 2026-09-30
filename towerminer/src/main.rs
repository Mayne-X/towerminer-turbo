// SPDX-License-Identifier: Apache-2.0
// Portions derived from jetsam-extminer (Apache-2.0, the Jetsam developers).
//! # towerminer — TowerWalk CPU miner for Jetsam
//!
//! Speaks exactly the protocol of `jetsam-extminer` (getBlockTemplate /
//! submitBlock, Bearer key, pool `nonce_prefix` in bits 96..128, a per-process
//! offset in bits 64..96, one second of submit margin), so it drops in front of
//! a node or of our pool unchanged. What differs is the engine:
//!
//! - persistent pinned worker threads, each owning its pads in 2 MiB pages and
//!   grinding the current job without a per-template rayon pass;
//! - several nonces walked in lockstep per thread (see `walk.rs`);
//! - a profile chosen from the L2 size the kernel reports, not a model name;
//! - every solution re-verified by the reference walk before it is submitted,
//!   a sentinel hash re-checked every 4096, a golden self-test at start-up;
//! - a thermal guard on its own thread (see `thermal.rs`): readings every
//!   250 ms, stop at 81 C or earlier when the slope projects 82 C (house rule).
mod gate;
mod rapl;
mod sys;
mod thermal;
mod walk;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use clap::Parser;
use jetsam_core::Block128;
use jetsam_poseidon2b::batch::FixedFieldNonceBatch;
use jetsam_poseidon2b::native::domain::TAG_POWHDR;
use jetsam_poseidon2b::towerwalk::Scratch;
use serde::{Deserialize, Serialize};

use gate::{FIELDS, NONCE_FIELD};
use thermal::Thermal;
use walk::{walk_dyn, MAX_PADS};

const VERSION: &str = concat!("towerminer/", env!("CARGO_PKG_VERSION"));
const SUBMIT_MARGIN: Duration = Duration::from_secs(1);
/// HTTP timeout of a block submission. The answer can legitimately take long:
/// the pool holds the request up to ~10 s (its own retries to the node), and
/// the node up to 30 s, since it finishes its proof before it seals the
/// block. v0.2.1 (5 s, one retry) printed "no answer" then "refused" for
/// blocks reported accepted on the pool side [2026-09-30; the pair is in the
/// cpu2 miner log]. One attempt only: the pool retries on its side, and a
/// duplicate submission is harmful.
const SUBMIT_TIMEOUT: Duration = Duration::from_secs(45);
/// Submissions in flight at once, each on its own thread: a second block found
/// while the first one waits for its answer goes out at once.
const SUBMIT_MAX_INFLIGHT: usize = 4;
/// Template polls: a worker that makes the pool fetch a fresh template from
/// the node waits for the node's proof (~8 s measured, the node may take up
/// to 30 s), and a short timeout would abandon exactly the request that
/// brings new work.
const POLL_TIMEOUT: Duration = Duration::from_secs(30);
/// --tune: a candidate is `noisy` when other processes kept the targeted CPUs
/// busier than this (fraction), before or during its window.
const TUNE_FOREIGN_MAX: f64 = 0.05;
const NONCE_REGION_SHIFT: u32 = 96;
const SPONGE_BATCH: usize = 256;
const SENTINEL_EVERY: u64 = 4096;
const EXIT_SELFTEST: i32 = 2;
const EXIT_DIVERGED: i32 = 3;
const EXIT_NO_HUGE: i32 = 4;
/// Default period of the duty-cycle gate. 1 s: the on-phases are long enough
/// for the package power limiter to hold the all-core V/f point, not the
/// burst boost of cores just out of idle [MEASURED 2026-09-28, 5950X, target
/// 56 C: 97.9 H/J at 1 s vs 90.4 at 500 ms and ~60 at 100 ms].
const DEFAULT_PWM_MS: u64 = 1000;

/// `--version`: a mutant build (gate self-check) can never pass for a release.
const VERSION_LONG: &str = if walk::MUTANT {
    concat!(env!("CARGO_PKG_VERSION"), "+mutant")
} else {
    env!("CARGO_PKG_VERSION")
};

#[derive(Parser, Debug)]
#[command(name = "towerminer", version = VERSION_LONG, about = "TowerWalk CPU miner for Jetsam")]
struct Cli {
    /// JSON-RPC endpoint of the Jetsam node or pool.
    #[arg(long, default_value = "http://127.0.0.1:9701", value_name = "URL")]
    rpc: String,
    /// Bearer token (pool key or node --mining-key).
    #[arg(long, value_name = "TOKEN", env = "TOWERMINER_KEY", hide_env_values = true)]
    key: Option<String>,
    /// Custom coinbase (node must run --allow-custom-coinbase). Empty = node payout.
    #[arg(long, default_value = "")]
    coinbase: String,
    /// CPUs this miner may use (e.g. 0-11,24-35). Default: the process affinity.
    #[arg(long, value_name = "LIST")]
    cpus: Option<String>,
    /// CPUs to leave alone (removed from --cpus / the affinity).
    #[arg(long, value_name = "LIST")]
    exclude_cpus: Option<String>,
    /// Worker threads per physical core: 1 or 2. Default from the L2 size.
    #[arg(long)]
    threads_per_core: Option<usize>,
    /// Cap on worker threads (after the per-core choice).
    #[arg(long)]
    threads: Option<usize>,
    /// Nonces walked together per thread, 1..=4. Default from the L2 size.
    #[arg(long)]
    pads: Option<usize>,
    /// Software prefetch of the next address: 0 or 1. Default from the L2 size.
    #[arg(long)]
    prefetch: Option<u8>,
    /// Walk kernel: base (v0.1) or fast (measured levers: byte-offset
    /// addressing + one-add fill at 1 pad, grouped fold at >= 2 pads).
    /// auto = fast.
    #[arg(long, value_enum, default_value_t = KernelArg::Auto)]
    kernel: KernelArg,
    /// Seeds hashed per sponge call (the 8-leaf AVX2 sponge needs >= 8).
    /// Default: 8 at 1 pad, else = pads.
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..=64))]
    seed_batch: Option<u16>,
    /// Pipelined fill anchors (1 pad, fast kernel, with or without
    /// --prefetch): the next seed's 16 fill folds ride in the free lane of
    /// the current seed's walk folds. auto = on at 1 pad with the fast kernel.
    #[arg(long, value_enum, default_value_t = PipeArg::Auto)]
    pipe: PipeArg,
    /// Use 4 KiB pages instead of transparent 2 MiB pages (diagnostic).
    #[arg(long)]
    no_huge: bool,
    /// Exit 4 unless every worker's pads really sit in 2 MiB pages (checked
    /// per worker in /proc/self/smaps). Default: a red warning and " 4K!" in
    /// the CPU line the pool shows.
    #[arg(long, conflicts_with = "no_huge")]
    require_huge: bool,
    /// Milliseconds between template polls.
    /// 250 ms: after a new block the pool answers from its cache, so polling
    /// faster halves the work spent on a dead parent for almost no cost.
    #[arg(long, default_value_t = 250)]
    poll_ms: u64,
    /// Hard thermal stop (C): exit 82 when max(Tctl, Tccd*) reaches it, or
    /// earlier when the last second's slope projects 82 C within one second.
    /// Cannot be raised above 82 (house rule).
    #[arg(long, default_value_t = 81.0, value_parser = parse_temp_stop)]
    temp_stop: f64,
    /// Target temperature (C) of the duty-cycle regulator. Default
    /// min(74, stop - 5); must be <= stop - 5.
    #[arg(long)]
    temp_target: Option<f64>,
    /// Milliseconds between two sensor readings of the thermal guard (50 ms
    /// while the duty-cycle regulator runs).
    #[arg(long, default_value_t = 250, value_parser = clap::value_parser!(u64).range(100..=500))]
    temp_poll_ms: u64,
    /// Run without any temperature sensor (no thermal guard at all). Every
    /// report then says so in red.
    #[arg(long)]
    no_thermal_guard: bool,
    /// Period of the duty-cycle gate (ms). All workers hash during the first
    /// `duty x period` and sleep the rest, in phase.
    #[arg(long, default_value_t = DEFAULT_PWM_MS, value_parser = clap::value_parser!(u64).range(10..=2000))]
    pwm_period_ms: u64,
    /// Floor of the duty cycle (0.05..1).
    #[arg(long, default_value_t = 0.10)]
    min_duty: f64,
    /// Start-up ramp (mining only): the duty climbs from 0.3 to 1 over this
    /// many seconds while the machine is well below target (0 = no ramp).
    #[arg(long, default_value_t = 10.0)]
    ramp_secs: f64,
    /// --bench-walk: run the duty-cycle regulator as when mining (off by
    /// default: a bench measures the kernel, not the cooler).
    #[arg(long)]
    bench_regulate: bool,
    /// DEPRECATED, ignored: replaced by --temp-target (duty-cycle regulator).
    #[arg(long, hide = true)]
    temp_pause: Option<f64>,
    /// DEPRECATED, ignored: replaced by --temp-target.
    #[arg(long, hide = true)]
    temp_resume: Option<f64>,
    /// Print a progress line every N seconds (mining default 15; bench: off).
    #[arg(long, value_name = "SECONDS")]
    report_secs: Option<u64>,
    /// Test hook: the thermal guard sleeps S seconds once, 5 s after start.
    #[arg(long, hide = true, value_name = "S")]
    debug_guard_stall: Option<f64>,
    /// Test hook: the limit the slope predictor projects against (<= 82).
    #[arg(long, hide = true, value_name = "C", value_parser = parse_temp_stop)]
    debug_temp_hard: Option<f64>,
    /// Test hook: behave as if no temperature sensor existed.
    #[arg(long, hide = true)]
    debug_no_sensor: bool,
    /// Measure the walk rate for N seconds with the chosen profile and exit.
    #[arg(long, value_name = "SECONDS")]
    bench_walk: Option<u64>,
    /// Measure the candidate profiles on THIS machine (N seconds each, two
    /// alternating rounds), keep the fastest, and remember it for later runs.
    #[arg(long, value_name = "SECONDS")]
    tune: Option<u64>,
    /// Where --tune stores its result (default ~/.config/towerminer/tune.json).
    #[arg(long, value_name = "FILE")]
    tune_file: Option<String>,
    /// --tune: store the result even when a candidate was measured on noisy
    /// CPUs (other processes kept the targeted CPUs, SMT siblings included,
    /// more than 5 % busy).
    #[arg(long)]
    tune_force: bool,
    /// Ignore a stored --tune result and use the built-in table.
    #[arg(long)]
    no_tune_file: bool,
    /// Which profile to load: the fastest (hashrate) or the most hashes per
    /// joule (efficiency), from the --tune file or the built-in table.
    #[arg(long, value_enum, default_value_t = Policy::Hashrate)]
    policy: Policy,
    /// --tune: cool below this temperature (C) before each candidate.
    #[arg(long, default_value_t = 60.0)]
    tune_cool: f64,
    /// --tune: kernels to try on every shape (comma list: fast, base).
    #[arg(long, value_enum, value_delimiter = ',', default_value = "fast")]
    tune_kernels: Vec<KernelArg>,
    /// --bench-walk: seconds of idle package power measured before the
    /// workers start (0 = skip; the marginal H/J needs it).
    #[arg(long, default_value_t = 3)]
    idle_secs: u64,
    /// Run the full bit-exact gate (all kernel shapes) and exit 0/1.
    #[arg(long)]
    gate: bool,
    /// Check this machine (CPU backend, caches, THP, a 2 MiB region, the
    /// temperature sensor, RAPL, glibc) and exit 0 (ready) or 1.
    #[arg(long)]
    check_hardware: bool,
    /// Random seeds added to --gate on top of the 256 golden vectors.
    #[arg(long, default_value_t = 1000)]
    gate_random: usize,
    /// Nonce-accounting gate: mine a permissive dummy target for N seconds
    /// with the chosen profile; every solution is recomputed from its nonce
    /// through the real seed path and the reference walk; duplicates and
    /// nonces outside the region are counted. Exit 0/1.
    #[arg(long, value_name = "SECONDS")]
    check_nonces: Option<u64>,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum KernelArg {
    Auto,
    Base,
    Fast,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum PipeArg {
    Auto,
    #[value(name = "0")]
    Off,
    #[value(name = "1")]
    On,
}

fn parse_temp_stop(s: &str) -> std::result::Result<f64, String> {
    let v: f64 = s.parse().map_err(|e| format!("{e}"))?;
    if !(40.0..=thermal::HARD_LIMIT_C).contains(&v) {
        return Err(format!("must be within 40..={} C (house rule)", thermal::HARD_LIMIT_C));
    }
    Ok(v)
}

impl Cli {
    /// Regulator target: explicit, else min(74, stop - 5).
    fn temp_target(&self) -> f64 {
        self.temp_target.unwrap_or_else(|| (self.temp_stop - 5.0).min(74.0))
    }
}

/// Cross-field checks clap cannot express.
fn validate(cli: &Cli) -> Result<()> {
    if !(0.05..=1.0).contains(&cli.min_duty) {
        return Err(anyhow!("--min-duty must be within 0.05..=1"));
    }
    if !(0.0..=600.0).contains(&cli.ramp_secs) {
        return Err(anyhow!("--ramp-secs must be within 0..=600"));
    }
    if let Some(t) = cli.temp_target {
        if t > cli.temp_stop - 5.0 {
            return Err(anyhow!("--temp-target {t} must be <= --temp-stop - 5 ({})", cli.temp_stop - 5.0));
        }
        if t < 30.0 {
            return Err(anyhow!("--temp-target {t} is below 30 C"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Profile
// ---------------------------------------------------------------------------

struct Profile {
    cpus: Vec<usize>,
    pads: usize,
    prefetch: bool,
    huge: bool,
    l2_kib: usize,
    l3_kib: usize,
    per_core: usize,
    /// walk::K_BASE or walk::K_FAST.
    kernel: u8,
    /// Seeds per sponge call (ring size; a multiple of pads unless piped).
    seed_batch: usize,
    pipe: bool,
    policy: Policy,
    /// Where the (threads/core, pads, prefetch) triple came from.
    source: String,
    tune_key: String,
}

fn cpuinfo_field(name: &str) -> Option<String> {
    std::fs::read_to_string("/proc/cpuinfo").ok().and_then(|s| {
        s.lines()
            .find(|l| l.split(':').next().map(|k| k.trim() == name).unwrap_or(false))
            .map(|l| l.splitn(2, ':').nth(1).unwrap_or("").trim().to_string())
    })
}

/// A profile shape: threads per core, pads per thread, prefetch, kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shape {
    tpc: usize,
    pads: usize,
    pf: bool,
    kernel: u8,
}

impl Shape {
    const fn new(tpc: usize, pads: usize, pf: bool, kernel: u8) -> Shape {
        Shape { tpc, pads, pf, kernel }
    }
    fn label(&self) -> String {
        format!("{}x{}x{} {}", self.tpc, self.pads, self.pf as u8, walk::kernel_name(self.kernel))
    }
}

/// Built-in table, per CPU family, for each policy.
///
/// v0.1 shapes [MEASURED 2026-09-26, whole machine, vs the node's own search
/// path on the same CPUs]: EPYC 7742 (Zen 2, family 23) 2/core, 2 pads,
/// prefetch +24 %; 5950X (Zen 3, 25, L2 512K) 1/core, 1 pad +35 %; 7950X3D and
/// 7900X (Zen 4, L2 1M) 1/core, 2 pads +42 %; 9950X3D (Zen 5, 26) 1/core,
/// 2 pads +28 % (one CCD, thermally capped). Zen 2 and Zen 3 share the L2 size
/// and still want opposite settings, so the family decides, not the cache
/// size. Anything unknown gets the node's own shape (both SMT siblings, one
/// pad) plus huge pages.
///
/// v0.2 adds the fast kernel everywhere [MEASURED on Zen 3 only, 2026-09-28:
/// +6.6 % at 1x1 (pipe + ring 8), +3.8 % at 2x2 prefetch (grouped fold); on
/// Zen 2/4/5 DERIVED from the same mechanism — `--tune` on the machine can
/// contradict it] and an efficiency shape: on the 5950X the L3 regime
/// 2 threads/core x 2 pads + prefetch gives 96.8 % of the rate for 84 % of
/// the power (142.8 vs 124.7 H/J, -5 C) [MEASURED]. Elsewhere the efficiency
/// shape is the rate shape until `--tune` has measured better.
///
/// v0.2.2 [MEASURED 2026-09-30, v0.2.1 binary, fast kernel]:
/// - Zen 4 (7950X3D, 8 cores of CCD1): 2x1x0 = 15.15 / 15.33 kH/s on the
///   bench and 15.26-15.31 kH/s mining, vs 1x2x0 (the v0.1 row) = 13.84
///   (13.86 mining): 2 threads/core x 1 pad, +10 %. Zen 5 (family 26, 1 MiB
///   L2) follows Zen 4 [DERIVED, not measured on Zen 5].
/// - Zen 2 (2x EPYC 7742, 112 cores, under a foreign load, so noisy, but
///   the same sign in both passes): 2x1x1 = 56.8 / 57.7 kH/s vs 2x2x1 (the
///   v0.1 row) = 54.0 / 52.8: 1 pad instead of 2, +7 %.
fn table(family: u32, l2_kib: usize, policy: Policy) -> (Shape, &'static str) {
    const F: u8 = walk::K_FAST;
    match (family, policy) {
        (23, _) => (Shape::new(2, 1, true, F), "table: Zen 2"),
        (25, Policy::Hashrate) if l2_kib < 1024 => (Shape::new(1, 1, false, F), "table: Zen 3"),
        (25, Policy::Efficiency) if l2_kib < 1024 => (Shape::new(2, 2, true, F), "table: Zen 3, efficiency"),
        (25 | 26, _) if l2_kib >= 1024 => (Shape::new(2, 1, false, F), "table: Zen 4/5"),
        _ => (Shape::new(2, 1, false, F), "table: unknown CPU, node shape + huge pages"),
    }
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum Policy {
    /// Highest rate (the profile the table or --tune measured fastest).
    Hashrate,
    /// Most hashes per joule (the profile --tune measured most efficient).
    Efficiency,
}

fn tune_path(cli: &Cli) -> std::path::PathBuf {
    if let Some(p) = &cli.tune_file {
        return p.into();
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::Path::new(&home).join(".config/towerminer/tune.json")
}

fn allowed_set(cli: &Cli) -> Result<BTreeSet<usize>> {
    let mut allowed: BTreeSet<usize> = match &cli.cpus {
        Some(l) => {
            let want = sys::parse_list(l);
            let aff = sys::allowed_cpus();
            let outside: Vec<String> = want.difference(&aff).map(|c| c.to_string()).collect();
            if !outside.is_empty() {
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| {
                    eprintln!(
                        "warning: --cpus asks for CPUs outside this process's affinity (cpuset/taskset), left out: {}",
                        outside.join(",")
                    )
                });
            }
            want.intersection(&aff).copied().collect()
        }
        None => sys::allowed_cpus(),
    };
    if let Some(ex) = &cli.exclude_cpus {
        for c in sys::parse_list(ex) {
            allowed.remove(&c);
        }
    }
    if allowed.is_empty() {
        return Err(anyhow!("no CPU left to mine on (check --cpus / --exclude-cpus)"));
    }
    Ok(allowed)
}

/// Key of a stored --tune result: same binary (sha256), same CPU model, same
/// CPU set, same THP mode. Anything else and the numbers are someone else's.
fn tune_key(cli: &Cli) -> Result<String> {
    let allowed = allowed_set(cli)?;
    let model = cpuinfo_field("model name").unwrap_or_default();
    let cpu_list: Vec<String> = allowed.iter().map(|c| c.to_string()).collect();
    Ok(format!("{VERSION}|{model}|{}|thp={}|sha={}", cpu_list.join(","), sys::thp_mode(), sys::self_sha256()))
}

fn shape_of(v: &serde_json::Value) -> Option<Shape> {
    Some(Shape {
        tpc: v["tpc"].as_u64()? as usize,
        pads: v["pads"].as_u64()? as usize,
        pf: v["pf"].as_bool()?,
        kernel: if v["kernel"].as_str()? == "base" { walk::K_BASE } else { walk::K_FAST },
    })
}

/// Build a profile. Precedence: explicit flags > stored --tune result for this
/// exact binary + CPU model + CPU set + THP mode (best rate or best H/J per
/// --policy) > built-in table.
fn profile_with(cli: &Cli, over: Option<Shape>) -> Result<Profile> {
    let allowed = allowed_set(cli)?;
    let topo = sys::Topology::detect(&allowed);
    let family: u32 = cpuinfo_field("cpu family").and_then(|v| v.parse().ok()).unwrap_or(0);
    let tune_key = tune_key(cli)?;
    let (mut sh, src) = table(family, topo.l2_kib, cli.policy);
    let mut source = src.to_string();
    if let Some(o) = over {
        sh = o;
        source = "tune candidate".into();
    } else if !cli.no_tune_file {
        if let Ok(txt) = std::fs::read_to_string(tune_path(cli)) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
                let which = if cli.policy == Policy::Efficiency { "best_hpj" } else { "best_hps" };
                if v["key"].as_str() == Some(tune_key.as_str()) {
                    if let Some(t) = shape_of(&v[which]) {
                        sh = t;
                        source = format!(
                            "tune file ({which}: {:.0} H/s, {:.1} H/J, measured {})",
                            v[which]["hps"].as_f64().unwrap_or(0.0),
                            v[which]["hpj"].as_f64().unwrap_or(0.0),
                            v["measured_utc"].as_str().unwrap_or("?")
                        );
                    }
                } else if v["key"].is_string() {
                    eprintln!(
                        "note: {} was measured for another binary, CPU set or THP mode; using the built-in table \
                         (run --tune again)",
                        tune_path(cli).display()
                    );
                }
            }
        }
    }
    if cli.threads_per_core.is_some() || cli.pads.is_some() || cli.prefetch.is_some() || cli.kernel != KernelArg::Auto {
        source = "command line".into();
    }
    let per_core = cli.threads_per_core.unwrap_or(sh.tpc).clamp(1, 2);
    let pads = cli.pads.unwrap_or(sh.pads);
    if !(1..=MAX_PADS).contains(&pads) {
        return Err(anyhow!("--pads must be 1..={MAX_PADS}"));
    }
    let prefetch = cli.prefetch.map(|v| v != 0).unwrap_or(sh.pf);
    // Order: first sibling of every core, then second siblings — so a --threads
    // cap fills physical cores before it doubles up on one.
    let mut cpus = Vec::new();
    for rank in 0..per_core {
        for core in &topo.cores {
            if let Some(&c) = core.get(rank) {
                cpus.push(c);
            }
        }
    }
    if let Some(n) = cli.threads {
        cpus.truncate(n.max(1));
    }
    let kernel = match cli.kernel {
        KernelArg::Base => walk::K_BASE,
        KernelArg::Fast => walk::K_FAST,
        KernelArg::Auto => sh.kernel,
    };
    // The piped walk carries the prefetch flag itself (walk_pipe::<PF>).
    let pipe = match cli.pipe {
        PipeArg::Off => false,
        PipeArg::Auto => pads == 1 && kernel == walk::K_FAST,
        PipeArg::On => {
            if pads != 1 || kernel != walk::K_FAST {
                return Err(anyhow!("--pipe 1 needs --pads 1 and the fast kernel"));
            }
            true
        }
    };
    let seed_batch = cli.seed_batch.map(|v| v as usize).unwrap_or(if pads == 1 { 8 } else { pads });
    Ok(Profile {
        cpus,
        pads,
        prefetch,
        huge: !cli.no_huge,
        l2_kib: topo.l2_kib,
        l3_kib: topo.l3_kib,
        per_core,
        kernel,
        seed_batch,
        pipe,
        policy: cli.policy,
        source,
        tune_key,
    })
}

fn profile(cli: &Cli) -> Result<Profile> {
    profile_with(cli, None)
}

impl Profile {
    /// Seeds per sponge call: a multiple of the pads, or (piped) at least 2
    /// so that the successor of every seed but the last is in the same ring.
    fn ring_len(&self) -> usize {
        if self.pipe {
            self.seed_batch.max(2)
        } else {
            self.seed_batch.max(self.pads).div_ceil(self.pads) * self.pads
        }
    }
}

// ---------------------------------------------------------------------------
// Shared state between the network thread and the workers
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Job {
    epoch: u64,
    template_id: String,
    height: u64,
    fields: [Block128; FIELDS],
    target: [u8; 32],
    walk: bool,
    /// Nonce every worker offsets from: region prefix | process offset.
    base: u128,
}

struct Shared {
    job: RwLock<Option<Arc<Job>>>,
    /// Workers grind only while now < deadline (nanos since `t0`).
    deadline_ns: AtomicU64,
    t0: Instant,
    stop: AtomicBool,
    hashed: AtomicU64,
    busy_ns: AtomicU64,
    /// Published by the thermal guard; checked by every worker every batch.
    th: Arc<Thermal>,
    /// Per worker, after its pads are mapped: see `HUGE_*`.
    huge: Vec<AtomicU8>,
    /// Epoch of the last accepted solution (0 = none): the poller idles on
    /// the same content until the tip moves.
    solved_epoch: AtomicU64,
    found: AtomicU64,
    /// Submissions the pool/node answered yes to.
    accepted: AtomicU64,
    /// Submissions answered no (stale, busy, refused).
    refused: AtomicU64,
    /// Submissions without an answer (transport error, timeout): the block
    /// may or may not have been accepted, so they count as neither.
    unknown: AtomicU64,
}

const HUGE_PENDING: u8 = 0;
const HUGE_OK: u8 = 1;
const HUGE_NO: u8 = 2;
const HUGE_UNKNOWN: u8 = 3;

impl Shared {
    fn new(job: Option<Arc<Job>>, deadline_ns: u64, th: Arc<Thermal>, workers: usize) -> Shared {
        Shared {
            job: RwLock::new(job),
            deadline_ns: AtomicU64::new(deadline_ns),
            t0: Instant::now(),
            stop: AtomicBool::new(false),
            hashed: AtomicU64::new(0),
            busy_ns: AtomicU64::new(0),
            th,
            huge: (0..workers).map(|_| AtomicU8::new(HUGE_PENDING)).collect(),
            solved_epoch: AtomicU64::new(0),
            found: AtomicU64::new(0),
            accepted: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            unknown: AtomicU64::new(0),
        }
    }

    /// Wait (up to `timeout`) for every worker to report its pages; returns
    /// (in 2M pages, reported, workers).
    fn huge_status(&self, timeout: Duration) -> (usize, usize, usize) {
        let t = Instant::now();
        loop {
            let st: Vec<u8> = self.huge.iter().map(|a| a.load(Ordering::Acquire)).collect();
            let done = st.iter().filter(|&&s| s != HUGE_PENDING).count();
            if done == st.len() || t.elapsed() >= timeout {
                return (st.iter().filter(|&&s| s == HUGE_OK).count(), done, st.len());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn now_ns(&self) -> u64 {
        self.t0.elapsed().as_nanos() as u64
    }
}

struct Found {
    epoch: u64,
    template_id: String,
    nonce: u128,
    digest: [u8; 32],
    t_found: Instant,
}

#[inline]
fn le256_lt(a: &[u8; 32], b: &[u8; 32]) -> bool {
    for i in (0..32).rev() {
        if a[i] != b[i] {
            return a[i] < b[i];
        }
    }
    false
}

fn diverged(what: &str) -> ! {
    eprintln!("FATAL: kernel diverged from the reference walk ({what}). Not submitting; exiting {EXIT_DIVERGED}.");
    std::process::exit(EXIT_DIVERGED);
}

/// Nonce layout: `base` (pool region 96..128 | process offset 64..96) |
/// worker id 52..64 | counter 0..52. 2^52 nonces per worker per job (about
/// 10^11 years at 1 kH/s); ids alias beyond 4096 workers.
#[inline]
fn nonce_at(base: u128, id: usize, counter: u128) -> u128 {
    debug_assert!(id < 4096);
    base | ((id as u128 & 0xFFF) << 52) | (counter & ((1u128 << 52) - 1))
}

fn worker(id: usize, cpu: usize, prof: &Profile, sh: &Shared, tx: mpsc::Sender<Found>) {
    if let Err(e) = sys::pin_current(cpu) {
        // Once per process: an unpinned worker still hashes right, but the
        // profile's per-core layout (and NUMA placement) no longer holds.
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            eprintln!("\x1b[31mWARNING: cannot pin worker {id} to CPU {cpu} ({e}); workers that fail to pin run unpinned\x1b[0m")
        });
    }
    let region = match sys::Region::new(prof.pads, prof.huge) {
        Ok(r) => r,
        Err(e) => thermal::exit_once(1, &format!("fatal: worker {id}: cannot map its scratchpad region: {e}")),
    };
    let state = match region.huge_kb() {
        None => HUGE_UNKNOWN,
        Some(kb) if prof.huge && kb >= region.expected_huge_kb() => HUGE_OK,
        Some(_) => HUGE_NO,
    };
    sh.huge[id].store(state, Ordering::Release);
    let p = prof.pads;
    let mut oracle_pad = Scratch::new();
    let mut seeds = [[0u8; 32]; SPONGE_BATCH];
    let mut outs = [[0u8; 32]; MAX_PADS];
    let mut cur: Option<Arc<Job>> = None;
    let mut hasher: Option<FixedFieldNonceBatch> = None;
    let mut counter: u128 = 0;
    let mut since_sentinel: u64 = 0;
    // Seed ring: `ring_n` seeds per sponge call (the 8-leaf AVX2 sponge halves
    // the cost of a seed from k = 8 on), consumed p at a time — or one at a
    // time when piped, double-buffered so the successor of the last ring
    // entry (whose fill anchors ride in the current walk) is always known.
    // ring[ri] is the seed of nonce_at(counter).
    let ring_n = prof.ring_len();
    let mut ring = vec![[0u8; 32]; ring_n];
    let mut ring2 = vec![[0u8; 32]; if prof.pipe { ring_n } else { 0 }];
    let mut ri = ring_n;
    let mut primed = false;
    let mut pre: Option<walk::Pre> = None;
    let mut cur_seeds = [[0u8; 32]; MAX_PADS];
    while !sh.stop.load(Ordering::Relaxed) {
        if !thermal::worker_may_hash(&sh.th) || !thermal::gate_open(&sh.th) {
            continue;
        }
        let job = sh.job.read().unwrap().clone();
        let live = job.is_some() && sh.now_ns() < sh.deadline_ns.load(Ordering::Relaxed);
        if !live {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        let job = job.unwrap();
        if cur.as_ref().map(|c| c.epoch) != Some(job.epoch) {
            hasher = Some(FixedFieldNonceBatch::new(TAG_POWHDR, &job.fields, NONCE_FIELD));
            counter = 0;
            ri = ring_n;
            primed = false;
            pre = None;
            cur = Some(job.clone());
        }
        let h = hasher.as_mut().unwrap();
        let start = nonce_at(job.base, id, counter);
        let t = Instant::now();
        if job.walk {
            if ri >= ring_n {
                if prof.pipe {
                    if primed {
                        std::mem::swap(&mut ring, &mut ring2);
                    } else {
                        h.hash_into(start, &mut ring);
                        primed = true;
                    }
                    h.hash_into(nonce_at(job.base, id, counter + ring_n as u128), &mut ring2);
                } else {
                    h.hash_into(start, &mut ring);
                }
                ri = 0;
            }
            let n_done = if prof.pipe {
                let seed = ring[ri];
                let next = if ri + 1 < ring_n { ring[ri + 1] } else { ring2[0] };
                let pc = match pre.take() {
                    Some(x) => x,
                    None => walk::pre_of(&seed),
                };
                let (d, pn) = unsafe { walk::walk_pipe_dyn(prof.prefetch, region.pads[0], &seed, &pc, Some(&next)) };
                pre = pn;
                outs[0] = d;
                cur_seeds[0] = seed;
                1
            } else {
                cur_seeds[..p].copy_from_slice(&ring[ri..ri + p]);
                unsafe { walk_dyn(p, prof.prefetch, prof.kernel, &region.pads, &cur_seeds[..p], &mut outs[..p]) };
                p
            };
            for k in 0..n_done {
                if le256_lt(&outs[k], &job.target) {
                    // Never submit what the reference walk would not reproduce.
                    let check = gate::oracle(&mut oracle_pad, &cur_seeds[k]);
                    if check != outs[k] {
                        diverged("candidate solution");
                    }
                    let _ = tx.send(Found {
                        epoch: job.epoch,
                        template_id: job.template_id.clone(),
                        nonce: start + k as u128,
                        digest: outs[k],
                        t_found: Instant::now(),
                    });
                }
            }
            since_sentinel += n_done as u64;
            if since_sentinel >= SENTINEL_EVERY {
                since_sentinel = 0;
                let slot = (counter as usize / n_done) % n_done;
                if gate::oracle(&mut oracle_pad, &cur_seeds[slot]) != outs[slot] {
                    diverged("sentinel");
                }
            }
            counter += n_done as u128;
            ri += n_done;
            sh.hashed.fetch_add(n_done as u64, Ordering::Relaxed);
        } else {
            // Pre-fork rule: the Poseidon2b digest itself is the PoW.
            h.hash_into(start, &mut seeds);
            for (k, s) in seeds.iter().enumerate() {
                if le256_lt(s, &job.target) {
                    let _ = tx.send(Found {
                        epoch: job.epoch,
                        template_id: job.template_id.clone(),
                        nonce: start + k as u128,
                        digest: *s,
                        t_found: Instant::now(),
                    });
                }
            }
            counter += SPONGE_BATCH as u128;
            sh.hashed.fetch_add(SPONGE_BATCH as u64, Ordering::Relaxed);
        }
        sh.busy_ns.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// RPC (identical wire format to jetsam-extminer)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct BlockTemplateResponse {
    template_id: String,
    pow_fields_hex: String,
    nonce_field_index: usize,
    difficulty_target_hex: String,
    height: u64,
    expires_in_seconds: u64,
    #[serde(default)]
    n_txs: usize,
    #[serde(default)]
    nonce_prefix: Option<u32>,
    #[serde(default)]
    pow_walk: bool,
}

#[derive(Serialize)]
struct Req<'a, P: Serialize> {
    jsonrpc: &'a str,
    id: u32,
    method: &'a str,
    params: P,
}

#[derive(Deserialize)]
struct Resp<T> {
    result: Option<T>,
    error: Option<serde_json::Value>,
}

struct Rpc {
    url: String,
    key: Option<String>,
    /// Self-description sent to the pool, so its dashboard names this machine
    /// without a hand-kept IP table (which goes stale silently).
    host: String,
    cpu: String,
    http: reqwest::blocking::Client,
    rate: Arc<AtomicU64>,
    /// The regulator's duty and the policy go into the CPU line the pool
    /// shows (" pwm62%", " eff"): the dashboard sees the thermal regime.
    th: Option<Arc<Thermal>>,
    eff: bool,
}

/// Why a call failed: no HTTP answer at all (connection, timeout: the far
/// end may still have acted on it), or an answer that says no.
#[derive(Debug)]
enum CallErr {
    Transport(String),
    Answer(String),
}

impl std::fmt::Display for CallErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallErr::Transport(e) | CallErr::Answer(e) => f.write_str(e),
        }
    }
}

/// Classify a submit refusal the way the pool words it.
fn classify_refusal(msg: &str) -> &'static str {
    let m = msg.to_ascii_lowercase();
    if ["expired", "unknown", "consumed", "stale"].iter().any(|w| m.contains(w)) {
        "stale"
    } else if m.contains("busy") {
        "busy"
    } else {
        "refused"
    }
}

impl Rpc {
    fn cpu_header(&self) -> String {
        let d = self.th.as_ref().map(|t| t.duty_milli.load(Ordering::Relaxed)).unwrap_or(1000);
        let mut sfx = String::new();
        if d < 1000 {
            sfx.push_str(&format!(" pwm{}%", (d + 5) / 10));
        }
        if self.eff {
            sfx.push_str(" eff");
        }
        if sfx.is_empty() {
            self.cpu.clone()
        } else {
            format!("{}{sfx}", header_safe(&self.cpu, 64 - sfx.len()))
        }
    }

    fn call_typed<P: Serialize, R: for<'de> Deserialize<'de>>(&self, method: &str, params: P) -> std::result::Result<R, CallErr> {
        let mut req = self.http.post(&self.url).json(&Req { jsonrpc: "2.0", id: 1, method, params });
        if let Some(k) = &self.key {
            req = req.header("Authorization", format!("Bearer {k}"));
        }
        let r = self.rate.load(Ordering::Relaxed);
        if r > 0 {
            req = req.header("X-Jetsam-Hashrate", r.to_string());
        }
        req = req
            .header("X-Jetsam-Version", VERSION)
            .header("X-Jetsam-PoW", "walk")
            .header("X-Jetsam-Host", &self.host)
            .header("X-Jetsam-CPU", self.cpu_header());
        let resp = req.send().map_err(|e| CallErr::Transport(format!("POST {}: {e}", self.url)))?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(CallErr::Answer("401 Unauthorized — wrong or missing --key".into()));
        }
        if !resp.status().is_success() {
            return Err(CallErr::Answer(format!("HTTP {} from {}", resp.status(), self.url)));
        }
        let body: Resp<R> = resp.json().map_err(|e| CallErr::Answer(format!("decode JSON-RPC response: {e}")))?;
        if let Some(e) = body.error {
            return Err(CallErr::Answer(format!("RPC error: {e}")));
        }
        body.result.ok_or_else(|| CallErr::Answer("RPC returned null result".into()))
    }
}

/// Where a found block goes: the pool/node (`Rpc`), or a test double.
trait Submit: Send + Sync {
    /// One `jetsam_submitBlock`; Ok = the block hash.
    fn submit(&self, template_id: &str, nonce_hex: &str) -> std::result::Result<String, CallErr>;
}

impl Submit for Rpc {
    fn submit(&self, template_id: &str, nonce_hex: &str) -> std::result::Result<String, CallErr> {
        self.call_typed::<_, String>("jetsam_submitBlock", (template_id, nonce_hex))
    }
}

fn decode_fields(hex_str: &str) -> Result<[Block128; FIELDS]> {
    let b = hex::decode(hex_str)?;
    if b.len() != FIELDS * 16 {
        return Err(anyhow!("pow_fields_hex must be {} bytes, got {}", FIELDS * 16, b.len()));
    }
    let mut f = [Block128::from(0u128); FIELDS];
    for (i, c) in b.chunks_exact(16).enumerate() {
        f[i] = Block128::from(u128::from_le_bytes(c.try_into().unwrap()));
    }
    Ok(f)
}

// ---------------------------------------------------------------------------
// Start-up
// ---------------------------------------------------------------------------

fn banner(prof: &Profile) {
    eprintln!(
        "{VERSION}  backend={}  L2={} KiB  L3={} KiB  THP={}",
        jetsam_core::cpu::selected_backend(),
        prof.l2_kib,
        prof.l3_kib,
        sys::thp_mode()
    );
    eprintln!(
        "profile: {} threads ({} per core)  pads/thread={}  prefetch={}  pages={}  kernel={}  ring={}  policy={:?}  [{}]",
        prof.cpus.len(),
        prof.per_core,
        prof.pads,
        prof.prefetch as u8,
        if prof.huge { "2M(THP)" } else { "4K" },
        walk::describe(prof.kernel, prof.pads, prof.pipe),
        prof.ring_len(),
        prof.policy,
        prof.source
    );
    // Report other miners, never touch them.
    let others = sys::other_miners();
    if !others.is_empty() {
        eprintln!("note: other miners on this machine (left alone): {}", others.join(" "));
    }
}

/// Resolve the sensor and start the guard, before any hashing (the self-test
/// included). No sensor: exit 5, unless --no-thermal-guard.
fn start_thermal(cli: &Cli) -> Arc<Thermal> {
    let sensor = if cli.debug_no_sensor { None } else { thermal::Sensor::detect() };
    let t0 = Instant::now();
    match sensor {
        None if !cli.no_thermal_guard => thermal::exit_once(
            thermal::EXIT_NO_SENSOR,
            "\x1b[31mTHERMAL: no CPU temperature sensor found (k10temp / zenpower / coretemp). Refusing to mine \
             without a thermal guard; pass --no-thermal-guard to run anyway. Exit 5.\x1b[0m",
        ),
        None => {
            eprintln!("\x1b[31mWARNING: --no-thermal-guard: NO temperature sensor, NO thermal protection.\x1b[0m");
            Arc::new(Thermal::new(t0, false))
        }
        Some(s) => {
            let th = Arc::new(Thermal::new(t0, true));
            let mining = !cli.gate && cli.bench_walk.is_none() && cli.tune.is_none() && cli.check_nonces.is_none();
            let regulate = mining || (cli.bench_walk.is_some() && cli.bench_regulate);
            let cfg = thermal::GuardCfg {
                stop_c: cli.temp_stop,
                hard_c: cli.debug_temp_hard.unwrap_or(thermal::HARD_LIMIT_C),
                poll_ms: cli.temp_poll_ms,
                stall: cli.debug_guard_stall.map(|s| (5.0, s)),
                reg: regulate.then(|| thermal::RegCfg {
                    target_c: cli.temp_target(),
                    min_duty: cli.min_duty,
                    period_ms: cli.pwm_period_ms,
                    ramp_secs: if mining { cli.ramp_secs } else { 0.0 },
                }),
            };
            eprintln!(
                "thermal guard: {}  now {:.1} C  stop {:.0} C (predictive vs {:.0} C)  poll {} ms  {}",
                s.describe(),
                s.read().map(|r| r.max_mc() as f64 / 1000.0).unwrap_or(f64::NAN),
                cfg.stop_c,
                cfg.hard_c,
                cfg.poll_ms,
                match &cfg.reg {
                    Some(r) => format!(
                        "regulator: target {:.0} C, pwm period {} ms, min duty {:.2}{}",
                        r.target_c,
                        r.period_ms,
                        r.min_duty,
                        if r.ramp_secs > 0.0 { format!(", ramp {:.0} s from 0.30", r.ramp_secs) } else { String::new() }
                    ),
                    None => "regulator: off".into(),
                }
            );
            thermal::Guard::start(s, cfg, th.clone());
            th
        }
    }
}

fn start_workers(prof: &Arc<Profile>, sh: &Arc<Shared>) -> (mpsc::Receiver<Found>, Vec<std::thread::JoinHandle<()>>) {
    let (tx, rx) = mpsc::channel();
    let mut hs = Vec::new();
    for (id, &cpu) in prof.cpus.iter().enumerate() {
        let (prof, sh, tx) = (prof.clone(), sh.clone(), tx.clone());
        hs.push(
            std::thread::Builder::new()
                .name(format!("walk{id}"))
                .spawn(move || worker(id, cpu, &prof, &sh, tx))
                .expect("spawn worker"),
        );
    }
    (rx, hs)
}

fn fmt_rate(h: f64) -> String {
    if h >= 1e6 {
        format!("{:.2} MH/s", h / 1e6)
    } else if h >= 1e3 {
        format!("{:.2} kH/s", h / 1e3)
    } else {
        format!("{h:.0} H/s")
    }
}

/// Every worker's pads in 2 MiB pages? Prints the result once; exits 4 under
/// --require-huge when they are not. Returns (in 2M pages, workers).
fn check_huge(cli: &Cli, prof: &Profile, sh: &Shared, quiet: bool) -> (usize, usize) {
    let (ok, done, n) = sh.huge_status(Duration::from_secs(2));
    if !prof.huge {
        if !quiet {
            eprintln!("huge pages: off (--no-huge): {n} workers in 4 KiB pages");
        }
        return (ok, n);
    }
    if ok == n {
        if !quiet {
            eprintln!("huge pages: {ok}/{n} workers in 2 MiB pages");
        }
    } else {
        let msg = format!(
            "huge pages: only {ok}/{n} workers in 2 MiB pages ({} not reported, THP={}): 4 KiB pads cost ~24 % of the rate",
            n - done,
            sys::thp_mode()
        );
        if cli.require_huge {
            thermal::exit_once(EXIT_NO_HUGE, &format!("\x1b[31m{msg}. --require-huge: exit {EXIT_NO_HUGE}.\x1b[0m"));
        }
        eprintln!("\x1b[31mWARNING: {msg}\x1b[0m");
    }
    (ok, n)
}

#[derive(Default, Clone)]
struct Measured {
    hps: f64,
    /// Peak of max(Tctl, Tccd*) during the window.
    peak_c: f64,
    tctl_max: f64,
    ccd_max: Vec<f64>,
    huge_ok: usize,
    workers: usize,
    anon_huge_kb: u64,
    /// Package / core power over the window (RAPL), W.
    w_pkg: Option<f64>,
    w_core: Option<f64>,
    w_min: Option<f64>,
    w_max: Option<f64>,
    /// Idle package power measured before the workers started, W.
    w_idle: Option<f64>,
    /// Mean scaling_cur_freq of the profile's CPUs, MHz.
    f_avg: Option<f64>,
    /// Share of the targeted CPUs kept busy by OTHER processes over the
    /// window (0..1): their busy time minus this process's CPU time.
    foreign: Option<f64>,
}

/// Snapshot for `foreign_share`: busy/total jiffies of the targeted CPUs and
/// this process's own CPU time.
struct CpuSnap {
    busy: u64,
    total: u64,
    own: u64,
}

fn cpu_snap(cpus: &[usize]) -> Option<CpuSnap> {
    let (busy, total) = sys::cpu_ticks(cpus)?;
    Some(CpuSnap { busy, total, own: sys::self_ticks()? })
}

/// Share of the CPUs' time between two snapshots spent by other processes.
fn foreign_share(a: &CpuSnap, b: &CpuSnap) -> Option<f64> {
    let total = b.total.checked_sub(a.total).filter(|&t| t > 0)?;
    let busy = b.busy.saturating_sub(a.busy);
    let own = b.own.saturating_sub(a.own);
    Some(busy.saturating_sub(own) as f64 / total as f64)
}

/// The CPUs whose load can bias a --tune measurement: the allowed set plus
/// the SMT siblings of every allowed CPU, even outside the set — a neighbour
/// on a sibling slows the core as much as one on the CPU itself.
fn targeted_cpus(cli: &Cli) -> Vec<usize> {
    let mut set = allowed_set(cli).unwrap_or_default();
    for c in set.clone() {
        if let Ok(s) = std::fs::read_to_string(format!("/sys/devices/system/cpu/cpu{c}/topology/thread_siblings_list")) {
            set.extend(sys::parse_list(&s));
        }
    }
    set.into_iter().collect()
}

impl Measured {
    fn hpj(&self) -> Option<f64> {
        self.w_pkg.map(|w| self.hps / w)
    }
    fn hpj_marg(&self) -> Option<f64> {
        match (self.w_pkg, self.w_idle) {
            (Some(w), Some(i)) if w > i => Some(self.hps / (w - i)),
            _ => None,
        }
    }
}

fn opt(v: Option<f64>, prec: usize) -> String {
    v.map(|x| format!("{x:.prec$}")).unwrap_or_else(|| "n/a".into())
}

/// Walk rate of `prof` on a dummy job, after a 2 s warm-up (pads faulted,
/// THP collapsed), with package power (RAPL, 1 Hz), temperatures and the mean
/// clock over the window. `idle_secs` > 0 first measures the idle package
/// power (marginal H/J). The thermal guard runs throughout (it exits the
/// process at the stop temperature).
fn measure(
    cli: &Cli,
    th: &Arc<Thermal>,
    rapl: Option<&rapl::Rapl>,
    prof: Arc<Profile>,
    secs: u64,
    idle_secs: u64,
    quiet: bool,
) -> Measured {
    let mut m = Measured::default();
    if let (Some(r), true) = (rapl, idle_secs > 0) {
        if let Some(a) = r.sample() {
            std::thread::sleep(Duration::from_secs(idle_secs));
            if let Some(b) = r.sample() {
                m.w_idle = Some(r.watts(&a, &b).0);
            }
        }
    }
    let job = Arc::new(Job {
        epoch: 1,
        template_id: "bench".into(),
        height: 0,
        fields: [Block128::from(0x9e37_79b9_7f4a_7c15u128); FIELDS],
        target: [0u8; 32],
        walk: true,
        base: 0,
    });
    let sh = Arc::new(Shared::new(Some(job), u64::MAX, th.clone(), prof.cpus.len()));
    let (_rx, hs) = start_workers(&prof, &sh);
    (m.huge_ok, m.workers) = check_huge(cli, &prof, &sh, quiet);
    std::thread::sleep(Duration::from_secs(2));
    let targets = targeted_cpus(cli);
    let snap0 = cpu_snap(&targets);
    let h0 = sh.hashed.load(Ordering::Relaxed);
    let t = Instant::now();
    th.reset_peak();
    let e0 = rapl.and_then(|r| r.sample());
    let mut e_prev = e0.clone();
    let (mut w_min, mut w_max) = (f64::MAX, 0.0f64);
    let mut freqs: Vec<f64> = Vec::new();
    let mut next_sample = t + Duration::from_secs(1);
    let every = cli.report_secs.map(|s| Duration::from_secs(s.max(1)));
    let mut next = every.map(|e| t + e);
    let mut last = (t, h0);
    let mut ccd_max: Vec<f64> = Vec::new();
    while t.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_millis(50));
        let now = Instant::now();
        m.tctl_max = m.tctl_max.max(th.tctl_c());
        for (i, c) in th.ccd_c().into_iter().enumerate() {
            if ccd_max.len() <= i {
                ccd_max.push(c);
            }
            ccd_max[i] = ccd_max[i].max(c);
        }
        if now >= next_sample {
            next_sample += Duration::from_secs(1);
            if let (Some(r), Some(a)) = (rapl, e_prev.as_ref()) {
                if let Some(b) = r.sample() {
                    let w = r.watts(a, &b).0;
                    (w_min, w_max) = (w_min.min(w), w_max.max(w));
                    e_prev = Some(b);
                }
            }
            if let Some(f) = rapl::mean_freq_mhz(&prof.cpus) {
                freqs.push(f);
            }
        }
        if let (Some(e), Some(n)) = (every, next) {
            if now >= n {
                next = Some(n + e);
                let h = sh.hashed.load(Ordering::Relaxed);
                let r = (h - last.1) as f64 / now.duration_since(last.0).as_secs_f64();
                last = (now, h);
                eprintln!(
                    "  t={:6.2}s  hashed={h}  rate={}  pwm={:.2}  {}",
                    now.duration_since(t).as_secs_f64(),
                    fmt_rate(r),
                    th.duty(),
                    th.summary()
                );
            }
        }
    }
    let n = sh.hashed.load(Ordering::Relaxed) - h0;
    let el = t.elapsed().as_secs_f64();
    if let (Some(a), Some(b)) = (snap0.as_ref(), cpu_snap(&targets).as_ref()) {
        m.foreign = foreign_share(a, b);
    }
    if let (Some(r), Some(a)) = (rapl, e0.as_ref()) {
        if let Some(b) = r.sample() {
            let (p, c) = r.watts(a, &b);
            m.w_pkg = Some(p);
            m.w_core = c;
        }
    }
    if w_max > 0.0 {
        (m.w_min, m.w_max) = (Some(w_min), Some(w_max));
    }
    m.f_avg = (!freqs.is_empty()).then(|| freqs.iter().sum::<f64>() / freqs.len() as f64);
    m.peak_c = th.peak_c();
    m.ccd_max = ccd_max;
    m.anon_huge_kb = sys::anon_huge_kb(); // while the pads are still mapped
    sh.stop.store(true, Ordering::Relaxed);
    for h in hs {
        let _ = h.join();
    }
    if !quiet {
        // Exact hash count of the whole run, for perf-stat ratios.
        println!("TOTAL_HASHED={}", sh.hashed.load(Ordering::Relaxed));
    }
    m.hps = n as f64 / el;
    m
}

/// Nonce-accounting gate (release gate, not covered by --gate): the seed
/// ring and the pipelined anchors decide which nonce a digest is reported
/// under. One digest in 16 meets the dummy target, so solutions stream out;
/// each one is recomputed from its reported nonce through the real seed path
/// and the reference walk. Duplicates and nonces outside the region fail.
fn check_nonces(cli: &Cli, th: &Arc<Thermal>, prof: Arc<Profile>, secs: u64) -> bool {
    let fields = [Block128::from(0x9e37_79b9_7f4a_7c15u128); FIELDS];
    let mut target = [0xFFu8; 32];
    target[31] = 0x10;
    let base: u128 = (7u128 << NONCE_REGION_SHIFT) | (0xABCDu128 << 64);
    let job = Arc::new(Job { epoch: 1, template_id: "check".into(), height: 0, fields, target, walk: true, base });
    let sh = Arc::new(Shared::new(Some(job), u64::MAX, th.clone(), prof.cpus.len()));
    let (rx, hs) = start_workers(&prof, &sh);
    // Solutions take the mining path: worker -> submit thread -> sink. The
    // sink here is the verifier; the latency is found -> submit.
    let (vtx, vrx) = mpsc::channel::<(Found, Duration)>();
    let submitter = start_submitter(rx, Sink::Check(vtx), &sh);
    check_huge(cli, &prof, &sh, false);
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut sc = Scratch::new();
    let mut seen = std::collections::HashSet::new();
    let mut lat_us: Vec<u64> = Vec::new();
    let (mut ok, mut bad, mut dup, mut region_bad) = (0u64, 0u64, 0u64, 0u64);
    while Instant::now() < deadline {
        if let Ok((f, lat)) = vrx.recv_timeout(Duration::from_millis(100)) {
            lat_us.push(lat.as_micros() as u64);
            let seed = gate::seed_of(&fields, f.nonce);
            let d = gate::oracle(&mut sc, &seed);
            if d == f.digest && le256_lt(&d, &target) {
                ok += 1;
            } else {
                bad += 1;
                eprintln!("MISMATCH nonce={} reported={} oracle={}", f.nonce, hex::encode(f.digest), hex::encode(d));
            }
            if !seen.insert(f.nonce) {
                dup += 1;
            }
            if f.nonce >> 64 != base >> 64 {
                region_bad += 1;
            }
        }
    }
    sh.stop.store(true, Ordering::Relaxed);
    for h in hs {
        let _ = h.join();
    }
    let _ = submitter.join();
    let hashed = sh.hashed.load(Ordering::Relaxed);
    lat_us.sort_unstable();
    let pct = |q: f64| lat_us.get(((lat_us.len() as f64 - 1.0) * q).round() as usize).copied().unwrap_or(0);
    println!(
        "CHECK-NONCES kernel={} pads={} prefetch={} ring={} hashed={hashed} solutions={} ok={ok} bad={bad} dup={dup} \
         outside_region={region_bad} unchecked={} submit_latency_us p50={} p99={} max={}",
        walk::describe(prof.kernel, prof.pads, prof.pipe),
        prof.pads,
        prof.prefetch as u8,
        prof.ring_len(),
        ok + bad,
        vrx.try_iter().count(),
        pct(0.5),
        pct(0.99),
        lat_us.last().copied().unwrap_or(0)
    );
    bad == 0 && dup == 0 && region_bad == 0 && ok > 0
}

fn open_rapl() -> Option<rapl::Rapl> {
    let r = rapl::Rapl::open();
    if r.is_none() {
        eprintln!("note: RAPL energy counters not readable (root or passwordless sudo needed): rapl=n/a");
    }
    r
}

fn run_bench(cli: &Cli, th: &Arc<Thermal>, prof: Arc<Profile>, secs: u64) {
    let rapl = open_rapl();
    let m = measure(cli, th, rapl.as_ref(), prof.clone(), secs, cli.idle_secs, false);
    let cores = prof.cpus.len().div_ceil(prof.per_core);
    let ccd: Vec<String> = m.ccd_max.iter().map(|c| format!("{c:.1}")).collect();
    println!(
        "BENCH-WALK threads={} ({}/core) pads={} prefetch={} pages={} kernel={} ring={} : {} ({:.1} H/s per thread)  \
         hps={:.1}  W_pkg={}  W_core={}  W_min={}  W_max={}  W_idle={}  HpJ={}  HpJ_marg={}  Tctl_max={:.1}  Tccd={}  \
         peak={:.1}  f_avg={}  cyc_per_hash={}  huge={}/{}  anon_huge={}KiB  rapl={}  guard={}",
        prof.cpus.len(),
        prof.per_core,
        prof.pads,
        prof.prefetch as u8,
        if prof.huge { "2M" } else { "4K" },
        walk::describe(prof.kernel, prof.pads, prof.pipe),
        prof.ring_len(),
        fmt_rate(m.hps),
        m.hps / prof.cpus.len() as f64,
        m.hps,
        opt(m.w_pkg, 1),
        opt(m.w_core, 1),
        opt(m.w_min, 1),
        opt(m.w_max, 1),
        opt(m.w_idle, 1),
        opt(m.hpj(), 2),
        opt(m.hpj_marg(), 2),
        m.tctl_max,
        if ccd.is_empty() { "n/a".into() } else { ccd.join("/") },
        m.peak_c,
        opt(m.f_avg, 0),
        // Core cycles per hash [derived]: mean clock x cores / rate.
        m.f_avg.map(|f| format!("{:.2}M", f * 1e6 * cores as f64 / m.hps / 1e6)).unwrap_or_else(|| "n/a".into()),
        m.huge_ok,
        m.workers,
        m.anon_huge_kb,
        rapl.as_ref().map(|r| r.mode.name()).unwrap_or("n/a"),
        if th.guarded { "ok" } else { "OFF" }
    );
}

fn utc_now() -> String {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let (d, s) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    // civil-from-days (Howard Hinnant), UTC only
    let z = d + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", s / 3600, (s / 60) % 60, s % 60)
}

fn cool_below(th: &Thermal, c: f64, max_secs: u64) {
    let mut w = 0;
    while th.guarded && th.temp_c() >= c && w < max_secs {
        std::thread::sleep(Duration::from_secs(1));
        w += 1;
    }
}

/// Try the candidate shapes (x kernels) on this machine; keep the fastest and
/// the most efficient. Two rounds in alternating order so a slow drift (heat,
/// a neighbour) cannot favour whichever candidate ran first; every candidate
/// starts below --tune-cool. A candidate that got hotter than the regulator
/// target is disqualified: its number measures the cooler, not the kernel.
/// The regulator is off here (the stop still applies).
///
/// Other processes on the targeted CPUs (the allowed set and its SMT
/// siblings) are measured 2 s before each candidate (workers stopped) and
/// during it (busy time minus our own): a
/// candidate that saw more than 5 % is `noisy`, and then the result is NOT
/// stored unless --tune-force — a noisy tune would pin a wrong profile for
/// every later run.
fn run_tune(cli: &Cli, th: &Arc<Thermal>, secs: u64) -> Result<()> {
    let base = profile(cli)?;
    let targets = targeted_cpus(cli);
    let big = base.l2_kib >= 1024;
    let mut shapes: Vec<(usize, usize, bool)> =
        vec![(1, 1, false), (1, 2, false), (1, 2, true), (2, 1, false), (2, 1, true), (2, 2, false), (2, 2, true)];
    if !big || base.l3_kib >= 65_536 {
        shapes.push((1, 3, true));
    }
    let mut kernels: Vec<u8> = cli
        .tune_kernels
        .iter()
        .map(|k| if *k == KernelArg::Base { walk::K_BASE } else { walk::K_FAST })
        .collect();
    kernels.dedup();
    let cands: Vec<Shape> =
        shapes.iter().flat_map(|&(t, p, f)| kernels.iter().map(move |&k| Shape::new(t, p, f, k))).collect();
    let rapl = open_rapl();
    // Idle power once, cool machine, before any candidate.
    cool_below(th, cli.tune_cool, 300);
    let w_idle = rapl.as_ref().and_then(|r| {
        let a = r.sample()?;
        std::thread::sleep(Duration::from_secs(cli.idle_secs.max(1)));
        Some(r.watts(&a, &r.sample()?).0)
    });
    let mut res: Vec<Vec<Measured>> = vec![Vec::new(); cands.len()];
    let mut hot = vec![false; cands.len()];
    // Highest foreign share seen per candidate (before or during, both rounds).
    let mut foreign: Vec<Option<f64>> = vec![None; cands.len()];
    for round in 0..2 {
        let order: Vec<usize> = if round == 0 { (0..cands.len()).collect() } else { (0..cands.len()).rev().collect() };
        for i in order {
            let p = Arc::new(profile_with(cli, Some(cands[i]))?);
            cool_below(th, cli.tune_cool, 300);
            let before = cpu_snap(&targets).and_then(|a| {
                std::thread::sleep(Duration::from_secs(2));
                foreign_share(&a, &cpu_snap(&targets)?)
            });
            let m = measure(cli, th, rapl.as_ref(), p.clone(), secs, 0, true);
            let over = m.peak_c > cli.temp_target();
            let f = [before, m.foreign].into_iter().flatten().reduce(f64::max);
            let noisy = f.map_or(false, |x| x > TUNE_FOREIGN_MAX);
            eprintln!(
                "tune  {:<12} {:>10}  {:>6} W  {:>7} H/J  peak {:.0} C  foreign {} / {} %{}{}",
                cands[i].label(),
                fmt_rate(m.hps),
                opt(m.w_pkg, 1),
                opt(m.hpj(), 2),
                m.peak_c,
                opt(before.map(|x| x * 100.0), 1),
                opt(m.foreign.map(|x| x * 100.0), 1),
                if noisy { "  NOISY" } else { "" },
                if over { "  (above the regulator target: disqualified)" } else { "" }
            );
            res[i].push(m);
            hot[i] |= over;
            foreign[i] = [foreign[i], f].into_iter().flatten().reduce(f64::max);
        }
    }
    let noisy: Vec<bool> = foreign.iter().map(|f| f.map_or(false, |x| x > TUNE_FOREIGN_MAX)).collect();
    // Spread of the two passes: sample coefficient of variation of the rate.
    let cv: Vec<Option<f64>> = res
        .iter()
        .map(|r| {
            let n = r.len() as f64;
            let mean = r.iter().map(|m| m.hps).sum::<f64>() / n;
            (r.len() >= 2 && mean > 0.0).then(|| {
                (r.iter().map(|m| (m.hps - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt() / mean
            })
        })
        .collect();
    let med = |v: Vec<f64>| {
        let mut v = v;
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        if v.is_empty() { 0.0 } else { (v[(v.len() - 1) / 2] + v[v.len() / 2]) / 2.0 }
    };
    let hps: Vec<f64> = res.iter().map(|r| med(r.iter().map(|m| m.hps).collect())).collect();
    let w: Vec<Option<f64>> =
        res.iter().map(|r| r.iter().all(|m| m.w_pkg.is_some()).then(|| med(r.iter().filter_map(|m| m.w_pkg).collect()))).collect();
    let hpj: Vec<Option<f64>> = (0..cands.len()).map(|i| w[i].map(|w| hps[i] / w)).collect();
    let ok: Vec<usize> = (0..cands.len()).filter(|&i| !hot[i]).collect();
    let best_hps = *ok
        .iter()
        .max_by(|&&a, &&b| hps[a].partial_cmp(&hps[b]).unwrap())
        .ok_or_else(|| anyhow!("every candidate ran above the regulator target — cool the machine or pass --cpus"))?;
    let best_hpj = ok
        .iter()
        .copied()
        .filter(|&i| hpj[i].is_some())
        .max_by(|&a, &b| hpj[a].partial_cmp(&hpj[b]).unwrap())
        .unwrap_or(best_hps);
    let entry = |i: usize| {
        let c = cands[i];
        let p = profile_with(cli, Some(c)).ok();
        serde_json::json!({
            "tpc": c.tpc, "pads": c.pads, "pf": c.pf, "kernel": walk::kernel_name(c.kernel),
            "ring": p.as_ref().map(|p| p.ring_len()), "pipe": p.as_ref().map(|p| p.pipe),
            "hps": hps[i].round(), "w": w[i].map(|x| (x * 10.0).round() / 10.0),
            "hpj": hpj[i].map(|x| (x * 100.0).round() / 100.0), "peak_c": res[i].iter().map(|m| m.peak_c).fold(0.0, f64::max),
            "disqualified": hot[i],
            "foreign_busy": foreign[i].map(|x| (x * 1000.0).round() / 1000.0),
            "cv": cv[i].map(|x| (x * 10000.0).round() / 10000.0),
            "noisy": noisy[i],
        })
    };
    let any_noisy = noisy.iter().any(|&n| n);
    let v = serde_json::json!({
        "version": 3,
        "key": base.tune_key,
        "best_hps": entry(best_hps),
        "best_hpj": entry(best_hpj),
        "w_idle": w_idle.map(|x| (x * 10.0).round() / 10.0),
        "noisy": any_noisy,
        "forced": any_noisy && cli.tune_force,
        "foreign_max": TUNE_FOREIGN_MAX,
        "table": (0..cands.len()).map(entry).collect::<Vec<_>>(),
        "measured_utc": utc_now(),
    });
    let path = tune_path(cli);
    let say = |tag: &str, i: usize| {
        let c = cands[i];
        println!(
            "TUNE {tag}: --threads-per-core {} --pads {} --prefetch {} --kernel {}  ({}, {} W, {} H/J)",
            c.tpc,
            c.pads,
            c.pf as u8,
            walk::kernel_name(c.kernel),
            fmt_rate(hps[i]),
            opt(w[i], 1),
            opt(hpj[i], 2)
        );
    };
    say("best-hps", best_hps);
    say("best-hpj", best_hpj);
    if any_noisy && !cli.tune_force {
        let which: Vec<String> = (0..cands.len())
            .filter(|&i| noisy[i])
            .map(|i| format!("{} ({:.1} %)", cands[i].label(), foreign[i].unwrap_or(0.0) * 100.0))
            .collect();
        println!(
            "TUNE NOT SAVED: other processes kept the targeted CPUs more than {:.0} % busy during {}; {} left as it was. \
             Stop the other load and run --tune again, or pass --tune-force to store this result anyway.",
            TUNE_FOREIGN_MAX * 100.0,
            which.join(", "),
            path.display()
        );
        return Err(anyhow!("noisy measurement, result not stored (--tune-force to store it)"));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&v)?)?;
    println!(
        "TUNE saved to {} (--policy hashrate loads best-hps, --policy efficiency best-hpj){}",
        path.display(),
        if any_noisy { "  [--tune-force: noisy candidates stored as measured]" } else { "" }
    );
    Ok(())
}

fn header_safe(s: &str, max: usize) -> String {
    s.chars().filter(|c| c.is_ascii_graphic() || *c == ' ').take(max).collect::<String>().trim().to_string()
}

fn cpu_model() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("model name")).map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string()))
        .unwrap_or_else(|| "unknown CPU".into())
        .replace("AMD ", "")
        .replace(" 16-Core Processor", "")
        .replace(" 12-Core Processor", "")
        .replace(" 64-Core Processor", "")
}

/// Content key of a template: the same work re-served keeps its epoch (the
/// workers keep their counters, nothing is hashed twice); new content at the
/// same height is new work. The template id is NOT part of it — the node
/// re-uses ids — but the pool's nonce region is.
fn content_key(t: &BlockTemplateResponse) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        t.pow_fields_hex,
        t.difficulty_target_hex,
        t.pow_walk,
        t.height,
        t.nonce_prefix.map(|p| p.to_string()).unwrap_or_else(|| "-".into())
    )
}

/// Search deadline of a template received at `received_ns`: its life minus
/// the submit margin (never before it arrived).
fn template_deadline_ns(received_ns: u64, expires_in_seconds: u64) -> u64 {
    received_ns + Duration::from_secs(expires_in_seconds).saturating_sub(SUBMIT_MARGIN).as_nanos() as u64
}

/// Bits 64..96 of every nonce: differ per process, so two miners given the
/// same region by a pool never grind the same nonces (jetsam-extminer's rule).
fn process_offset(clock_nanos: u128, pid: u128) -> u128 {
    ((clock_nanos ^ (pid << 12)) & 0xFFFF_FFFF) << 64
}

#[derive(Debug, PartialEq, Eq)]
enum TplAction {
    /// Already won this content: no job until the tip moves.
    Idle,
    /// New content: new epoch, counters restart.
    NewJob(u64),
    /// Same content re-served: keep the epoch, refresh the deadline (and the
    /// template id if it changed).
    Same,
}

#[derive(Default)]
struct Poller {
    epoch: u64,
    cur_key: Option<String>,
    solved_key: Option<String>,
}

impl Poller {
    fn on_template(&mut self, key: &str, solved_epoch: u64) -> TplAction {
        if solved_epoch != 0 && solved_epoch == self.epoch && self.cur_key.is_some() {
            self.solved_key = self.cur_key.clone();
        }
        if self.solved_key.as_deref() == Some(key) {
            TplAction::Idle
        } else if self.cur_key.as_deref() != Some(key) {
            self.epoch += 1;
            self.cur_key = Some(key.to_string());
            self.solved_key = None;
            TplAction::NewJob(self.epoch)
        } else {
            TplAction::Same
        }
    }
}

/// Same content re-served under another template id: swap the id in the live
/// job of that epoch (same epoch, so the workers keep their counters). A job
/// already cleared (block won) or replaced stays as it is.
fn refresh_template_id(job: &RwLock<Option<Arc<Job>>>, epoch: u64, id: &str) -> bool {
    let mut g = job.write().unwrap();
    match g.as_ref() {
        Some(j) if j.epoch == epoch && j.template_id != id => {
            *g = Some(Arc::new(Job { template_id: id.to_string(), ..(**j).clone() }));
            true
        }
        _ => false,
    }
}

/// Where the submit dispatcher sends a solution.
enum Sink {
    /// The pool or node (its own HTTP client, SUBMIT_TIMEOUT).
    Net(Arc<dyn Submit>),
    /// --check-nonces: no network; forward to the verifier with the latency.
    Check(mpsc::Sender<(Found, Duration)>),
}

/// One submission, on its own thread: a single attempt, and the counters
/// follow the answer — yes (accepted), no (refused), or none (unknown: a
/// transport error or a timeout says nothing about the block).
fn submit_one(sub: &dyn Submit, tid: &str, f: Found, sh: &Shared) {
    let nonce_hex = hex::encode(f.nonce.to_le_bytes());
    let latency = f.t_found.elapsed();
    let t = Instant::now();
    let res = sub.submit(tid, &nonce_hex);
    let answer_ms = t.elapsed().as_millis();
    match res {
        Ok(hash) => {
            let n = sh.accepted.fetch_add(1, Ordering::Relaxed) + 1;
            sh.solved_epoch.store(f.epoch, Ordering::Relaxed);
            {
                let mut g = sh.job.write().unwrap();
                if g.as_ref().map(|j| j.epoch) == Some(f.epoch) {
                    *g = None;
                }
            }
            eprintln!(
                "└─ SOLVED  nonce={}  digest={}…  hash={}…  latency={} us  answer={answer_ms} ms  [accepted {n}/{}]",
                f.nonce,
                hex::encode(&f.digest[24..]),
                &hash[..hash.len().min(20)],
                latency.as_micros(),
                sh.found.load(Ordering::Relaxed)
            );
        }
        Err(CallErr::Answer(e)) => {
            sh.refused.fetch_add(1, Ordering::Relaxed);
            eprintln!("└─ submit {}: {e}  (nonce={} answer={answer_ms} ms)", classify_refusal(&e), f.nonce);
        }
        Err(CallErr::Transport(e)) => {
            sh.unknown.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "└─ submit: no answer, state unknown (the block may still be accepted; not retried): {e}  \
                 (nonce={} after {answer_ms} ms)",
                f.nonce
            );
        }
    }
}

/// The submit dispatcher: owns the workers' channel. A live solution goes out
/// at once on its own thread, at most SUBMIT_MAX_INFLIGHT at a time, so a
/// block waiting for its answer (up to SUBMIT_TIMEOUT) never holds back the
/// next one; nor does whatever the poller is waiting on. Returns when the
/// workers are gone and every submission has its answer.
fn submit_loop(rx: mpsc::Receiver<Found>, sink: Sink, sh: Arc<Shared>) {
    let slots = Arc::new((std::sync::Mutex::new(0usize), std::sync::Condvar::new()));
    let mut inflight: Vec<std::thread::JoinHandle<()>> = Vec::new();
    while let Ok(f) = rx.recv() {
        sh.found.fetch_add(1, Ordering::Relaxed);
        // Still live? Take the CURRENT template id of that epoch: the pool may
        // have re-served the same content under a new id.
        let tid = match sh.job.read().unwrap().as_ref() {
            Some(j) if j.epoch == f.epoch => j.template_id.clone(),
            _ => {
                eprintln!("└─ solution for a replaced template dropped (tpl={} nonce={})", f.template_id, f.nonce);
                continue;
            }
        };
        let sub = match &sink {
            Sink::Check(tx) => {
                let lat = f.t_found.elapsed();
                let _ = tx.send((f, lat));
                continue;
            }
            Sink::Net(s) => s.clone(),
        };
        {
            let (m, cv) = &*slots;
            let mut n = m.lock().unwrap();
            while *n >= SUBMIT_MAX_INFLIGHT {
                n = cv.wait(n).unwrap();
            }
            *n += 1;
        }
        inflight.retain(|h| !h.is_finished());
        let (sh, slots) = (sh.clone(), slots.clone());
        inflight.push(
            std::thread::Builder::new()
                .name("submit".into())
                .spawn(move || {
                    submit_one(&*sub, &tid, f, &sh);
                    let (m, cv) = &*slots;
                    *m.lock().unwrap() -= 1;
                    cv.notify_one();
                })
                .expect("spawn submit thread"),
        );
    }
    for h in inflight {
        let _ = h.join();
    }
}

fn start_submitter(rx: mpsc::Receiver<Found>, sink: Sink, sh: &Arc<Shared>) -> std::thread::JoinHandle<()> {
    let sh = sh.clone();
    std::thread::Builder::new()
        .name("submit-dispatch".into())
        .spawn(move || submit_loop(rx, sink, sh))
        .expect("spawn submit dispatcher")
}

fn mine(cli: &Cli, th: &Arc<Thermal>, prof: Arc<Profile>) -> Result<()> {
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    let ident_cpu = format!(
        "{} - {}t ({}/core, {} pads{})",
        cpu_model(),
        prof.cpus.len(),
        prof.per_core,
        prof.pads,
        if prof.prefetch { ", pf" } else { "" }
    );
    let mut rpc = Rpc {
        url: cli.rpc.clone(),
        key: cli.key.clone(),
        host: header_safe(host.trim(), 32),
        cpu: header_safe(&ident_cpu, 64),
        http: reqwest::blocking::Client::builder().timeout(POLL_TIMEOUT).build()?,
        rate: Arc::new(AtomicU64::new(0)),
        th: Some(th.clone()),
        eff: prof.policy == Policy::Efficiency,
    };
    let sh = Arc::new(Shared::new(None, 0, th.clone(), prof.cpus.len()));
    let (rx, _workers) = start_workers(&prof, &sh);
    let (huge_ok, workers) = check_huge(cli, &prof, &sh, false);
    if prof.huge && huge_ok < workers {
        // Tell the pool's dashboard too: a 4K miner is a slow miner.
        rpc.cpu = format!("{} 4K!", header_safe(&rpc.cpu, 59));
    }
    // The submitter has its own client: a poll stuck in a timeout never
    // delays a found block.
    let submit_rpc = Rpc {
        url: rpc.url.clone(),
        key: rpc.key.clone(),
        host: rpc.host.clone(),
        cpu: rpc.cpu.clone(),
        http: reqwest::blocking::Client::builder().timeout(SUBMIT_TIMEOUT).build()?,
        rate: rpc.rate.clone(),
        th: rpc.th.clone(),
        eff: rpc.eff,
    };
    let _submitter = start_submitter(rx, Sink::Net(Arc::new(submit_rpc)), &sh);
    eprintln!("rpc={}  auth={}  poll={}ms", cli.rpc, if cli.key.is_some() { "bearer" } else { "none" }, cli.poll_ms);

    let entropy = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u128;
    // Same separation as jetsam-extminer: bits 64..96 differ per process, so two
    // miners given the same region by a pool never grind the same nonces.
    let process_offset: u128 = process_offset(entropy, u128::from(std::process::id()));
    let solo_base: u128 = (entropy & 0xFFFF_FFFF) << 96;

    let mut poller = Poller::default();
    let mut backoff = Duration::from_millis(250);
    // Same refusal from a live pool, every poll: logged once per 10 s.
    let mut last_refusal: Option<(String, Instant)> = None;
    let mut last_report = Instant::now();
    let mut win: std::collections::VecDeque<(Instant, u64, u64)> = Default::default();
    let report_every = Duration::from_secs(cli.report_secs.unwrap_or(15).max(1));
    let mut region_seen: Option<u32> = None;

    loop {
        // Template (solutions go out on the submit thread, never from here).
        match rpc.call_typed::<_, BlockTemplateResponse>("jetsam_getBlockTemplate", [cli.coinbase.as_str()]) {
            Ok(t) => {
                backoff = Duration::from_millis(250);
                last_refusal = None;
                let received = sh.now_ns();
                if t.nonce_field_index != NONCE_FIELD {
                    return Err(anyhow!("template nonce_field_index must be {NONCE_FIELD}, got {}", t.nonce_field_index));
                }
                if t.nonce_prefix != region_seen {
                    if let Some(p) = t.nonce_prefix {
                        eprintln!("nonce region {p} assigned by the pool");
                    }
                    region_seen = t.nonce_prefix;
                }
                let key = content_key(&t);
                let deadline = template_deadline_ns(received, t.expires_in_seconds);
                match poller.on_template(&key, sh.solved_epoch.load(Ordering::Relaxed)) {
                    TplAction::Idle => {} // already won this one; idle until the tip moves
                    TplAction::NewJob(epoch) => {
                        let fields = decode_fields(&t.pow_fields_hex)?;
                        let target: [u8; 32] = hex::decode(&t.difficulty_target_hex)?
                            .try_into()
                            .map_err(|_| anyhow!("difficulty_target must be 32 bytes"))?;
                        let base = match t.nonce_prefix {
                            Some(p) => (u128::from(p) << NONCE_REGION_SHIFT) | process_offset,
                            None => solo_base | process_offset,
                        };
                        let rule = if t.pow_walk { "TowerWalk" } else { "sponge" };
                        eprintln!(
                            "┌─ h={} txs={} rule={rule} expires={}s tpl={}…",
                            t.height,
                            t.n_txs,
                            t.expires_in_seconds,
                            &t.template_id[..t.template_id.len().min(16)]
                        );
                        *sh.job.write().unwrap() = Some(Arc::new(Job {
                            epoch,
                            template_id: t.template_id.clone(),
                            height: t.height,
                            fields,
                            target,
                            walk: t.pow_walk,
                            base,
                        }));
                        sh.deadline_ns.store(deadline, Ordering::Relaxed);
                    }
                    TplAction::Same => {
                        // Same content re-served: the pool's view of its remaining
                        // life wins, and so does its current template id.
                        refresh_template_id(&sh.job, poller.epoch, &t.template_id);
                        sh.deadline_ns.store(deadline, Ordering::Relaxed);
                    }
                }
            }
            // No answer: back off exponentially, up to 2 s.
            Err(CallErr::Transport(e)) => {
                eprintln!("template fetch failed: {e} — retry in {:?}", backoff);
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(2));
            }
            // The pool answered (it is alive, e.g. no template yet): the next
            // poll comes after the normal --poll-ms, never later.
            Err(CallErr::Answer(e)) => {
                backoff = Duration::from_millis(250);
                let repeat = matches!(&last_refusal, Some((m, t)) if *m == e && t.elapsed() < Duration::from_secs(10));
                if !repeat {
                    eprintln!("template fetch refused: {e} — retry in {} ms", cli.poll_ms);
                    last_refusal = Some((e, Instant::now()));
                }
            }
        }

        // Rate over a 10 s window, published to the pool on the next call.
        let now = Instant::now();
        win.push_back((now, sh.hashed.load(Ordering::Relaxed), sh.busy_ns.load(Ordering::Relaxed)));
        while win.len() > 2 && now.duration_since(win[0].0) > Duration::from_secs(10) {
            win.pop_front();
        }
        let (t0, h0, _) = win[0];
        let dt = now.duration_since(t0).as_secs_f64();
        if dt > 1.0 {
            rpc.rate.store(((win.back().unwrap().1 - h0) as f64 / dt) as u64, Ordering::Relaxed);
        }
        if last_report.elapsed() >= report_every {
            last_report = Instant::now();
            let job = sh.job.read().unwrap().clone();
            let (b0, b1) = (win[0].2, win.back().unwrap().2);
            let duty = if dt > 1.0 { (b1 - b0) as f64 / 1e9 / dt / prof.cpus.len() as f64 * 100.0 } else { 0.0 };
            eprintln!(
                "⛏  {}  h={}  duty={duty:.1}%  pwm={:.2}  {}  found={} accepted={} refused={} unknown={}",
                fmt_rate(rpc.rate.load(Ordering::Relaxed) as f64),
                job.map(|j| j.height.to_string()).unwrap_or_else(|| "-".into()),
                th.duty(),
                th.summary(),
                sh.found.load(Ordering::Relaxed),
                sh.accepted.load(Ordering::Relaxed),
                sh.refused.load(Ordering::Relaxed),
                sh.unknown.load(Ordering::Relaxed),
            );
        }
        std::thread::sleep(Duration::from_millis(cli.poll_ms));
    }
}

/// --check-hardware: everything the miner relies on, one line each.
fn check_hardware(cli: &Cli) -> bool {
    let mut ok = true;
    let mut line = |good: bool, what: &str, v: String| {
        ok &= good;
        println!("{} {what:<10} {v}", if good { "ok  " } else { "FAIL" });
    };
    line(true, "binary", format!("{VERSION} sha256={}", sys::self_sha256()));
    match jetsam_core::cpu::ensure_production_hardware() {
        Ok(_) => line(true, "backend", format!("{}", jetsam_core::cpu::selected_backend())),
        Err(e) => line(false, "backend", format!("{e}")),
    }
    let glibc = unsafe { std::ffi::CStr::from_ptr(libc::gnu_get_libc_version()) }.to_string_lossy().to_string();
    line(true, "glibc", format!("runtime {glibc}"));
    match profile(cli) {
        Ok(p) => line(
            true,
            "cpu",
            format!("{} | L2 {} KiB, L3 {} KiB | profile {}", cpu_model(), p.l2_kib, p.l3_kib, p.source),
        ),
        Err(e) => line(false, "cpu", format!("{e}")),
    }
    let thp = sys::thp_mode();
    line(thp != "never", "thp", thp.clone());
    match sys::Region::new(1, true) {
        Ok(r) => {
            let kb = r.huge_kb();
            let good = kb.map(|k| k >= r.expected_huge_kb()).unwrap_or(false);
            line(good, "hugepage", format!("test region: AnonHugePages {} KiB of {}", opt(kb.map(|k| k as f64), 0), r.expected_huge_kb()));
        }
        Err(e) => line(false, "hugepage", format!("mmap: {e}")),
    }
    match thermal::Sensor::detect() {
        Some(s) => {
            let r = s.read().unwrap_or_default();
            line(true, "sensor", format!("{}  now {:.1} C", s.describe(), r.max_mc() as f64 / 1000.0));
        }
        None => line(false, "sensor", "none (k10temp / zenpower / coretemp): the miner refuses to start without --no-thermal-guard".into()),
    }
    match rapl::Rapl::open() {
        Some(r) => line(true, "rapl", format!("readable ({})", r.mode.name())),
        None => line(true, "rapl", "n/a (root or passwordless sudo needed; only --bench-walk/--tune use it)".into()),
    }
    ok
}

fn main() {
    let cli = Cli::parse();
    if cli.check_hardware {
        std::process::exit(if check_hardware(&cli) { 0 } else { 1 });
    }
    if let Err(e) = validate(&cli) {
        eprintln!("error: {e}");
        std::process::exit(2);
    }
    if let Err(e) = jetsam_core::cpu::ensure_production_hardware() {
        eprintln!("fatal: {e}");
        std::process::exit(1);
    }
    if cli.gate {
        // The gate runs every kernel shape at once on all cores: it gets the
        // same guard as mining (stop at 81 C, predictive), without the
        // regulator, and the same watchdog (gate threads at nice +10 check
        // the reading's age before every walk). A tripped guard exits
        // non-zero, so the gate fails safe.
        let th = start_thermal(&cli);
        let ok = gate::full(cli.gate_random, &th);
        if th.guarded {
            println!(
                "gate thermal guard: {} readings, longest gap {:.0} ms (poll {} ms, stale after {} ms), peak {:.1} C",
                th.readings.load(Ordering::Relaxed),
                th.gap_max_ns.load(Ordering::Relaxed) as f64 / 1e6,
                cli.temp_poll_ms,
                thermal::STALE_NS / 1_000_000,
                th.peak_c()
            );
        }
        std::process::exit(if ok { 0 } else { 1 });
    }
    if cli.temp_pause.is_some() || cli.temp_resume.is_some() {
        eprintln!(
            "warning: --temp-pause/--temp-resume are deprecated and ignored: replaced by --temp-target \
             (duty-cycle regulator) and --temp-stop"
        );
    }
    let prof = match profile(&cli) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            eprintln!("fatal: {e}");
            std::process::exit(1);
        }
    };
    banner(&prof);
    if prof.huge && sys::thp_mode() == "never" {
        eprintln!(
            "\x1b[31mTHP is disabled on this machine (transparent_hugepage/enabled = never): the pads would sit in \
             4 KiB pages (~24 % slower). Enable THP (madvise) or pass --no-huge. Exit {EXIT_NO_HUGE}.\x1b[0m"
        );
        std::process::exit(EXIT_NO_HUGE);
    }
    // The guard runs before anything hashes, the self-test included.
    let th = start_thermal(&cli);
    match gate::self_test(prof.pads, prof.prefetch, prof.kernel, prof.huge, prof.pipe, 16, &th) {
        Ok(n) => eprintln!("self-test: {n}/{n} golden vectors bit-exact on this profile"),
        Err(e) => {
            eprintln!("SELF-TEST FAILED: {e}. Refusing to mine; exit {EXIT_SELFTEST}.");
            std::process::exit(EXIT_SELFTEST);
        }
    }
    if let Some(secs) = cli.check_nonces {
        std::process::exit(if check_nonces(&cli, &th, prof, secs.max(1)) { 0 } else { 1 });
    }
    if let Some(secs) = cli.tune {
        if let Err(e) = run_tune(&cli, &th, secs.max(3)) {
            eprintln!("tune failed: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    if let Some(secs) = cli.bench_walk {
        run_bench(&cli, &th, prof, secs.max(1));
        return;
    }
    if let Err(e) = mine(&cli, &th, prof) {
        eprintln!("fatal: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> std::result::Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("towerminer").chain(args.iter().copied()))
    }

    #[test]
    fn cli_rejects_target_above_stop_minus_5() {
        let c = cli(&["--temp-stop", "70", "--temp-target", "66"]).unwrap();
        assert!(validate(&c).is_err(), "66 > 70 - 5 must be refused");
        let c = cli(&["--temp-stop", "70", "--temp-target", "65"]).unwrap();
        assert!(validate(&c).is_ok());
        // The stop can never be raised above the house rule.
        assert!(cli(&["--temp-stop", "83"]).is_err());
        assert!(cli(&["--temp-stop", "82"]).is_ok());
        // Without an explicit target the default follows a lowered stop.
        let c = cli(&["--temp-stop", "60"]).unwrap();
        assert!(validate(&c).is_ok());
        assert_eq!(c.temp_target(), 55.0);
        assert_eq!(cli(&[]).unwrap().temp_target(), 74.0);
        // Sensor cadence is bounded.
        assert!(cli(&["--temp-poll-ms", "600"]).is_err());
        assert!(cli(&["--temp-poll-ms", "99"]).is_err());
    }

    // ---------------------------------------------------------------------
    // Ported from jetsam-extminer (names kept), adapted to the persistent
    // workers: the "cursor" is the per-epoch counter of each worker.
    // ---------------------------------------------------------------------

    fn test_profile(pads: usize, kernel: u8, pipe: bool, seed_batch: usize) -> Profile {
        let cpu = *sys::allowed_cpus().iter().next().unwrap();
        Profile {
            cpus: vec![cpu],
            pads,
            prefetch: false,
            huge: true,
            l2_kib: 512,
            l3_kib: 0,
            per_core: 1,
            kernel,
            seed_batch,
            pipe,
            policy: Policy::Hashrate,
            source: "test".into(),
            tune_key: String::new(),
        }
    }

    const FIELDS0: [u128; FIELDS] = [0x1234_5678_9abc_def0; FIELDS];

    fn job(epoch: u64, base: u128, walk: bool, target: [u8; 32]) -> Arc<Job> {
        Arc::new(Job {
            epoch,
            template_id: format!("tpl{epoch}"),
            height: 7,
            fields: FIELDS0.map(Block128::from),
            target,
            walk,
            base,
        })
    }

    fn shared(j: Option<Arc<Job>>, deadline_ns: u64) -> Arc<Shared> {
        Arc::new(Shared::new(j, deadline_ns, Arc::new(Thermal::new(Instant::now(), false)), 1))
    }

    /// Run one worker until `n` solutions are in (or `max` elapsed).
    fn collect(prof: Profile, sh: &Arc<Shared>, n: usize, max: Duration) -> (Vec<Found>, Vec<std::thread::JoinHandle<()>>) {
        let (rx, hs) = start_workers(&Arc::new(prof), sh);
        let t = Instant::now();
        let mut v = Vec::new();
        while v.len() < n && t.elapsed() < max {
            if let Ok(f) = rx.recv_timeout(Duration::from_millis(20)) {
                v.push(f);
            }
        }
        (v, hs)
    }

    fn stop(sh: &Arc<Shared>, hs: Vec<std::thread::JoinHandle<()>>) {
        sh.stop.store(true, Ordering::Relaxed);
        for h in hs {
            h.join().unwrap();
        }
    }

    const FAR: u64 = u64::MAX;

    #[test]
    fn nonce_submission_is_canonical_little_endian_hex() {
        let nonce = 0x0123_4567_89ab_cdef_fedc_ba98_7654_3210u128;
        let encoded = hex::encode(nonce.to_le_bytes());
        assert_eq!(encoded.len(), 32);
        assert_eq!(u128::from_le_bytes(hex::decode(encoded).unwrap().try_into().unwrap()), nonce);
    }

    #[test]
    fn template_deadline_reserves_submit_margin() {
        let received = 5_000_000_000u64;
        assert_eq!(template_deadline_ns(received, 30) - received, 29_000_000_000);
        assert_eq!(template_deadline_ns(received, 1), received);
        assert_eq!(template_deadline_ns(received, 0), received);
    }

    #[test]
    fn expired_template_stops_before_hashing_and_keeps_its_place() {
        let base = 3u128 << NONCE_REGION_SHIFT;
        let sh = shared(Some(job(1, base, true, [0xFF; 32])), 0);
        let (v, hs) = collect(test_profile(1, walk::K_FAST, true, 8), &sh, 1, Duration::from_millis(150));
        assert!(v.is_empty(), "an expired template must not be hashed");
        assert_eq!(sh.hashed.load(Ordering::Relaxed), 0);
        stop(&sh, hs);
        // Same epoch, deadline pushed back: the search starts where it was, at 0.
        let sh = shared(Some(job(1, base, true, [0xFF; 32])), FAR);
        let (v, hs) = collect(test_profile(1, walk::K_FAST, true, 8), &sh, 1, Duration::from_secs(5));
        stop(&sh, hs);
        assert_eq!(v[0].nonce, nonce_at(base, 0, 0), "an expired pass hashed nothing, so it must not skip that range");
    }

    #[test]
    fn the_cursor_never_re_searches_a_range_it_already_covered() {
        let base = 9u128 << NONCE_REGION_SHIFT;
        let sh = shared(Some(job(1, base, true, [0xFF; 32])), FAR);
        let (v, hs) = collect(test_profile(1, walk::K_FAST, true, 8), &sh, 24, Duration::from_secs(10));
        stop(&sh, hs);
        assert_eq!(v.len(), 24);
        for w in v.windows(2) {
            assert_eq!(w[1].nonce, w[0].nonce + 1, "every nonce once, in order");
        }
    }

    #[test]
    fn a_pass_cut_short_by_the_deadline_still_moves_the_cursor_forward() {
        let base = 11u128 << NONCE_REGION_SHIFT;
        let sh = shared(Some(job(1, base, true, [0xFF; 32])), FAR);
        let (rx, hs) = start_workers(&Arc::new(test_profile(1, walk::K_FAST, true, 8)), &sh);
        let mut last = None;
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(100) {
            if let Ok(f) = rx.recv_timeout(Duration::from_millis(10)) {
                last = Some(f.nonce);
            }
        }
        // The deadline passes mid-ring: the worker stops between two hashes.
        sh.deadline_ns.store(0, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(50));
        while let Ok(f) = rx.try_recv() {
            last = Some(f.nonce);
        }
        let last = last.expect("some solutions before the deadline");
        sh.deadline_ns.store(FAR, Ordering::Relaxed);
        let next = rx.recv_timeout(Duration::from_secs(5)).expect("resumes").nonce;
        stop(&sh, hs);
        assert_eq!(next, last + 1, "resumed at {next}, the pass before ended at {last}");
    }

    #[test]
    fn a_solution_surfaces_as_soon_as_one_thread_finds_it() {
        let sh = shared(Some(job(1, 0, true, [0xFF; 32])), FAR);
        let t = Instant::now();
        let (v, hs) = collect(test_profile(1, walk::K_FAST, true, 8), &sh, 1, Duration::from_secs(5));
        let dt = t.elapsed();
        stop(&sh, hs);
        assert_eq!(v.len(), 1);
        // One walk is ~1 ms; the solution is sent from the worker the moment
        // its hash is checked, not at the end of a batch or a poll.
        assert!(dt < Duration::from_millis(500), "first solution after {dt:?}");
    }

    #[test]
    fn a_second_miner_in_the_same_region_starts_somewhere_else() {
        let region = u128::from(7u32) << NONCE_REGION_SHIFT;
        let first = region | process_offset(1_111, 4_242);
        let second = region | process_offset(1_111, 9_999);
        assert_ne!(first, second, "same clock, different pid must diverge");
        // A worker covers 2^52 nonces per job: the offsets are 2^64 apart.
        assert!(first.abs_diff(second) >= 1u128 << 64);
        let highest = region | process_offset(u128::MAX, u128::MAX);
        assert_eq!(highest >> NONCE_REGION_SHIFT, 7, "no worker can walk out of its region");
    }

    #[test]
    fn a_pool_region_is_wider_than_any_machine_can_search() {
        let first = u128::from(0u32) << NONCE_REGION_SHIFT;
        let second = u128::from(1u32) << NONCE_REGION_SHIFT;
        assert_eq!(second - first, 1u128 << 96);
        // 4096 workers x 2^52 nonces fit in one process offset (2^64).
        assert_eq!(4096u128 << 52, 1u128 << 64);
        assert_eq!(u128::from(u32::MAX) << NONCE_REGION_SHIFT, u128::MAX - ((1u128 << 96) - 1));
    }

    #[test]
    fn a_template_without_a_pool_region_still_parses() {
        let solo = r#"{"template_id":"ab","pow_fields_hex":"00","nonce_field_index":0,
            "difficulty_target_hex":"ff","height":7,"expires_in_seconds":120,"n_txs":0}"#;
        let parsed: BlockTemplateResponse = serde_json::from_str(solo).expect("node template");
        assert_eq!(parsed.nonce_prefix, None);
        let pooled = r#"{"template_id":"ab","pow_fields_hex":"00","nonce_field_index":0,
            "difficulty_target_hex":"ff","height":7,"expires_in_seconds":120,"n_txs":0,
            "nonce_prefix":3}"#;
        let parsed: BlockTemplateResponse = serde_json::from_str(pooled).expect("pool template");
        assert_eq!(parsed.nonce_prefix, Some(3));
    }

    #[test]
    fn the_walk_is_off_unless_the_node_asks_for_it() {
        let without = r#"{"template_id":"ab","pow_fields_hex":"00","nonce_field_index":0,
            "difficulty_target_hex":"ff","height":7,"expires_in_seconds":120,"n_txs":0}"#;
        let parsed: BlockTemplateResponse = serde_json::from_str(without).expect("old node");
        assert!(!parsed.pow_walk, "absent must mean off");
        let with = r#"{"template_id":"ab","pow_fields_hex":"00","nonce_field_index":0,
            "difficulty_target_hex":"ff","height":7,"expires_in_seconds":120,"n_txs":0,
            "pow_walk":true}"#;
        let parsed: BlockTemplateResponse = serde_json::from_str(with).expect("forked node");
        assert!(parsed.pow_walk);
    }

    #[test]
    fn the_walked_search_finds_a_nonce_under_a_trivial_target() {
        let sh = shared(Some(job(1, 0, true, [0xFF; 32])), FAR);
        let (v, hs) = collect(test_profile(1, walk::K_FAST, true, 8), &sh, 1, Duration::from_secs(5));
        stop(&sh, hs);
        let f = &v[0];
        let seed = gate::seed_of(&FIELDS0.map(Block128::from), f.nonce);
        let walked = jetsam_poseidon2b::towerwalk::towerwalk_digest(&seed);
        assert_eq!(walked, f.digest, "the reported digest is the reference walk of that nonce");
        assert_ne!(walked, seed, "the walk must not be the identity");
    }

    // ---------------------------------------------------------------------
    // towerminer's own
    // ---------------------------------------------------------------------

    #[test]
    fn nonce_layout_worker_bits_and_region() {
        let base = (0xDEAD_BEEFu128 << 96) | (0x0123_4567u128 << 64);
        let n = nonce_at(base, 0xABC, 0x000F_FFFF_FFFF_FFFF);
        assert_eq!(n >> 96, 0xDEAD_BEEF, "pool region");
        assert_eq!((n >> 64) & 0xFFFF_FFFF, 0x0123_4567, "process offset");
        assert_eq!((n >> 52) & 0xFFF, 0xABC, "worker id");
        assert_eq!(n & ((1 << 52) - 1), 0x000F_FFFF_FFFF_FFFF, "counter");
        // The counter wraps inside its 52 bits instead of spilling into the id.
        assert_eq!(nonce_at(base, 1, 1u128 << 52), nonce_at(base, 1, 0));
    }

    #[test]
    fn le256_lt_bounds() {
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        assert!(!le256_lt(&a, &b), "equal is not below");
        b[31] = 1;
        assert!(le256_lt(&a, &b));
        assert!(!le256_lt(&b, &a));
        // The most significant byte is the last one (little-endian).
        a[0] = 0xFF;
        assert!(le256_lt(&a, &b));
        a[31] = 1;
        assert!(!le256_lt(&a, &b), "0xFF in byte 0 outweighs nothing at byte 31");
    }

    #[test]
    fn seed_ring_yields_consecutive_nonces() {
        let fields = FIELDS0.map(Block128::from);
        let mut sc = Scratch::new();
        // (pads, kernel, pipe, ring, prefetch): piped double ring, the same
        // with prefetch, odd piped ring, plain ring of 8 at 1 pad, grouped
        // fold at 2 pads, 3 pads ring 6.
        for (pads, k, pipe, ring, pf) in [
            (1, walk::K_FAST, true, 8, false),
            (1, walk::K_FAST, true, 8, true),
            (1, walk::K_FAST, true, 3, false),
            (1, walk::K_BASE, false, 8, false),
            (2, walk::K_FAST, false, 2, false),
            (3, walk::K_FAST, false, 4, false),
        ] {
            let base = 5u128 << NONCE_REGION_SHIFT;
            let sh = shared(Some(job(1, base, true, [0xFF; 32])), FAR);
            let mut prof = test_profile(pads, k, pipe, ring);
            prof.prefetch = pf;
            let (v, hs) = collect(prof, &sh, 20, Duration::from_secs(20));
            stop(&sh, hs);
            assert_eq!(v.len(), 20);
            assert_eq!(v[0].nonce, nonce_at(base, 0, 0));
            for (i, f) in v.iter().enumerate() {
                assert_eq!(f.nonce, nonce_at(base, 0, i as u128), "pads={pads} pipe={pipe} ring={ring}");
                let d = gate::oracle(&mut sc, &gate::seed_of(&fields, f.nonce));
                assert_eq!(d, f.digest, "pads={pads} pipe={pipe} ring={ring}: digest reported under the wrong nonce");
            }
        }
    }


    fn tpl(id: &str, fields: &str, height: u64, prefix: Option<u32>) -> BlockTemplateResponse {
        BlockTemplateResponse {
            template_id: id.into(),
            pow_fields_hex: fields.into(),
            nonce_field_index: 0,
            difficulty_target_hex: "ff".into(),
            height,
            expires_in_seconds: 12,
            n_txs: 0,
            nonce_prefix: prefix,
            pow_walk: true,
        }
    }

    #[test]
    fn content_key_ignores_template_id_and_includes_prefix() {
        assert_eq!(content_key(&tpl("a", "00", 7, Some(3))), content_key(&tpl("b", "00", 7, Some(3))));
        assert_ne!(content_key(&tpl("a", "00", 7, Some(3))), content_key(&tpl("a", "00", 7, Some(4))));
        assert_ne!(content_key(&tpl("a", "00", 7, Some(3))), content_key(&tpl("a", "00", 7, None)));
        assert_ne!(content_key(&tpl("a", "00", 7, None)), content_key(&tpl("a", "01", 7, None)));
    }

    #[test]
    fn same_key_new_id_swaps_id_keeps_epoch() {
        let mut p = Poller::default();
        assert_eq!(p.on_template("k1", 0), TplAction::NewJob(1));
        assert_eq!(p.on_template("k1", 0), TplAction::Same);
        let lock = RwLock::new(Some(Arc::new(Job {
            epoch: 1,
            template_id: "old".into(),
            height: 7,
            fields: [Block128::from(1u128); FIELDS],
            target: [0x80; 32],
            walk: true,
            base: 42,
        })));
        assert!(refresh_template_id(&lock, 1, "new"));
        let j = lock.read().unwrap().clone().unwrap();
        assert_eq!((j.epoch, j.template_id.as_str(), j.base, j.height), (1, "new", 42, 7));
        assert!(!refresh_template_id(&lock, 1, "new"), "same id: nothing to do");
        assert!(!refresh_template_id(&lock, 2, "other"), "another epoch's job is never touched");
        *lock.write().unwrap() = None;
        assert!(!refresh_template_id(&lock, 1, "x"), "a won (cleared) job stays cleared");
    }

    #[test]
    fn solved_key_idles_same_content_and_mines_new_content_at_same_height() {
        let mut p = Poller::default();
        assert_eq!(p.on_template("h7-a", 0), TplAction::NewJob(1));
        // Epoch 1 won: the same content re-served is idle...
        assert_eq!(p.on_template("h7-a", 1), TplAction::Idle);
        assert_eq!(p.on_template("h7-a", 1), TplAction::Idle);
        // ...new content at the same height (our block lost the race) is mined.
        assert_eq!(p.on_template("h7-b", 1), TplAction::NewJob(2));
        assert_eq!(p.on_template("h7-b", 1), TplAction::Same, "an old win never idles new work");
        assert_eq!(p.on_template("h8", 1), TplAction::NewJob(3));
    }

    #[test]
    fn refusal_classification() {
        assert_eq!(classify_refusal("RPC error: template expired"), "stale");
        assert_eq!(classify_refusal("RPC error: unknown template"), "stale");
        assert_eq!(classify_refusal("RPC error: template already consumed"), "stale");
        assert_eq!(classify_refusal("RPC error: node busy, retry"), "busy");
        assert_eq!(classify_refusal("RPC error: invalid proof of work"), "refused");
        assert_eq!(classify_refusal("HTTP 500 Internal Server Error"), "refused");
    }

    #[test]
    fn submitter_forwards_live_solutions_and_drops_replaced_ones() {
        let lock_job = Arc::new(Job {
            epoch: 2,
            template_id: "t2".into(),
            height: 7,
            fields: [Block128::from(1u128); FIELDS],
            target: [0x80; 32],
            walk: true,
            base: 0,
        });
        let sh = Arc::new(Shared::new(Some(lock_job), u64::MAX, Arc::new(Thermal::new(Instant::now(), false)), 1));
        let (tx, rx) = mpsc::channel();
        let (vtx, vrx) = mpsc::channel();
        let h = start_submitter(rx, Sink::Check(vtx), &sh);
        let f = |epoch, nonce| Found { epoch, template_id: "x".into(), nonce, digest: [0; 32], t_found: Instant::now() };
        tx.send(f(1, 10)).unwrap(); // replaced epoch: dropped
        tx.send(f(2, 11)).unwrap(); // live: forwarded
        drop(tx);
        h.join().unwrap();
        let got: Vec<(Found, Duration)> = vrx.try_iter().collect();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0.nonce, 11);
        assert!(got[0].1 < Duration::from_millis(100), "found -> submit took {:?}", got[0].1);
        assert_eq!(sh.found.load(Ordering::Relaxed), 2);
    }

    /// Test double for the pool: records when each submission starts and how
    /// many run at once, answers after `delay`.
    struct SlowSink {
        delay: Duration,
        answer: fn() -> std::result::Result<String, CallErr>,
        starts: std::sync::Mutex<Vec<Instant>>,
        now: std::sync::atomic::AtomicUsize,
        peak: std::sync::atomic::AtomicUsize,
    }

    impl SlowSink {
        fn new(delay: Duration, answer: fn() -> std::result::Result<String, CallErr>) -> Arc<SlowSink> {
            Arc::new(SlowSink { delay, answer, starts: Default::default(), now: Default::default(), peak: Default::default() })
        }
    }

    impl Submit for SlowSink {
        fn submit(&self, _tid: &str, _nonce_hex: &str) -> std::result::Result<String, CallErr> {
            self.starts.lock().unwrap().push(Instant::now());
            let n = self.now.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(n, Ordering::SeqCst);
            std::thread::sleep(self.delay);
            self.now.fetch_sub(1, Ordering::SeqCst);
            (self.answer)()
        }
    }

    fn found(nonce: u128) -> Found {
        Found { epoch: 1, template_id: "tpl1".into(), nonce, digest: [0; 32], t_found: Instant::now() }
    }

    /// Run the dispatcher on `founds` against `sink`, wait for every answer.
    fn submit_all(sink: &Arc<SlowSink>, founds: Vec<Found>) -> Arc<Shared> {
        let sh = shared(Some(job(1, 0, true, [0x80; 32])), FAR);
        let (tx, rx) = mpsc::channel();
        let h = start_submitter(rx, Sink::Net(sink.clone()), &sh);
        for f in founds {
            tx.send(f).unwrap();
        }
        drop(tx);
        h.join().unwrap();
        sh
    }

    #[test]
    fn a_second_block_goes_out_while_the_first_waits_for_its_answer() {
        let sink = SlowSink::new(Duration::from_secs(2), || Ok("ab".repeat(32)));
        let sh = shared(Some(job(1, 0, true, [0x80; 32])), FAR);
        let (tx, rx) = mpsc::channel();
        let h = start_submitter(rx, Sink::Net(sink.clone()), &sh);
        tx.send(found(10)).unwrap();
        let second = found(11);
        let sent = second.t_found;
        tx.send(second).unwrap();
        let t = Instant::now();
        while sink.starts.lock().unwrap().len() < 2 && t.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(2));
        }
        let starts = sink.starts.lock().unwrap().clone();
        assert_eq!(starts.len(), 2, "the second block waited for the first one's answer");
        let lag = starts[1].duration_since(sent);
        assert!(lag < Duration::from_millis(100), "second submission started {lag:?} after it was found");
        drop(tx);
        h.join().unwrap();
        assert_eq!(sh.accepted.load(Ordering::Relaxed), 2, "both answered yes");
        assert_eq!((sh.refused.load(Ordering::Relaxed), sh.unknown.load(Ordering::Relaxed)), (0, 0));
    }

    #[test]
    fn no_answer_is_unknown_not_refused_and_never_retried() {
        let sink = SlowSink::new(Duration::ZERO, || Err(CallErr::Transport("operation timed out".into())));
        let sh = submit_all(&sink, vec![found(1)]);
        assert_eq!(sink.starts.lock().unwrap().len(), 1, "one attempt only: the pool retries on its side");
        assert_eq!(sh.accepted.load(Ordering::Relaxed), 0);
        assert_eq!(sh.refused.load(Ordering::Relaxed), 0);
        assert_eq!(sh.unknown.load(Ordering::Relaxed), 1);
        assert!(sh.job.read().unwrap().is_some(), "an unanswered block does not end the job");

        let sink = SlowSink::new(Duration::ZERO, || Err(CallErr::Answer("RPC error: template expired".into())));
        let sh = submit_all(&sink, vec![found(2)]);
        assert_eq!(sink.starts.lock().unwrap().len(), 1);
        assert_eq!((sh.accepted.load(Ordering::Relaxed), sh.refused.load(Ordering::Relaxed)), (0, 1));
        assert_eq!(sh.unknown.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn at_most_four_submissions_in_flight() {
        // Refusals keep the job live, so all six go out; 4 at a time.
        let sink = SlowSink::new(Duration::from_millis(300), || Err(CallErr::Answer("RPC error: busy".into())));
        let sh = submit_all(&sink, (0..6).map(found).collect());
        assert_eq!(sink.starts.lock().unwrap().len(), 6);
        assert_eq!(sink.peak.load(Ordering::SeqCst), SUBMIT_MAX_INFLIGHT);
        assert_eq!(sh.refused.load(Ordering::Relaxed), 6);
    }

    #[test]
    fn table_rows_measured_2026_09_30() {
        const F: u8 = walk::K_FAST;
        let s = |fam, l2, p| table(fam, l2, p).0;
        for p in [Policy::Hashrate, Policy::Efficiency] {
            assert_eq!(s(23, 512, p), Shape::new(2, 1, true, F), "Zen 2");
            assert_eq!(s(25, 1024, p), Shape::new(2, 1, false, F), "Zen 4");
            assert_eq!(s(26, 1024, p), Shape::new(2, 1, false, F), "Zen 5 follows Zen 4");
        }
        // Zen 3 (L2 512 KiB) unchanged.
        assert_eq!(s(25, 512, Policy::Hashrate), Shape::new(1, 1, false, F));
        assert_eq!(s(25, 512, Policy::Efficiency), Shape::new(2, 2, true, F));
    }

    #[test]
    fn pipe_keeps_prefetch() {
        let prof = |a: &[&str]| profile(&cli(&[&["--no-tune-file"], a].concat()).unwrap());
        let p = prof(&["--pads", "1", "--prefetch", "1"]).unwrap();
        assert!(p.pipe && p.prefetch, "auto: piped at 1 pad, prefetch kept");
        let p = prof(&["--pads", "1", "--prefetch", "1", "--pipe", "1"]).unwrap();
        assert!(p.pipe && p.prefetch);
        let p = prof(&["--pads", "1", "--prefetch", "1", "--pipe", "0"]).unwrap();
        assert!(!p.pipe && p.prefetch);
        assert!(!prof(&["--pads", "2", "--prefetch", "1"]).unwrap().pipe);
        assert!(prof(&["--pads", "2", "--pipe", "1"]).is_err());
        assert!(prof(&["--pads", "1", "--kernel", "base", "--pipe", "1"]).is_err());
    }
}
