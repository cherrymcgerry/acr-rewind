//! Cross-checks between captured snapshots and shared memory.
//!
//! Snapshots live in the backend's own frame, which may differ from the shared-memory frame,
//! so only frame-independent quantities are compared: speed magnitudes, displacement
//! distances, rpm, gear and wheel spin.

use rewind_core::{CarSnapshot, DVec3};

/// The subset of `acpmf_physics` / `acpmf_graphics` used for validation.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ShmSample {
    pub packet_id: i32,
    pub velocity: [f32; 3],
    pub rpm: i32,
    pub gear: i32,
    pub wheel_speed: [f32; 4],
    /// Graphics carCoordinates (stays valid when the physics page is dead).
    pub position: Option<[f32; 3]>,
    /// The physics page reads all-zero (seen after crashes / stage restarts): velocity, rpm,
    /// gear and wheel values are meaningless.
    pub physics_dead: bool,
}

impl ShmSample {
    pub fn speed(&self) -> f64 {
        vec3(self.velocity).length()
    }
    pub fn pos(&self) -> Option<DVec3> {
        self.position.map(vec3)
    }
}

/// ACR's physics page can go all-zero (after a crash / stage restart) while packetId keeps
/// rising. A live car never has heading, pitch and roll all exactly 0.
pub fn physics_page_dead(p: &acr_shm::Physics) -> bool {
    p.velocity == [0.0; 3] && p.rpm == 0 && p.heading == 0.0 && p.pitch == 0.0 && p.roll == 0.0
}

fn vec3(v: [f32; 3]) -> DVec3 {
    DVec3::new(v[0] as f64, v[1] as f64, v[2] as f64)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Comparison {
    pub speed_snap: f64,
    pub speed_shm: f64,
    pub rpm_snap: f64,
    pub rpm_shm: i32,
    pub gear_snap: i32,
    pub gear_shm: i32,
    pub wheel_snap: [f64; 4],
    pub wheel_shm: [f32; 4],
    /// Displacement since the previous comparison: (snapshot frame, shm frame).
    pub displacement: Option<(f64, f64)>,
    pub physics_dead: bool,
}

impl Comparison {
    /// `None` when the physics page is dead.
    pub fn speed_ok(&self) -> Option<bool> {
        (!self.physics_dead)
            .then(|| (self.speed_snap - self.speed_shm).abs() <= 0.5 + 0.05 * self.speed_shm.max(self.speed_snap))
    }

    /// Ratio of snapshot / shm displacement; ~1.0 when units agree.
    pub fn displacement_ratio(&self) -> Option<f64> {
        let (a, b) = self.displacement?;
        (b > 0.5).then(|| a / b)
    }

    pub fn summary(&self) -> String {
        format!(
            "speed snap {:.2} / shm {:.2} m/s [{}], rpm {:.0} / {}, gear {} / {}, wheels {:.1?} / {:.1?}, displacement ratio {}",
            self.speed_snap,
            self.speed_shm,
            match self.speed_ok() {
                Some(true) => "ok",
                Some(false) => "MISMATCH",
                None => "physics page all-zero",
            },
            self.rpm_snap,
            self.rpm_shm,
            self.gear_snap,
            self.gear_shm,
            self.wheel_snap,
            self.wheel_shm,
            self.displacement_ratio()
                .map_or("n/a".into(), |r| format!("{r:.3}")),
        )
    }
}

pub fn compare(snap: &CarSnapshot, shm: &ShmSample, prev: Option<(&CarSnapshot, &ShmSample)>) -> Comparison {
    let displacement = prev.and_then(|(ps, pm)| {
        let a = (snap.pos - ps.pos).length();
        let b = (shm.pos()? - pm.pos()?).length();
        Some((a, b))
    });
    Comparison {
        speed_snap: snap.lin_vel.length(),
        speed_shm: shm.speed(),
        rpm_snap: snap.rpm,
        rpm_shm: shm.rpm,
        gear_snap: snap.gear,
        gear_shm: shm.gear,
        wheel_snap: std::array::from_fn(|i| snap.wheels[i].angular_vel),
        wheel_shm: shm.wheel_speed,
        displacement,
        physics_dead: shm.physics_dead,
    }
}

/// Context for the check run shortly after a resume.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResumeCheck {
    /// Live state when the rewind started (snapshot frame) and the shm sample at that time.
    pub start_snap: CarSnapshot,
    pub start_shm: Option<ShmSample>,
    /// State written on release.
    pub resumed: CarSnapshot,
    /// Seconds since release.
    pub elapsed: f64,
}

/// Seconds after release at which the check runs.
pub const RESUME_CHECK_DELAY: f64 = 0.15;

