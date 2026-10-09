//! Rotation conversions matching Unreal Engine's `FRotator` <-> `FQuat` math, plus 3x3
//! matrix helpers for the sim-car backend.

use glam::{DMat3, DQuat, DVec3};

/// UE `FRotator` in degrees.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rotator {
    pub pitch: f64,
    pub yaw: f64,
    pub roll: f64,
}

/// `FRotator::Quaternion()`.
pub fn rotator_to_quat(r: Rotator) -> DQuat {
    let half = std::f64::consts::PI / 360.0;
    let (sp, cp) = (r.pitch * half).sin_cos();
    let (sy, cy) = (r.yaw * half).sin_cos();
    let (sr, cr) = (r.roll * half).sin_cos();
    DQuat::from_xyzw(
        cr * sp * sy - sr * cp * cy,
        -cr * sp * cy - sr * cp * sy,
        cr * cp * sy - sr * sp * cy,
        cr * cp * cy + sr * sp * sy,
    )
}

fn normalize_axis(a: f64) -> f64 {
    let mut a = a % 360.0;
    if a > 180.0 {
        a -= 360.0;
    } else if a < -180.0 {
        a += 360.0;
    }
    a
}

/// `FQuat::Rotator()`.
pub fn quat_to_rotator(q: DQuat) -> Rotator {
    let (x, y, z, w) = (q.x, q.y, q.z, q.w);
    let singularity = z * x - w * y;
    let yaw_y = 2.0 * (w * z + x * y);
    let yaw_x = 1.0 - 2.0 * (y * y + z * z);
    const THRESHOLD: f64 = 0.499_999_5;
    let yaw = yaw_y.atan2(yaw_x).to_degrees();
    if singularity < -THRESHOLD {
        Rotator { pitch: -90.0, yaw, roll: normalize_axis(-yaw - 2.0 * x.atan2(w).to_degrees()) }
    } else if singularity > THRESHOLD {
        Rotator { pitch: 90.0, yaw, roll: normalize_axis(yaw - 2.0 * x.atan2(w).to_degrees()) }
    } else {
        Rotator {
            pitch: (2.0 * singularity).asin().to_degrees(),
            yaw,
            roll: (-2.0 * (w * x + y * z)).atan2(1.0 - 2.0 * (x * x + y * y)).to_degrees(),
        }
    }
}

/// Builds a rotation from 9 matrix values stored row-by-row (`rows = true`) or
/// column-by-column. Re-orthonormalises to tolerate float noise.
pub fn quat_from_matrix(m: [f64; 9], rows: bool) -> Option<DQuat> {
    let a = DVec3::new(m[0], m[1], m[2]);
    let b = DVec3::new(m[3], m[4], m[5]);
    let c = DVec3::new(m[6], m[7], m[8]);
    let mat = if rows { DMat3::from_cols(a, b, c).transpose() } else { DMat3::from_cols(a, b, c) };
    if !mat.is_finite() || (mat.determinant() - 1.0).abs() > 0.05 {
        return None;
    }
    Some(DQuat::from_mat3(&mat).normalize())
}

/// Inverse of [`quat_from_matrix`].
pub fn quat_to_matrix(q: DQuat, rows: bool) -> [f64; 9] {
    let m = DMat3::from_quat(q.normalize());
    let m = if rows { m.transpose() } else { m };
    let c = m.to_cols_array();
    [c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7], c[8]]
}

pub const CM_PER_M: f64 = 100.0;

/// Signed axis permutation from UE world space (metres) to the sim / shm frame:
/// `sim[i] = sign[i] * ue[axis[i]]`. Lengths are converted cm <-> m separately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AxisMap {
    pub axis: [usize; 3],
    pub sign: [i8; 3],
}

impl AxisMap {
    pub const IDENTITY: Self = Self { axis: [0, 1, 2], sign: [1, 1, 1] };

