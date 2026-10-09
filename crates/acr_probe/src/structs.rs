//! `struct-dump` (hex + float views) and `struct-correlate` (find velocity / orientation /
//! rpm / wheel / gear fields near a known position by fitting shm samples over time).

use crate::oracle::ShmSample;
use crate::process::{symbolize, ModuleEntry};
use crate::scan::NumType;
use crate::variants::{scalar_variants, vec3_variants, Variant};
use serde::Serialize;

// ---- struct-dump ---------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct DumpRow {
    pub addr: String,
    pub offset: i64,
    pub hex: String,
    pub ascii: String,
    pub f32: Vec<f64>,
    pub f64: Vec<f64>,
    /// Per qword: `module+rva` or `heap` for plausible pointers.
    pub ptr: Vec<Option<String>>,
}

pub fn dump_rows(buf: &[u8], base: usize, origin: usize, modules: &[ModuleEntry]) -> Vec<DumpRow> {
    buf.chunks(16)
        .enumerate()
        .map(|(i, row)| {
            let addr = base + i * 16;
            let hex = row.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ");
            let ascii = row.iter().map(|&b| if (0x20..0x7F).contains(&b) { b as char } else { '.' }).collect();
            let f32 = row.as_chunks::<4>().0.iter().map(|c| f64::from(f32::from_le_bytes(*c))).collect();
            let qwords: Vec<u64> = row.as_chunks::<8>().0.iter().map(|c| u64::from_le_bytes(*c)).collect();
            let f64 = qwords.iter().map(|&q| f64::from_bits(q)).collect();
            let ptr = qwords
                .iter()
                .map(|&q| {
                    let p = q as usize;
                    symbolize(modules, p).or_else(|| acr_ue::mem::is_plausible_ptr(p).then(|| "heap?".into()))
                })
                .collect();
            DumpRow { addr: format!("{addr:#x}"), offset: addr as i64 - origin as i64, hex, ascii, f32, f64, ptr }
        })
        .collect()
}

pub fn fmt_num(v: f64) -> String {
    if !v.is_finite() {
        return format!("{v}");
    }
    let a = v.abs();
    if a != 0.0 && !(1e-4..1e9).contains(&a) {
        "~".into()
    } else {
        format!("{v:.4}")
    }
}

pub fn print_rows(rows: &[DumpRow]) {
    for r in rows {
        let f32s: Vec<String> = r.f32.iter().map(|v| fmt_num(*v)).collect();
        let f64s: Vec<String> = r.f64.iter().map(|v| fmt_num(*v)).collect();
        let ptrs: Vec<String> = r.ptr.iter().flatten().cloned().collect();
        let sign = if r.offset < 0 { "-" } else { "+" };
        println!(
            "{sign}{:#06x} {:>14} {:<47} {:<16} f32[{}] f64[{}]{}",
            r.offset.unsigned_abs(),
            r.addr,
            r.hex,
            r.ascii,
            f32s.join(" "),
            f64s.join(" "),
            if ptrs.is_empty() { String::new() } else { format!(" ptr[{}]", ptrs.join(" ")) }
        );
    }
}

// ---- struct-correlate ----------------------------------------------------------------

