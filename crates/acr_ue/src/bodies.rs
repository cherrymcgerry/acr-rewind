//! Kunos rigid-body locator (`[raw_offsets.sim_car] locator = "vtable_scan"`, re-notes §8).
//!
//! Every simulated rigid body is a pool object whose first qword is the rigid-body vtable
//! (`module_base + vtable_rva`). The locator sweeps committed private RW memory for 8-aligned
//! qwords equal to the vtable, reads each hit as a body, groups the bodies by the parent pointer
//! at `parent_offset`, re-reads them, and picks the group with a body at the shared-memory car
//! position. The parent is shared with unrelated world objects (live: 28 bodies, 6 of them
//! 20-800 m away), so the car is only the bodies of that group within `car_radius_m` of the
//! main body (and, while moving, with a matching velocity). The same code runs in-process (the
//! hook, on a background thread) and externally (`acr-probe find-car`) through [`ScanSource`].

use crate::math::{quat_from_matrix, quat_to_matrix};
use crate::mem::{is_plausible_ptr, Memory, ScanRegion, ScanSource};
use crate::sigs::{MatrixLayout, SimCarSpec, ValueType};
use glam::{DQuat, DVec3};
use std::collections::{BTreeMap, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// Bodies closer than this to each other count as duplicates (the main chassis has a twin).
pub const DUPLICATE_EPS_M: f64 = 1e-3;
/// Sanity bounds for a live body (metres, m/s, rad/s).
const MAX_COORD_M: f64 = 1e7;
const MAX_LIN_VEL: f64 = 1e4;
const MAX_ANG_VEL: f64 = 1e4;
/// Cap on |w| when widening the velocity tolerance with distance (angular velocity is only
/// "probable"; a garbage value must not admit everything).
const MAX_SPIN_RAD_S: f64 = 10.0;

/// Where the typed fields of one body live, relative to the object start.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BodyLayout {
    pub value_type: ValueType,
    /// Sim length units per metre.
    pub length_scale: f64,
    pub parent: usize,
    pub position: usize,
    pub rotation: usize,
    /// Elements per matrix row in memory.
    pub row_stride: usize,
    pub rows: bool,
    pub linear_velocity: Option<usize>,
    pub angular_velocity: Option<usize>,
    /// Bytes read per body (covers every field).
    pub span: usize,
}

impl BodyLayout {
    pub fn from_spec(sp: &SimCarSpec) -> Option<Self> {
        Some(Self {
            value_type: sp.value_type,
            length_scale: sp.length_scale,
            parent: sp.parent_offset,
            position: sp.position?,
            rotation: sp.rotation_matrix?,
            row_stride: sp.rotation_matrix_row_stride,
            rows: sp.rotation_matrix_layout == MatrixLayout::Rows,
            linear_velocity: sp.linear_velocity,
            angular_velocity: sp.angular_velocity,
            span: sp.body_span(),
        })
    }

    fn size(&self) -> usize {
        self.value_type.size()
    }

    fn get(&self, buf: &[u8], off: usize) -> Option<f64> {
        let b = buf.get(off..off + self.size())?;
        Some(match self.value_type {
            ValueType::F32 => f64::from(f32::from_le_bytes(b.try_into().ok()?)),
            ValueType::F64 => f64::from_le_bytes(b.try_into().ok()?),
        })
    }

    fn get_vec3(&self, buf: &[u8], off: usize) -> Option<DVec3> {
        let s = self.size();
        Some(DVec3::new(self.get(buf, off)?, self.get(buf, off + s)?, self.get(buf, off + 2 * s)?))
    }

    fn encode(&self, vals: &[f64]) -> Vec<u8> {
        let mut out = Vec::with_capacity(vals.len() * self.size());
        for &v in vals {
            match self.value_type {
                ValueType::F32 => out.extend((v as f32).to_le_bytes()),
                ValueType::F64 => out.extend(v.to_le_bytes()),
            }
        }
        out
    }

    /// Memory offset of matrix element `i` (0..9, in the configured layout order).
    fn matrix_slot(&self, i: usize) -> usize {
        self.rotation + ((i / 3) * self.row_stride + i % 3) * self.size()
    }
}

/// Typed state of one rigid body, in the sim's own frame (lengths in metres).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BodyState {
    pub pos: DVec3,
    pub rot: DQuat,
    pub lin_vel: DVec3,
    pub ang_vel: DVec3,
}

impl BodyState {
    /// Lerp for vectors, shortest-arc slerp for the rotation.
    pub fn interpolate(&self, other: &Self, alpha: f64) -> Self {
        let a = alpha.clamp(0.0, 1.0);
        let b_rot = if self.rot.dot(other.rot) < 0.0 { -other.rot } else { other.rot };
        Self {
            pos: self.pos.lerp(other.pos, a),
            rot: self.rot.slerp(b_rot, a).normalize(),
            lin_vel: self.lin_vel.lerp(other.lin_vel, a),
            ang_vel: self.ang_vel.lerp(other.ang_vel, a),
        }
    }

    /// Same pose, zero velocities (scrubbing).
    pub fn at_rest(&self) -> Self {
        Self { lin_vel: DVec3::ZERO, ang_vel: DVec3::ZERO, ..*self }
    }

    pub fn is_sane(&self) -> bool {
        self.pos.is_finite()
            && self.pos.abs().max_element() < MAX_COORD_M
            && self.rot.is_finite()
            && self.lin_vel.is_finite()
            && self.lin_vel.length() < MAX_LIN_VEL
            && self.ang_vel.is_finite()
            && self.ang_vel.length() < MAX_ANG_VEL
    }
}

/// One body object as read from memory.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Body {
    pub addr: usize,
    pub parent: usize,
    pub state: BodyState,
}

/// Reads and sanity-checks the body at `addr` (one read of `layout.span` bytes). `None` if
/// unreadable, the parent pointer is implausible, the rotation is not a rotation, or any value
/// is out of range.
pub fn read_body<M: Memory + ?Sized>(mem: &M, layout: &BodyLayout, addr: usize) -> Option<Body> {
    check_body(mem, layout, addr).ok()
}

/// Rotation tolerance (row length / dot products) when identifying bodies in a sweep.
pub const SCAN_ORTHO_TOL: f64 = 0.01;
/// Rotation tolerance for bodies already known to be the car's: a damaged car's chassis
/// matrix shears slightly (live: 1.5 % after a crash, 3 deg from the actor rotation).
pub const LOCKED_ORTHO_TOL: f64 = 0.1;

/// [`read_body`] with the reason a body is rejected (for logs).
pub fn check_body<M: Memory + ?Sized>(mem: &M, layout: &BodyLayout, addr: usize) -> Result<Body, String> {
    check_body_tol(mem, layout, addr, SCAN_ORTHO_TOL)
}