    /// Parses `["+x", "-z", "+y"]` (entry i = which signed UE axis becomes sim axis i).
    pub fn parse(spec: &[String]) -> Result<Self, String> {
        if spec.len() != 3 {
            return Err(format!("axis map needs 3 entries, got {}", spec.len()));
        }
        let mut m = Self::IDENTITY;
        for (i, s) in spec.iter().enumerate() {
            let s = s.trim().to_ascii_lowercase();
            let (sign, name) = match s.strip_prefix('-') {
                Some(rest) => (-1, rest),
                None => (1, s.strip_prefix('+').unwrap_or(&s)),
            };
            m.axis[i] = match name {
                "x" => 0,
                "y" => 1,
                "z" => 2,
                _ => return Err(format!("bad axis '{s}' (use +x, -y, ...)")),
            };
            m.sign[i] = sign;
        }
        let mut seen = [false; 3];
        for a in m.axis {
            if std::mem::replace(&mut seen[a], true) {
                return Err(format!("axis map {spec:?} is not a permutation"));
            }
        }
        Ok(m)
    }

    /// All 48 signed permutations.
    pub fn all() -> Vec<Self> {
        const PERMS: [[usize; 3]; 6] = [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]];
        let mut out = Vec::with_capacity(48);
        for axis in PERMS {
            for s in 0..8u8 {
                let sign = [0, 1, 2].map(|k| if (s >> k) & 1 == 1 { -1 } else { 1 });
                out.push(Self { axis, sign });
            }
        }
        out
    }

    /// The matrix `M` with `sim = M * ue`.
    pub fn matrix(&self) -> DMat3 {
        let mut rows = [DVec3::ZERO; 3];
        for (i, r) in rows.iter_mut().enumerate() {
            r[self.axis[i]] = f64::from(self.sign[i]);
        }
        DMat3::from_cols(rows[0], rows[1], rows[2]).transpose()
    }

    /// +1 for a proper rotation, -1 if the map flips handedness (UE is left-handed).
    pub fn det(&self) -> f64 {
        self.matrix().determinant()
    }

    pub fn ue_to_sim(&self, v: DVec3) -> DVec3 {
        self.matrix() * v
    }

    pub fn sim_to_ue(&self, v: DVec3) -> DVec3 {
        self.matrix().transpose() * v
    }

    /// Orientation: `R_sim = M R_ue M^T` (a proper rotation even when `det M = -1`).
    pub fn ue_to_sim_rot(&self, q: DQuat) -> DQuat {
        let m = self.matrix();
        DQuat::from_mat3(&(m * DMat3::from_quat(q) * m.transpose())).normalize()
    }

    pub fn sim_to_ue_rot(&self, q: DQuat) -> DQuat {
        let m = self.matrix();
        DQuat::from_mat3(&(m.transpose() * DMat3::from_quat(q) * m)).normalize()
    }

    /// Angular velocity is a pseudovector: it also flips sign under a handedness change.
    pub fn ue_to_sim_ang_vel(&self, w: DVec3) -> DVec3 {
        self.det() * self.ue_to_sim(w)
    }

    pub fn sim_to_ue_ang_vel(&self, w: DVec3) -> DVec3 {
        self.det() * self.sim_to_ue(w)
    }

    /// `sim = (+ue.y, -ue.z, +ue.x)` style description.
    pub fn describe(&self) -> String {
        let parts: Vec<String> = (0..3)
            .map(|i| format!("{}ue.{}", if self.sign[i] < 0 { '-' } else { '+' }, ['x', 'y', 'z'][self.axis[i]]))
            .collect();
        format!("sim = ({})", parts.join(", "))
    }

    /// The same map in signatures.toml syntax.
    pub fn to_toml(&self) -> String {
        let parts: Vec<String> = (0..3)
            .map(|i| format!("\"{}{}\"", if self.sign[i] < 0 { '-' } else { '+' }, ['x', 'y', 'z'][self.axis[i]]))
            .collect();
        format!("[{}]", parts.join(", "))
    }
}

/// UE centimetres -> metres.
pub fn cm_to_m(v: DVec3) -> DVec3 {
    v / CM_PER_M
}

