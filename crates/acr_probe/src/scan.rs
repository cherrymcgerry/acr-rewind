//! Value scanning: find every address whose value(s) match an shm-derived target under any
//! coordinate [`Variant`], then narrow the candidates down over repeated samples.
//!
//! The per-buffer logic ([`Matcher::scan`], [`keep`]) is pure; [`scan_memory`] and
//! [`refresh`] drive it in parallel over a [`ReadMem`] with bounded buffers.

use crate::mem::{chunk_jobs, read_segments, MemRegion, ReadMem};
use crate::variants::Variant;
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum NumType {
    F32,
    F64,
}

impl NumType {
    pub fn size(self) -> usize {
        match self {
            Self::F32 => 4,
            Self::F64 => 8,
        }
    }

    /// Reads one value at `off` (little endian). `None` if out of bounds.
    #[inline]
    pub fn read(self, buf: &[u8], off: usize) -> Option<f64> {
        match self {
            Self::F32 => buf.get(off..off + 4).map(|b| f64::from(f32::from_le_bytes(b.try_into().unwrap()))),
            Self::F64 => buf.get(off..off + 8).map(|b| f64::from_le_bytes(b.try_into().unwrap())),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::F32 => "f32",
            Self::F64 => "f64",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    pub addr: usize,
    pub variant: u16,
}

/// Matches `arity` values spaced `stride` elements apart against a target under each variant.
#[derive(Clone, Debug)]
pub struct Matcher {
    pub ty: NumType,
    pub stride: usize,
    pub align: usize,
    pub tol: f64,
    pub variants: Vec<Variant>,
    target: Vec<f64>,
    expected: Vec<Vec<f64>>,
    anchor: Vec<usize>,
    bands: Vec<(f64, f64)>,
}

impl Matcher {
    pub fn new(ty: NumType, stride: usize, align: usize, tol: f64, variants: Vec<Variant>, target: &[f64]) -> Self {
        let a = (0..target.len()).max_by(|&i, &j| target[i].abs().total_cmp(&target[j].abs())).unwrap_or(0);
        let expected = variants.iter().map(|v| v.apply(target)).collect();
        let anchor = variants.iter().map(|v| v.perm.iter().position(|&p| p as usize == a).unwrap_or(0)).collect();
        let mut bands: Vec<(f64, f64)> = Vec::new();
        for v in &variants {
            let mag = target.get(a).map_or(0.0, |t| t.abs());
            let b = ((mag - tol).max(0.0) * v.scale, (mag + tol) * v.scale);
            if !bands.contains(&b) {
                bands.push(b);
            }
        }
        Self {
            ty,
            stride: stride.max(1),
            align: align.max(1),
            tol,
            variants,
            target: target.to_vec(),
            expected,
            anchor,
            bands,
        }
    }

    pub fn arity(&self) -> usize {
        self.target.len()
    }

    /// Bytes covered by one candidate.
    pub fn span(&self) -> usize {
        ((self.arity().max(1) - 1) * self.stride + 1) * self.ty.size()
    }

    fn slot(&self, k: usize) -> usize {
        k * self.stride * self.ty.size()
    }

    /// A full-memory scan needs a target that is clearly non-zero, or every zeroed page matches.
    pub fn scan_ready(&self) -> Result<(), String> {
        let mag = self.target.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        if !mag.is_finite() {
            return Err("target is not finite".into());
        }
        if mag <= 2.0 * self.tol {
            return Err(format!(
                "target magnitude {mag:.4} is within 2x tolerance ({}) of zero; every zeroed byte would match. \
                 Move the car / pick another target, or lower --tol",
                self.tol
            ));
        }
        Ok(())
    }

    /// Reads the `arity` values of a candidate starting at `off`.
    pub fn values_at(&self, buf: &[u8], off: usize) -> Option<Vec<f64>> {
        (0..self.arity()).map(|k| self.ty.read(buf, off + self.slot(k))).collect()
    }

    /// True if `vals` match the target under `variant` within tolerance.
    pub fn check(&self, variant: usize, vals: &[f64]) -> bool {
        let tol = self.tol * self.variants[variant].scale;
        let exp = &self.expected[variant];
        vals.len() == exp.len() && vals.iter().zip(exp).all(|(v, e)| (v - e).abs() <= tol)
    }

    /// Largest per-component error in shm units.
    #[cfg(test)]
    pub fn error(&self, variant: usize, vals: &[f64]) -> f64 {
        let s = self.variants[variant].scale;
        vals.iter().zip(&self.expected[variant]).fold(0.0f64, |m, (v, e)| m.max((v - e).abs() / s))
    }

    /// Scans `buf` (mapped at `base`) and appends hits whose start address is `< owned_end`.
    pub fn scan(&self, buf: &[u8], base: usize, owned_end: usize, out: &mut Vec<Hit>) {
        let size = self.ty.size();
        if buf.len() < size {
            return;
        }
        let first = (self.align - base % self.align) % self.align;
        let mut x = first;
        let last = buf.len() - size;
        while x <= last {
            let Some(v) = self.ty.read(buf, x) else {
                break;
            };
            let a = v.abs();
            if self.bands.iter().any(|&(lo, hi)| a >= lo && a <= hi) {
                for vi in 0..self.variants.len() {
                    let k = self.anchor[vi];
                    let tol = self.tol * self.variants[vi].scale;
                    if (v - self.expected[vi][k]).abs() > tol {
                        continue;
                    }
                    let Some(start) = x.checked_sub(self.slot(k)) else {
                        continue;
                    };
                    let addr = base + start;
                    if addr >= owned_end {
                        continue;
                    }
                    if let Some(vals) = self.values_at(buf, start) {
                        if self.check(vi, &vals) {
                            out.push(Hit { addr, variant: vi as u16 });
                        }
                    }
                }
            }
            x += self.align;
        }
    }
}

/// Candidate addresses with their variant and last-read values (flat, compact).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CandidateSet {
    pub arity: usize,
    pub addrs: Vec<u64>,
    pub variants: Vec<u16>,
    pub values: Vec<f64>,
}

const MAGIC: &[u8; 8] = b"ACRCAND1";

impl CandidateSet {
    pub fn new(arity: usize) -> Self {
        Self { arity, ..Default::default() }
    }