/// [`check_body`] accepting rotations within `ortho_tol` of orthonormal.
pub fn check_body_tol<M: Memory + ?Sized>(
    mem: &M,
    layout: &BodyLayout,
    addr: usize,
    ortho_tol: f64,
) -> Result<Body, String> {
    let mut buf = vec![0u8; layout.span];
    if !mem.read(addr, &mut buf) {
        return Err("unreadable".into());
    }
    let short = || "layout outside the read span".to_owned();
    let parent = u64::from_le_bytes(
        buf.get(layout.parent..layout.parent + 8).ok_or_else(short)?.try_into().map_err(|_| short())?,
    ) as usize;
    if !is_plausible_ptr(parent) {
        return Err(format!("implausible parent {parent:#x}"));
    }
    let inv = 1.0 / layout.length_scale;
    let pos = layout.get_vec3(&buf, layout.position).ok_or_else(short)? * inv;
    let mut m = [0.0; 9];
    for (i, v) in m.iter_mut().enumerate() {
        *v = layout.get(&buf, layout.matrix_slot(i)).ok_or_else(short)?;
    }
    let lin_vel = match layout.linear_velocity {
        Some(o) => layout.get_vec3(&buf, o).ok_or_else(short)? * inv,
        None => DVec3::ZERO,
    };
    let ang_vel = match layout.angular_velocity {
        Some(o) => layout.get_vec3(&buf, o).ok_or_else(short)?,
        None => DVec3::ZERO,
    };
    let values = || {
        format!(
            "pos ({:.3}, {:.3}, {:.3}) |v| {:.3} |w| {:.3} rot rows ({:.3}, {:.3}, {:.3}) ({:.3}, {:.3}, {:.3}) ({:.3}, {:.3}, {:.3})",
            pos.x,
            pos.y,
            pos.z,
            lin_vel.length(),
            ang_vel.length(),
            m[0],
            m[1],
            m[2],
            m[3],
            m[4],
            m[5],
            m[6],
            m[7],
            m[8]
        )
    };
    if !is_orthonormal(&m, ortho_tol) {
        return Err(format!("rotation not orthonormal: {}", values()));
    }
    let rot = quat_from_matrix(m, layout.rows).ok_or_else(|| format!("rotation not convertible: {}", values()))?;
    let state = BodyState { pos, rot, lin_vel, ang_vel };
    if !state.is_sane() {
        return Err(format!("non-finite or out-of-range state: {}", values()));
    }
    Ok(Body { addr, parent, state })
}

/// Rows (equivalently columns) of a rotation are unit length and mutually orthogonal.
fn is_orthonormal(m: &[f64; 9], tol: f64) -> bool {
    let r = [DVec3::new(m[0], m[1], m[2]), DVec3::new(m[3], m[4], m[5]), DVec3::new(m[6], m[7], m[8])];
    r.iter().all(|v| v.is_finite() && (v.length() - 1.0).abs() < tol)
        && r[0].dot(r[1]).abs() < tol
        && r[0].dot(r[2]).abs() < tol
        && r[1].dot(r[2]).abs() < tol
}

/// Velocity of a point rigidly attached to a body moving at `v` / spinning at `w` about
/// `origin`.
pub fn rigid_point_velocity(v: DVec3, w: DVec3, origin: DVec3, p: DVec3) -> DVec3 {
    v + w.cross(p - origin)
}

/// Cheap per-tick check that `addr` still holds a body of the expected car.
pub fn header_ok<M: Memory + ?Sized>(mem: &M, layout: &BodyLayout, addr: usize, vtable: usize, parent: usize) -> bool {
    mem.read_ptr(addr) == Some(vtable) && mem.read_ptr(addr + layout.parent) == Some(parent)
}

/// Writes position, the 3x3 rotation and both velocities of the body at `addr`. Only those
/// fields are touched (padding / the 4th matrix column are left alone). `Err(addr)` of the
/// first failed write.
pub fn write_body<M: Memory + ?Sized>(mem: &M, layout: &BodyLayout, addr: usize, s: &BodyState) -> Result<(), usize> {
    let sc = layout.length_scale;
    let put = |off: usize, vals: &[f64]| mem.write(addr + off, &layout.encode(vals)).then_some(()).ok_or(addr + off);
    let p = s.pos * sc;
    put(layout.position, &[p.x, p.y, p.z])?;
    let m = quat_to_matrix(s.rot, layout.rows);
    for row in 0..3 {
        put(layout.matrix_slot(row * 3), &m[row * 3..row * 3 + 3])?;
    }
    if let Some(o) = layout.linear_velocity {
        let v = s.lin_vel * sc;
        put(o, &[v.x, v.y, v.z])?;
    }
    if let Some(o) = layout.angular_velocity {
        let w = s.ang_vel;
        put(o, &[w.x, w.y, w.z])?;
    }
    Ok(())
}

/// Writes only the linear and angular velocity of the body at `addr`.
pub fn write_body_velocities<M: Memory + ?Sized>(
    mem: &M,
    layout: &BodyLayout,
    addr: usize,
    lin: DVec3,
    ang: DVec3,
) -> Result<(), usize> {
    let put =
        |off: usize, v: DVec3| mem.write(addr + off, &layout.encode(&[v.x, v.y, v.z])).then_some(()).ok_or(addr + off);
    if let Some(o) = layout.linear_velocity {
        put(o, lin * layout.length_scale)?;
    }
    if let Some(o) = layout.angular_velocity {
        put(o, ang)?;
    }
    Ok(())
}

/// What [`apply_body_states`] writes per body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyWriteScope {
    /// Linear + angular velocity only.
    Velocities,
    /// Pose and velocities.
    Full,
}

/// Writes recorded `states` to `addrs` (same order). `at_rest` zeroes the velocities (scrubbing).
pub fn apply_body_states<M: Memory + ?Sized>(
    mem: &M,
    layout: &BodyLayout,
    addrs: &[usize],
    states: &[BodyState],
    scope: BodyWriteScope,
    at_rest: bool,
) -> Result<(), usize> {
    for (&a, s) in addrs.iter().zip(states) {
        let s = if at_rest { s.at_rest() } else { *s };
        match scope {
            BodyWriteScope::Velocities => write_body_velocities(mem, layout, a, s.lin_vel, s.ang_vel)?,
            BodyWriteScope::Full => write_body(mem, layout, a, &s)?,
        }
    }
    Ok(())
}

// ---- Sweep ----------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScanStats {
    pub regions: usize,
    pub bytes: usize,
    pub unreadable_bytes: usize,
    pub hits: usize,
    /// True if `max_hits` stopped the sweep early.
    pub truncated: bool,
    /// True if only the regions that held the previous car were swept.
    pub quick: bool,
    pub elapsed: Duration,
}

const CHUNK: usize = 1 << 20;
const PAGE: usize = 0x1000;

/// Addresses of every 8-aligned qword equal to `value` in `src.scan_regions()`. Unreadable
/// chunks are retried page by page. The sweep's own read buffer is excluded (in-process it
/// holds copies of the memory it just read).
pub fn find_qword<S: ScanSource + ?Sized>(src: &S, value: u64, max_hits: usize) -> (Vec<usize>, ScanStats) {
    find_qword_in(src, &src.scan_regions(), value, max_hits)
}