/// `Err(reason)` if shared memory shows the restore did not reach the simulation.
pub fn check_resume(c: &ResumeCheck, now: &ShmSample) -> Result<(), String> {
    // 1) Displacement: rewinding moved the car `d_snap` metres; the sim must have moved too.
    let d_snap = (c.start_snap.pos - c.resumed.pos).length();
    if let (Some(start), Some(now_pos)) = (c.start_shm.and_then(|s| s.pos()), now.pos()) {
        let d_shm = (start - now_pos).length();
        // Allow for the distance driven since release.
        let slack = now.speed() * c.elapsed + 1.0;
        if d_snap > 5.0 && (d_shm - d_snap).abs() > slack + 0.3 * d_snap {
            return Err(format!(
                "rewound {d_snap:.1} m but the simulation moved {d_shm:.1} m: the write does not reach the sim"
            ));
        }
    }
    // 2) Speed: right after release the sim speed must be close to the restored speed.
    if now.physics_dead {
        return Ok(());
    }
    let want = c.resumed.lin_vel.length();
    let got = now.speed();
    let tol = 3.0 + 0.25 * want.max(got) + 15.0 * c.elapsed;
    if (want - got).abs() > tol {
        return Err(format!("restored speed {want:.1} m/s but the simulation reports {got:.1} m/s"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(x: f64, speed: f64) -> CarSnapshot {
        CarSnapshot {
            pos: DVec3::new(x, 0.0, 0.0),
            lin_vel: DVec3::new(speed, 0.0, 0.0),
            rpm: 4000.0,
            gear: 3,
            ..Default::default()
        }
    }

    fn shm(x: f32, speed: f32) -> ShmSample {
        ShmSample {
            velocity: [0.0, 0.0, speed],
            position: Some([0.0, 0.0, x]),
            rpm: 4000,
            gear: 3,
            ..Default::default()
        }
    }

    #[test]
    fn comparison_is_frame_independent() {
        // Different axes in the two frames: speed and displacement still agree.
        let c = compare(&snap(10.0, 20.0), &shm(110.0, 20.2), Some((&snap(0.0, 20.0), &shm(100.0, 20.0))));
        assert_eq!(c.speed_ok(), Some(true));
        assert!((c.displacement_ratio().unwrap() - 1.0).abs() < 1e-6);
        assert!(c.summary().contains("[ok]"));
        let bad = compare(&snap(10.0, 20.0), &shm(0.0, 2.0), None);
        assert_eq!(bad.speed_ok(), Some(false));
        assert_eq!(bad.displacement_ratio(), None);
        assert!(bad.summary().contains("MISMATCH"));
    }

    #[test]
    fn resume_check_passes_when_sim_follows() {
        let c = ResumeCheck {
            start_snap: snap(100.0, 30.0),
            start_shm: Some(shm(500.0, 30.0)),
            resumed: snap(40.0, 15.0),
            elapsed: 0.15,
        };
        // Sim jumped back 60 m and drives at the restored speed.
        assert_eq!(check_resume(&c, &shm(442.0, 15.5)), Ok(()));
    }

    #[test]
    fn resume_check_detects_overwritten_pose() {
        let c = ResumeCheck {
            start_snap: snap(100.0, 30.0),
            start_shm: Some(shm(500.0, 30.0)),
            resumed: snap(40.0, 30.0),
            elapsed: 0.15,
        };
        // Sim kept going from where it was (proxy teleport overwritten by the sim).
        let err = check_resume(&c, &shm(504.5, 30.0)).unwrap_err();
        assert!(err.contains("does not reach the sim"), "{err}");
    }

    #[test]
    fn resume_check_detects_speed_mismatch() {
        let c = ResumeCheck { start_snap: snap(0.0, 0.0), start_shm: None, resumed: snap(0.0, 2.0), elapsed: 0.15 };
        assert!(check_resume(&c, &shm(0.0, 40.0)).is_err());
        assert!(check_resume(&c, &shm(0.0, 3.0)).is_ok());
    }

    #[test]
    fn dead_physics_page_skips_speed_but_keeps_displacement() {
        let dead = |x: f32| ShmSample { position: Some([0.0, 0.0, x]), physics_dead: true, ..Default::default() };
        let c = ResumeCheck {
            start_snap: snap(100.0, 30.0),
            start_shm: Some(shm(500.0, 30.0)),
            resumed: snap(40.0, 30.0),
            elapsed: 0.15,
        };
        // Speed reads 0 but the page is dead: not a failure.
        assert_eq!(check_resume(&c, &dead(442.0)), Ok(()));
        // The graphics position still proves an ignored write.
        assert!(check_resume(&c, &dead(504.5)).is_err());
        let cmp = compare(&snap(0.0, 20.0), &dead(0.0), None);
        assert_eq!(cmp.speed_ok(), None);
        assert!(cmp.summary().contains("all-zero"));
    }

    #[test]
    fn physics_page_dead_heuristic() {
        let mut p = acr_shm::Physics::default();
        assert!(physics_page_dead(&p));
        p.heading = 0.3;
        assert!(!physics_page_dead(&p));
        p.heading = 0.0;
        p.rpm = 900;
        assert!(!physics_page_dead(&p));
    }
}