/// One sampled window: shm truth plus the bytes of `[wbase, wbase + len)` at the same time.
pub struct Sample {
    pub shm: ShmSample,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FieldHit {
    pub field: String,
    pub addr: usize,
    pub offset: i64,
    pub value_type: String,
    pub detail: String,
    /// Fit error in shm units (lower is better); for orientation 1 - |velocity alignment|.
    pub error: f64,
    /// Same axis permutation/signs as the position field (the sim keeps one frame).
    pub same_frame: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Correlation {
    pub base: usize,
    pub pos_addr: usize,
    pub pos_variant: String,
    pub value_type: String,
    pub hits: Vec<FieldHit>,
    pub notes: Vec<String>,
    pub proposal_toml: String,
}

pub struct CorrelateParams {
    pub wbase: usize,
    pub pos_addr: usize,
    pub base: usize,
    pub ty: NumType,
    pub pos_variant: Variant,
    pub vel_tol: f64,
    pub ang_tol: f64,
    pub max_hits_per_field: usize,
}

fn read_vals(s: &Sample, off: usize, ty: NumType, n: usize, stride: usize) -> Option<Vec<f64>> {
    (0..n).map(|k| ty.read(&s.bytes, off + k * stride * ty.size())).collect()
}

fn max_err(v: &Variant, mem: &[f64], shm: &[f64]) -> f64 {
    v.apply(shm).iter().zip(mem).fold(0.0f64, |m, (e, x)| m.max((e - x).abs() / v.scale))
}

/// Best-fitting vec3 fields for `extract(shm)` across all samples. With `world_frame`, fits in
/// the position's axis frame rank first; local-frame quantities rank by error alone.
#[allow(clippy::too_many_arguments)]
fn fit_vec3(
    samples: &[Sample],
    p: &CorrelateParams,
    field: &str,
    variants: &[Variant],
    tol: f64,
    world_frame: bool,
    extract: impl Fn(&ShmSample) -> [f64; 3],
) -> Vec<FieldHit> {
    let len = samples[0].bytes.len();
    let mut hits = Vec::new();
    for ty in [NumType::F32, NumType::F64] {
        let span = 3 * ty.size();
        for off in (0..len.saturating_sub(span - 1)).step_by(4) {
            for v in variants {
                let mut worst = 0.0f64;
                for s in samples {
                    let Some(mem) = read_vals(s, off, ty, 3, 1) else {
                        worst = f64::INFINITY;
                        break;
                    };
                    worst = worst.max(max_err(v, &mem, &extract(&s.shm)));
                    if worst.is_nan() || worst > tol {
                        break;
                    }
                }
                if worst <= tol {
                    let addr = p.wbase + off;
                    hits.push(FieldHit {
                        field: field.into(),
                        addr,
                        offset: addr as i64 - p.base as i64,
                        value_type: ty.name().into(),
                        detail: v.describe(),
                        error: worst,
                        same_frame: !world_frame || (v.perm == p.pos_variant.perm && v.signs == p.pos_variant.signs),
                    });
                }
            }
        }
    }
    rank(hits, p)
}

fn fit_scalar(
    samples: &[Sample],
    p: &CorrelateParams,
    field: &str,
    variants: &[Variant],
    tol: impl Fn(f64) -> f64,
    extract: impl Fn(&ShmSample) -> f64,
) -> Vec<FieldHit> {
    let len = samples[0].bytes.len();
    let mut hits = Vec::new();
    for ty in [NumType::F32, NumType::F64] {
        for off in (0..len.saturating_sub(ty.size() - 1)).step_by(4) {
            for v in variants {
                let mut worst = 0.0f64;
                for s in samples {
                    let truth = extract(&s.shm);
                    let Some(mem) = ty.read(&s.bytes, off) else {
                        worst = f64::INFINITY;
                        break;
                    };
                    let e = (mem - v.apply(&[truth])[0]).abs() / v.scale;
                    worst = worst.max(e);
                    if e.is_nan() || e > tol(truth) {
                        worst = f64::INFINITY;
                        break;
                    }
                }
                if worst.is_finite() {
                    let addr = p.wbase + off;
                    hits.push(FieldHit {
                        field: field.into(),
                        addr,
                        offset: addr as i64 - p.base as i64,
                        value_type: ty.name().into(),
                        detail: v.describe(),
                        error: worst,
                        same_frame: true,
                    });
                }
            }
        }
    }
    rank(hits, p)
}

/// Same frame as the position first, then lowest error, then closest to the position field;
/// keep the best few. (A near-zero component lets a shifted window fit in another frame.)
fn rank(mut hits: Vec<FieldHit>, p: &CorrelateParams) -> Vec<FieldHit> {
    hits.sort_by(|a, b| {
        b.same_frame
            .cmp(&a.same_frame)
            .then(a.error.total_cmp(&b.error))
            .then((a.addr as i64 - p.pos_addr as i64).abs().cmp(&(b.addr as i64 - p.pos_addr as i64).abs()))
    });
    hits.truncate(p.max_hits_per_field);
    hits
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn norm(a: [f64; 3]) -> f64 {
    dot(a, a).sqrt()
}

fn det3(m: &[f64; 9]) -> f64 {
    m[0] * (m[4] * m[8] - m[5] * m[7]) - m[1] * (m[3] * m[8] - m[5] * m[6]) + m[2] * (m[3] * m[7] - m[4] * m[6])
}

pub fn is_orthonormal(m: &[f64; 9], eps: f64) -> bool {
    let r = |i: usize| [m[3 * i], m[3 * i + 1], m[3 * i + 2]];
    (0..3).all(|i| (norm(r(i)) - 1.0).abs() < eps)
        && dot(r(0), r(1)).abs() < eps
        && dot(r(0), r(2)).abs() < eps
        && dot(r(1), r(2)).abs() < eps
        && (det3(m).abs() - 1.0).abs() < eps
}

/// Rows and columns of a 3x3 as six candidate basis vectors, labelled.
fn basis(m: &[f64; 9]) -> [(&'static str, [f64; 3]); 6] {
    [
        ("row0", [m[0], m[1], m[2]]),
        ("row1", [m[3], m[4], m[5]]),
        ("row2", [m[6], m[7], m[8]]),
        ("col0", [m[0], m[3], m[6]]),
        ("col1", [m[1], m[4], m[7]]),
        ("col2", [m[2], m[5], m[8]]),
    ]
}

pub fn quat_to_matrix(x: f64, y: f64, z: f64, w: f64) -> [f64; 9] {
    [
        1.0 - 2.0 * (y * y + z * z),
        2.0 * (x * y - z * w),
        2.0 * (x * z + y * w),
        2.0 * (x * y + z * w),
        1.0 - 2.0 * (x * x + z * z),
        2.0 * (y * z - x * w),
        2.0 * (x * z - y * w),
        2.0 * (y * z + x * w),
        1.0 - 2.0 * (x * x + y * y),
    ]
}

/// Mean alignment of each basis vector with the (memory-frame) velocity direction over the
/// samples where the car moves. Returns the best `(label, mean dot)`.
fn best_alignment(mats: &[[f64; 9]], vel_dirs: &[Option<[f64; 3]>]) -> Option<(&'static str, f64)> {
    let mut sums = [0.0f64; 6];
    let mut n = 0usize;
    for (m, v) in mats.iter().zip(vel_dirs) {
        let Some(v) = v else { continue };
        for (i, (_, b)) in basis(m).iter().enumerate() {
            sums[i] += dot(*b, *v);
        }
        n += 1;
    }
    if n == 0 {
        return None;
    }
    let labels = basis(&mats[0]).map(|(l, _)| l);
    (0..6).map(|i| (labels[i], sums[i] / n as f64)).max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
}

const MIN_SPEED: f64 = 2.0;

fn orientation_hits(samples: &[Sample], p: &CorrelateParams, notes: &mut Vec<String>) -> Vec<FieldHit> {
    let dir_of = |s: &ShmSample| {
        let v = p.pos_variant.apply(&s.vel);
        let v = [v[0], v[1], v[2]];
        let n = norm(v);
        (n / p.pos_variant.scale >= MIN_SPEED).then(|| [v[0] / n, v[1] / n, v[2] / n])
    };
    let dirs: Vec<Option<[f64; 3]>> = samples.iter().map(|s| dir_of(&s.shm)).collect();
    let moving = dirs.iter().filter(|d| d.is_some()).count();
    if moving == 0 {
        notes.push(format!(
            "orientation: car never above {MIN_SPEED} m/s, so candidates are not checked against the driving direction (low confidence)"
        ));
    }
    let len = samples[0].bytes.len();
    let mut hits = Vec::new();
    for ty in [NumType::F32, NumType::F64] {
        for stride in [3usize, 4] {
            let span = (2 * stride + 3) * ty.size();
            for off in (0..len.saturating_sub(span - 1)).step_by(4) {
                let mut mats = Vec::with_capacity(samples.len());
                for s in samples {
                    let mut m = [0.0; 9];
                    let mut ok = true;
                    for (i, slot) in m.iter_mut().enumerate() {
                        match ty.read(&s.bytes, off + ((i / 3) * stride + i % 3) * ty.size()) {
                            Some(v) => *slot = v,
                            None => ok = false,
                        }
                    }
                    if !ok || !is_orthonormal(&m, 2e-3) {
                        break;
                    }
                    mats.push(m);
                }
                if mats.len() != samples.len() {
                    continue;
                }
                let ident = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
                if mats.iter().all(|m| m == &ident) {
                    continue;
                }
                let align = best_alignment(&mats, &dirs);
                if moving > 0 && !align.is_some_and(|(_, a)| a.abs() > 0.9) {
                    continue;
                }
                let addr = p.wbase + off;
                let (label, a) = align.unwrap_or(("?", 0.0));
                hits.push(FieldHit {
                    field: "rotation_matrix".into(),
                    addr,
                    offset: addr as i64 - p.base as i64,
                    value_type: ty.name().into(),
                    detail: format!(
                        "row_stride {stride}; forward = {}{label} (alignment {a:.3})",
                        if a < 0.0 { "-" } else { "+" }
                    ),
                    error: 1.0 - a.abs(),
                    same_frame: true,
                });
            }
        }
        // Quaternions, xyzw and wxyz.
        for off in (0..len.saturating_sub(4 * ty.size() - 1)).step_by(4) {
            for order in ["xyzw", "wxyz"] {
                let mut mats = Vec::with_capacity(samples.len());
                for s in samples {
                    let Some(q) = read_vals(s, off, ty, 4, 1) else { break };
                    let n = (q.iter().map(|v| v * v).sum::<f64>()).sqrt();
                    if (n - 1.0).abs() > 2e-3 || q.iter().filter(|v| v.abs() < 1e-9).count() >= 3 {
                        break;
                    }
                    let (x, y, z, w) =
                        if order == "xyzw" { (q[0], q[1], q[2], q[3]) } else { (q[1], q[2], q[3], q[0]) };
                    mats.push(quat_to_matrix(x, y, z, w));
                }
                if mats.len() != samples.len() {
                    continue;
                }
                let align = best_alignment(&mats, &dirs);
                if moving > 0 && !align.is_some_and(|(_, a)| a.abs() > 0.9) {
                    continue;
                }
                let addr = p.wbase + off;
                let (label, a) = align.unwrap_or(("?", 0.0));
                hits.push(FieldHit {
                    field: "orientation_quat".into(),
                    addr,
                    offset: addr as i64 - p.base as i64,
                    value_type: ty.name().into(),
                    detail: format!(
                        "order {order}; forward = {}{label} (alignment {a:.3})",
                        if a < 0.0 { "-" } else { "+" }
                    ),
                    error: 1.0 - a.abs(),
                    same_frame: true,
                });
            }
        }
    }
    hits
}

fn gear_hits(samples: &[Sample], p: &CorrelateParams, notes: &mut Vec<String>) -> Vec<FieldHit> {
    let mut distinct: Vec<i32> = samples.iter().map(|s| s.shm.gear).collect();
    distinct.sort_unstable();
    distinct.dedup();
    if distinct.len() < 2 {
        notes.push("gear: constant during sampling; shift up/down while correlating to locate it".into());
        return Vec::new();
    }
    let len = samples[0].bytes.len();
    let mut hits = Vec::new();
    for off in (0..len.saturating_sub(3)).step_by(4) {
        for (label, neutral, f) in [
            ("raw shm convention (0=R 1=N 2=1st)", 1, (|s: &ShmSample| s.gear_raw) as fn(&ShmSample) -> i32),
            ("signed (-1=R 0=N 1=1st)", 0, |s: &ShmSample| s.gear),
        ] {
            let ok = samples.iter().all(|s| {
                s.bytes.get(off..off + 4).map(|b| i32::from_le_bytes(b.try_into().unwrap())) == Some(f(&s.shm))
            });
            if ok {
                let addr = p.wbase + off;
                hits.push(FieldHit {
                    field: "gear".into(),
                    addr,
                    offset: addr as i64 - p.base as i64,
                    value_type: "i32".into(),
                    detail: format!("{label}; gear_neutral_value = {neutral}"),
                    error: 0.0,
                    same_frame: true,
                });
            }
        }
    }
    rank(hits, p)
}

/// Fits every known field around a position address. `samples` must be non-empty and all
/// windows the same length.
pub fn correlate(samples: &[Sample], p: &CorrelateParams) -> Correlation {
    let mut notes = Vec::new();
    let mut hits = Vec::new();
    let max_speed = samples.iter().map(|s| s.shm.speed()).fold(0.0, f64::max);
    let mut scales = vec![p.pos_variant.scale, 1.0, 100.0];
    scales.dedup();
    scales.sort_by(f64::total_cmp);
    scales.dedup();
    let v3 = vec3_variants(&scales, true);
    if max_speed < 1.0 {
        notes.push("velocity: car below 1 m/s in every sample; drive slowly to locate velocity fields".into());
    } else {
        hits.extend(fit_vec3(samples, p, "linear_velocity", &v3, p.vel_tol, true, |s| s.vel));
        hits.extend(fit_vec3(samples, p, "local_velocity", &v3, p.vel_tol, false, |s| s.local_vel));
    }
    let max_w = samples.iter().map(|s| norm(s.shm.local_ang_vel)).fold(0.0, f64::max);
    if max_w < 0.05 {
        notes.push("angular_velocity: no rotation during sampling; steer while correlating to locate it".into());
    } else {
        let av = vec3_variants(&[1.0], true);
        hits.extend(fit_vec3(samples, p, "angular_velocity (local frame)", &av, p.ang_tol, false, |s| s.local_ang_vel));
    }
    hits.extend(orientation_hits(samples, p, &mut notes));
    let max_rpm = samples.iter().map(|s| s.shm.rpm).fold(0.0, f64::max);
    if max_rpm < 500.0 {
        notes.push("engine_rpm: engine not running / rpm below 500".into());
    } else {
        let rv = scalar_variants(&[1.0, std::f64::consts::TAU / 60.0], false);
        hits.extend(fit_scalar(samples, p, "engine_rpm", &rv, |t| (t.abs() * 0.03).max(50.0), |s| s.rpm));
    }
    for w in 0..4 {
        let max = samples.iter().map(|s| s.shm.wheel_speed[w].abs()).fold(0.0, f64::max);
        if max < 2.0 {
            if w == 0 {
                notes.push("wheel_angular_speed: wheels not spinning above 2 rad/s".into());
            }
            continue;
        }
        let wv = scalar_variants(&[1.0], true);
        hits.extend(fit_scalar(samples, p, &format!("wheel_angular_speed[{w}]"), &wv, |_| 0.5, |s| s.wheel_speed[w]));
    }
    hits.extend(gear_hits(samples, p, &mut notes));
    let proposal_toml = proposal(p, &hits, &mut notes);
    Correlation {
        base: p.base,
        pos_addr: p.pos_addr,
        pos_variant: p.pos_variant.describe(),
        value_type: p.ty.name().into(),
        hits,
        notes,
        proposal_toml,
    }
}

fn best<'a>(hits: &'a [FieldHit], field: &str) -> Option<&'a FieldHit> {
    hits.iter().find(|h| h.field == field)
}

fn off_str(h: &FieldHit, notes: &mut Vec<String>) -> String {
    if h.offset < 0 {
        notes.push(format!(
            "{} is {:#x} bytes below the base; pass a lower --base (the struct start found by pointer-scan)",
            h.field, -h.offset
        ));
        format!("-{:#x}", -h.offset)
    } else {
        format!("{:#x}", h.offset)
    }
}

fn proposal(p: &CorrelateParams, hits: &[FieldHit], notes: &mut Vec<String>) -> String {
    let mut t = String::from("[raw_offsets.sim_car]\n");
    t.push_str(&format!(
        "# proposed by acr-probe struct-correlate: struct base {:#x}, position at {:#x} ({})\n",
        p.base,
        p.pos_addr,
        p.pos_variant.describe()
    ));
    t.push_str(&format!("value_type = \"{}\"\n", p.ty.name()));
    t.push_str(&format!("length_scale = {:.1}\n", p.pos_variant.scale));
    let pos_off = p.pos_addr as i64 - p.base as i64;
    t.push_str(&format!("position = {:#x}\n", pos_off.max(0)));
    let typed = |h: &FieldHit, notes: &mut Vec<String>| {
        if h.value_type != p.ty.name() && h.value_type != "i32" {
            notes.push(format!(
                "{} is {} but value_type is {}; sim_car reads all floats with one type",
                h.field,
                h.value_type,
                p.ty.name()
            ));
        }
    };
    let rot = best(hits, "rotation_matrix");
    let quat = best(hits, "orientation_quat");
    match (rot, quat) {
        (Some(r), _) if quat.is_none_or(|q| r.error <= q.error) => {
            typed(r, notes);
            let stride = if r.detail.contains("row_stride 4") { 4 } else { 3 };
            t.push_str(&format!("rotation_matrix = {}   # {}\n", off_str(r, notes), r.detail));
            t.push_str("rotation_matrix_layout = \"rows\"\n");
            t.push_str(&format!("rotation_matrix_row_stride = {stride}\n"));
        }
        (_, Some(q)) => {
            typed(q, notes);
            if q.detail.contains("wxyz") {
                notes.push(
                    "orientation_quat is stored w,x,y,z; sim_car expects x,y,z,w (use the matrix or add support)"
                        .into(),
                );
            }
            t.push_str(&format!("orientation_quat = {}   # {}\n", off_str(q, notes), q.detail));
        }
        _ => t.push_str("# rotation: not found in window\n"),
    }
    for (field, key) in [("linear_velocity", "linear_velocity"), ("angular_velocity (local frame)", "angular_velocity")]
    {
        match best(hits, field) {
            Some(h) => {
                typed(h, notes);
                t.push_str(&format!("{key} = {}   # {} err {:.3}\n", off_str(h, notes), h.detail, h.error));
            }
            None => t.push_str(&format!("# {key}: not found in window\n")),
        }
    }
    let wheels: Vec<Option<&FieldHit>> = (0..4).map(|w| best(hits, &format!("wheel_angular_speed[{w}]"))).collect();
    if wheels.iter().all(Option::is_some) {
        let offs: Vec<String> = wheels.iter().flatten().map(|h| off_str(h, notes)).collect();
        t.push_str(&format!("wheel_angular_speed = [{}]\n", offs.join(", ")));
        if wheels.iter().flatten().any(|h| h.detail.contains("-v")) {
            notes.push("some wheel speeds are stored negated; sim_car writes rad/s as read (same frame), so this is only informational".into());
        }
    } else {
        t.push_str("# wheel_angular_speed: not all four found in window\n");
    }
    match best(hits, "engine_rpm") {
        Some(h) => {
            typed(h, notes);
            let rad = !h.detail.starts_with("mem = 1 *");
            t.push_str(&format!("engine_rpm = {}   # {}\n", off_str(h, notes), h.detail));
            t.push_str(&format!("engine_rpm_is_rad_s = {rad}\n"));
        }
        None => t.push_str("# engine_rpm: not found in window\n"),
    }
    match best(hits, "gear") {
        Some(h) => {
            let neutral = if h.detail.contains("= 1") { 1 } else { 0 };
            t.push_str(&format!("gear = {}   # {}\n", off_str(h, notes), h.detail));
            t.push_str(&format!("gear_neutral_value = {neutral}\n"));
        }
        None => t.push_str("# gear: not found (shift during sampling)\n"),
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(buf: &mut [u8], off: usize, vals: &[f64]) {
        for (i, v) in vals.iter().enumerate() {
            buf[off + i * 8..off + i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }
    }

    /// Synthetic car struct: UE-style cm, Z-up. mem = 100 * (z, x, y) for pos/vel.
    fn synth(t: f64) -> Sample {
        let yaw = 0.3 + 0.2 * t;
        let speed = 15.0;
        // shm frame: Y up, heading in XZ plane; forward = (sin yaw, 0, cos yaw)
        let fwd = [yaw.sin(), 0.0, yaw.cos()];
        let shm = ShmSample {
            pos: [100.0 + t * fwd[0] * speed, 5.0, -50.0 + t * fwd[2] * speed],
            vel: [fwd[0] * speed, 0.0, fwd[2] * speed],
            local_vel: [0.0, 0.0, speed],
            local_ang_vel: [0.02 + 0.01 * t, 0.2, -0.03],
            rpm: 4000.0 + 500.0 * t,
            gear: if t < 0.5 { 2 } else { 3 },
            gear_raw: if t < 0.5 { 3 } else { 4 },
            wheel_speed: [50.0 + t, 50.5 + t, 51.0 + t, 51.5 + t],
            ..Default::default()
        };
        let to_mem = |v: [f64; 3]| [100.0 * v[2], 100.0 * v[0], 100.0 * v[1]];
        let mut b = vec![0u8; 0x400];
        put(&mut b, 0x100, &to_mem(shm.pos));
        // rotation 3x3 rows (f64), row0 = forward in memory frame (z,x,y) -> (fwd.z, fwd.x, 0)
        let f = [fwd[2], fwd[0], 0.0];
        let up = [0.0, 0.0, 1.0];
        let right = [up[1] * f[2] - up[2] * f[1], up[2] * f[0] - up[0] * f[2], up[0] * f[1] - up[1] * f[0]];
        put(&mut b, 0x140, &[f[0], f[1], f[2], right[0], right[1], right[2], up[0], up[1], up[2]]);
        put(&mut b, 0x200, &to_mem(shm.vel));
        put(&mut b, 0x220, &shm.local_ang_vel);
        b[0x240..0x244].copy_from_slice(&((shm.rpm * std::f64::consts::TAU / 60.0) as f32).to_le_bytes());
        for w in 0..4 {
            put(&mut b, 0x280 + w * 8, &[shm.wheel_speed[w]]);
        }
        b[0x2C0..0x2C4].copy_from_slice(&shm.gear_raw.to_le_bytes());
        Sample { shm, bytes: b }
    }

    #[test]
    fn correlates_synthetic_struct() {
        let samples: Vec<Sample> = (0..10).map(|i| synth(i as f64 * 0.1)).collect();
        let wbase = 0x7000_0000usize;
        let p = CorrelateParams {
            wbase,
            pos_addr: wbase + 0x100,
            base: wbase + 0x100,
            ty: NumType::F64,
            pos_variant: Variant { perm: vec![2, 0, 1], signs: vec![1, 1, 1], scale: 100.0 },
            vel_tol: 0.3,
            ang_tol: 0.05,
            max_hits_per_field: 4,
        };
        let c = correlate(&samples, &p);
        let off = |f: &str| best(&c.hits, f).map(|h| h.offset);
        assert_eq!(off("linear_velocity"), Some(0x100));
        assert_eq!(off("angular_velocity (local frame)"), Some(0x120));
        assert_eq!(off("rotation_matrix"), Some(0x40));
        assert_eq!(off("engine_rpm"), Some(0x140));
        assert_eq!(off("wheel_angular_speed[3]"), Some(0x198));
        assert_eq!(off("gear"), Some(0x1C0));
        let r = best(&c.hits, "rotation_matrix").unwrap();
        assert!(r.detail.contains("+row0"), "{}", r.detail);
        let t = &c.proposal_toml;
        assert!(t.contains("position = 0x0"), "{t}");
        assert!(t.contains("linear_velocity = 0x100"), "{t}");
        assert!(t.contains("rotation_matrix = 0x40"), "{t}");
        assert!(t.contains("rotation_matrix_row_stride = 3"), "{t}");
        assert!(t.contains("wheel_angular_speed = [0x180, 0x188, 0x190, 0x198]"), "{t}");
        assert!(t.contains("engine_rpm_is_rad_s = true"), "{t}");
        assert!(t.contains("gear_neutral_value = 1"), "{t}");
        assert!(t.contains("length_scale = 100.0"), "{t}");
        assert!(c.notes.iter().any(|n| n.contains("engine_rpm is f32")), "{:?}", c.notes);
        // the proposal must be valid TOML that acr_ue accepts once a chain is added
        let doc = format!("schema_version = 1\n[raw_offsets]\n{t}locator = \"chain\"\nchain = [0x10]\n");
        let sigs = acr_ue::Signatures::from_toml_str(&doc).unwrap();
        assert!(sigs.raw_offsets.sim_car.is_configured());
        assert_eq!(sigs.raw_offsets.sim_car.linear_velocity, Some(0x100));
    }

    #[test]
    fn stationary_car_gets_notes_not_false_hits() {
        let mut s = synth(0.0);
        s.shm.vel = [0.0; 3];
        s.shm.local_ang_vel = [0.0; 3];
        let samples = vec![s];
        let p = CorrelateParams {
            wbase: 0x1000,
            pos_addr: 0x1100,
            base: 0x1000,
            ty: NumType::F64,
            pos_variant: Variant::identity(3, 1.0),
            vel_tol: 0.3,
            ang_tol: 0.05,
            max_hits_per_field: 4,
        };
        let c = correlate(&samples, &p);
        assert!(best(&c.hits, "linear_velocity").is_none());
        assert!(c.notes.iter().any(|n| n.starts_with("velocity")));
        assert!(c.notes.iter().any(|n| n.starts_with("gear")));
    }

    #[test]
    fn orthonormal_and_quat() {
        let q = (0.3f64 / 2.0).sin();
        let w = (0.3f64 / 2.0).cos();
        let m = quat_to_matrix(0.0, 0.0, q, w);
        assert!(is_orthonormal(&m, 1e-9));
        assert!((m[0] - 0.3f64.cos()).abs() < 1e-12);
        assert!(!is_orthonormal(&[2.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0], 1e-3));
    }

    #[test]
    fn dump_rows_annotate() {
        let mods =
            vec![ModuleEntry { name: "acr.exe".into(), base: 0x1_4000_0000, size: 0x1000_0000, path: String::new() }];
        let mut buf = vec![0u8; 32];
        buf[0..8].copy_from_slice(&(0x1_4000_1234u64).to_le_bytes());
        buf[8..12].copy_from_slice(&1.5f32.to_le_bytes());
        buf[16..20].copy_from_slice(b"ABCD");
        let rows = dump_rows(&buf, 0x5000, 0x5010, &mods);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].offset, -0x10);
        assert_eq!(rows[0].ptr[0].as_deref(), Some("acr.exe+0x1234"));
        assert_eq!(rows[0].f32[2], 1.5);
        assert!(rows[1].ascii.starts_with("ABCD"));
    }
}