/// Metres -> UE centimetres.
pub fn m_to_cm(v: DVec3) -> DVec3 {
    v * CM_PER_M
}

/// Accumulates position pairs (UE metres, sim metres) and decides which [`AxisMap`] relates
/// them. A map is accepted once it fits every pair within `tol` while every other map has
/// been off by more than `2 * tol` at least once (so near-zero coordinates can't fool it).
#[derive(Clone, Debug)]
pub struct AxisFit {
    tol: f64,
    worst: Vec<(AxisMap, f64)>,
    samples: usize,
}

impl AxisFit {
    pub fn new(tol: f64) -> Self {
        Self { tol, worst: AxisMap::all().into_iter().map(|m| (m, 0.0)).collect(), samples: 0 }
    }

    pub fn add(&mut self, ue_m: DVec3, sim: DVec3) {
        if !(ue_m.is_finite() && sim.is_finite()) {
            return;
        }
        self.samples += 1;
        for (m, w) in &mut self.worst {
            *w = w.max((m.ue_to_sim(ue_m) - sim).length());
        }
    }

    pub fn samples(&self) -> usize {
        self.samples
    }

    /// Best map and its worst-case error so far.
    pub fn best(&self) -> Option<(AxisMap, f64)> {
        self.worst.iter().copied().min_by(|a, b| a.1.total_cmp(&b.1)).filter(|_| self.samples > 0)
    }

