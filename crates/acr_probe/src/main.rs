//! `acr-probe`: read-only external RE tooling for acr.exe.
//!
//! The target is opened with `PROCESS_VM_READ | PROCESS_QUERY_INFORMATION` only; nothing here
//! can write to, suspend, or inject into it. Shared memory (acpmf_*) is the ground-truth oracle.

mod dumper7;
mod engine;
mod findcar;
mod mem;
mod oracle;
mod pointers;
mod process;
mod scan;
mod session;
mod strings;
mod structs;
mod variants;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use mem::{PageCache, ReadMem, RegionFilter, RegionKind};
use oracle::{Oracle, ShmSample, Target};
use process::{find_target, symbolize, ModuleEntry, RemoteProcess};
use scan::{keep, refresh, scan_memory, CandidateSet, Matcher, NextMode, NumType};
use serde::{Deserialize, Serialize};
use session::{unix_now, Session, Step, WorkDir};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use variants::{vec3_variants, Variant};

/// Parses `0x`-prefixed hex or decimal.
pub fn parse_u64(s: &str) -> Result<u64, String> {
    let t = s.trim().replace(['_', '`'], "");
    match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(h, 16).map_err(|e| format!("bad hex '{s}': {e}")),
        None => t.parse().map_err(|e| format!("bad number '{s}': {e}")),
    }
}

fn parse_usize(s: &str) -> Result<usize, String> {
    parse_u64(s).map(|v| v as usize)
}

#[derive(Parser)]
#[command(name = "acr-probe", version, about = "Read-only external RE probe for Assetto Corsa Rally (acr.exe)")]
struct Cli {
    #[command(flatten)]
    g: Global,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args, Clone)]
struct Global {
    /// Print machine-readable JSON instead of text.
    #[arg(long, global = true)]
    json: bool,
    /// Target process executable name.
    #[arg(long, global = true, default_value = "acr.exe")]
    process: String,
    /// Target PID (overrides --process).
    #[arg(long, global = true)]
    pid: Option<u32>,
    /// Work directory for sessions, scan results and reports.
    #[arg(long, global = true, default_value = "re-work")]
    work_dir: PathBuf,
    /// signatures.toml to test / update.
    #[arg(long, global = true, default_value = "config/signatures.toml")]
    sigs: PathBuf,
    /// Scan session name (several scans can be kept side by side).
    #[arg(long, global = true, default_value = "default")]
    session: String,
    /// Use a saved `shm --save` JSON sample instead of live shared memory.
    #[arg(long, global = true)]
    shm_file: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List the target's modules (base, size, path).
    Modules {
        #[arg(long)]
        filter: Option<String>,
    },
    /// Print shared-memory telemetry (the scan oracle).
    Shm {
        #[arg(long, default_value_t = 1)]
        count: usize,
        #[arg(long, default_value_t = 500)]
        interval_ms: u64,
        /// Save the last sample as JSON (usable later with --shm-file).
        #[arg(long)]
        save: Option<PathBuf>,
    },
    /// First value scan of all committed readable memory against the current shm value.
    ScanInit(ScanInitArgs),
    /// Narrow the current scan (fresh shm match by default).
    ScanNext {
        #[arg(long, group = "mode")]
        changed: bool,
        #[arg(long, group = "mode")]
        unchanged: bool,
        #[arg(long, group = "mode")]
        increased: bool,
        #[arg(long, group = "mode")]
        decreased: bool,
        /// Override the session tolerance for this step (match mode).
        #[arg(long)]
        tol: Option<f64>,
        /// Threshold for changed/unchanged/increased/decreased (memory units).
        #[arg(long, default_value_t = 1e-4)]
        eps: f64,
        /// Allow continuing a session recorded against another PID (addresses are probably stale).
        #[arg(long)]
        force: bool,
    },
    /// Show the current candidates with live values.
    ScanList {
        #[arg(long, default_value_t = 25)]
        limit: usize,
    },
    /// Sample addresses at N Hz next to the shm value.
    Watch {
        /// Addresses (hex, or module+0xrva). Empty = first --top candidates of the session.
        #[arg(value_parser = parse_addr_arg)]
        addrs: Vec<AddrArg>,
        #[arg(long, default_value_t = 10.0)]
        seconds: f64,
        #[arg(long, default_value_t = 10.0)]
        hz: f64,
        #[arg(long, value_enum)]
        r#type: Option<NumType>,
        /// Values per address (3 for vectors).
        #[arg(long)]
        arity: Option<usize>,
        #[arg(long, default_value_t = 8)]
        top: usize,
        /// shm value shown alongside.
        #[arg(long, value_enum)]
        target: Option<Target>,
    },
    /// Hex/float/pointer dump of a memory range.
    StructDump {
        #[arg(value_parser = parse_addr_arg)]
        addr: AddrArg,
        #[arg(long, default_value = "0x200", value_parser = parse_usize)]
        len: usize,
        /// Bytes to include before `addr`.
        #[arg(long, default_value = "0", value_parser = parse_usize)]
        before: usize,
    },
    /// Correlate memory around a known position address with shm fields over time.
    StructCorrelate {
        /// Address of the position vector (from scan-list).
        #[arg(value_parser = parse_addr_arg)]
        pos_addr: AddrArg,
        /// Struct base for reported offsets (default: pos_addr).
        #[arg(long, value_parser = parse_addr_arg)]
        base: Option<AddrArg>,
        #[arg(long, value_enum)]
        r#type: Option<NumType>,
        /// Window radius around pos_addr.
        #[arg(long, default_value = "0x1000", value_parser = parse_usize)]
        range: usize,
        #[arg(long, default_value_t = 40)]
        samples: usize,
        #[arg(long, default_value_t = 100)]
        interval_ms: u64,
        #[arg(long, default_value_t = 0.3)]
        vel_tol: f64,
        #[arg(long, default_value_t = 0.05)]
        ang_tol: f64,
    },
    /// Pointer chains to an address, rooted in the module's .data/.rdata or a live pawn.
    PointerScan {
        #[arg(value_parser = parse_addr_arg)]
        addr: AddrArg,
        #[arg(long, default_value_t = 5)]
        max_depth: usize,
        #[arg(long, default_value = "0x1000", value_parser = parse_usize)]
        max_offset: usize,
        #[arg(long, default_value_t = 2_000_000)]
        max_nodes: usize,
        #[arg(long, default_value_t = 500)]
        max_results: usize,
        /// What the address holds, so pointer-verify can check it after a restart.
        #[arg(long, value_enum, default_value = "pos")]
        target: Target,
        #[arg(long, value_enum)]
        r#type: Option<NumType>,
        /// Root chains in the live instance of this UClass (chain_root = "pawn") instead of the
        /// module's .data/.rdata. "auto" = first live vehicle_pawn_class_candidates class.
        #[arg(long)]
        pawn_class: Option<String>,
        /// Bytes of the pawn object treated as root slots.
        #[arg(long, default_value = "0x2000", value_parser = parse_usize)]
        pawn_len: usize,
    },
    /// Re-resolve saved pointer chains (e.g. after a game restart) and check them against shm.
    PointerVerify {
        #[arg(long)]
        file: Option<PathBuf>,
        /// Drop chains that fail from the file.
        #[arg(long)]
        prune: bool,
    },
    /// Evaluate every candidate/offset in signatures.toml against the running game.
    SigTest,
    /// Locate the player car's rigid bodies by vtable scan (the hook's default sim_car
    /// locator) and compare the main body with shm. Saves re-work/find-car.json.
    FindCar {
        /// Override [raw_offsets.sim_car] vtable_rva.
        #[arg(long, value_parser = parse_u64)]
        vtable_rva: Option<u64>,
        /// Body-to-shm match tolerance in metres (default: match_tolerance_m).
        #[arg(long)]
        tol: Option<f64>,
        /// Car bodies lie within this distance of the main body (default: car_radius_m).
        #[arg(long)]
        radius: Option<f64>,
        /// Live samples of the chosen main body vs shm after the scan.
        #[arg(long, default_value_t = 10)]
        samples: usize,
        #[arg(long, default_value_t = 200)]
        interval_ms: u64,
        /// List the bodies of every group, not only the chosen car.
        #[arg(long)]
        all: bool,
    },
    /// Search memory for ASCII / UTF-16 strings.
    Strings {
        pattern: String,
        #[arg(short = 'i', long)]
        ignore_case: bool,
        #[arg(long)]
        module: Option<String>,
        #[arg(long)]
        all_modules: bool,
        /// Also search private (heap) memory.
        #[arg(long)]
        heap: bool,
        #[arg(long, default_value_t = 200)]
        max: usize,
    },
    /// Enumerate UClasses / UScriptStructs via GObjects (external reflection walk).
    UeDumpClasses {
        /// Case-insensitive name substrings (repeatable). Default: vehicle-ish/online keywords.
        #[arg(long)]
        filter: Vec<String>,
        /// Every class, no filter.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        props: bool,
        #[arg(long)]
        funcs: bool,
        #[arg(long)]
        instances: bool,
        #[arg(long, default_value_t = 5)]
        max_instances: usize,
    },
    /// Import a Dumper-7 SDK: Offsets → signatures.toml, plus class candidates.
    ImportDumper7 {
        /// Dumper-7 output dir (or any parent of CppSDK/SDK/Basic.hpp).
        #[arg(default_value = "C:\\Dumper-7")]
        dir: PathBuf,
        /// Report only; don't modify signatures.toml.
        #[arg(long)]
        dry_run: bool,
        /// Also prepend the best vehicle pawn / movement classes to [reflection].
        #[arg(long)]
        apply_classes: bool,
    },
}