/// [`find_qword`] over an explicit region list.
pub fn find_qword_in<M: Memory + ?Sized>(
    src: &M,
    regions: &[ScanRegion],
    value: u64,
    max_hits: usize,
) -> (Vec<usize>, ScanStats) {
    let start = Instant::now();
    let mut stats = ScanStats::default();
    let mut hits = Vec::new();
    let mut buf = vec![0u8; CHUNK];
    let own = buf.as_ptr() as usize..buf.as_ptr() as usize + buf.len();
    let needle = value.to_le_bytes();
    let scan = |data: &[u8], base: usize, hits: &mut Vec<usize>| {
        for (i, w) in data.as_chunks::<8>().0.iter().enumerate() {
            if *w == needle {
                let a = base + i * 8;
                if !own.contains(&a) {
                    hits.push(a);
                }
            }
        }
    };
    'regions: for r in regions {
        stats.regions += 1;
        let end = r.base.saturating_add(r.size);
        let mut at = r.base & !7;
        while at < end {
            let len = CHUNK.min(end - at);
            if src.read(at, &mut buf[..len]) {
                scan(&buf[..len], at, &mut hits);
                stats.bytes += len;
            } else {
                let mut p = at;
                while p < at + len {
                    let n = (PAGE - p % PAGE).min(at + len - p);
                    if src.read(p, &mut buf[..n]) {
                        scan(&buf[..n], p, &mut hits);
                        stats.bytes += n;
                    } else {
                        stats.unreadable_bytes += n;
                    }
                    p += n;
                }
            }
            if hits.len() >= max_hits {
                hits.truncate(max_hits);
                stats.truncated = true;
                break 'regions;
            }
            at += len;
        }
    }
    stats.hits = hits.len();
    stats.elapsed = start.elapsed();
    (hits, stats)
}

/// Bodies sharing one parent pointer, sorted by address.
#[derive(Clone, Debug, PartialEq)]
pub struct BodyGroup {
    pub parent: usize,
    pub bodies: Vec<Body>,
}

impl BodyGroup {
    /// Distance from `p` to the nearest body.
    pub fn distance_to(&self, p: DVec3) -> f64 {
        self.bodies.iter().map(|b| (b.state.pos - p).length()).fold(f64::INFINITY, f64::min)
    }

    pub fn centroid(&self) -> DVec3 {
        let n = self.bodies.len().max(1) as f64;
        self.bodies.iter().map(|b| b.state.pos).sum::<DVec3>() / n
    }
}

/// Reads every hit as a body and groups the valid ones by parent. Returns the groups (by
/// parent address) and the number of hits that were not valid bodies.
pub fn group_bodies<M: Memory + ?Sized>(mem: &M, layout: &BodyLayout, hits: &[usize]) -> (Vec<BodyGroup>, usize) {
    let mut by_parent: BTreeMap<usize, Vec<Body>> = BTreeMap::new();
    let mut rejected = 0;
    for &a in hits {
        match read_body(mem, layout, a) {
            Some(b) => by_parent.entry(b.parent).or_default().push(b),
            None => rejected += 1,
        }
    }
    let groups = by_parent
        .into_iter()
        .map(|(parent, mut bodies)| {
            bodies.sort_by_key(|b| b.addr);
            bodies.dedup_by_key(|b| b.addr);
            BodyGroup { parent, bodies }
        })
        .collect();
    (groups, rejected)
}

/// Re-reads every body in place (a sweep takes seconds to minutes, so the states read while
/// grouping are stale for a moving car). Bodies that stopped being valid or changed parent are
/// dropped, as are emptied groups. Returns the number dropped.
pub fn reread_groups<M: Memory + ?Sized>(mem: &M, layout: &BodyLayout, groups: &mut Vec<BodyGroup>) -> usize {
    let mut dropped = 0;
    for g in groups.iter_mut() {
        let before = g.bodies.len();
        g.bodies =
            g.bodies.iter().filter_map(|b| read_body(mem, layout, b.addr).filter(|n| n.parent == g.parent)).collect();
        dropped += before - g.bodies.len();
    }
    groups.retain(|g| !g.bodies.is_empty());
    dropped
}

/// Which bodies of the matched group make up the car.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CarSelect {
    /// Max distance (m) between a body and the shm car position for the car to match.
    pub tolerance_m: f64,
    /// Car bodies lie within this distance (m) of the main body.
    pub radius_m: f64,
    /// While moving: max linear-velocity difference to the main body (m/s), widened by
    /// |w| x distance.
    pub velocity_tolerance_mps: f64,
    /// The velocity test applies above this main-body speed (m/s).
    pub moving_mps: f64,
    /// shm carCoordinates lag (s); position checks allow `|v| * shm_lag_s` more.
    pub shm_lag_s: f64,
}

/// Body-vs-shm position tolerance at `speed` (m/s): shm carCoordinates trails the physics.
pub fn shm_tolerance(base_m: f64, shm_lag_s: f64, speed: f64) -> f64 {
    base_m + speed.abs() * shm_lag_s
}

impl CarSelect {
    pub fn from_spec(sp: &SimCarSpec) -> Self {
        Self {
            tolerance_m: sp.match_tolerance_m,
            radius_m: sp.car_radius_m,
            velocity_tolerance_mps: sp.car_velocity_tolerance_mps,
            moving_mps: sp.car_moving_mps,
            shm_lag_s: sp.shm_lag_s,
        }
    }

    /// Match tolerance for a body moving at `speed` (m/s).
    pub fn position_tolerance(&self, speed: f64) -> f64 {
        shm_tolerance(self.tolerance_m, self.shm_lag_s, speed)
    }

    /// How far `b` is outside its speed-aware tolerance of `hint` (<= 0 = matches).
    fn hint_excess(&self, b: &BodyState, hint: DVec3) -> f64 {
        (b.pos - hint).length() - self.position_tolerance(b.lin_vel.length())
    }

    /// Whether `b` moves with the main body `p` (always true for `p` itself).
    pub fn is_member(&self, p: &BodyState, b: &BodyState) -> bool {
        let r = (b.pos - p.pos).length();
        if r > self.radius_m {
            return false;
        }
        if p.lin_vel.length() <= self.moving_mps {
            return true;
        }
        let spin = p.ang_vel.length().min(MAX_SPIN_RAD_S);
        (b.lin_vel - p.lin_vel).length() <= self.velocity_tolerance_mps + spin * r
    }
}

/// The player car: the main chassis body plus the bodies of its group that move with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CarBodies {
    pub parent: usize,
    pub primary: usize,
    /// The car's bodies, sorted by address (includes `primary`). Nothing else is ever written.
    pub bodies: Vec<usize>,
    /// Bodies of the parent group left out (too far from the main body or moving differently).
    pub excluded: usize,
}

impl CarBodies {
    /// Every body except the primary, in address order.
    pub fn others(&self) -> impl Iterator<Item = usize> + '_ {
        self.bodies.iter().copied().filter(move |&b| b != self.primary)
    }
}

