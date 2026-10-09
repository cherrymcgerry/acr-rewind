use glam::{DQuat, DVec3};
use serde::{Deserialize, Serialize};

/// Per-wheel state needed for a jolt-free resume.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WheelState {
    /// Wheel spin rate around its axle (rad/s).
    pub angular_vel: f64,
    /// Steering angle of the wheel (rad).
    pub steer_angle: f64,
    /// Suspension compression (m).
    pub suspension_travel: f64,
}

impl WheelState {
    pub fn lerp(&self, other: &Self, alpha: f64) -> Self {
        Self {
            angular_vel: lerp(self.angular_vel, other.angular_vel, alpha),
            steer_angle: lerp(self.steer_angle, other.steer_angle, alpha),
            suspension_travel: lerp(self.suspension_travel, other.suspension_travel, alpha),
        }
    }
}

/// Driver inputs at capture time (normalised: pedals 0..1, steer -1..1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Inputs {
    pub throttle: f64,
    pub brake: f64,
    pub clutch: f64,
    pub handbrake: f64,
    pub steer: f64,
}

impl Inputs {
    pub fn lerp(&self, other: &Self, alpha: f64) -> Self {
        Self {
            throttle: lerp(self.throttle, other.throttle, alpha),
            brake: lerp(self.brake, other.brake, alpha),
            clutch: lerp(self.clutch, other.clutch, alpha),
            handbrake: lerp(self.handbrake, other.handbrake, alpha),
            steer: lerp(self.steer, other.steer, alpha),
        }
    }
}

/// One captured state of the player car.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CarSnapshot {
    /// Timeline time (s). Assigned by [`crate::RewindController`] when recording.
    pub t: f64,
    /// World position (m).
    pub pos: DVec3,
    /// World orientation.
    pub rot: DQuat,
    /// World linear velocity (m/s).
    pub lin_vel: DVec3,
    /// World angular velocity (rad/s).
    pub ang_vel: DVec3,
    /// Wheels in FL, FR, RL, RR order.
    pub wheels: [WheelState; 4],
    /// Gear: -1 reverse, 0 neutral, 1.. forward.
    pub gear: i32,
    /// Engine speed (rpm).
    pub rpm: f64,
    pub inputs: Inputs,
}

impl Default for CarSnapshot {
    fn default() -> Self {
        Self {
            t: 0.0,
            pos: DVec3::ZERO,
            rot: DQuat::IDENTITY,
            lin_vel: DVec3::ZERO,
            ang_vel: DVec3::ZERO,
            wheels: [WheelState::default(); 4],
            gear: 0,
            rpm: 0.0,
            inputs: Inputs::default(),
        }
    }
}

impl CarSnapshot {
    /// Interpolates between `self` (alpha = 0) and `other` (alpha = 1).
    ///
    /// Position, velocities, wheels, rpm and inputs are lerped; rotation is slerped along the
    /// shortest arc. Gear is discrete and snaps to the nearer sample.
    pub fn interpolate(&self, other: &Self, alpha: f64) -> Self {
        let alpha = alpha.clamp(0.0, 1.0);
        let wheels = std::array::from_fn(|i| self.wheels[i].lerp(&other.wheels[i], alpha));
        Self {
            t: lerp(self.t, other.t, alpha),
            pos: self.pos.lerp(other.pos, alpha),
            rot: slerp_shortest(self.rot, other.rot, alpha),
            lin_vel: self.lin_vel.lerp(other.lin_vel, alpha),
            ang_vel: self.ang_vel.lerp(other.ang_vel, alpha),
            wheels,
            gear: if alpha < 0.5 { self.gear } else { other.gear },
            rpm: lerp(self.rpm, other.rpm, alpha),
            inputs: self.inputs.lerp(&other.inputs, alpha),
        }
    }

    /// True if every float field is finite and the rotation is (roughly) normalised.
    pub fn is_valid(&self) -> bool {
        self.t.is_finite()
            && self.pos.is_finite()
            && self.rot.is_finite()
            && (self.rot.length() - 1.0).abs() < 1e-3
            && self.lin_vel.is_finite()
            && self.ang_vel.is_finite()
            && self.rpm.is_finite()
            && self
                .wheels
                .iter()
                .all(|w| w.angular_vel.is_finite() && w.steer_angle.is_finite() && w.suspension_travel.is_finite())
    }
}