#[derive(Args)]
struct ScanInitArgs {
    #[arg(long, value_enum)]
    target: Target,
    #[arg(long, value_enum, default_value = "f32")]
    r#type: NumType,
    /// Tolerance in shm units (default per target).
    #[arg(long)]
    tol: Option<f64>,
    /// Unit scales (memory = scale × shm); default per target (e.g. pos: 1, 100).
    #[arg(long, value_delimiter = ',')]
    scales: Vec<f64>,
    /// Only the identity axis order (still tries sign flips).
    #[arg(long)]
    no_permute: bool,
    /// Element stride for vectors, in elements (1 = packed).
    #[arg(long, default_value_t = 1)]
    stride: usize,
    /// Address alignment in bytes (default = value size).
    #[arg(long)]
    align: Option<usize>,
    /// Wheel index for --target wheel_speed (0 FL, 1 FR, 2 RL, 3 RR).
    #[arg(long, default_value_t = 0)]
    wheel: usize,
    /// Value for --target custom.
    #[arg(long)]
    value: Option<f64>,
    #[arg(long, default_value_t = 20_000_000)]
    max_candidates: usize,
    /// Also scan file-mapped views.
    #[arg(long)]
    mapped: bool,
    /// Include read-only memory too.
    #[arg(long)]
    readonly: bool,
}

#[derive(Clone, Debug)]
struct AddrArg {
    module: Option<String>,
    off: u64,
}

fn parse_addr_arg(s: &str) -> Result<AddrArg, String> {
    if let Some((m, off)) = s.split_once('+') {
        if m.contains('.') || !m.trim_start_matches("0x").chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(AddrArg { module: Some(m.to_string()), off: parse_u64(off)? });
        }
        return Ok(AddrArg { module: None, off: parse_u64(m)? + parse_u64(off)? });
    }
    Ok(AddrArg { module: None, off: parse_u64(s)? })
}

impl AddrArg {
    fn resolve(&self, modules: &[ModuleEntry]) -> Result<usize> {
        match &self.module {
            None => Ok(self.off as usize),
            Some(m) => modules
                .iter()
                .find(|e| e.name.eq_ignore_ascii_case(m))
                .map(|e| e.base + self.off as usize)
                .ok_or_else(|| anyhow!("module {m} not loaded")),
        }
    }
}

struct Ctx {
    g: Global,
    work: WorkDir,
}

impl Ctx {
    fn open(&self) -> Result<(RemoteProcess, Vec<ModuleEntry>)> {
        let pid = find_target(&self.g.process, self.g.pid)?;
        let p = RemoteProcess::open(pid)?;
        let m = p.modules()?;
        Ok((p, m))
    }

    fn oracle(&self) -> Result<Oracle> {
        Oracle::open(self.g.shm_file.as_deref())
    }

    fn emit<T: Serialize>(&self, v: &T, text: impl FnOnce()) -> Result<()> {
        if self.g.json {
            println!("{}", serde_json::to_string_pretty(v)?);
        } else {
            text();
        }
        Ok(())
    }

    fn sigs_text(&self) -> Result<String> {
        std::fs::read_to_string(&self.g.sigs).with_context(|| format!("reading {}", self.g.sigs.display()))
    }

    fn sigs(&self) -> Result<acr_ue::sigs::Signatures> {
        acr_ue::sigs::Signatures::from_toml_str(&self.sigs_text()?)
            .map_err(|e| anyhow!("{}: {e}", self.g.sigs.display()))
    }

    fn load_session(&self) -> Result<(Session, CandidateSet)> {
        let s: Session = self
            .work
            .load_json(&self.work.session_json(&self.g.session))
            .with_context(|| format!("no scan session '{}'; run scan-init first", self.g.session))?;
        let c = CandidateSet::load(&self.work.candidates_bin(&self.g.session))?;
        Ok((s, c))
    }

    fn save_session(&self, s: &Session, c: &CandidateSet) -> Result<()> {
        self.work.ensure(&format!("scans/{}", self.g.session))?;
        c.save(&self.work.candidates_bin(&self.g.session))?;
        self.work.save_json(&self.work.session_json(&self.g.session), s)
    }
}

fn fmt_vals(v: &[f64]) -> String {
    let parts: Vec<String> = v.iter().map(|x| structs::fmt_num(*x)).collect();
    format!("({})", parts.join(", "))
}