/// Picks the group containing a body within its speed-aware tolerance
/// ([`CarSelect::position_tolerance`]) of `hint` (the shm car position); the group whose
/// best body is furthest inside its tolerance wins, ties go to the larger group. The primary
/// body is the nearest matching body to the hint, preferring one that has an exact duplicate
/// (the chassis twin), then the lower address. The car is the bodies of that group that pass
/// [`CarSelect::is_member`] against the primary. Returns the group index and the car.
pub fn choose_car(groups: &[BodyGroup], hint: Option<DVec3>, sel: &CarSelect) -> Result<(usize, CarBodies), String> {
    let hint = hint
        .filter(|h| h.is_finite())
        .ok_or_else(|| format!("shm car position unavailable; {} body groups not disambiguated", groups.len()))?;
    if groups.is_empty() {
        return Err("no rigid bodies found".into());
    }
    let excess = |g: &BodyGroup| g.bodies.iter().map(|b| sel.hint_excess(&b.state, hint)).fold(f64::INFINITY, f64::min);
    let (gi, e) = groups
        .iter()
        .enumerate()
        .map(|(i, g)| (i, excess(g)))
        .min_by(|a, b| a.1.total_cmp(&b.1).then_with(|| groups[b.0].bodies.len().cmp(&groups[a.0].bodies.len())))
        .expect("groups not empty");
    if e > 0.0 {
        let d = groups[gi].distance_to(hint);
        return Err(format!(
            "no body within {} m + |v| x {} s of shm position ({:.2}, {:.2}, {:.2}); nearest of {} groups is {d:.2} m away",
            sel.tolerance_m,
            sel.shm_lag_s,
            hint.x,
            hint.y,
            hint.z,
            groups.len()
        ));
    }
    let g = &groups[gi];
    let near: Vec<&Body> = g.bodies.iter().filter(|b| sel.hint_excess(&b.state, hint) <= 0.0).collect();
    let twinned: Vec<&Body> = near
        .iter()
        .copied()
        .filter(|b| {
            g.bodies.iter().any(|o| o.addr != b.addr && (o.state.pos - b.state.pos).length() <= DUPLICATE_EPS_M)
        })
        .collect();
    let pool = if twinned.is_empty() { near } else { twinned };
    let primary = **pool
        .iter()
        .min_by(|a, b| {
            (a.state.pos - hint).length().total_cmp(&(b.state.pos - hint).length()).then(a.addr.cmp(&b.addr))
        })
        .expect("nearest body is within tol");
    let bodies: Vec<usize> = g
        .bodies
        .iter()
        .filter(|b| b.addr == primary.addr || sel.is_member(&primary.state, &b.state))
        .map(|b| b.addr)
        .collect();
    Ok((gi, CarBodies { parent: g.parent, primary: primary.addr, excluded: g.bodies.len() - bodies.len(), bodies }))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScanConfig {
    /// Absolute vtable address (`module_base + vtable_rva`).
    pub vtable: usize,
    pub layout: BodyLayout,
    pub select: CarSelect,
    pub max_hits: usize,
}

impl ScanConfig {
    pub const DEFAULT_MAX_HITS: usize = 200_000;

    pub fn from_spec(sp: &SimCarSpec, module_base: usize) -> Option<Self> {
        Some(Self {
            vtable: module_base.checked_add(usize::try_from(sp.vtable_rva?).ok()?)?,
            layout: BodyLayout::from_spec(sp)?,
            select: CarSelect::from_spec(sp),
            max_hits: Self::DEFAULT_MAX_HITS,
        })
    }
}

#[derive(Clone, Debug)]
pub struct LocateReport {
    pub vtable: usize,
    pub stats: ScanStats,
    /// Vtable hits that did not read as a valid body.
    pub rejected: usize,
    /// Valid bodies that were gone at the post-sweep re-read.
    pub vanished: usize,
    pub groups: Vec<BodyGroup>,
    pub hint: Option<DVec3>,
    /// Group index and car, or why none was chosen.
    pub chosen: Result<(usize, CarBodies), String>,
}

impl LocateReport {
    pub fn summary(&self) -> String {
        let s = &self.stats;
        let head = format!(
            "{}{} regions, {} MiB in {:.2} s: {} vtable hits, {} rejected, {} vanished at re-read, {} groups",
            if s.quick { "quick rescan of " } else { "" },
            s.regions,
            s.bytes >> 20,
            s.elapsed.as_secs_f64(),
            s.hits,
            self.rejected,
            self.vanished,
            self.groups.len()
        );
        match &self.chosen {
            Ok((gi, car)) => format!(
                "{head}; car = group {gi} (parent {:#x}, {} car bodies, {} other bodies of the group excluded, main body {:#x})",
                car.parent,
                car.bodies.len(),
                car.excluded,
                car.primary
            ),
            Err(e) => format!("{head}; no car: {e}"),
        }
    }
}

/// Full locate over `src.scan_regions()`.
pub fn locate<S: ScanSource + ?Sized>(src: &S, cfg: &ScanConfig, hint: &dyn Fn() -> Option<DVec3>) -> LocateReport {
    locate_in(src, &src.scan_regions(), cfg, hint)
}

/// Sweep `regions`, read + group the hits, re-read every body, then compare with a `hint` taken
/// right after the re-read (a moving car is matched against its current position, not where it
/// was when the sweep passed it).
pub fn locate_in<M: Memory + ?Sized>(
    src: &M,
    regions: &[ScanRegion],
    cfg: &ScanConfig,
    hint: &dyn Fn() -> Option<DVec3>,
) -> LocateReport {
    let (hits, stats) = find_qword_in(src, regions, cfg.vtable as u64, cfg.max_hits);
    let (mut groups, rejected) = group_bodies(src, &cfg.layout, &hits);
    let vanished = reread_groups(src, &cfg.layout, &mut groups);
    let hint = hint();
    let chosen = choose_car(&groups, hint, &cfg.select);
    LocateReport { vtable: cfg.vtable, stats, rejected, vanished, groups, hint, chosen }
}

/// The current scan regions overlapping any region that held a body of `car`.
pub fn regions_holding(regions: &[ScanRegion], car: &CarBodies) -> Vec<ScanRegion> {
    regions
        .iter()
        .filter(|r| car.bodies.iter().any(|&a| a >= r.base && a < r.base.saturating_add(r.size)))
        .copied()
        .collect()
}

/// Remembered between scans: where the last car lived and how many bodies it had.
#[derive(Clone, Debug, Default)]
pub struct ScanMemory {
    pub regions: Vec<ScanRegion>,
    pub bodies: usize,
}

/// Locate that first re-sweeps only the regions which held the previous car. The quick result
/// is used if it finds a car with at least as many bodies as before; otherwise a full sweep
/// runs. Updates `memory` after a successful locate.
pub fn locate_with_memory<S: ScanSource + ?Sized>(
    src: &S,
    cfg: &ScanConfig,
    hint: &dyn Fn() -> Option<DVec3>,
    memory: &mut ScanMemory,
) -> LocateReport {
    let all = src.scan_regions();
    let mut rep = None;
    if !memory.regions.is_empty() {
        let prio: Vec<ScanRegion> = all
            .iter()
            .filter(|r| memory.regions.iter().any(|m| r.base < m.base + m.size && m.base < r.base + r.size))
            .copied()
            .collect();
        if !prio.is_empty() {
            let mut quick = locate_in(src, &prio, cfg, hint);
            quick.stats.quick = true;
            if quick.chosen.as_ref().is_ok_and(|(_, c)| c.bodies.len() >= memory.bodies) {
                rep = Some(quick);
            }
        }
    }
    let rep = rep.unwrap_or_else(|| locate_in(src, &all, cfg, hint));
    if let Ok((_, car)) = &rep.chosen {
        *memory = ScanMemory { regions: regions_holding(&all, car), bodies: car.bodies.len() };
    }
    rep
}

// ---- Background scanning --------------------------------------------------------------------

/// Current shm car position (graphics carCoordinates), callable from any thread.
pub type HintFn = Arc<dyn Fn() -> Option<DVec3> + Send + Sync>;

/// Runs [`locate`] on a dedicated thread so the game tick never waits for a sweep.
pub struct BackgroundScanner {
    tx: Option<mpsc::Sender<()>>,
    result: Arc<Mutex<Option<LocateReport>>>,
    busy: Arc<AtomicBool>,
    interval: Duration,
    last_request: Option<Instant>,
}

impl BackgroundScanner {
    pub fn spawn<S: ScanSource + Send + Sync + 'static>(
        src: Arc<S>,
        cfg: ScanConfig,
        hint: HintFn,
        interval: Duration,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<()>();
        let result: Arc<Mutex<Option<LocateReport>>> = Arc::default();
        let busy = Arc::new(AtomicBool::new(false));
        let (r, b) = (result.clone(), busy.clone());
        let spawned = std::thread::Builder::new().name("acr-rewind-scan".into()).spawn(move || {
            let mut memory = ScanMemory::default();
            while rx.recv().is_ok() {
                while rx.try_recv().is_ok() {}
                let rep = catch_unwind(AssertUnwindSafe(|| locate_with_memory(&*src, &cfg, &*hint, &mut memory)));
                if let (Ok(rep), Ok(mut slot)) = (rep, r.lock()) {
                    *slot = Some(rep);
                }
                b.store(false, Ordering::Release);
            }
        });
        if let Err(e) = &spawned {
            tracing::error!("sim_car: could not start the scan thread: {e}");
        }
        Self { tx: spawned.ok().map(|_| tx), result, busy, interval, last_request: None }
    }

    /// Starts a scan unless one is running or the previous one started less than `interval`
    /// ago. Never blocks. Returns true if a scan was started.
    pub fn request(&mut self) -> bool {
        let Some(tx) = &self.tx else {
            return false;
        };
        if self.busy.load(Ordering::Acquire) || self.last_request.is_some_and(|t| t.elapsed() < self.interval) {
            return false;
        }
        self.busy.store(true, Ordering::Release);
        self.last_request = Some(Instant::now());
        if tx.send(()).is_err() {
            self.busy.store(false, Ordering::Release);
            return false;
        }
        true
    }

    /// The finished report, if any (non-blocking).
    pub fn take(&mut self) -> Option<LocateReport> {
        self.result.try_lock().ok()?.take()
    }

    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }
}