    pub fn len(&self) -> usize {
        self.addrs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.addrs.is_empty()
    }

    pub fn push(&mut self, addr: u64, variant: u16, vals: &[f64]) {
        debug_assert_eq!(vals.len(), self.arity);
        self.addrs.push(addr);
        self.variants.push(variant);
        self.values.extend_from_slice(vals);
    }

    pub fn values_of(&self, i: usize) -> &[f64] {
        &self.values[i * self.arity..(i + 1) * self.arity]
    }

    pub fn sort(&mut self) {
        let mut idx: Vec<usize> = (0..self.len()).collect();
        idx.sort_by_key(|&i| (self.addrs[i], self.variants[i]));
        let mut out = Self::new(self.arity);
        for i in idx {
            out.push(self.addrs[i], self.variants[i], self.values_of(i));
        }
        *self = out;
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let mut buf = Vec::with_capacity(20 + self.len() * (10 + 8 * self.arity));
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&(self.arity as u32).to_le_bytes());
        buf.extend_from_slice(&(self.len() as u64).to_le_bytes());
        for i in 0..self.len() {
            buf.extend_from_slice(&self.addrs[i].to_le_bytes());
            buf.extend_from_slice(&self.variants[i].to_le_bytes());
            for v in self.values_of(i) {
                buf.extend_from_slice(&v.to_le_bytes());
            }
        }
        let tmp = path.with_extension("tmp");
        std::fs::File::create(&tmp)
            .and_then(|mut f| f.write_all(&buf))
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let mut buf = Vec::new();
        std::fs::File::open(path)
            .and_then(|mut f| f.read_to_end(&mut buf))
            .with_context(|| format!("reading {} (run scan-init first)", path.display()))?;
        if buf.len() < 20 || &buf[..8] != MAGIC {
            bail!("{} is not a candidate file", path.display());
        }
        let arity = u32::from_le_bytes(buf[8..12].try_into()?) as usize;
        let n = u64::from_le_bytes(buf[12..20].try_into()?) as usize;
        let rec = 10 + 8 * arity;
        if buf.len() != 20 + n * rec {
            bail!("{} is truncated", path.display());
        }
        let mut out = Self::new(arity);
        out.addrs.reserve(n);
        for r in buf[20..].chunks_exact(rec) {
            let vals: Vec<f64> = r[10..].as_chunks::<8>().0.iter().map(|c| f64::from_le_bytes(*c)).collect();
            out.push(u64::from_le_bytes(r[..8].try_into()?), u16::from_le_bytes(r[8..10].try_into()?), &vals);
        }
        Ok(out)
    }

