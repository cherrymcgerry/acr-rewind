//! `find-car`: runs the hook's rigid-body locator (`acr_ue::bodies`, `locator = "vtable_scan"`)
//! against the target through the read-only [`ReadMem`] view, then samples the chosen car next
//! to shared memory. Nothing is written; the same code path decides which bodies the hook
//! would snapshot and restore.

use crate::mem::{ReadMem, RegionKind};
use acr_ue::bodies::{self, Body, BodyState, CarBodies, LocateReport, ScanConfig};
use acr_ue::mem::{Memory, ScanRegion, ScanSource};
use acr_ue::sigs::SimCarSpec;
use anyhow::{bail, Result};
use glam::DVec3;
use serde::Serialize;

/// Uncached [`Memory`] + [`ScanSource`] over a [`ReadMem`]: scan regions are committed private
/// pages that are writable and not executable (the same set the in-process scan uses).
pub struct RemoteScan<R: ReadMem>(pub R);

impl<R: ReadMem> Memory for RemoteScan<R> {
    fn read(&self, addr: usize, buf: &mut [u8]) -> bool {
        buf.is_empty() || (addr >= 0x10000 && self.0.read(addr, buf))
    }

    fn write(&self, _addr: usize, _data: &[u8]) -> bool {
        false
    }
}

impl<R: ReadMem> ScanSource for RemoteScan<R> {
    fn scan_regions(&self) -> Vec<ScanRegion> {
        self.0
            .regions()
            .into_iter()
            .filter(|r| r.kind == RegionKind::Private && r.writable && !r.executable)
            .map(|r| ScanRegion { base: r.base, size: r.size })
            .collect()
    }
}