fn print_sample(s: &ShmSample) {
    println!(
        "packet {} status {} | pos ({:.3}, {:.3}, {:.3}) [{}] | vel ({:.3}, {:.3}, {:.3}) {:.1} km/h",
        s.packet_id, s.status, s.pos[0], s.pos[1], s.pos[2], s.pos_source, s.vel[0], s.vel[1], s.vel[2], s.speed_kmh
    );
    println!(
        "  heading {:.4} pitch {:.4} roll {:.4} | ang vel (local) ({:.3}, {:.3}, {:.3}) | rpm {:.0} gear {} (raw {}) | wheels ({:.2}, {:.2}, {:.2}, {:.2})",
        s.heading,
        s.pitch,
        s.roll,
        s.local_ang_vel[0],
        s.local_ang_vel[1],
        s.local_ang_vel[2],
        s.rpm,
        s.gear,
        s.gear_raw,
        s.wheel_speed[0],
        s.wheel_speed[1],
        s.wheel_speed[2],
        s.wheel_speed[3]
    );
}

fn variant_histogram(s: &Session, c: &CandidateSet) -> Vec<(String, usize)> {
    let mut counts = vec![0usize; s.variants.len()];
    for &v in &c.variants {
        if let Some(x) = counts.get_mut(v as usize) {
            *x += 1;
        }
    }
    let mut out: Vec<(String, usize)> =
        counts.into_iter().enumerate().filter(|(_, n)| *n > 0).map(|(i, n)| (s.variant_desc[i].clone(), n)).collect();
    out.sort_by_key(|a| std::cmp::Reverse(a.1));
    out
}

// ---- commands ------------------------------------------------------------------------------

fn cmd_modules(cx: &Ctx, filter: Option<&str>) -> Result<()> {
    let (p, mods) = cx.open()?;
    let f = filter.map(str::to_ascii_lowercase);
    let list: Vec<&ModuleEntry> =
        mods.iter().filter(|m| f.as_ref().is_none_or(|f| m.name.to_ascii_lowercase().contains(f.as_str()))).collect();
    cx.emit(&serde_json::json!({ "pid": p.pid, "modules": list }), || {
        println!("pid {} ({} modules)", p.pid, mods.len());
        for m in &list {
            println!("{:#014x} {:>10x} {:<32} {}", m.base, m.size, m.name, m.path);
        }
    })
}

fn cmd_shm(cx: &Ctx, count: usize, interval: u64, save: Option<PathBuf>) -> Result<()> {
    let o = cx.oracle()?;
    let mut last = None;
    for i in 0..count.max(1) {
        if i > 0 {
            std::thread::sleep(Duration::from_millis(interval));
        }
        let s = o.sample()?;
        cx.emit(&s, || print_sample(&s))?;
        last = Some(s);
    }
    if let (Some(path), Some(s)) = (save, last) {
        std::fs::write(&path, serde_json::to_vec_pretty(&s)?)?;
        eprintln!("saved {}", path.display());
    }
    Ok(())
}