// ---- History --------------------------------------------------------------------------------

/// States of the non-primary bodies captured alongside snapshots, keyed by timeline time.
/// Sampling interpolates between the bracketing captures exactly like the snapshot buffer.
#[derive(Debug, Default)]
pub struct BodyHistory {
    capacity: usize,
    entries: VecDeque<(f64, Vec<BodyState>)>,
    pending: Option<Vec<BodyState>>,
}

impl BodyHistory {
    pub fn new(capacity: usize) -> Self {
        Self { capacity: capacity.max(1), entries: VecDeque::new(), pending: None }
    }

    /// Holds states captured during `read_state` until the controller assigns their time.
    pub fn stage(&mut self, states: Vec<BodyState>) {
        self.pending = Some(states);
    }

    pub fn commit(&mut self, t: f64) {
        if let Some(s) = self.pending.take() {
            self.push(t, s);
        }
    }

    fn push(&mut self, t: f64, s: Vec<BodyState>) {
        while self.entries.back().is_some_and(|(last, _)| *last >= t - 1e-9) {
            self.entries.pop_back();
        }
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back((t, s));
    }

    /// Captures further than this from `t` don't describe the car at `t` (the history starts
    /// later, ends earlier, or has a gap while the bodies were not locked).
    pub const MAX_SAMPLE_DISTANCE_S: f64 = 0.1;

    /// States at `t`, interpolated between the bracketing captures. `None` unless a capture
    /// lies within [`Self::MAX_SAMPLE_DISTANCE_S`] of `t` on each side that is used (live: a
    /// history restarted at a crash site was "sampled" 14 s before its start and its
    /// near-zero velocities overwrote a 46 m/s resume). Captures with different body counts
    /// fall back to the nearer one.
    pub fn sample(&self, t: f64) -> Option<Vec<BodyState>> {
        if t.is_nan() {
            return None;
        }
        let near = |et: f64| (et - t).abs() <= Self::MAX_SAMPLE_DISTANCE_S;
        let ub = self.entries.partition_point(|(et, _)| *et <= t);
        if ub == 0 {
            return self.entries.front().filter(|(et, _)| near(*et)).map(|(_, s)| s.clone());
        }
        if ub == self.entries.len() {
            return self.entries.back().filter(|(et, _)| near(*et)).map(|(_, s)| s.clone());
        }
        let (ta, a) = &self.entries[ub - 1];
        let (tb, b) = &self.entries[ub];
        if !near(*ta) && !near(*tb) {
            return None;
        }
        let span = tb - ta;
        let alpha = if span > 0.0 { (t - ta) / span } else { 0.0 };
        if a.len() != b.len() || !(near(*ta) && near(*tb)) {
            return Some(if near(*ta) && (alpha < 0.5 || !near(*tb)) { a.clone() } else { b.clone() });
        }
        Some(a.iter().zip(b).map(|(x, y)| x.interpolate(y, alpha)).collect())
    }

    /// Re-orders the columns of every capture: new column `i` = old column `keep[i]`.
    /// Captures with a different body count than `old_len` are dropped.
    pub fn remap(&mut self, keep: &[usize], old_len: usize) {
        let pick = |s: &Vec<BodyState>| keep.iter().map(|&i| s[i]).collect::<Vec<_>>();
        self.entries.retain(|(_, s)| s.len() == old_len);
        for (_, s) in self.entries.iter_mut() {
            *s = pick(s);
        }
        self.pending = self.pending.take().filter(|s| s.len() == old_len).map(|s| pick(&s));
    }

    pub fn truncate_after(&mut self, t: f64) {
        while self.entries.back().is_some_and(|(et, _)| *et > t + 1e-9) {
            self.entries.pop_back();
        }
    }