    /// Keeps entries where `keep[i]`, replacing their values with `fresh[i]`.
    pub fn retain_update(&mut self, keep: &[bool], fresh: &[Option<Vec<f64>>]) {
        let mut out = Self::new(self.arity);
        for i in 0..self.len() {
            if keep[i] {
                let vals = fresh[i].clone().unwrap_or_else(|| self.values_of(i).to_vec());
                out.push(self.addrs[i], self.variants[i], &vals);
            }
        }
        *self = out;
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ScanStats {
    pub regions: usize,
    pub jobs: usize,
    pub bytes_scanned: u64,
    pub bytes_unreadable: u64,
    pub millis: u128,
    pub aborted: bool,
}

const CHUNK: usize = 4 << 20;

/// Parallel scan of `regions`. Fails if more than `max_hits` candidates are found.
pub fn scan_memory<R: ReadMem>(
    mem: &R,
    regions: &[MemRegion],
    m: &Matcher,
    max_hits: usize,
) -> Result<(CandidateSet, ScanStats)> {
    let t0 = Instant::now();
    let jobs = chunk_jobs(regions, CHUNK, m.span() + m.align);
    let count = AtomicUsize::new(0);
    let abort = AtomicBool::new(false);
    let scanned = AtomicUsize::new(0);
    let unreadable = AtomicUsize::new(0);
    let per_job: Vec<Vec<(Hit, Vec<f64>)>> = jobs
        .par_iter()
        .map_init(Vec::new, |buf: &mut Vec<u8>, job| {
            if abort.load(Ordering::Relaxed) {
                return Vec::new();
            }
            buf.resize(job.read_len, 0);
            let segs = read_segments(mem, job.base, &mut buf[..job.read_len]);
            let mut hits = Vec::new();
            let mut ok = 0usize;
            for &(s, e) in &segs {
                ok += e - s;
                m.scan(&buf[s..e], job.base + s, job.base + job.len, &mut hits);
            }
            scanned.fetch_add(ok.min(job.len), Ordering::Relaxed);
            unreadable.fetch_add(job.read_len.saturating_sub(ok), Ordering::Relaxed);
            if count.fetch_add(hits.len(), Ordering::Relaxed) + hits.len() > max_hits {
                abort.store(true, Ordering::Relaxed);
            }
            hits.into_iter()
                .filter_map(|h| {
                    let off = h.addr - job.base;
                    m.values_at(&buf[..job.read_len], off).map(|v| (h, v))
                })
                .collect()
        })
        .collect();
    let stats = ScanStats {
        regions: regions.len(),
        jobs: jobs.len(),
        bytes_scanned: scanned.load(Ordering::Relaxed) as u64,
        bytes_unreadable: unreadable.load(Ordering::Relaxed) as u64,
        millis: t0.elapsed().as_millis(),
        aborted: abort.load(Ordering::Relaxed),
    };
    if stats.aborted {
        bail!("more than {max_hits} candidates; tighten --tol, use a less common target, or raise --max-candidates");
    }
    let mut set = CandidateSet::new(m.arity());
    for (h, v) in per_job.into_iter().flatten() {
        set.push(h.addr as u64, h.variant, &v);
    }
    set.sort();
    Ok((set, stats))
}

/// Re-reads every candidate (grouped into ≤64 KiB windows, in parallel). `None` = unreadable.
pub fn refresh<R: ReadMem>(mem: &R, set: &CandidateSet, ty: NumType, stride: usize) -> Vec<Option<Vec<f64>>> {
    const WINDOW: u64 = 64 << 10;
    let span = (((set.arity.max(1) - 1) * stride.max(1) + 1) * ty.size()) as u64;
    let mut groups: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < set.len() {
        let start = set.addrs[i];
        let mut j = i + 1;
        while j < set.len() && set.addrs[j] + span - start <= WINDOW {
            j += 1;
        }
        groups.push((i, j));
        i = j;
    }
    groups
        .par_iter()
        .map_init(Vec::new, |buf: &mut Vec<u8>, &(i, j)| {
            let start = set.addrs[i] as usize;
            let len = (set.addrs[j - 1] + span) as usize - start;
            buf.resize(len, 0);
            let segs = read_segments(mem, start, &mut buf[..len]);
            (i..j)
                .map(|c| {
                    let off = set.addrs[c] as usize - start;
                    let end = off + span as usize;
                    segs.iter().any(|&(s, e)| off >= s && end <= e).then(|| {
                        (0..set.arity)
                            .map(|k| ty.read(buf, off + k * stride.max(1) * ty.size()).unwrap_or(f64::NAN))
                            .collect()
                    })
                })
                .collect::<Vec<_>>()
        })
        .flatten()
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum NextMode {
    /// Still matches the fresh shm value under its recorded variant.
    Match,
    Changed,
    Unchanged,
    /// First memory component increased.
    Increased,
    Decreased,
}

/// Whether a candidate survives a `scan-next` step.
pub fn keep(mode: NextMode, m: &Matcher, variant: usize, prev: &[f64], cur: Option<&[f64]>, eps: f64) -> bool {
    let Some(cur) = cur else {
        return false;
    };
    if cur.iter().any(|v| !v.is_finite()) {
        return false;
    }
    let diff = |k: usize| cur[k] - prev[k];
    match mode {
        NextMode::Match => m.check(variant, cur),
        NextMode::Changed => (0..cur.len()).any(|k| diff(k).abs() > eps),
        NextMode::Unchanged => (0..cur.len()).all(|k| diff(k).abs() <= eps),
        NextMode::Increased => diff(0) > eps,
        NextMode::Decreased => diff(0) < -eps,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::{FakeMem, RegionKind};
    use crate::variants::{scalar_variants, vec3_variants};

    fn f64s(v: &[f64]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }
    fn f32s(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    #[test]
    fn finds_permuted_scaled_vec3_f64() {
        // shm pos (x, y-up, z) = (120.5, 3.25, -840.75); game stores UE cm (z, x, y) = 100*(z, x, y)
        let shm = [120.5, 3.25, -840.75];
        let mut buf = vec![0u8; 0x200];
        buf[0x48..0x60].copy_from_slice(&f64s(&[-84075.0, 12050.0, 325.0]));
        // decoy: right values, wrong order that no variant maps (x appears twice)
        buf[0x100..0x118].copy_from_slice(&f64s(&[12050.0, 12050.0, 325.0]));
        let m = Matcher::new(NumType::F64, 1, 4, 0.05, vec3_variants(&[1.0, 100.0], true), &shm);
        assert!(m.scan_ready().is_ok());
        let mut hits = Vec::new();
        m.scan(&buf, 0x1000, usize::MAX, &mut hits);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].addr, 0x1048);
        let v = &m.variants[hits[0].variant as usize];
        assert_eq!(v.describe(), "mem = 100 * (+z, +x, +y)");
        let vals = m.values_at(&buf, 0x48).unwrap();
        assert!(m.error(hits[0].variant as usize, &vals) < 1e-9);
    }

    #[test]
    fn finds_f32_with_tolerance_stride_and_sign_flip() {
        let shm = [10.0, -2.0, 55.5];
        let mut buf = vec![0u8; 0x100];
        // column-major 4x4 translation: components 4 floats apart, axis z negated
        let vals = [10.01f32, -2.0, -55.49];
        for (k, v) in vals.iter().enumerate() {
            let at = 0x20 + k * 16;
            buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
        }
        let m = Matcher::new(NumType::F32, 4, 4, 0.05, vec3_variants(&[1.0], true), &shm);
        let mut hits = Vec::new();
        m.scan(&buf, 0, usize::MAX, &mut hits);
        assert_eq!(hits.len(), 1);
        assert_eq!(m.variants[hits[0].variant as usize].describe(), "mem = 1 * (+x, +y, -z)");
        // owned_end excludes hits beyond the job boundary
        let mut hits = Vec::new();
        m.scan(&buf, 0, 0x20, &mut hits);
        assert!(hits.is_empty());
    }

    #[test]
    fn scalar_rpm_in_rad_per_s() {
        let rpm = 4500.0;
        let rad = rpm * std::f64::consts::TAU / 60.0;
        let mut buf = vec![0u8; 0x40];
        buf[0x10..0x14].copy_from_slice(&f32s(&[rad as f32]));
        buf[0x20..0x24].copy_from_slice(&f32s(&[4512.0]));
        let m = Matcher::new(
            NumType::F32,
            1,
            4,
            30.0,
            scalar_variants(&[1.0, std::f64::consts::TAU / 60.0], false),
            &[rpm],
        );
        let mut hits = Vec::new();
        m.scan(&buf, 0x5000, usize::MAX, &mut hits);
        let found: Vec<(usize, u16)> = hits.iter().map(|h| (h.addr, h.variant)).collect();
        assert_eq!(found, vec![(0x5010, 1), (0x5020, 0)]);
    }

    #[test]
    fn near_zero_target_refused_for_scans_but_checkable() {
        let m = Matcher::new(NumType::F32, 1, 4, 0.3, vec3_variants(&[1.0], true), &[0.1, 0.0, -0.2]);
        assert!(m.scan_ready().is_err());
        assert!(m.check(0, &[0.0, 0.0, 0.0]));
        assert!(!m.check(0, &[1.0, 0.0, 0.0]));
    }

    #[test]
    fn next_modes() {
        let m = Matcher::new(NumType::F32, 1, 4, 0.1, scalar_variants(&[1.0], false), &[5.0]);
        assert!(keep(NextMode::Match, &m, 0, &[0.0], Some(&[5.05]), 0.0));
        assert!(!keep(NextMode::Match, &m, 0, &[0.0], Some(&[5.2]), 0.0));
        assert!(!keep(NextMode::Match, &m, 0, &[0.0], None, 0.0));
        assert!(keep(NextMode::Changed, &m, 0, &[1.0], Some(&[1.5]), 1e-6));
        assert!(!keep(NextMode::Changed, &m, 0, &[1.0], Some(&[1.0]), 1e-6));
        assert!(keep(NextMode::Unchanged, &m, 0, &[1.0], Some(&[1.0]), 1e-6));
        assert!(keep(NextMode::Increased, &m, 0, &[1.0], Some(&[2.0]), 1e-6));
        assert!(!keep(NextMode::Increased, &m, 0, &[1.0], Some(&[0.5]), 1e-6));
        assert!(keep(NextMode::Decreased, &m, 0, &[1.0], Some(&[0.5]), 1e-6));
        assert!(!keep(NextMode::Unchanged, &m, 0, &[1.0], Some(&[f64::NAN]), 1e-6));
    }

    #[test]
    fn parallel_scan_refresh_and_persist() {
        let mut mem = FakeMem::new();
        // 9 MiB region so the scan spans several 4 MiB jobs; place a hit across a job boundary.
        mem.add(0x1000_0000, vec![0u8; 9 << 20], RegionKind::Private, true);
        mem.add(0x2000_0000, vec![0u8; 0x1000], RegionKind::Private, true);
        let boundary = 0x1000_0000 + (4 << 20) - 8;
        mem.put(boundary, &f64s(&[1.0, 2.0, 3.0]));
        mem.put(0x2000_0100, &f64s(&[100.0, 200.0, 300.0]));
        let regions = mem.regions();
        let m = Matcher::new(NumType::F64, 1, 8, 0.01, vec3_variants(&[1.0, 100.0], false), &[1.0, 2.0, 3.0]);
        let (set, stats) = scan_memory(&mem, &regions, &m, 100).unwrap();
        assert_eq!(set.addrs, vec![boundary as u64, 0x2000_0100]);
        assert_eq!(set.variants, vec![0, 1]);
        assert_eq!(set.values_of(1), &[100.0, 200.0, 300.0]);
        assert_eq!(stats.jobs, 4);
        assert!(scan_memory(&mem, &regions, &m, 1).is_err(), "cap enforced");

        mem.put(0x2000_0108, &f64s(&[250.0]));
        let fresh = refresh(&mem, &set, NumType::F64, 1);
        assert_eq!(fresh[1].as_deref(), Some(&[100.0, 250.0, 300.0][..]));
        let keepv: Vec<bool> = (0..set.len())
            .map(|i| keep(NextMode::Changed, &m, set.variants[i] as usize, set.values_of(i), fresh[i].as_deref(), 1e-9))
            .collect();
        let mut s2 = set.clone();
        s2.retain_update(&keepv, &fresh);
        assert_eq!(s2.addrs, vec![0x2000_0100]);
        assert_eq!(s2.values_of(0), &[100.0, 250.0, 300.0]);

        let dir = std::env::temp_dir().join(format!("acr_probe_cand_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("c.bin");
        set.save(&p).unwrap();
        assert_eq!(CandidateSet::load(&p).unwrap(), set);
        std::fs::write(&p, b"junk").unwrap();
        assert!(CandidateSet::load(&p).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refresh_marks_unreadable() {
        let mut mem = FakeMem::new();
        mem.add(0x3000_0000, vec![0u8; 0x1000], RegionKind::Private, true);
        let mut set = CandidateSet::new(1);
        set.push(0x3000_0010, 0, &[0.0]);
        set.push(0x5000_0000, 0, &[0.0]);
        let fresh = refresh(&mem, &set, NumType::F32, 1);
        assert_eq!(fresh[0].as_deref(), Some(&[0.0][..]));
        assert_eq!(fresh[1], None);
    }

    #[cfg(windows)]
    #[test]
    fn scans_own_process_memory() {
        use crate::mem::{ReadMem, RegionFilter};
        let shm = [1234.5678, -42.125, 987.0625];
        // UE-style storage: cm, (x, z, y)
        let stored: Box<[f64; 3]> = Box::new([123456.78, 98706.25, -4212.5]);
        let addr = stored.as_ptr() as usize;
        let me = crate::process::RemoteProcess::open(std::process::id()).unwrap();
        let regions = RegionFilter::DATA.apply(&me.regions());
        let m = Matcher::new(NumType::F64, 1, 8, 0.01, vec3_variants(&[1.0, 100.0], true), &shm);
        let (set, _) = scan_memory(&me, &regions, &m, 10_000).unwrap();
        let i = set.addrs.iter().position(|&a| a == addr as u64).expect("own value found");
        assert_eq!(m.variants[set.variants[i] as usize].describe(), "mem = 100 * (+x, +z, +y)");
        std::hint::black_box(&stored);
    }
}