#[derive(Clone, Debug, Default)]
pub struct FindCarOptions {
    /// Overrides `[raw_offsets.sim_car] vtable_rva`.
    pub vtable_rva: Option<u64>,
    /// Overrides `match_tolerance_m`.
    pub tolerance_m: Option<f64>,
    /// Overrides `car_radius_m`.
    pub radius_m: Option<f64>,
    /// List every body of every group, not just the chosen car.
    pub all: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct BodyRow {
    pub addr: String,
    pub pos: [f64; 3],
    /// Unit quaternion x, y, z, w.
    pub rot: [f64; 4],
    pub lin_vel: [f64; 3],
    pub ang_vel: [f64; 3],
    /// Distance to the shm car position (m).
    pub dist_to_shm: Option<f64>,
    /// Distance to the chosen main body (m).
    pub dist_to_primary: Option<f64>,
    pub primary: bool,
    /// Part of the car (the bodies the hook would snapshot and restore).
    pub in_car: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct GroupRow {
    pub index: usize,
    pub parent: String,
    pub bodies: usize,
    pub centroid: [f64; 3],
    /// Distance from the shm position to the nearest body of the group (m).
    pub dist_to_shm: Option<f64>,
    pub chosen: bool,
    /// Only for the chosen group, or every group with `--all`.
    pub body_rows: Vec<BodyRow>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FindCarReport {
    pub module_base: String,
    pub vtable_rva: String,
    pub vtable: String,
    pub regions: usize,
    pub scanned_mib: f64,
    pub unreadable_mib: f64,
    pub seconds: f64,
    pub vtable_hits: usize,
    pub truncated: bool,
    pub rejected_hits: usize,
    pub shm_pos: Option<[f64; 3]>,
    pub groups: Vec<GroupRow>,
    pub chosen_parent: Option<String>,
    pub primary: Option<String>,
    /// The car's bodies (subset of the chosen group).
    pub car_bodies: Vec<String>,
    /// Bodies of the chosen group left out of the car.
    pub excluded: usize,
    pub error: Option<String>,
    pub summary: String,
}

/// One live re-read of the chosen primary body next to shm.
#[derive(Clone, Debug, Serialize)]
pub struct LiveRow {
    pub t: f64,
    pub body_pos: Option<[f64; 3]>,
    pub body_vel: Option<[f64; 3]>,
    pub shm_pos: Option<[f64; 3]>,
    pub shm_vel: Option<[f64; 3]>,
    pub pos_err_m: Option<f64>,
    /// Speed-aware match tolerance at the body's speed (`match_tolerance_m + |v| * shm_lag_s`).
    pub pos_tol_m: Option<f64>,
    /// Error against the shm position extrapolated by `shm velocity * shm_lag_s`.
    pub pos_err_extrapolated_m: Option<f64>,
    pub vel_err_ms: Option<f64>,
    /// Headers of all chosen bodies still match (vtable + parent).
    pub headers_ok: bool,
}

pub fn scan_config(sp: &SimCarSpec, module_base: usize, o: &FindCarOptions) -> Result<ScanConfig> {
    let mut sp = sp.clone();
    if let Some(rva) = o.vtable_rva {
        sp.vtable_rva = Some(rva);
    }
    if let Some(t) = o.tolerance_m {
        sp.match_tolerance_m = t;
    }
    if let Some(r) = o.radius_m {
        sp.car_radius_m = r;
    }
    match ScanConfig::from_spec(&sp, module_base) {
        Some(c) => Ok(c),
        None => bail!("[raw_offsets.sim_car] needs vtable_rva, position and rotation_matrix for find-car"),
    }
}

fn row(b: &Body, shm: Option<DVec3>, car: Option<&CarBodies>, primary_pos: Option<DVec3>) -> BodyRow {
    let s = &b.state;
    BodyRow {
        addr: format!("{:#x}", b.addr),
        pos: s.pos.to_array(),
        rot: s.rot.to_array(),
        lin_vel: s.lin_vel.to_array(),
        ang_vel: s.ang_vel.to_array(),
        dist_to_shm: shm.map(|h| (s.pos - h).length()),
        dist_to_primary: primary_pos.map(|p| (s.pos - p).length()),
        primary: car.is_some_and(|c| c.primary == b.addr),
        in_car: car.is_some_and(|c| c.bodies.contains(&b.addr)),
    }
}

/// Runs the shared locator once. `shm_pos` is called after the sweep (and the body re-read),
/// so a moving car is matched against its current graphics carCoordinates.
pub fn find_car<S: ScanSource + ?Sized>(
    src: &S,
    module_base: usize,
    cfg: &ScanConfig,
    rva: u64,
    shm_pos: &dyn Fn() -> Option<[f64; 3]>,
    all: bool,
) -> (FindCarReport, LocateReport) {
    let rep = bodies::locate(src, cfg, &|| shm_pos().map(DVec3::from_array));
    let hint = rep.hint;
    let chosen = rep.chosen.as_ref().ok();
    let car = chosen.map(|(_, c)| c);
    let primary_pos =
        car.and_then(|c| rep.groups.iter().flat_map(|g| &g.bodies).find(|b| b.addr == c.primary)).map(|b| b.state.pos);
    let groups = rep
        .groups
        .iter()
        .enumerate()
        .map(|(i, g)| {
            let is_chosen = chosen.is_some_and(|(gi, _)| *gi == i);
            GroupRow {
                index: i,
                parent: format!("{:#x}", g.parent),
                bodies: g.bodies.len(),
                centroid: g.centroid().to_array(),
                dist_to_shm: hint.map(|h| g.distance_to(h)),
                chosen: is_chosen,
                body_rows: if is_chosen || all {
                    g.bodies.iter().map(|b| row(b, hint, car, primary_pos)).collect()
                } else {
                    Vec::new()
                },
            }
        })
        .collect();
    let s = &rep.stats;
    let out = FindCarReport {
        module_base: format!("{module_base:#x}"),
        vtable_rva: format!("{rva:#x}"),
        vtable: format!("{:#x}", rep.vtable),
        regions: s.regions,
        scanned_mib: s.bytes as f64 / (1 << 20) as f64,
        unreadable_mib: s.unreadable_bytes as f64 / (1 << 20) as f64,
        seconds: s.elapsed.as_secs_f64(),
        vtable_hits: s.hits,
        truncated: s.truncated,
        rejected_hits: rep.rejected,
        shm_pos: hint.map(|h| h.to_array()),
        groups,
        chosen_parent: car.map(|c| format!("{:#x}", c.parent)),
        primary: car.map(|c| format!("{:#x}", c.primary)),
        car_bodies: car.map_or_else(Vec::new, |c| c.bodies.iter().map(|b| format!("{b:#x}")).collect()),
        excluded: car.map_or(0, |c| c.excluded),
        error: rep.chosen.as_ref().err().cloned(),
        summary: rep.summary(),
    };
    (out, rep)
}

/// Re-reads the primary body (and checks every body header) for one live sample.
pub fn live_row<M: Memory + ?Sized>(
    mem: &M,
    cfg: &ScanConfig,
    car: &bodies::CarBodies,
    t: f64,
    shm: Option<([f64; 3], [f64; 3])>,
) -> LiveRow {
    let st: Option<BodyState> = bodies::read_body(mem, &cfg.layout, car.primary).map(|b| b.state);
    let headers_ok = car.bodies.iter().all(|&b| bodies::header_ok(mem, &cfg.layout, b, cfg.vtable, car.parent));
    let (sp, sv) = (shm.map(|s| DVec3::from_array(s.0)), shm.map(|s| DVec3::from_array(s.1)));
    LiveRow {
        t,
        body_pos: st.map(|s| s.pos.to_array()),
        body_vel: st.map(|s| s.lin_vel.to_array()),
        shm_pos: sp.map(|v| v.to_array()),
        shm_vel: sv.map(|v| v.to_array()),
        pos_err_m: st.zip(sp).map(|(s, p)| (s.pos - p).length()),
        pos_tol_m: st.map(|s| cfg.select.position_tolerance(s.lin_vel.length())),
        pos_err_extrapolated_m: st.zip(sp.zip(sv)).map(|(s, (p, v))| (s.pos - (p + v * cfg.select.shm_lag_s)).length()),
        vel_err_ms: st.zip(sv).map(|(s, v)| (s.lin_vel - v).length()),
        headers_ok,
    }
}

pub fn print_report(r: &FindCarReport) {
    println!(
        "vtable {} (module {} + {}): {} regions, {:.0} MiB scanned ({:.1} MiB unreadable) in {:.2} s",
        r.vtable, r.module_base, r.vtable_rva, r.regions, r.scanned_mib, r.unreadable_mib, r.seconds
    );
    println!(
        "{} vtable hits{}, {} not valid bodies, {} groups; shm pos {}",
        r.vtable_hits,
        if r.truncated { " (TRUNCATED)" } else { "" },
        r.rejected_hits,
        r.groups.len(),
        r.shm_pos.map_or("unavailable".into(), |p| format!("({:.3}, {:.3}, {:.3})", p[0], p[1], p[2]))
    );
    for g in &r.groups {
        println!(
            "{} group {:>3} parent {} {:>3} bodies centroid ({:.2}, {:.2}, {:.2}) dist {}",
            if g.chosen { "=>" } else { "  " },
            g.index,
            g.parent,
            g.bodies,
            g.centroid[0],
            g.centroid[1],
            g.centroid[2],
            g.dist_to_shm.map_or("-".into(), |d| format!("{d:.2} m"))
        );
        for b in &g.body_rows {
            println!(
                "      {}{} pos ({:.3}, {:.3}, {:.3}) d {} main {} vel ({:.2}, {:.2}, {:.2}) ang ({:.2}, {:.2}, {:.2}) q ({:.3}, {:.3}, {:.3}, {:.3})",
                if b.primary {
                    "*"
                } else if b.in_car {
                    "+"
                } else {
                    " "
                },
                b.addr,
                b.pos[0],
                b.pos[1],
                b.pos[2],
                b.dist_to_shm.map_or("-".into(), |d| format!("{d:.3}")),
                b.dist_to_primary.map_or("-".into(), |d| format!("{d:.2}")),
                b.lin_vel[0],
                b.lin_vel[1],
                b.lin_vel[2],
                b.ang_vel[0],
                b.ang_vel[1],
                b.ang_vel[2],
                b.rot[0],
                b.rot[1],
                b.rot[2],
                b.rot[3]
            );
        }
    }
    match (&r.primary, &r.error) {
        (Some(p), _) => println!(
            "car: parent {} main body {} (*), {} car bodies (* and +), {} other bodies of the group excluded",
            r.chosen_parent.as_deref().unwrap_or("?"),
            p,
            r.car_bodies.len(),
            r.excluded
        ),
        (None, Some(e)) => println!("no car: {e}"),
        _ => {}
    }
}

pub fn print_live(l: &LiveRow) {
    let v3 = |v: Option<[f64; 3]>| v.map_or("-".into(), |v| format!("({:.3}, {:.3}, {:.3})", v[0], v[1], v[2]));
    println!(
        "t {:>6.2}s body pos {} vel {} | shm pos {} vel {} | err pos {}{} (lag-extrapolated {}) vel {}{}",
        l.t,
        v3(l.body_pos),
        v3(l.body_vel),
        v3(l.shm_pos),
        v3(l.shm_vel),
        l.pos_err_m.map_or("-".into(), |e| format!("{e:.3} m")),
        match (l.pos_err_m, l.pos_tol_m) {
            (Some(e), Some(t)) => format!(" / tol {t:.2} m{}", if e > t { " MISMATCH" } else { "" }),
            _ => String::new(),
        },
        l.pos_err_extrapolated_m.map_or("-".into(), |e| format!("{e:.3} m")),
        l.vel_err_ms.map_or("-".into(), |e| format!("{e:.3} m/s")),
        if l.headers_ok { "" } else { "  HEADERS CHANGED (car moved / stage reloaded)" }
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::FakeMem;

    const BASE: usize = 0x1_4000_0000;
    const RVA: u64 = 0xa8d9830;
    const POOL: usize = 0x2_5000_0000;

    fn sigs() -> acr_ue::Signatures {
        acr_ue::Signatures::from_toml_str(include_str!("../../../config/signatures.toml")).unwrap()
    }

    /// Body bytes per the shipped layout: identity-yaw rotation, given position / velocity.
    fn put_body(m: &mut FakeMem, obj: usize, parent: u64, pos: [f64; 3], vel: [f64; 3]) {
        m.put_u64(obj, BASE as u64 + RVA);
        m.put_u64(obj + 8, parent);
        let f = |v: &[f64]| v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>();
        m.put(obj + 0x138, &f(&pos));
        m.put(obj + 0x158, &f(&[1.0, 0.0, 0.0]));
        m.put(obj + 0x178, &f(&[0.0, 1.0, 0.0]));
        m.put(obj + 0x198, &f(&[0.0, 0.0, 1.0]));
        m.put(obj + 0x1d8, &f(&vel));
        m.put(obj + 0x1f8, &f(&[0.0, 0.0, 0.1]));
    }

    fn world() -> FakeMem {
        let mut m = FakeMem::new();
        m.add(POOL, vec![0; 0x300 * 16], RegionKind::Private, true);
        // Same vtable bytes in a read-only image page must be ignored.
        m.add(0x1_5000_0000, (BASE as u64 + RVA).to_le_bytes().repeat(8), RegionKind::Image, false);
        let obj = |i: usize| POOL + i * 0x300 + 8;
        put_body(&mut m, obj(2), 0x3_0000_1000, [10.0, 1.0, -5.0], [3.0, 0.0, 0.0]);
        put_body(&mut m, obj(3), 0x3_0000_1000, [10.0, 1.0, -5.0], [3.0, 0.0, 0.0]);
        put_body(&mut m, obj(4), 0x3_0000_1000, [11.2, 0.6, -5.0], [3.0, 0.0, 0.0]);
        // World objects sharing the car's parent (seen live): far away, and near but moving
        // differently.
        put_body(&mut m, obj(6), 0x3_0000_1000, [790.0, 1.0, -5.0], [0.0; 3]);
        put_body(&mut m, obj(7), 0x3_0000_1000, [12.0, 1.0, -5.0], [-3.0, 0.0, 0.0]);
        put_body(&mut m, obj(9), 0x3_0000_2000, [400.0, 1.0, 0.0], [0.0; 3]);
        m
    }

    #[test]
    fn remote_scan_regions_are_private_writable_only() {
        let m = world();
        let r = RemoteScan(&m).scan_regions();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].base, POOL);
        assert!(!RemoteScan(&m).write(POOL, &[0]));
    }

    #[test]
    fn finds_the_car_with_the_shared_locator() {
        let m = world();
        let src = RemoteScan(&m);
        let cfg = scan_config(&sigs().raw_offsets.sim_car, BASE, &FindCarOptions::default()).unwrap();
        let calls = std::cell::Cell::new(0);
        let shm = || {
            calls.set(calls.get() + 1);
            Some([10.2, 1.0, -5.0])
        };
        let (r, rep) = find_car(&src, BASE, &cfg, RVA, &shm, false);
        assert_eq!(calls.get(), 1, "shm is read once, after the sweep");
        assert_eq!(r.vtable_hits, 6, "{}", r.summary);
        assert_eq!(r.groups.len(), 2);
        assert_eq!(r.chosen_parent.as_deref(), Some("0x300001000"));
        assert_eq!(r.primary, Some(format!("{:#x}", POOL + 2 * 0x300 + 8)));
        assert_eq!(r.car_bodies.len(), 3, "far and parked group mates excluded");
        assert_eq!(r.excluded, 2);
        let g = r.groups.iter().find(|g| g.chosen).unwrap();
        assert_eq!(g.body_rows.len(), 5);
        assert_eq!(g.body_rows.iter().filter(|b| b.in_car).count(), 3);
        assert!(g.body_rows.iter().filter(|b| !b.in_car).all(|b| b.dist_to_primary.unwrap() > 1.5));
        assert!(r.groups.iter().filter(|g| !g.chosen).all(|g| g.body_rows.is_empty()));
        let car = rep.chosen.unwrap().1;
        let l = live_row(&src, &cfg, &car, 0.0, Some(([10.0, 1.0, -5.0], [3.0, 0.0, 0.5])));
        assert!(l.headers_ok);
        assert!(l.pos_err_m.unwrap() < 1e-9);
        assert!((l.vel_err_ms.unwrap() - 0.5).abs() < 1e-9);
        let speed = DVec3::from_array(l.body_vel.unwrap()).length();
        assert!((l.pos_tol_m.unwrap() - (2.0 + speed * 0.06)).abs() < 1e-9, "speed-aware tolerance");
        let ahead = DVec3::from_array([10.0, 1.0, -5.0]) + DVec3::new(3.0, 0.0, 0.5) * 0.06;
        let body = DVec3::from_array(l.body_pos.unwrap());
        assert!((l.pos_err_extrapolated_m.unwrap() - (body - ahead).length()).abs() < 1e-9);
    }

    #[test]
    fn options_override_and_missing_hint_is_reported() {
        let m = world();
        let src = RemoteScan(&m);
        let o = FindCarOptions { vtable_rva: Some(0x1234), tolerance_m: Some(0.5), radius_m: Some(7.0), all: true };
        let cfg = scan_config(&sigs().raw_offsets.sim_car, BASE, &o).unwrap();
        assert_eq!(cfg.vtable, BASE + 0x1234);
        assert_eq!((cfg.select.tolerance_m, cfg.select.radius_m), (0.5, 7.0));
        let cfg = scan_config(&sigs().raw_offsets.sim_car, BASE, &FindCarOptions { all: true, ..Default::default() })
            .unwrap();
        let (r, _) = find_car(&src, BASE, &cfg, RVA, &|| None, true);
        assert!(r.primary.is_none());
        assert!(r.error.as_deref().unwrap().contains("shm car position unavailable"));
        assert!(r.groups.iter().all(|g| !g.body_rows.is_empty()), "--all lists every group");
    }
}