    /// Resume at `t`: drop the future and keep the interpolated state at `t` as the newest
    /// entry, mirroring the snapshot buffer (which re-pushes the resume snapshot).
    pub fn resume_at(&mut self, t: f64) {
        let at = self.sample(t);
        self.truncate_after(t);
        if let Some(s) = at {
            self.push(t, s);
        }
        self.pending = None;
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.pending = None;
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::mem::SliceMemory;

    pub const VT: usize = 0x1_4a8d_9830;
    pub const POOL: usize = 0x2_5000_0000;
    pub const SLOT: usize = 0x300;

    pub fn layout() -> BodyLayout {
        let sigs = crate::Signatures::from_toml_str(include_str!("../../../config/signatures.toml")).unwrap();
        BodyLayout::from_spec(&sigs.raw_offsets.sim_car).unwrap()
    }

    /// Writes a body object (vtable + parent + state) at `obj` using the shipped layout.
    pub fn put_body(mem: &SliceMemory, obj: usize, parent: usize, s: &BodyState) {
        assert!(mem.write(obj, &(VT as u64).to_le_bytes()));
        assert!(mem.write(obj + 8, &(parent as u64).to_le_bytes()));
        write_body(mem, &layout(), obj, s).unwrap();
    }

    pub fn state(x: f64, yaw: f64) -> BodyState {
        BodyState {
            pos: DVec3::new(x, 12.0, -300.0),
            rot: DQuat::from_rotation_z(yaw),
            lin_vel: DVec3::new(20.0, 0.0, 1.0),
            ang_vel: DVec3::new(0.0, 0.0, 0.3),
        }
    }

    /// A pool with two cars (parents P1 at x=100, P2 at x=500), a stale freed slot and noise.
    pub fn world() -> SliceMemory {
        let mem = SliceMemory::new(POOL, vec![0; SLOT * 32]);
        let (p1, p2) = (0x3_0000_1000usize, 0x3_0000_2000usize);
        let obj = |i: usize| POOL + i * SLOT + 8;
        // Car 1: chassis twin at slots 5 and 6, suspension parts around it.
        put_body(&mem, obj(5), p1, &state(100.0, 0.5));
        put_body(&mem, obj(6), p1, &state(100.0, 0.5));
        for (k, i) in [1usize, 3, 9, 12].iter().enumerate() {
            put_body(&mem, obj(*i), p1, &state(100.0 + 1.2 * (k as f64 - 1.5), 0.5));
        }
        // A world object sharing car 1's parent, 800 m away (seen live): never part of the car.
        put_body(&mem, obj(14), p1, &state(900.0, 0.0));
        // Car 2 far away.
        put_body(&mem, obj(20), p2, &state(500.0, 0.0));
        put_body(&mem, obj(21), p2, &state(501.0, 0.0));
        // Vtable qword with garbage after it (rejected), and an unaligned copy (ignored).
        assert!(mem.write(obj(25), &(VT as u64).to_le_bytes()));
        assert!(mem.write(obj(27) + 3, &(VT as u64).to_le_bytes()));
        mem
    }

    #[test]
    fn body_roundtrip_touches_only_typed_fields() {
        let mem = SliceMemory::new(POOL, vec![0xAB; SLOT]);
        let l = layout();
        let s = state(1.5, 0.3);
        assert!(mem.write(POOL + 8, &0x3_0000_0000u64.to_le_bytes()));
        write_body(&mem, &l, POOL, &s).unwrap();
        let b = read_body(&mem, &l, POOL).unwrap();
        assert_eq!(b.parent, 0x3_0000_0000);
        assert!((b.state.pos - s.pos).length() < 1e-12);
        assert!(b.state.rot.angle_between(s.rot) < 1e-9);
        assert_eq!(b.state.lin_vel, s.lin_vel);
        assert_eq!(b.state.ang_vel, s.ang_vel);
        // 4th matrix column and the gap before the velocity are untouched.
        assert_eq!(mem.bytes(POOL + 0x158 + 0x18, 8), vec![0xAB; 8]);
        assert_eq!(mem.bytes(POOL + 0x1b8, 0x20), vec![0xAB; 0x20]);
        assert_eq!(mem.bytes(POOL + 0x210, 8), vec![0xAB; 8]);
        assert_eq!(mem.read_f64(POOL + 0x138), Some(1.5));
        assert_eq!(mem.read_f64(POOL + 0x1d8), Some(20.0));
    }

    #[test]
    fn apply_body_states_scopes() {
        let mem = world();
        let l = layout();
        let addrs = [POOL + 5 * SLOT + 8, POOL + 6 * SLOT + 8];
        let mut moved = state(140.0, 1.0);
        moved.lin_vel = DVec3::new(-3.0, 0.5, 0.0);
        moved.ang_vel = DVec3::new(0.2, 0.0, -0.1);
        let states = [moved, moved];
        apply_body_states(&mem, &l, &addrs, &states, BodyWriteScope::Velocities, false).unwrap();
        let b = read_body(&mem, &l, addrs[1]).unwrap().state;
        assert_eq!(b.pos.x, 100.0, "velocities scope leaves the pose");
        assert_eq!((b.lin_vel, b.ang_vel), (moved.lin_vel, moved.ang_vel));
        apply_body_states(&mem, &l, &addrs, &states, BodyWriteScope::Velocities, true).unwrap();
        let b = read_body(&mem, &l, addrs[0]).unwrap().state;
        assert_eq!((b.lin_vel, b.ang_vel), (DVec3::ZERO, DVec3::ZERO));
        apply_body_states(&mem, &l, &addrs, &states, BodyWriteScope::Full, true).unwrap();
        let b = read_body(&mem, &l, addrs[0]).unwrap().state;
        assert_eq!(b.pos.x, 140.0);
        assert!(b.rot.angle_between(moved.rot) < 1e-6);
        assert_eq!(b.lin_vel, DVec3::ZERO);
        assert_eq!(apply_body_states(&mem, &l, &[0x10], &states, BodyWriteScope::Full, false), Err(0x10 + 0x138));
    }

    #[test]
    fn read_body_rejects_garbage() {
        let mem = SliceMemory::new(POOL, vec![0; SLOT]);
        let l = layout();
        assert!(read_body(&mem, &l, POOL).is_none(), "null parent / zero matrix");
        assert!(mem.write(POOL + 8, &0x3_0000_0000u64.to_le_bytes()));
        assert!(read_body(&mem, &l, POOL).is_none(), "zero matrix");
        write_body(&mem, &l, POOL, &state(0.0, 0.0)).unwrap();
        assert!(read_body(&mem, &l, POOL).is_some());
        assert!(mem.write_f64(POOL + 0x140, f64::NAN));
        assert!(read_body(&mem, &l, POOL).is_none());
        assert!(read_body(&mem, &l, POOL + SLOT).is_none(), "unmapped");
    }

    #[test]
    fn find_qword_aligned_only_and_page_fallback() {
        let mem = world();
        let (hits, st) = find_qword(&mem, VT as u64, 1000);
        let obj = |i: usize| POOL + i * SLOT + 8;
        assert_eq!(hits, vec![obj(1), obj(3), obj(5), obj(6), obj(9), obj(12), obj(14), obj(20), obj(21), obj(25)]);
        assert_eq!(st.hits, 10);
        assert_eq!(st.bytes, SLOT * 32);
        assert!(!st.truncated);
        let (few, st) = find_qword(&mem, VT as u64, 2);
        assert_eq!(few.len(), 2);
        assert!(st.truncated);
    }

    #[test]
    fn groups_and_choice() {
        let mem = world();
        let l = layout();
        let (hits, _) = find_qword(&mem, VT as u64, 1000);
        let (groups, rejected) = group_bodies(&mem, &l, &hits);
        assert_eq!(rejected, 1);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].bodies.len(), 7);
        assert_eq!(groups[1].bodies.len(), 2);
        let obj = |i: usize| POOL + i * SLOT + 8;
        let sel = select();
        // Hint slightly off (shm lag): the chassis twin wins over a nearer suspension part.
        let hint = DVec3::new(99.3, 12.0, -300.0);
        let (gi, car) = choose_car(&groups, Some(hint), &sel).unwrap();
        assert_eq!(gi, 0);
        assert_eq!(car.parent, 0x3_0000_1000);
        assert_eq!(car.primary, obj(5), "lower address of the twin pair");
        assert_eq!(car.bodies.len(), 6);
        assert_eq!(car.excluded, 1);
        assert!(!car.bodies.contains(&obj(14)), "the far world object is never part of the car");
        assert_eq!(car.others().count(), 5);
        assert!(!car.others().any(|b| b == car.primary));
        let (gi, car) = choose_car(&groups, Some(DVec3::new(500.2, 12.0, -300.0)), &sel).unwrap();
        assert_eq!((gi, car.primary), (1, obj(20)), "no twins: nearest body");
        assert!(choose_car(&groups, Some(DVec3::new(300.0, 12.0, -300.0)), &sel).unwrap_err().contains("nearest"));
        assert!(choose_car(&groups, None, &sel).is_err());
        assert!(choose_car(&groups, Some(DVec3::NAN), &sel).is_err());
        assert!(choose_car(&[], Some(hint), &sel).is_err());
    }

    pub fn select() -> CarSelect {
        let sigs = crate::Signatures::from_toml_str(include_str!("../../../config/signatures.toml")).unwrap();
        CarSelect::from_spec(&sigs.raw_offsets.sim_car)
    }

    pub fn scan_config() -> ScanConfig {
        ScanConfig { vtable: VT, layout: layout(), select: select(), max_hits: 1000 }
    }

    #[test]
    fn car_select_radius_and_velocity() {
        let sel = select();
        assert_eq!((sel.tolerance_m, sel.radius_m), (2.0, 4.0), "shipped defaults");
        let mut p = state(0.0, 0.0);
        p.ang_vel = DVec3::ZERO;
        let base = p;
        let at = |x: f64, vx: f64| BodyState {
            pos: base.pos + DVec3::new(x, 0.0, 0.0),
            lin_vel: DVec3::new(vx, 0.0, 1.0),
            ..base
        };
        assert!(sel.is_member(&p, &p));
        assert!(sel.is_member(&p, &at(2.4, 20.0)));
        assert!(!sel.is_member(&p, &at(4.5, 20.0)), "outside the radius");
        assert!(!sel.is_member(&p, &at(1.0, 0.0)), "parked object next to a moving car");
        assert!(sel.is_member(&p, &at(1.0, 17.5)), "within the velocity tolerance");
        // Spinning car: the tolerance widens with |w| x distance.
        p.ang_vel = DVec3::new(0.0, 0.0, 2.0);
        assert!(sel.is_member(&p, &at(2.0, 14.0)), "3 + 2*2 = 7 m/s allowed");
        assert!(!sel.is_member(&p, &at(2.0, 12.0)));
        // Stopped car: no velocity test.
        let mut stopped = p;
        stopped.lin_vel = DVec3::ZERO;
        assert!(sel.is_member(&stopped, &BodyState { lin_vel: DVec3::new(0.5, 0.0, 0.0), ..at(1.0, 0.0) }));
    }

    #[test]
    fn match_tolerance_grows_with_speed() {
        let sel = select();
        assert_eq!(sel.shm_lag_s, 0.06, "shipped default");
        assert_eq!(sel.position_tolerance(0.0), 2.0);
        assert!((sel.position_tolerance(-50.0) - 5.0).abs() < 1e-12);
        let mk = |parent: usize, x: f64, vx: f64| BodyGroup {
            parent,
            bodies: vec![Body {
                addr: parent + 0x10,
                parent,
                state: BodyState { pos: DVec3::new(x, 0.0, 0.0), lin_vel: DVec3::new(vx, 0.0, 0.0), ..state(0.0, 0.0) },
            }],
        };
        // A car at 50 m/s is 3.5 m ahead of the lagging shm position: matched (2 + 50 * 0.06 = 5).
        let hint = Some(DVec3::ZERO);
        let (gi, _) = choose_car(&[mk(0x1000, 3.5, 50.0)], hint, &sel).unwrap();
        assert_eq!(gi, 0);
        // The same distance standing still is not.
        let e = choose_car(&[mk(0x1000, 3.5, 0.0)], hint, &sel).unwrap_err();
        assert!(e.contains("2 m + |v| x 0.06 s"), "{e}");
        // The fast car beats a nearer parked object that is outside its own tolerance.
        let (gi, c) = choose_car(&[mk(0x2000, 2.5, 0.0), mk(0x1000, 3.5, 50.0)], hint, &sel).unwrap();
        assert_eq!((gi, c.parent), (1, 0x1000));
    }

    #[test]
    fn moving_car_excludes_parked_group_mates() {
        let mem = SliceMemory::new(POOL, vec![0; SLOT * 8]);
        let parent = 0x3_0000_1000usize;
        let obj = |i: usize| POOL + i * SLOT + 8;
        let car = state(0.0, 0.0);
        put_body(&mem, obj(0), parent, &car);
        put_body(&mem, obj(1), parent, &car);
        put_body(&mem, obj(2), parent, &BodyState { pos: car.pos + DVec3::new(1.5, 0.5, 0.0), ..car });
        // A parked body of the same parent within the radius.
        put_body(
            &mem,
            obj(3),
            parent,
            &BodyState { pos: car.pos + DVec3::new(3.0, 0.0, 0.0), lin_vel: DVec3::ZERO, ang_vel: DVec3::ZERO, ..car },
        );
        let rep = locate(&mem, &scan_config(), &|| Some(car.pos));
        let (_, c) = rep.chosen.unwrap();
        assert_eq!(c.bodies, vec![obj(0), obj(1), obj(2)]);
        assert_eq!(c.excluded, 1);
    }

    /// Memory whose bodies move after the sweep has passed them (a long sweep, a moving car).
    struct MovingAfterSweep {
        mem: SliceMemory,
        reads: std::sync::atomic::AtomicUsize,
        move_after: usize,
        moved: AtomicBool,
    }

    impl Memory for MovingAfterSweep {
        fn read(&self, addr: usize, buf: &mut [u8]) -> bool {
            let n = self.reads.fetch_add(1, Ordering::SeqCst);
            if n == self.move_after && !self.moved.swap(true, Ordering::SeqCst) {
                for i in [5usize, 6] {
                    let a = POOL + i * SLOT + 8;
                    let mut s = read_body(&self.mem, &layout(), a).unwrap().state;
                    s.pos.x += 40.0;
                    write_body(&self.mem, &layout(), a, &s).unwrap();
                }
            }
            self.mem.read(addr, buf)
        }
        fn write(&self, _: usize, _: &[u8]) -> bool {
            false
        }
    }

    impl ScanSource for MovingAfterSweep {
        fn scan_regions(&self) -> Vec<ScanRegion> {
            self.mem.scan_regions()
        }
    }

    #[test]
    fn positions_are_reread_after_the_sweep() {
        // The sweep + grouping reads happen first; the car then drives 40 m before the re-read.
        let src = MovingAfterSweep {
            mem: world(),
            reads: Default::default(),
            move_after: usize::MAX,
            moved: AtomicBool::new(false),
        };
        let (hits, _) = find_qword(&src, VT as u64, 1000);
        let _ = group_bodies(&src, &layout(), &hits);
        let before = src.reads.load(Ordering::SeqCst);
        let src = MovingAfterSweep { move_after: before, reads: Default::default(), ..src };
        let rep = locate(&src, &scan_config(), &|| Some(DVec3::new(140.0, 12.0, -300.0)));
        let (_, car) = rep.chosen.as_ref().expect("matched against the re-read position");
        assert_eq!(car.primary, POOL + 5 * SLOT + 8);
        assert_eq!(car.bodies, vec![POOL + 5 * SLOT + 8, POOL + 6 * SLOT + 8], "suspension parts stayed behind");
    }

    #[test]
    fn rescan_tries_the_last_regions_first() {
        let mem = SliceMemory::new(POOL, vec![0; SLOT * 32]);
        for (i, r) in [(5usize, 0.0), (6, 0.0), (7, 1.0)] {
            put_body(&mem, POOL + i * SLOT + 8, 0x3_0000_1000, &state(100.0 + r, 0.5));
        }
        let other = 0x7_0000_0000usize;
        mem.add_region(other, vec![0; SLOT * 4]);
        let cfg = scan_config();
        let hint = || Some(DVec3::new(100.0, 12.0, -300.0));
        let mut memo = ScanMemory::default();
        let first = locate_with_memory(&mem, &cfg, &hint, &mut memo);
        assert!(!first.stats.quick);
        assert_eq!((memo.regions.len(), memo.bodies), (1, 3));
        let second = locate_with_memory(&mem, &cfg, &hint, &mut memo);
        assert!(second.stats.quick, "{}", second.summary());
        assert!(second.summary().starts_with("quick rescan of 1 regions"));
        assert_eq!(second.chosen.unwrap().1.bodies.len(), 3);
        // The car moved to another region: the quick pass fails and a full sweep finds it.
        for i in [5usize, 6, 7] {
            assert!(mem.write(POOL + i * SLOT + 8, &[0u8; 8]));
        }
        for (k, r) in [0.0, 0.0, 1.0].iter().enumerate() {
            put_body(&mem, other + 8 + k * SLOT, 0x3_0000_1000, &state(100.0 + r, 0.5));
        }
        let third = locate_with_memory(&mem, &cfg, &hint, &mut memo);
        assert!(!third.stats.quick);
        assert_eq!(third.chosen.unwrap().1.bodies.len(), 3);
        assert_eq!(memo.regions[0].base, other);
    }

    #[test]
    fn locate_end_to_end() {
        let mem = world();
        let rep = locate(&mem, &scan_config(), &|| Some(DVec3::new(100.0, 12.0, -300.0)));
        let (_, car) = rep.chosen.as_ref().unwrap();
        assert_eq!(car.primary, POOL + 5 * SLOT + 8);
        assert!(rep.summary().contains("6 car bodies, 1 other bodies of the group excluded"), "{}", rep.summary());
    }

    #[test]
    fn background_scanner_delivers_and_throttles() {
        let mem = Arc::new(world());
        let cfg = scan_config();
        let hint: HintFn = Arc::new(|| Some(DVec3::new(100.0, 12.0, -300.0)));
        let mut s = BackgroundScanner::spawn(mem, cfg, hint, Duration::from_secs(3600));
        assert!(s.request());
        assert!(!s.request(), "throttled / busy");
        let start = Instant::now();
        let rep = loop {
            if let Some(r) = s.take() {
                break r;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "scan never finished");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(rep.chosen.is_ok());
        assert!(!s.is_busy());
        assert!(!s.request(), "interval not elapsed");
    }

    #[test]
    fn history_interpolates_and_resumes() {
        let mut h = BodyHistory::new(10);
        h.commit(0.0);
        assert!(h.is_empty());
        for (i, t) in [0.0, 0.1, 0.2].iter().enumerate() {
            h.stage(vec![state(i as f64 * 10.0, 0.0), state(100.0, i as f64 * 0.1)]);
            h.commit(*t);
        }
        let s = h.sample(0.15).unwrap();
        assert!((s[0].pos.x - 15.0).abs() < 1e-9);
        assert!(s[1].rot.angle_between(DQuat::from_rotation_z(0.15)) < 1e-9);
        assert_eq!(h.sample(-0.05).unwrap()[0].pos.x, 0.0, "just before the first capture");
        assert_eq!(h.sample(0.25).unwrap()[0].pos.x, 20.0, "just after the last capture");
        assert!(h.sample(-1.0).is_none(), "long before the history starts");
        assert!(h.sample(9.0).is_none(), "long after it ends");
        assert!(h.sample(f64::NAN).is_none());
        h.resume_at(0.05);
        assert_eq!(h.len(), 2);
        assert!((h.sample(0.05).unwrap()[0].pos.x - 5.0).abs() < 1e-9, "interpolated resume entry");
        // A capture with a different body count falls back to the nearer neighbour.
        h.stage(vec![state(50.0, 0.0)]);
        h.commit(0.1);
        assert_eq!(h.sample(0.06).unwrap().len(), 2);
        assert_eq!(h.sample(0.09).unwrap().len(), 1);
        h.truncate_after(0.0);
        assert_eq!(h.len(), 1);
        h.clear();
        assert!(h.is_empty());
        let mut small = BodyHistory::new(2);
        for t in [0.0, 1.0, 2.0] {
            small.stage(vec![state(t, 0.0)]);
            small.commit(t);
        }
        assert_eq!(small.len(), 2);
        assert_eq!(small.sample(1.0).unwrap()[0].pos.x, 1.0);
        assert!(small.sample(0.0).is_none(), "evicted");
    }

    #[test]
    fn history_gap_is_not_interpolated_across() {
        let mut h = BodyHistory::new(100);
        for t in [0.0, 0.02, 0.04, 5.0, 5.02] {
            h.stage(vec![state(t * 10.0, 0.0)]);
            h.commit(t);
        }
        assert!(h.sample(2.5).is_none(), "middle of a 5 s gap");
        assert_eq!(h.sample(0.1).unwrap()[0].pos.x, 0.4, "edge of the gap: nearest capture");
        assert_eq!(h.sample(4.95).unwrap()[0].pos.x, 50.0);
        assert!((h.sample(5.01).unwrap()[0].pos.x - 50.1).abs() < 1e-9);
    }

    #[test]
    fn history_remap_keeps_and_reorders_columns() {
        let mut h = BodyHistory::new(10);
        h.stage(vec![state(1.0, 0.0), state(2.0, 0.0), state(3.0, 0.0)]);
        h.commit(0.0);
        h.stage(vec![state(9.0, 0.0)]);
        h.commit(0.05);
        h.stage(vec![state(4.0, 0.0), state(5.0, 0.0), state(6.0, 0.0)]);
        h.remap(&[2, 0], 3);
        assert_eq!(h.len(), 1, "the capture with another body count is dropped");
        let s = h.sample(0.0).unwrap();
        assert_eq!((s[0].pos.x, s[1].pos.x), (3.0, 1.0));
        h.commit(0.1);
        let s = h.sample(0.1).unwrap();
        assert_eq!((s.len(), s[0].pos.x, s[1].pos.x), (2, 6.0, 4.0), "pending remapped too");
    }

    #[test]
    fn rigid_point_velocity_adds_spin() {
        let v = rigid_point_velocity(
            DVec3::new(10.0, 0.0, 0.0),
            DVec3::new(0.0, 1.0, 0.0),
            DVec3::ZERO,
            DVec3::new(0.0, 0.0, 2.0),
        );
        assert!((v - DVec3::new(12.0, 0.0, 0.0)).length() < 1e-12);
    }

    #[test]
    fn at_rest_and_interpolate_shortest_arc() {
        let a = state(0.0, 0.1);
        let mut b = state(10.0, 0.3);
        b.rot = -b.rot;
        let m = a.interpolate(&b, 0.5);
        assert!(m.rot.angle_between(DQuat::from_rotation_z(0.2)) < 1e-9);
        assert_eq!(m.at_rest().lin_vel, DVec3::ZERO);
        assert_eq!(m.at_rest().pos, m.pos);
    }
}