    pub fn resolved(&self) -> Option<(AxisMap, f64)> {
        let (best, err) = self.best()?;
        let ambiguous = self.worst.iter().any(|(m, e)| *m != best && *e <= 2.0 * self.tol);
        (err <= self.tol && !ambiguous).then_some((best, err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn axis_map_parse_and_describe() {
        let m = AxisMap::parse(&["+y".into(), "-z".into(), "x".into()]).unwrap();
        assert_eq!(m.axis, [1, 2, 0]);
        assert_eq!(m.sign, [1, -1, 1]);
        assert_eq!(m.describe(), "sim = (+ue.y, -ue.z, +ue.x)");
        assert_eq!(m.to_toml(), "[\"+y\", \"-z\", \"+x\"]");
        assert!(AxisMap::parse(&["+x".into(), "+x".into(), "+y".into()]).unwrap_err().contains("permutation"));
        assert!(AxisMap::parse(&["+x".into(), "+w".into(), "+y".into()]).is_err());
        assert!(AxisMap::parse(&["+x".into()]).is_err());
        assert_eq!(AxisMap::all().len(), 48);
    }

    #[test]
    fn axis_map_vectors_rotations_and_units() {
        let ue_cm = DVec3::new(12_345.0, -6_789.0, 250.0);
        let ue = cm_to_m(ue_cm);
        assert_eq!(ue, DVec3::new(123.45, -67.89, 2.5));
        assert_eq!(m_to_cm(ue), ue_cm);
        for m in AxisMap::all() {
            let sim = m.ue_to_sim(ue);
            for i in 0..3 {
                assert_eq!(sim[i], f64::from(m.sign[i]) * ue[m.axis[i]]);
            }
            assert!((m.sim_to_ue(sim) - ue).length() < 1e-12);
            let q = DQuat::from_euler(glam::EulerRot::ZYX, 0.7, -0.2, 0.1);
            let qs = m.ue_to_sim_rot(q);
            assert!((qs.length() - 1.0).abs() < 1e-9);
            // Rotating a mapped vector in sim == mapping the rotated vector.
            let v = DVec3::new(1.0, 2.0, -0.5);
            assert!((qs * m.ue_to_sim(v) - m.ue_to_sim(q * v)).length() < 1e-9, "{m:?}");
            assert!(m.sim_to_ue_rot(qs).angle_between(q) < 1e-6);
            // Angular velocity: omega x r must map consistently (pseudovector rule).
            let w = DVec3::new(0.1, -0.4, 0.9);
            let lhs = m.ue_to_sim_ang_vel(w).cross(m.ue_to_sim(v));
            assert!((lhs - m.ue_to_sim(w.cross(v))).length() < 1e-9, "{m:?}");
            assert!((m.sim_to_ue_ang_vel(m.ue_to_sim_ang_vel(w)) - w).length() < 1e-12);
        }
    }

    #[test]
    fn axis_fit_needs_unambiguous_samples() {
        let truth = AxisMap::parse(&["+y".into(), "+z".into(), "+x".into()]).unwrap();
        let mut f = AxisFit::new(1.0);
        assert!(f.resolved().is_none());
        // z ~ 0: the sign of that axis can't be told apart yet.
        f.add(DVec3::new(100.0, -40.0, 0.1), truth.ue_to_sim(DVec3::new(100.0, -40.0, 0.1)));
        assert!(f.resolved().is_none());
        assert_eq!(
            f.best().unwrap().0.ue_to_sim(DVec3::new(100.0, -40.0, 0.1)),
            truth.ue_to_sim(DVec3::new(100.0, -40.0, 0.1))
        );
        let p = DVec3::new(130.0, -35.0, 12.0);
        f.add(p, truth.ue_to_sim(p) + DVec3::splat(0.3));
        let (m, err) = f.resolved().unwrap();
        assert_eq!(m, truth);
        assert!(err < 1.0);
        // A world-origin offset breaks every map: never resolved.
        let mut g = AxisFit::new(1.0);
        g.add(p, truth.ue_to_sim(p) + DVec3::new(500.0, 0.0, 0.0));
        assert!(g.resolved().is_none());
    }

    #[test]
    fn yaw_only_matches_ue() {
        let q = rotator_to_quat(Rotator { pitch: 0.0, yaw: 90.0, roll: 0.0 });
        // UE: yaw 90 rotates around +Z.
        assert!(q.angle_between(DQuat::from_rotation_z(90f64.to_radians())) < 1e-9);
    }

    #[test]
    fn rotator_roundtrip() {
        for &(p, y, r) in
            &[(0.0, 0.0, 0.0), (10.0, 20.0, 30.0), (-45.0, 170.0, -120.0), (89.0, -90.0, 5.0), (-30.0, -179.0, 179.0)]
        {
            let rot = Rotator { pitch: p, yaw: y, roll: r };
            let back = quat_to_rotator(rotator_to_quat(rot));
            assert!(close(back.pitch, p) && close(back.yaw, y) && close(back.roll, r), "{rot:?} -> {back:?}");
        }
    }

    #[test]
    fn gimbal_lock_is_finite_and_equivalent() {
        let rot = Rotator { pitch: 90.0, yaw: 30.0, roll: 10.0 };
        let q = rotator_to_quat(rot);
        let back = quat_to_rotator(q);
        assert_eq!(back.pitch, 90.0);
        assert!(rotator_to_quat(back).angle_between(q) < 1e-6);
    }

    #[test]
    fn matrix_roundtrip_both_layouts() {
        let q = DQuat::from_euler(glam::EulerRot::YXZ, 0.3, -0.2, 1.1);
        for rows in [true, false] {
            let m = quat_to_matrix(q, rows);
            let back = quat_from_matrix(m, rows).unwrap();
            assert!(back.angle_between(q) < 1e-9);
        }
        let rows = quat_to_matrix(q, true);
        let cols = quat_to_matrix(q, false);
        assert_ne!(rows, cols);
        // Row-major layout: first three values are the first row of the rotation matrix.
        let m = DMat3::from_quat(q);
        assert!(close(rows[1], m.row(0).y));
    }

    #[test]
    fn rejects_garbage_matrix() {
        assert!(quat_from_matrix([0.0; 9], true).is_none());
        assert!(quat_from_matrix([f64::NAN; 9], true).is_none());
        let scaled = [2.0, 0.0, 0.0, 0.0, 2.0, 0.0, 0.0, 0.0, 2.0];
        assert!(quat_from_matrix(scaled, true).is_none());
    }
}