#[inline]
fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

fn slerp_shortest(a: DQuat, b: DQuat, t: f64) -> DQuat {
    let b = if a.dot(b) < 0.0 { -b } else { b };
    a.slerp(b, t).normalize()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::{FRAC_PI_2, PI};

    fn snap(t: f64, x: f64) -> CarSnapshot {
        CarSnapshot { t, pos: DVec3::new(x, 0.0, 0.0), ..Default::default() }
    }

    #[test]
    fn interpolate_endpoints_and_midpoint() {
        let a = CarSnapshot { rpm: 1000.0, gear: 2, ..snap(0.0, 0.0) };
        let b = CarSnapshot { rpm: 3000.0, gear: 3, ..snap(1.0, 10.0) };
        assert_eq!(a.interpolate(&b, 0.0), a);
        let end = a.interpolate(&b, 1.0);
        assert_eq!(end.pos, b.pos);
        assert_eq!(end.gear, 3);
        let mid = a.interpolate(&b, 0.5);
        assert!((mid.t - 0.5).abs() < 1e-12);
        assert!((mid.pos.x - 5.0).abs() < 1e-12);
        assert!((mid.rpm - 2000.0).abs() < 1e-12);
        assert_eq!(a.interpolate(&b, 0.49).gear, 2);
        assert_eq!(a.interpolate(&b, 0.5).gear, 3);
    }

    #[test]
    fn interpolate_clamps_alpha() {
        let a = snap(0.0, 0.0);
        let b = snap(1.0, 10.0);
        assert_eq!(a.interpolate(&b, -1.0).pos.x, 0.0);
        assert_eq!(a.interpolate(&b, 2.0).pos.x, 10.0);
    }

    #[test]
    fn rotation_slerp_midpoint() {
        let a = CarSnapshot { rot: DQuat::IDENTITY, ..Default::default() };
        let b = CarSnapshot { rot: DQuat::from_rotation_z(FRAC_PI_2), ..Default::default() };
        let mid = a.interpolate(&b, 0.5).rot;
        assert!(mid.angle_between(DQuat::from_rotation_z(FRAC_PI_2 / 2.0)) < 1e-9);
        assert!((mid.length() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn rotation_slerp_takes_shortest_path_with_flipped_sign() {
        let a = CarSnapshot { rot: DQuat::from_rotation_y(0.1), ..Default::default() };
        // Same orientation as rotation_y(0.3) but in the opposite hemisphere.
        let b = CarSnapshot { rot: -DQuat::from_rotation_y(0.3), ..Default::default() };
        let mid = a.interpolate(&b, 0.5).rot;
        assert!(mid.angle_between(DQuat::from_rotation_y(0.2)) < 1e-9);
    }

    #[test]
    fn rotation_across_pi_boundary() {
        let a = CarSnapshot { rot: DQuat::from_rotation_z(PI - 0.1), ..Default::default() };
        let b = CarSnapshot { rot: DQuat::from_rotation_z(-PI + 0.1), ..Default::default() };
        let mid = a.interpolate(&b, 0.5).rot;
        assert!(mid.angle_between(DQuat::from_rotation_z(PI)) < 1e-9);
    }

    #[test]
    fn wheels_and_inputs_lerp() {
        let mut a = CarSnapshot::default();
        let mut b = CarSnapshot::default();
        a.wheels[2].angular_vel = 10.0;
        b.wheels[2].angular_vel = 20.0;
        a.inputs.throttle = 0.0;
        b.inputs.throttle = 1.0;
        let m = a.interpolate(&b, 0.25);
        assert!((m.wheels[2].angular_vel - 12.5).abs() < 1e-12);
        assert!((m.inputs.throttle - 0.25).abs() < 1e-12);
    }

    #[test]
    fn validity() {
        assert!(CarSnapshot::default().is_valid());
        let bad = CarSnapshot { pos: DVec3::new(f64::NAN, 0.0, 0.0), ..Default::default() };
        assert!(!bad.is_valid());
        let bad_rot = CarSnapshot { rot: DQuat::from_xyzw(0.0, 0.0, 0.0, 2.0), ..Default::default() };
        assert!(!bad_rot.is_valid());
    }
}