fn cmd_scan_init(cx: &Ctx, a: &ScanInitArgs) -> Result<()> {
    let sample = cx.oracle()?.sample()?;
    let target = a.target.values(&sample, a.wheel, a.value)?;
    let tol = a.tol.unwrap_or(a.target.default_tol());
    let scales = if a.scales.is_empty() { a.target.default_scales() } else { a.scales.clone() };
    let variants = a.target.variants(&scales, !a.no_permute);
    let align = a.align.unwrap_or(a.r#type.size());
    let m = Matcher::new(a.r#type, a.stride, align, tol, variants.clone(), &target);
    m.scan_ready().map_err(|e| anyhow!(e))?;
    let (p, mods) = cx.open()?;
    let base = mods.iter().find(|e| e.name.eq_ignore_ascii_case(&cx.g.process)).map_or(0, |e| e.base);
    let filter = RegionFilter { mapped: a.mapped, writable_only: !a.readonly, ..RegionFilter::DATA };
    let regions = filter.apply(&p.regions());
    eprintln!(
        "scanning {} regions ({:.1} MiB) for {} {} ≈ {} (tol {tol}, {} variants)...",
        regions.len(),
        regions.iter().map(|r| r.size).sum::<usize>() as f64 / 1048576.0,
        a.r#type.name(),
        format!("{:?}", a.target).to_lowercase(),
        fmt_vals(&target),
        variants.len()
    );
    let (set, stats) = scan_memory(&p, &regions, &m, a.max_candidates)?;
    let session = Session {
        version: 1,
        pid: p.pid,
        module_base: base as u64,
        target: a.target,
        value_type: a.r#type,
        tol,
        stride: a.stride,
        align,
        wheel: a.wheel,
        custom_value: a.value,
        variant_desc: variants.iter().map(Variant::describe).collect(),
        variants,
        count: set.len(),
        history: vec![Step {
            op: "scan-init".into(),
            mode: None,
            before: 0,
            after: set.len(),
            target: target.clone(),
            unix_time: unix_now(),
            millis: stats.millis,
        }],
    };
    cx.save_session(&session, &set)?;
    let hist = variant_histogram(&session, &set);
    cx.emit(&serde_json::json!({ "candidates": set.len(), "stats": stats, "target": target, "variants": hist }), || {
        println!(
            "{} candidates ({:.1} MiB scanned, {:.1} MiB unreadable, {} ms)",
            set.len(),
            stats.bytes_scanned as f64 / 1048576.0,
            stats.bytes_unreadable as f64 / 1048576.0,
            stats.millis
        );
        for (d, n) in hist.iter().take(8) {
            println!("  {n:>10}  {d}");
        }
    })
}

fn cmd_scan_next(cx: &Ctx, mode: NextMode, tol: Option<f64>, eps: f64, force: bool) -> Result<()> {
    let (mut s, mut set) = cx.load_session()?;
    let (p, _) = cx.open()?;
    if p.pid != s.pid && !force {
        bail!("session '{}' was recorded for pid {}, game is now pid {}: addresses are stale. Run scan-init again (or --force)", cx.g.session, s.pid, p.pid);
    }
    let t0 = Instant::now();
    let target = if mode == NextMode::Match {
        s.target.values(&cx.oracle()?.sample()?, s.wheel, s.custom_value)?
    } else {
        s.history
            .iter()
            .rev()
            .find(|h| !h.target.is_empty())
            .map_or_else(|| vec![1.0; s.target.arity()], |h| h.target.clone())
    };
    let m = Matcher::new(s.value_type, s.stride, s.align, tol.unwrap_or(s.tol), s.variants.clone(), &target);
    let fresh = refresh(&p, &set, s.value_type, s.stride);
    let kept: Vec<bool> = (0..set.len())
        .map(|i| keep(mode, &m, set.variants[i] as usize, set.values_of(i), fresh[i].as_deref(), eps))
        .collect();
    let before = set.len();
    set.retain_update(&kept, &fresh);
    s.count = set.len();
    s.history.push(Step {
        op: "scan-next".into(),
        mode: Some(mode),
        before,
        after: set.len(),
        target: if mode == NextMode::Match { target.clone() } else { Vec::new() },
        unix_time: unix_now(),
        millis: t0.elapsed().as_millis(),
    });
    cx.save_session(&s, &set)?;
    let hist = variant_histogram(&s, &set);
    cx.emit(&serde_json::json!({ "before": before, "after": set.len(), "mode": mode, "variants": hist }), || {
        println!("{:?}: {before} -> {} candidates", mode, set.len());
        if mode == NextMode::Match {
            println!("  shm target {}", fmt_vals(&target));
        }
        for (d, n) in hist.iter().take(8) {
            println!("  {n:>10}  {d}");
        }
        if set.is_empty() {
            println!("  nothing left: the value may not be stored in this form; retry scan-init with --type f64 or other --scales");
        } else if set.len() <= 50 {
            println!("  small enough: run `acr-probe scan-list`");
        }
    })
}

#[derive(Serialize)]
struct ListRow {
    addr: String,
    location: String,
    variant: String,
    stored: Vec<f64>,
    live: Option<Vec<f64>>,
    as_shm: Option<Vec<f64>>,
}

fn cmd_scan_list(cx: &Ctx, limit: usize) -> Result<()> {
    let (s, set) = cx.load_session()?;
    let live = cx.open().ok().filter(|(p, _)| p.pid == s.pid);
    let n = set.len().min(limit);
    let mut sub = CandidateSet::new(set.arity);
    for i in 0..n {
        sub.push(set.addrs[i], set.variants[i], set.values_of(i));
    }
    let fresh = live.as_ref().map(|(p, _)| refresh(p, &sub, s.value_type, s.stride));
    let mods = live.as_ref().map(|(_, m)| m.as_slice()).unwrap_or(&[]);
    let rows: Vec<ListRow> = (0..n)
        .map(|i| {
            let v = &s.variants[sub.variants[i] as usize];
            let lv = fresh.as_ref().and_then(|f| f[i].clone());
            ListRow {
                addr: format!("{:#x}", sub.addrs[i]),
                location: symbolize(mods, sub.addrs[i] as usize).unwrap_or_else(|| "heap".into()),
                variant: s.variant_desc[sub.variants[i] as usize].clone(),
                stored: sub.values_of(i).to_vec(),
                as_shm: lv.as_ref().map(|l| v.invert(l)),
                live: lv,
            }
        })
        .collect();
    cx.emit(&serde_json::json!({ "session": cx.g.session, "total": set.len(), "history": s.history, "rows": rows, "sessions": cx.work.sessions() }), || {
        println!(
            "session '{}': {:?} {} tol {} — {} candidates (pid {}{})",
            cx.g.session,
            s.target,
            s.value_type.name(),
            s.tol,
            set.len(),
            s.pid,
            if live.is_some() { "" } else { ", not running: stored values only" }
        );
        for r in &rows {
            println!(
                "{:>16} {:<22} {:<28} live {}  (as shm {})",
                r.addr,
                r.location,
                r.variant,
                r.live.as_deref().map_or("-".into(), fmt_vals),
                r.as_shm.as_deref().map_or("-".into(), fmt_vals)
            );
        }
        if set.len() > n {
            println!("... {} more (--limit)", set.len() - n);
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn cmd_watch(
    cx: &Ctx,
    addrs: &[AddrArg],
    seconds: f64,
    hz: f64,
    ty: Option<NumType>,
    arity: Option<usize>,
    top: usize,
    target: Option<Target>,
) -> Result<()> {
    let (p, mods) = cx.open()?;
    let session = cx.load_session().ok();
    let mut list: Vec<(usize, Option<Variant>)> = Vec::new();
    for a in addrs {
        let addr = a.resolve(&mods)?;
        let v = session.as_ref().and_then(|(s, c)| {
            c.addrs.iter().position(|&x| x as usize == addr).map(|i| s.variants[c.variants[i] as usize].clone())
        });
        list.push((addr, v));
    }
    if list.is_empty() {
        let (s, c) = session.as_ref().context("no addresses given and no scan session")?;
        for i in 0..c.len().min(top) {
            list.push((c.addrs[i] as usize, Some(s.variants[c.variants[i] as usize].clone())));
        }
    }
    let ty = ty.or(session.as_ref().map(|(s, _)| s.value_type)).unwrap_or(NumType::F32);
    let arity = arity.or(session.as_ref().map(|(s, _)| s.target.arity())).unwrap_or(3);
    let stride = session.as_ref().map_or(1, |(s, _)| s.stride);
    let target = target.or(session.as_ref().map(|(s, _)| s.target));
    let (wheel, custom) = session.as_ref().map_or((0, None), |(s, _)| (s.wheel, s.custom_value));
    let oracle = cx.oracle().ok();
    let period = Duration::from_secs_f64(1.0 / hz.max(0.1));
    let t0 = Instant::now();
    let mut ticks = Vec::new();
    while t0.elapsed().as_secs_f64() < seconds {
        let tick = Instant::now();
        let shm = oracle.as_ref().and_then(|o| o.sample().ok());
        let tv = match (target, &shm) {
            (Some(t), Some(s)) => t.values(s, wheel, custom).ok(),
            _ => None,
        };
        let mut row = Vec::new();
        for (addr, v) in &list {
            let mut buf = vec![0u8; ((arity - 1) * stride + 1) * ty.size()];
            let vals = p.read(*addr, &mut buf).then(|| {
                (0..arity).map(|k| ty.read(&buf, k * stride * ty.size()).unwrap_or(f64::NAN)).collect::<Vec<_>>()
            });
            let as_shm = match (&vals, v) {
                (Some(x), Some(v)) => Some(v.invert(x)),
                _ => None,
            };
            row.push(serde_json::json!({ "addr": format!("{addr:#x}"), "mem": vals, "as_shm": as_shm }));
        }
        let t = t0.elapsed().as_secs_f64();
        if cx.g.json {
            ticks.push(serde_json::json!({ "t": t, "shm": tv, "values": row }));
        } else {
            let mut line = format!("{t:6.2}s shm {:<28}", tv.as_deref().map_or("-".into(), fmt_vals));
            for r in &row {
                let shown = r.get("as_shm").filter(|v| !v.is_null()).or(r.get("mem")).cloned().unwrap_or_default();
                let vals: Vec<f64> = serde_json::from_value(shown).unwrap_or_default();
                line.push_str(&format!(" | {}", if vals.is_empty() { "unreadable".into() } else { fmt_vals(&vals) }));
            }
            println!("{line}");
        }
        if let Some(rest) = period.checked_sub(tick.elapsed()) {
            std::thread::sleep(rest);
        }
    }
    if cx.g.json {
        let header: Vec<String> = list.iter().map(|(a, _)| format!("{a:#x}")).collect();
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "addrs": header, "ticks": ticks }))?);
    }
    Ok(())
}

fn cmd_struct_dump(cx: &Ctx, addr: &AddrArg, len: usize, before: usize) -> Result<()> {
    let (p, mods) = cx.open()?;
    let origin = addr.resolve(&mods)?;
    let start = origin.saturating_sub(before) & !7;
    let mut buf = vec![0u8; len + (origin - start)];
    let segs = mem::read_segments(&p, start, &mut buf);
    if segs.is_empty() {
        bail!("{start:#x}..+{:#x} is unreadable", buf.len());
    }
    let rows = structs::dump_rows(&buf, start, origin, &mods);
    cx.emit(&rows, || {
        println!("{origin:#x} ({})", symbolize(&mods, origin).unwrap_or_else(|| "heap".into()));
        structs::print_rows(&rows)
    })
}

/// Best variant mapping `mem` to the shm position (min error), searched over axis orders/signs.
fn fit_pos_variant(mem: &[f64], shm: &[f64; 3]) -> (Variant, f64) {
    vec3_variants(&[1.0, 100.0], true)
        .into_iter()
        .map(|v| {
            let e = v.apply(shm).iter().zip(mem).fold(0.0f64, |m, (a, b)| m.max((a - b).abs() / v.scale));
            (v, e)
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap()
}

#[allow(clippy::too_many_arguments)]
fn cmd_struct_correlate(
    cx: &Ctx,
    pos: &AddrArg,
    base: Option<&AddrArg>,
    ty: Option<NumType>,
    range: usize,
    n: usize,
    interval: u64,
    vel_tol: f64,
    ang_tol: f64,
) -> Result<()> {
    let (p, mods) = cx.open()?;
    let pos_addr = pos.resolve(&mods)?;
    let base = base.map(|b| b.resolve(&mods)).transpose()?.unwrap_or(pos_addr);
    let session = cx.load_session().ok().filter(|(s, _)| s.target == Target::Pos && s.pid == p.pid);
    let ty = ty.or(session.as_ref().map(|(s, _)| s.value_type)).unwrap_or(NumType::F32);
    let wbase = pos_addr.saturating_sub(range).min(base) & !7;
    let wlen = (pos_addr + range).max(base + 0x10) - wbase;
    let oracle = cx.oracle()?;
    let mut samples = Vec::with_capacity(n);
    eprintln!("sampling {n} x {wlen:#x} bytes around {pos_addr:#x} every {interval} ms; keep driving/steering as instructed...");
    for i in 0..n {
        if i > 0 {
            std::thread::sleep(Duration::from_millis(interval));
        }
        let shm = oracle.sample()?;
        let mut bytes = vec![0u8; wlen];
        if mem::read_segments(&p, wbase, &mut bytes).is_empty() {
            bail!("window {wbase:#x} unreadable (object freed? car changed?)");
        }
        samples.push(structs::Sample { shm, bytes });
    }
    let s0 = &samples[0];
    let off = pos_addr - wbase;
    let pos_mem: Vec<f64> = (0..3).map(|k| ty.read(&s0.bytes, off + k * ty.size()).unwrap_or(f64::NAN)).collect();
    let (pos_variant, err) = session
        .as_ref()
        .and_then(|(s, c)| {
            c.addrs
                .iter()
                .position(|&a| a as usize == pos_addr)
                .map(|i| (s.variants[c.variants[i] as usize].clone(), 0.0))
        })
        .unwrap_or_else(|| fit_pos_variant(&pos_mem, &s0.shm.pos));
    if err > 0.5 {
        eprintln!(
            "warning: {pos_addr:#x} ({}) doesn't match the shm position (error {err:.2} m); is --type right?",
            fmt_vals(&pos_mem)
        );
    }
    let c = structs::correlate(
        &samples,
        &structs::CorrelateParams { wbase, pos_addr, base, ty, pos_variant, vel_tol, ang_tol, max_hits_per_field: 5 },
    );
    let path = cx.work.file("struct-correlate.json");
    cx.work.save_json(&path, &c)?;
    cx.emit(&c, || {
        println!("position {pos_addr:#x} = {}; offsets relative to {base:#x}", c.pos_variant);
        for h in &c.hits {
            println!(
                "  {:<34} {:>+8x} {:<4} err {:<9.4} {}{}",
                h.field,
                h.offset,
                h.value_type,
                h.error,
                h.detail,
                if h.same_frame { "" } else { " (other frame/layout)" }
            );
        }
        for nt in &c.notes {
            println!("  note: {nt}");
        }
        println!("\nproposed config (also in {}):\n{}", path.display(), c.proposal_toml);
    })
}

#[derive(Serialize, Deserialize)]
struct PointerFile {
    created_unix: u64,
    pid: u32,
    module: String,
    module_base: String,
    target_addr: String,
    target: Target,
    value_type: NumType,
    variant: Option<Variant>,
    /// "module" or "pawn".
    chain_root: String,
    /// Pawn class the chains start from (chain_root = "pawn").
    #[serde(default)]
    pawn_class: Option<String>,
    chains: Vec<pointers::Chain>,
    #[serde(default)]
    verified: Vec<VerifyResult>,
}

#[derive(Clone, Serialize, Deserialize)]
struct VerifyResult {
    unix_time: u64,
    pid: u32,
    ok: usize,
    total: usize,
}

/// Live pawn: `class` = "auto" picks the first live `vehicle_pawn_class_candidates` class.
fn locate_pawn<R: ReadMem>(cx: &Ctx, pc: &PageCache<R>, mods: &[ModuleEntry], class: &str) -> Result<(String, usize)> {
    let sigs = cx.sigs()?;
    let module = engine::LoadedModule::load(pc, mods, &sigs)?;
    let dm: &dyn acr_ue::mem::Memory = pc;
    let ue = engine::open_ue(dm, &module, &sigs)?;
    let want = (class != "auto").then_some(class);
    engine::find_pawn(&ue, &sigs, want)
        .with_context(|| format!("no live instance of pawn class '{class}' (are you on a stage?)"))
}

#[allow(clippy::too_many_arguments)]
fn cmd_pointer_scan(
    cx: &Ctx,
    addr: &AddrArg,
    depth: usize,
    max_off: usize,
    max_nodes: usize,
    max_results: usize,
    target: Target,
    ty: Option<NumType>,
    pawn_class: Option<&str>,
    pawn_len: usize,
) -> Result<()> {
    let (p, mods) = cx.open()?;
    let target_addr = addr.resolve(&mods)?;
    let module =
        mods.iter().find(|m| m.name.eq_ignore_ascii_case(&cx.g.process)).context("game module not in module list")?;
    let pc = PageCache::new(&p, 1 << 16);
    let (roots, root_base, chain_root, pawn_name) = match pawn_class {
        Some(class) => {
            let (name, pawn) = locate_pawn(cx, &pc, &mods, class)?;
            eprintln!("rooting chains in {name} instance {pawn:#x} (+{pawn_len:#x})");
            (vec![(pawn, pawn + pawn_len)], pawn, "pawn", Some(name))
        }
        None => {
            let info = acr_ue::pe::ModuleInfo::parse(&pc, module.base).context("PE headers")?;
            let roots: Vec<(usize, usize)> = info
                .sections
                .iter()
                .filter(|s| s.name == ".data" || s.name == ".rdata")
                .map(|s| (module.base + s.rva, module.base + s.rva + s.size))
                .collect();
            if roots.is_empty() {
                bail!("no .data/.rdata sections in {}", module.name);
            }
            (roots, module.base, "module", None)
        }
    };
    let session = cx.load_session().ok().filter(|(s, _)| s.pid == p.pid);
    let (ty, variant) = match &session {
        Some((s, c)) => (
            ty.unwrap_or(s.value_type),
            c.addrs.iter().position(|&a| a as usize == target_addr).map(|i| s.variants[c.variants[i] as usize].clone()),
        ),
        None => (ty.unwrap_or(NumType::F32), None),
    };
    let regions = RegionFilter { image: true, private: true, mapped: false, writable_only: false, executable: false }
        .apply(&p.regions());
    eprintln!("pointer scan to {target_addr:#x}: depth {depth}, max offset {max_off:#x}, {} regions", regions.len());
    let (chains, stats) = pointers::pointer_scan(
        &p,
        &regions,
        &pointers::PtrScanParams {
            target: target_addr,
            max_depth: depth,
            max_offset: max_off,
            max_nodes,
            max_results,
            roots,
            root_base,
        },
    );
    let file = PointerFile {
        created_unix: unix_now(),
        pid: p.pid,
        module: module.name.clone(),
        module_base: format!("{:#x}", module.base),
        target_addr: format!("{target_addr:#x}"),
        target,
        value_type: ty,
        variant,
        chain_root: chain_root.into(),
        pawn_class: pawn_name,
        chains,
        verified: Vec::new(),
    };
    let path = cx.work.file("pointers.json");
    cx.work.save_json(&path, &file)?;
    cx.emit(&serde_json::json!({ "file": path, "levels": stats, "chains": file.chains }), || {
        for s in &stats {
            println!(
                "  depth {}: {} pointers, {} static roots, {} kept{} ({} ms)",
                s.depth,
                s.found,
                s.roots,
                s.kept,
                if s.truncated { " (TRUNCATED: raise --max-nodes or lower --max-offset)" } else { "" },
                s.millis
            );
        }
        println!("{} chains saved to {}; shortest:", file.chains.len(), path.display());
        for c in file.chains.iter().take(10) {
            println!("{}\n", c.toml(&file.chain_root));
        }
        if !file.chains.is_empty() {
            println!("restart the game, get back on stage, then run `acr-probe pointer-verify`");
        }
    })
}

fn cmd_pointer_verify(cx: &Ctx, file: Option<PathBuf>, prune: bool) -> Result<()> {
    let path = file.unwrap_or_else(|| cx.work.file("pointers.json"));
    let mut pf: PointerFile = cx.work.load_json(&path)?;
    let (p, mods) = cx.open()?;
    let module = mods.iter().find(|m| m.name.eq_ignore_ascii_case(&pf.module)).context("module not loaded")?;
    let root = match &pf.pawn_class {
        Some(class) => locate_pawn(cx, &PageCache::new(&p, 1 << 16), &mods, class)?.1,
        None => module.base,
    };
    let live = mem::Live(&p);
    let shm = cx.oracle().ok().and_then(|o| o.sample().ok());
    let expected = shm.as_ref().and_then(|s| pf.target.values(s, 0, None).ok());
    let arity = pf.target.arity();
    let tol = pf.target.default_tol() * 4.0;
    let mut results = Vec::new();
    let mut keep_mask = Vec::new();
    for c in &pf.chains {
        let base = acr_ue::sim_car::resolve_chain(&live, root, &c.chain);
        let addr = base.map(|b| b + c.field_offset as usize);
        let vals = addr.and_then(|a| {
            let mut buf = vec![0u8; arity * pf.value_type.size()];
            ReadMem::read(&p, a, &mut buf).then(|| {
                (0..arity)
                    .map(|k| pf.value_type.read(&buf, k * pf.value_type.size()).unwrap_or(f64::NAN))
                    .collect::<Vec<f64>>()
            })
        });
        let as_shm = match (&vals, &pf.variant) {
            (Some(v), Some(var)) => Some(var.invert(v)),
            (Some(v), None) => Some(v.clone()),
            _ => None,
        };
        let ok = match (&as_shm, &expected) {
            (Some(a), Some(e)) => a.iter().zip(e).all(|(x, y)| (x - y).abs() <= tol),
            (Some(a), None) => a.iter().all(|x| x.is_finite()),
            _ => false,
        };
        keep_mask.push(ok);
        results.push(serde_json::json!({
            "chain": c.chain.iter().map(|o| format!("{o:#x}")).collect::<Vec<_>>(),
            "field_offset": format!("{:#x}", c.field_offset),
            "resolved": addr.map(|a| format!("{a:#x}")),
            "as_shm": as_shm,
            "ok": ok,
        }));
    }
    let ok = keep_mask.iter().filter(|k| **k).count();
    pf.verified.push(VerifyResult { unix_time: unix_now(), pid: p.pid, ok, total: pf.chains.len() });
    if prune {
        let mut it = keep_mask.iter();
        pf.chains.retain(|_| *it.next().unwrap());
    }
    cx.work.save_json(&path, &pf)?;
    let same_pid = pf.pid == p.pid;
    cx.emit(&serde_json::json!({ "pid": p.pid, "expected": expected, "ok": ok, "total": keep_mask.len(), "results": results }), || {
        if same_pid {
            println!("note: same pid as the scan; restart the game to prove the chains are static");
        }
        println!("shm {}: {ok}/{} chains resolve to it", expected.as_deref().map_or("unavailable".into(), fmt_vals), keep_mask.len());
        for (r, c) in results.iter().zip(&pf.chains).filter(|(r, _)| r["ok"] == true).take(5) {
            println!("{}  -> {}\n", c.toml(&pf.chain_root), r["resolved"]);
        }
        if prune {
            println!("pruned file keeps {} chains", pf.chains.len());
        }
    })
}

fn cmd_sig_test(cx: &Ctx) -> Result<()> {
    let sigs = cx.sigs()?;
    let (p, mods) = cx.open()?;
    let pc = PageCache::new(&p, 1 << 16);
    let shm = cx.oracle().ok().and_then(|o| o.sample().ok());
    let rep = engine::sig_test(&pc, &mods, &sigs, shm.as_ref())?;
    cx.work.save_json(&cx.work.file("sig-test.json"), &rep)?;
    cx.emit(&rep, || {
        println!(
            "{} @ {} size {} (original filename ok: {:?})",
            rep.module, rep.module_base, rep.module_size, rep.original_filename_ok
        );
        for i in &rep.items {
            let mark = match i.plausible {
                Some(true) => "OK  ",
                Some(false) => "FAIL",
                None => "?   ",
            };
            println!(
                "[{mark}] {:<14} {}{}{} {}",
                i.section,
                i.source,
                i.matches.map_or(String::new(), |m| format!(" matches={m}")),
                i.resolved
                    .as_ref()
                    .map_or(String::new(), |r| format!(" -> {r} (rva {})", i.rva.as_deref().unwrap_or("-"))),
                if i.detail.is_empty() { String::new() } else { format!("— {}", i.detail) }
            );
        }
        if let Some(n) = rep.objects {
            println!("objects {n}, classes {:?}; missing core classes {:?}", rep.classes, rep.missing_core_classes);
            println!("functions found {}, missing {:?}", rep.functions_found.len(), rep.functions_missing);
            println!(
                "vehicle pawn classes present {:?}; movement {:?}",
                rep.vehicle_pawn_classes_present, rep.movement_classes_present
            );
            println!("blocked online classes live now: {:?}", rep.blocked_online_classes_live);
        }
        for s in &rep.summary {
            println!("=> {s}");
        }
    })
}

fn cmd_find_car(cx: &Ctx, o: &findcar::FindCarOptions, samples: usize, interval: u64) -> Result<()> {
    let sigs = cx.sigs()?;
    let (p, mods) = cx.open()?;
    let module = mods
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case(&cx.g.process))
        .or_else(|| mods.first())
        .context("no modules")?;
    let cfg = findcar::scan_config(&sigs.raw_offsets.sim_car, module.base, o)?;
    let rva = (cfg.vtable - module.base) as u64;
    let oracle = cx.oracle().ok();
    let shm = || oracle.as_ref().and_then(|o| o.sample().ok());
    let src = findcar::RemoteScan(&p);
    let (rep, loc) = findcar::find_car(&src, module.base, &cfg, rva, &|| shm().map(|s| s.pos), o.all);
    if !cx.g.json {
        findcar::print_report(&rep);
    }
    let mut live = Vec::new();
    if let Ok((_, car)) = &loc.chosen {
        let t0 = Instant::now();
        for i in 0..samples {
            if i > 0 {
                std::thread::sleep(Duration::from_millis(interval));
            }
            let s = shm();
            let l = findcar::live_row(&src, &cfg, car, t0.elapsed().as_secs_f64(), s.map(|s| (s.pos, s.vel)));
            if !cx.g.json {
                findcar::print_live(&l);
            }
            live.push(l);
        }
    }
    let out = serde_json::json!({ "pid": p.pid, "report": rep, "live": live });
    cx.work.save_json(&cx.work.file("find-car.json"), &out)?;
    cx.emit(&out, || println!("saved re-work/find-car.json"))
}

#[allow(clippy::too_many_arguments)]
fn cmd_strings(
    cx: &Ctx,
    pattern: &str,
    fold: bool,
    module: Option<&str>,
    all_modules: bool,
    heap: bool,
    max: usize,
) -> Result<()> {
    let (p, mods) = cx.open()?;
    let modname = module.unwrap_or(&cx.g.process);
    let ranges: Vec<(usize, usize)> = mods
        .iter()
        .filter(|m| all_modules || m.name.eq_ignore_ascii_case(modname))
        .map(|m| (m.base, m.base + m.size))
        .collect();
    let regions: Vec<mem::MemRegion> = p
        .regions()
        .into_iter()
        .filter(|r| {
            (heap && r.kind == RegionKind::Private)
                || (r.kind == RegionKind::Image && ranges.iter().any(|&(s, e)| r.base >= s && r.base < e))
        })
        .collect();
    let hits = strings::search(&p, &regions, pattern, fold, max);
    cx.emit(&hits, || {
        for h in &hits {
            println!(
                "{:#014x} {:<24} {:?} {}",
                h.addr,
                symbolize(&mods, h.addr).unwrap_or_else(|| "heap".into()),
                h.encoding,
                h.text
            );
        }
        println!("{} hits", hits.len());
    })
}

const DEFAULT_CLASS_FILTERS: &[&str] =
    &["vehicle", "car", "sim", "physics", "wheel", "online", "lobby", "leaderboard", "timeattack"];

fn cmd_ue_dump(cx: &Ctx, filter: Vec<String>, all: bool, o: engine::DumpOptions) -> Result<()> {
    let sigs = cx.sigs()?;
    let (p, mods) = cx.open()?;
    let pc = PageCache::new(&p, 1 << 16);
    let module = engine::LoadedModule::load(&pc, &mods, &sigs)?;
    let dm: &dyn acr_ue::mem::Memory = &pc;
    let ue = engine::open_ue(dm, &module, &sigs)?;
    let filters = if all {
        Vec::new()
    } else if filter.is_empty() {
        DEFAULT_CLASS_FILTERS.iter().map(|s| s.to_string()).collect()
    } else {
        filter
    };
    let classes = engine::dump_classes(&ue, &engine::DumpOptions { filters, ..o })?;
    cx.work.save_json(&cx.work.file("ue-classes.json"), &classes)?;
    cx.emit(&classes, || {
        for c in &classes {
            println!(
                "{:<12} {:<48} {:#x}{} {}",
                c.kind,
                c.name,
                c.properties_size.unwrap_or(0),
                if c.instance_count > 0 {
                    format!(" [{} live: {}]", c.instance_count, c.instances.join(" "))
                } else {
                    String::new()
                },
                c.hierarchy.iter().skip(1).take(4).cloned().collect::<Vec<_>>().join(" < ")
            );
            for pr in &c.properties {
                println!("      {:>8} {:>6}  {}", pr.offset, pr.size, pr.name);
            }
            if !c.functions.is_empty() {
                println!("      fn {}", c.functions.join(", "));
            }
        }
        println!("{} classes (saved to re-work/ue-classes.json)", classes.len());
    })
}

fn cmd_import_dumper7(cx: &Ctx, dir: PathBuf, dry_run: bool, apply_classes: bool) -> Result<()> {
    let text = cx.sigs_text()?;
    let (rep, new_text) = dumper7::import(&dir, &text, &dumper7::ImportOptions { apply_classes })?;
    acr_ue::sigs::Signatures::from_toml_str(&new_text)
        .map_err(|e| anyhow!("updated signatures.toml would not parse: {e}"))?;
    cx.work.save_json(&cx.work.file("dumper7-import.json"), &rep)?;
    let wrote = !dry_run && new_text != text;
    if wrote {
        let backup = cx.work.ensure("backups")?.join(format!("signatures.{}.toml", unix_now()));
        std::fs::write(&backup, &text)?;
        std::fs::write(&cx.g.sigs, &new_text)?;
    }
    cx.emit(&rep, || {
        println!("SDK {} ({} files, {} classes)", rep.sdk_dir, rep.files_parsed, rep.classes_parsed);
        println!("Offsets: {:?}", rep.offsets);
        for c in &rep.changes {
            println!("  {:<48} {} -> {}   ({})", c.key, c.old.as_deref().unwrap_or("unset"), c.new, c.source);
        }
        if rep.changes.is_empty() {
            println!("  signatures.toml already up to date");
        }
        let show = |title: &str, list: &[dumper7::ClassCandidate]| {
            if list.is_empty() {
                return;
            }
            println!("{title}:");
            for c in list.iter().take(12) {
                println!(
                    "  {:<40} {:<8} {} <- {}",
                    c.name,
                    c.size,
                    c.package,
                    c.supers.iter().take(3).cloned().collect::<Vec<_>>().join(" < ")
                );
                if !c.interesting_members.is_empty() {
                    println!(
                        "      members: {}",
                        c.interesting_members.iter().take(6).cloned().collect::<Vec<_>>().join("; ")
                    );
                }
                if !c.big_pads.is_empty() {
                    println!("      unreflected blocks: {}", c.big_pads.join(", "));
                }
                if !c.interesting_functions.is_empty() {
                    println!("      functions: {}", c.interesting_functions.join(", "));
                }
            }
        };
        println!("Chaos Vehicles present: {}", rep.has_chaos_vehicles);
        show("Pawn candidates", &rep.pawns);
        show("Component candidates", &rep.components);
        show("Game modes / states", &rep.game_modes);
        show("Online / leaderboard", &rep.online);
        show("Other vehicle/sim classes", &rep.other);
        for n in &rep.notes {
            println!("note: {n}");
        }
        println!(
            "{} (full report in re-work/dumper7-import.json)",
            if dry_run {
                "dry run: signatures.toml not modified"
            } else if wrote {
                "signatures.toml updated (backup in re-work/backups)"
            } else {
                "no changes written"
            }
        );
    })
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cx = Ctx { work: WorkDir::new(cli.g.work_dir.clone()), g: cli.g };
    match cli.cmd {
        Cmd::Modules { filter } => cmd_modules(&cx, filter.as_deref()),
        Cmd::Shm { count, interval_ms, save } => cmd_shm(&cx, count, interval_ms, save),
        Cmd::ScanInit(a) => cmd_scan_init(&cx, &a),
        Cmd::ScanNext { changed, unchanged, increased, decreased, tol, eps, force } => {
            let mode = if changed {
                NextMode::Changed
            } else if unchanged {
                NextMode::Unchanged
            } else if increased {
                NextMode::Increased
            } else if decreased {
                NextMode::Decreased
            } else {
                NextMode::Match
            };
            cmd_scan_next(&cx, mode, tol, eps, force)
        }
        Cmd::ScanList { limit } => cmd_scan_list(&cx, limit),
        Cmd::Watch { addrs, seconds, hz, r#type, arity, top, target } => {
            cmd_watch(&cx, &addrs, seconds, hz, r#type, arity, top, target)
        }
        Cmd::StructDump { addr, len, before } => cmd_struct_dump(&cx, &addr, len, before),
        Cmd::StructCorrelate { pos_addr, base, r#type, range, samples, interval_ms, vel_tol, ang_tol } => {
            cmd_struct_correlate(&cx, &pos_addr, base.as_ref(), r#type, range, samples, interval_ms, vel_tol, ang_tol)
        }
        Cmd::PointerScan {
            addr,
            max_depth,
            max_offset,
            max_nodes,
            max_results,
            target,
            r#type,
            pawn_class,
            pawn_len,
        } => cmd_pointer_scan(
            &cx,
            &addr,
            max_depth,
            max_offset,
            max_nodes,
            max_results,
            target,
            r#type,
            pawn_class.as_deref(),
            pawn_len,
        ),
        Cmd::PointerVerify { file, prune } => cmd_pointer_verify(&cx, file, prune),
        Cmd::SigTest => cmd_sig_test(&cx),
        Cmd::FindCar { vtable_rva, tol, radius, samples, interval_ms, all } => cmd_find_car(
            &cx,
            &findcar::FindCarOptions { vtable_rva, tolerance_m: tol, radius_m: radius, all },
            samples,
            interval_ms,
        ),
        Cmd::Strings { pattern, ignore_case, module, all_modules, heap, max } => {
            cmd_strings(&cx, &pattern, ignore_case, module.as_deref(), all_modules, heap, max)
        }
        Cmd::UeDumpClasses { filter, all, props, funcs, instances, max_instances } => cmd_ue_dump(
            &cx,
            filter,
            all,
            engine::DumpOptions { filters: Vec::new(), props, funcs, instances, max_instances },
        ),
        Cmd::ImportDumper7 { dir, dry_run, apply_classes } => cmd_import_dumper7(&cx, dir, dry_run, apply_classes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_numbers_and_addresses() {
        assert_eq!(parse_u64("0x1F"), Ok(0x1f));
        assert_eq!(parse_u64("31"), Ok(31));
        assert_eq!(parse_u64("0x0000_7ff6`12340000"), Ok(0x7ff6_1234_0000));
        let a = parse_addr_arg("acr.exe+0x10").unwrap();
        assert_eq!(a.module.as_deref(), Some("acr.exe"));
        let mods = [ModuleEntry { name: "ACR.exe".into(), base: 0x1_4000_0000, size: 0x100_0000, path: String::new() }];
        assert_eq!(a.resolve(&mods).unwrap(), 0x1_4000_0010);
        assert_eq!(parse_addr_arg("0x1000+0x20").unwrap().resolve(&[]).unwrap(), 0x1020);
        assert!(parse_addr_arg("nope.dll+0x1").unwrap().resolve(&mods).is_err());
    }

    #[test]
    fn cli_parses_every_subcommand() {
        for args in [
            &["acr-probe", "modules"][..],
            &["acr-probe", "--json", "shm", "--count", "3"],
            &["acr-probe", "scan-init", "--target", "pos", "--type", "f64", "--tol", "0.05"],
            &["acr-probe", "scan-init", "--target", "custom", "--value", "1.5", "--scales", "1,100"],
            &["acr-probe", "scan-next", "--unchanged"],
            &["acr-probe", "scan-list", "--session", "vel"],
            &["acr-probe", "watch", "0x1000", "acr.exe+0x20", "--seconds", "5"],
            &["acr-probe", "struct-dump", "0x1000", "--len", "0x400"],
            &["acr-probe", "struct-correlate", "0x1000", "--range", "0x800"],
            &["acr-probe", "pointer-scan", "0x1000", "--max-depth", "5", "--max-offset", "0x1000"],
            &["acr-probe", "pointer-scan", "0x1000", "--pawn-class", "auto", "--pawn-len", "0x4000"],
            &["acr-probe", "pointer-verify", "--prune"],
            &["acr-probe", "sig-test"],
            &["acr-probe", "find-car"],
            &["acr-probe", "--json", "find-car", "--vtable-rva", "0xa8d9830", "--tol", "3", "--samples", "0", "--all"],
            &["acr-probe", "strings", "-i", "Leaderboard", "--heap"],
            &["acr-probe", "ue-dump-classes", "--filter", "vehicle", "--props"],
            &["acr-probe", "import-dumper7", "C:\\Dumper-7", "--dry-run"],
        ] {
            Cli::try_parse_from(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        }
        assert!(Cli::try_parse_from(["acr-probe", "scan-next", "--changed", "--unchanged"]).is_err());
    }

    #[test]
    fn fits_position_variant() {
        let shm = [10.0, 2.0, -30.0];
        let mem = [-3000.0, 1000.0, 200.0];
        let (v, e) = fit_pos_variant(&mem, &shm);
        assert!(e < 1e-9);
        assert_eq!(v.describe(), "mem = 100 * (+z, +x, +y)");
    }
}
