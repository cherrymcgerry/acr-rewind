//! Ground truth from the game's shared memory (or a JSON file, for synthetic runs/tests).

use crate::variants::{scalar_variants, vec3_variants, Variant};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One reading of the shm values the probe cares about (f64 for convenience).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShmSample {
    pub packet_id: i32,
    pub status: String,
    /// `graphics.carCoordinates` of the player car (m).
    pub pos: [f64; 3],
    /// "player" if found via playerCarID, "car0" if the first table entry was used.
    pub pos_source: String,
    /// World velocity (m/s).
    pub vel: [f64; 3],
    pub local_vel: [f64; 3],
    /// Car-local angular velocity (rad/s).
    pub local_ang_vel: [f64; 3],
    pub heading: f64,
    pub pitch: f64,
    pub roll: f64,
    pub rpm: f64,
    /// -1 R, 0 N, 1.. forward.
    pub gear: i32,
    /// Raw shm gear (0 R, 1 N, 2 = 1st).
    pub gear_raw: i32,
    /// FL, FR, RL, RR (rad/s).
    pub wheel_speed: [f64; 4],
    pub speed_kmh: f64,
}

impl ShmSample {
    pub fn speed(&self) -> f64 {
        let v = self.vel;
        (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
    }

    pub fn from_snapshot(s: &acr_shm::Snapshot, raw_g: &acr_shm::layout::RawGraphics) -> Self {
        let p = &s.physics;
        let f3 = |v: [f32; 3]| v.map(f64::from);
        let (pos, src) = match acr_shm::car_position(raw_g) {
            Some((v, acr_shm::CarPosSource::Player)) => (f3(v), "player"),
            Some((v, acr_shm::CarPosSource::Car0)) => (f3(v), "car0"),
            None => (f3({ raw_g.carCoordinates }[0]), "none"),
        };
        Self {
            packet_id: p.packet_id,
            status: s.graphics.status.to_string(),
            pos,
            pos_source: src.into(),
            vel: f3(p.velocity),
            local_vel: f3(p.local_velocity),
            local_ang_vel: f3(p.local_angular_vel),
            heading: p.heading.into(),
            pitch: p.pitch.into(),
            roll: p.roll.into(),
            rpm: p.rpm.into(),
            gear: p.gear,
            gear_raw: p.gear + 1,
            wheel_speed: std::array::from_fn(|i| p.wheels[i].angular_speed.into()),
            speed_kmh: p.speed_kmh.into(),
        }
    }
}

pub enum Oracle {
    Live(acr_shm::SharedMemory),
    /// Re-read on every sample, so a test harness can rewrite the file between steps.
    File(PathBuf),
}

impl Oracle {
    pub fn open(file: Option<&Path>) -> Result<Self> {
        match file {
            Some(p) => Ok(Self::File(p.to_path_buf())),
            None => Ok(Self::Live(acr_shm::SharedMemory::open().context("opening AC Rally shared memory")?)),
        }
    }

    pub fn sample(&self) -> Result<ShmSample> {
        match self {
            Self::Live(shm) => {
                let raw_g = shm.read_raw_graphics();
                Ok(ShmSample::from_snapshot(&shm.snapshot(), &raw_g))
            }
            Self::File(p) => {
                let s = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
                Ok(serde_json::from_str(&s).with_context(|| format!("parsing {}", p.display()))?)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    /// graphics.carCoordinates (m)
    Pos,
    /// physics.velocity, world (m/s)
    Vel,
    /// physics.localVelocity (m/s)
    LocalVel,
    /// physics.localAngularVel (rad/s)
    AngVel,
    /// physics.rpms
    Rpm,
    /// physics.wheelAngularSpeed[--wheel] (rad/s)
    WheelSpeed,
    /// --value
    Custom,
}

impl Target {
    pub fn arity(self) -> usize {
        match self {
            Self::Pos | Self::Vel | Self::LocalVel | Self::AngVel => 3,
            _ => 1,
        }
    }

    pub fn default_tol(self) -> f64 {
        match self {
            Self::Pos => 0.05,
            Self::Vel | Self::LocalVel => 0.3,
            Self::AngVel => 0.05,
            Self::Rpm => 30.0,
            Self::WheelSpeed => 0.5,
            Self::Custom => 0.001,
        }
    }

    /// Unit scales tried by default (memory units per shm unit).
    pub fn default_scales(self) -> Vec<f64> {
        use std::f64::consts::TAU;
        match self {
            Self::Pos | Self::Vel | Self::LocalVel => vec![1.0, 100.0],
            Self::AngVel | Self::Custom => vec![1.0],
            Self::Rpm => vec![1.0, TAU / 60.0],
            Self::WheelSpeed => vec![1.0, 60.0 / TAU],
        }
    }

    pub fn variants(self, scales: &[f64], permute: bool) -> Vec<Variant> {
        match self.arity() {
            3 => vec3_variants(scales, permute),
            _ => scalar_variants(scales, matches!(self, Self::WheelSpeed | Self::Custom) && permute),
        }
    }

    pub fn values(self, s: &ShmSample, wheel: usize, custom: Option<f64>) -> Result<Vec<f64>> {
        Ok(match self {
            Self::Pos => s.pos.to_vec(),
            Self::Vel => s.vel.to_vec(),
            Self::LocalVel => s.local_vel.to_vec(),
            Self::AngVel => s.local_ang_vel.to_vec(),
            Self::Rpm => vec![s.rpm],
            Self::WheelSpeed => {
                if wheel > 3 {
                    bail!("--wheel must be 0..=3 (FL, FR, RL, RR)");
                }
                vec![s.wheel_speed[wheel]]
            }
            Self::Custom => vec![custom.context("--target custom needs --value")?],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_oracle_and_targets() {
        let dir = std::env::temp_dir().join(format!("acr_probe_oracle_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("shm.json");
        std::fs::write(&p, r#"{"pos":[1,2,3],"vel":[3,0,4],"rpm":4000,"wheel_speed":[1,2,3,4]}"#).unwrap();
        let o = Oracle::open(Some(&p)).unwrap();
        let s = o.sample().unwrap();
        assert_eq!(s.speed(), 5.0);
        assert_eq!(Target::Pos.values(&s, 0, None).unwrap(), vec![1.0, 2.0, 3.0]);
        assert_eq!(Target::WheelSpeed.values(&s, 2, None).unwrap(), vec![3.0]);
        assert!(Target::WheelSpeed.values(&s, 4, None).is_err());
        assert!(Target::Custom.values(&s, 0, None).is_err());
        assert_eq!(Target::Custom.values(&s, 0, Some(7.5)).unwrap(), vec![7.5]);
        assert_eq!(Target::Pos.variants(&Target::Pos.default_scales(), true).len(), 96);
        assert_eq!(Target::Rpm.variants(&Target::Rpm.default_scales(), true).len(), 2);
        assert_eq!(Target::WheelSpeed.variants(&[1.0], true).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
