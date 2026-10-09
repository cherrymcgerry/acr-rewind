//! Safe, decoded views of the three pages.

use crate::layout::{cstr, wstr, RawGraphics, RawMozaHeader, RawPhysics, RawStatic};
use crate::quirks;

/// `AC_STATUS` from the graphics page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Status {
    #[default]
    Off,
    Replay,
    Live,
    Pause,
    Unknown(i32),
}

impl Status {
    pub fn from_raw(v: i32) -> Self {
        match v {
            0 => Self::Off,
            1 => Self::Replay,
            2 => Self::Live,
            3 => Self::Pause,
            v => Self::Unknown(v),
        }
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => f.write_str("Off"),
            Self::Replay => f.write_str("Replay"),
            Self::Live => f.write_str("Live"),
            Self::Pause => f.write_str("Pause"),
            Self::Unknown(v) => write!(f, "Unknown({v})"),
        }
    }
}

/// Per-wheel physics in FL, FR, RL, RR order.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WheelPhysics {
    pub slip: f32,
    /// Vertical load (N).
    pub load: f32,
    /// Tyre pressure (psi).
    pub pressure: f32,
    /// Spin rate (rad/s).
    pub angular_speed: f32,
    /// Suspension travel (m).
    pub suspension_travel: f32,
    pub core_temp_c: f32,
    pub brake_temp_c: f32,
    pub slip_ratio: f32,
    pub slip_angle: f32,
    pub contact_point: [f32; 3],
}

/// Decoded physics page.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Physics {
    /// Increments every physics update; use it to detect stale data.
    pub packet_id: i32,
    pub gas: f32,
    pub brake: f32,
    pub clutch: f32,
    /// Steering angle (rad).
    pub steer_angle: f32,
    /// -1 = reverse, 0 = neutral, 1.. = forward.
    pub gear: i32,
    pub rpm: i32,
    pub max_rpm: i32,
    pub speed_kmh: f32,
    pub fuel: f32,
    /// World-space velocity (m/s).
    pub velocity: [f32; 3],
    /// Car-local velocity (m/s).
    pub local_velocity: [f32; 3],
    /// Car-local angular velocity (rad/s).
    pub local_angular_vel: [f32; 3],
    /// Acceleration in g: lateral, longitudinal, vertical.
    pub acc_g: [f32; 3],
    /// Orientation (rad).
    pub heading: f32,
    pub pitch: f32,
    pub roll: f32,
    pub cg_height: f32,
    pub wheels: [WheelPhysics; 4],
    pub water_temp_c: f32,
    pub turbo_boost: f32,
    pub tyres_out: i32,
    pub car_damage: [f32; 5],
    pub is_ai_controlled: bool,
    pub is_engine_running: bool,
}

impl From<&RawPhysics> for Physics {
    fn from(r: &RawPhysics) -> Self {
        let r = *r;
        let (slip, load, pressure, ang) = (r.wheelSlip, r.wheelLoad, r.wheelsPressure, r.wheelAngularSpeed);
        let (susp, core, brake_t) = (r.suspensionTravel, r.tyreCoreTemperature, r.brakeTemp);
        let (sr, sa, cp) = (r.slipRatio, r.slipAngle, r.tyreContactPoint);
        let wheels = std::array::from_fn(|i| WheelPhysics {
            slip: slip[i],
            load: load[i],
            pressure: pressure[i],
            angular_speed: ang[i],
            suspension_travel: susp[i],
            core_temp_c: quirks::temp_c(core[i]),
            brake_temp_c: quirks::temp_c(brake_t[i]),
            slip_ratio: sr[i],
            slip_angle: sa[i],
            contact_point: cp[i],
        });
        Self {
            packet_id: r.packetId,
            gas: r.gas,
            brake: r.brake,
            clutch: r.clutch,
            steer_angle: r.steerAngle,
            gear: quirks::gear(r.gear),
            rpm: r.rpms,
            max_rpm: r.currentMaxRpm,
            speed_kmh: r.speedKmh,
            fuel: r.fuel,
            velocity: r.velocity,
            local_velocity: r.localVelocity,
            local_angular_vel: r.localAngularVel,
            acc_g: r.accG,
            heading: r.heading,
            pitch: r.pitch,
            roll: r.roll,
            cg_height: r.cgHeight,
            wheels,
            water_temp_c: quirks::temp_c(r.waterTemp),
            turbo_boost: r.turboBoost,
            tyres_out: r.numberOfTyresOut,
            car_damage: r.carDamage,
            is_ai_controlled: r.isAIControlled != 0,
            is_engine_running: r.isEngineRunning != 0,
        }
    }
}

/// Decoded graphics page (session state).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Graphics {
    pub packet_id: i32,
    pub status: Status,
    /// Raw `AC_SESSION_TYPE`.
    pub session: i32,
    pub current_time: String,
    pub i_current_time_ms: i32,
    pub completed_laps: i32,
    pub position: i32,
    pub distance_traveled: f32,
    pub normalized_car_position: f32,
    pub active_cars: i32,
    pub player_car_id: i32,
    /// World position of the player car, if it appears in the car table.
    pub player_position: Option<[f32; 3]>,
    pub replay_time_multiplier: f32,
    pub is_valid_lap: bool,
    pub tyre_compound: String,
}

/// Where [`car_position`] took the player car position from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CarPosSource {
    /// `carCoordinates[i]` with `carID[i] == playerCarID` among the first `activeCars`.
    Player,
    /// `carCoordinates[0]`: ACR leaves `activeCars` / `carID` / `playerCarID` at 0 while
    /// driving solo, so the table lookup finds nothing and only slot 0 is filled.
    Car0,
}

/// Player car position from the graphics page: the table entry of the player if listed,
/// else slot 0. `None` unless the value is finite and not the origin (no car on stage).
pub fn car_position(r: &RawGraphics) -> Option<([f32; 3], CarPosSource)> {
    let (ids, coords) = ({ r.carID }, { r.carCoordinates });
    let active_cars = r.activeCars;
    let active = usize::try_from(active_cars).unwrap_or(0).min(ids.len());
    let player = { r.playerCarID };
    let (p, src) = match ids[..active].iter().position(|&id| id == player) {
        Some(i) => (coords[i], CarPosSource::Player),
        None => (coords[0], CarPosSource::Car0),
    };
    let on_stage = p.iter().all(|v| v.is_finite()) && p.iter().map(|v| v * v).sum::<f32>() > 1e-6;
    on_stage.then_some((p, src))
}

impl From<&RawGraphics> for Graphics {
    fn from(r: &RawGraphics) -> Self {
        let r = *r;
        let (ids, coords) = (r.carID, r.carCoordinates);
        let active = usize::try_from(r.activeCars).unwrap_or(0).min(ids.len());
        let player_position = ids[..active].iter().position(|&id| id == r.playerCarID).map(|i| coords[i]);
        Self {
            packet_id: r.packetId,
            status: Status::from_raw(r.status),
            session: r.session,
            current_time: wstr(&{ r.currentTime }),
            i_current_time_ms: r.iCurrentTime,
            completed_laps: r.completedLaps,
            position: r.position,
            distance_traveled: r.distanceTraveled,
            normalized_car_position: r.normalizedCarPosition,
            active_cars: r.activeCars,
            player_car_id: r.playerCarID,
            player_position,
            replay_time_multiplier: r.replayTimeMultiplier,
            is_valid_lap: r.isValidLap != 0,
            tyre_compound: wstr(&{ r.tyreCompound }),
        }
    }
}

/// Decoded static page (session/car constants). May be all zeros until a stage loads.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StaticInfo {
    pub sm_version: String,
    pub ac_version: String,
    pub car_model: String,
    pub track: String,
    pub track_configuration: String,
    pub player_name: String,
    pub player_surname: String,
    pub max_rpm: i32,
    pub max_fuel: f32,
    pub max_torque: f32,
    pub max_power: f32,
    pub suspension_max_travel: [f32; 4],
    pub tyre_radius: [f32; 4],
    /// ACC's online flag. Unverified on AC Rally; treat only as a hint for the online guard.
    pub is_online: bool,
}

impl StaticInfo {
    /// True once the game has written anything into the page.
    pub fn is_populated(&self) -> bool {
        !self.sm_version.is_empty() || !self.car_model.is_empty() || self.max_rpm > 0
    }
}

impl From<&RawStatic> for StaticInfo {
    fn from(r: &RawStatic) -> Self {
        let r = *r;
        Self {
            sm_version: wstr(&{ r.smVersion }),
            ac_version: wstr(&{ r.acVersion }),
            car_model: wstr(&{ r.carModel }),
            track: wstr(&{ r.track }),
            track_configuration: wstr(&{ r.trackConfiguration }),
            player_name: wstr(&{ r.playerName }),
            player_surname: wstr(&{ r.playerSurname }),
            max_rpm: r.maxRpm,
            max_fuel: r.maxFuel,
            max_torque: r.maxTorque,
            max_power: r.maxPower,
            suspension_max_travel: r.suspensionMaxTravel,
            tyre_radius: r.tyreRadius,
            is_online: r.isOnline != 0,
        }
    }
}

/// Decoded header of the `acpmf_Moza` page.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MozaInfo {
    pub num_vehicles: i32,
    pub focus_vehicle: i32,
    /// Empty offline; value in ACR online lobbies unverified.
    pub server_name: String,
}

impl From<&RawMozaHeader> for MozaInfo {
    fn from(r: &RawMozaHeader) -> Self {
        let r = *r;
        Self {
            num_vehicles: r.numVehicles,
            focus_vehicle: r.focusVehicle,
            server_name: cstr(&{ r.serverName }).trim().to_owned(),
        }
    }
}

/// All three pages read at (approximately) the same moment.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub physics: Physics,
    pub graphics: Graphics,
    pub static_info: StaticInfo,
}

impl Snapshot {
    pub fn status(&self) -> Status {
        self.graphics.status
    }

    /// Player is actively driving (not paused, not watching a replay).
    pub fn is_live(&self) -> bool {
        self.graphics.status == Status::Live
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{RawPage, GRAPHICS_SIZE, PHYSICS_SIZE, STATIC_SIZE};
    use std::mem::offset_of;

    fn put_i32(buf: &mut [u8], off: usize, v: i32) {
        buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put_f32(buf: &mut [u8], off: usize, v: f32) {
        buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put_wstr(buf: &mut [u8], off: usize, s: &str) {
        for (i, c) in s.encode_utf16().enumerate() {
            buf[off + i * 2..off + i * 2 + 2].copy_from_slice(&c.to_le_bytes());
        }
    }

    #[test]
    fn status_from_raw() {
        assert_eq!(Status::from_raw(0), Status::Off);
        assert_eq!(Status::from_raw(1), Status::Replay);
        assert_eq!(Status::from_raw(2), Status::Live);
        assert_eq!(Status::from_raw(3), Status::Pause);
        assert_eq!(Status::from_raw(9), Status::Unknown(9));
        assert_eq!(Status::Unknown(9).to_string(), "Unknown(9)");
    }

    #[test]
    fn parse_physics_from_bytes() {
        let mut b = vec![0u8; PHYSICS_SIZE];
        put_i32(&mut b, 0, 1234);
        put_f32(&mut b, 4, 0.75); // gas
        put_i32(&mut b, 16, 3); // gear raw 3 = 2nd
        put_i32(&mut b, 20, 6500);
        put_f32(&mut b, 28, 123.5);
        put_f32(&mut b, 32 + 8, -4.0); // velocity.z
        put_f32(&mut b, offset_of!(RawPhysics, wheelAngularSpeed) + 12, 88.0); // RR
        put_f32(&mut b, offset_of!(RawPhysics, tyreCoreTemperature), 363.15); // FL Kelvin
        put_f32(&mut b, offset_of!(RawPhysics, heading), 1.5);
        put_f32(&mut b, offset_of!(RawPhysics, clutch), 0.25);
        put_i32(&mut b, offset_of!(RawPhysics, currentMaxRpm), 7500);
        put_f32(&mut b, offset_of!(RawPhysics, waterTemp), 343.6);
        put_i32(&mut b, offset_of!(RawPhysics, isEngineRunning), 1);

        let p = Physics::from(&RawPhysics::from_bytes(&b));
        assert_eq!(p.packet_id, 1234);
        assert_eq!(p.gas, 0.75);
        assert_eq!(p.gear, 2);
        assert_eq!(p.rpm, 6500);
        assert_eq!(p.max_rpm, 7500);
        assert_eq!(p.speed_kmh, 123.5);
        assert_eq!(p.velocity, [0.0, 0.0, -4.0]);
        assert_eq!(p.wheels[3].angular_speed, 88.0);
        assert!((p.wheels[0].core_temp_c - 90.0).abs() < 1e-3);
        assert_eq!(p.heading, 1.5);
        assert_eq!(p.clutch, 0.25);
        assert!((p.water_temp_c - 70.45).abs() < 1e-3);
        assert!(p.is_engine_running);
        assert!(!p.is_ai_controlled);
    }

    #[test]
    fn parse_graphics_from_bytes() {
        let mut b = vec![0u8; GRAPHICS_SIZE];
        put_i32(&mut b, 0, 55);
        put_i32(&mut b, 4, 2);
        put_wstr(&mut b, offset_of!(RawGraphics, currentTime), "1:23.456");
        put_i32(&mut b, offset_of!(RawGraphics, activeCars), 2);
        let ids = offset_of!(RawGraphics, carID);
        put_i32(&mut b, ids, 10);
        put_i32(&mut b, ids + 4, 42);
        put_i32(&mut b, offset_of!(RawGraphics, playerCarID), 42);
        let coords = offset_of!(RawGraphics, carCoordinates);
        put_f32(&mut b, coords + 12, 1.0);
        put_f32(&mut b, coords + 16, 2.0);
        put_f32(&mut b, coords + 20, 3.0);
        put_wstr(&mut b, offset_of!(RawGraphics, tyreCompound), "gravel");
        put_f32(&mut b, offset_of!(RawGraphics, replayTimeMultiplier), 1.0);

        let g = Graphics::from(&RawGraphics::from_bytes(&b));
        assert_eq!(g.packet_id, 55);
        assert_eq!(g.status, Status::Live);
        assert_eq!(g.current_time, "1:23.456");
        assert_eq!(g.player_position, Some([1.0, 2.0, 3.0]));
        assert_eq!(g.tyre_compound, "gravel");
        assert_eq!(g.replay_time_multiplier, 1.0);
    }

    #[test]
    fn car_position_falls_back_to_slot_0() {
        // What ACR reports while driving solo: no table entries, only slot 0 filled.
        let mut b = vec![0u8; GRAPHICS_SIZE];
        let coords = offset_of!(RawGraphics, carCoordinates);
        let raw = RawGraphics::from_bytes(&b);
        assert_eq!(car_position(&raw), None, "origin = no car on stage");
        put_f32(&mut b, coords, -1350.5);
        put_f32(&mut b, coords + 4, 340.7);
        put_f32(&mut b, coords + 8, -1987.5);
        let raw = RawGraphics::from_bytes(&b);
        assert_eq!(Graphics::from(&raw).player_position, None, "table lookup alone fails");
        assert_eq!(car_position(&raw), Some(([-1350.5, 340.7, -1987.5], CarPosSource::Car0)));
        // A listed player wins over slot 0.
        put_i32(&mut b, offset_of!(RawGraphics, activeCars), 2);
        put_i32(&mut b, offset_of!(RawGraphics, carID) + 4, 9);
        put_i32(&mut b, offset_of!(RawGraphics, playerCarID), 9);
        put_f32(&mut b, coords + 12, 5.0);
        let raw = RawGraphics::from_bytes(&b);
        assert_eq!(car_position(&raw), Some(([5.0, 0.0, 0.0], CarPosSource::Player)));
        // Non-finite garbage is not a car.
        put_i32(&mut b, offset_of!(RawGraphics, activeCars), 0);
        put_f32(&mut b, coords, f32::NAN);
        assert_eq!(car_position(&RawGraphics::from_bytes(&b)), None);
    }

    #[test]
    fn graphics_player_not_in_table() {
        let mut b = vec![0u8; GRAPHICS_SIZE];
        put_i32(&mut b, offset_of!(RawGraphics, activeCars), 999); // garbage count is clamped
        put_i32(&mut b, offset_of!(RawGraphics, playerCarID), 7);
        let g = Graphics::from(&RawGraphics::from_bytes(&b));
        assert_eq!(g.player_position, None);
        assert_eq!(g.status, Status::Off);
    }

    #[test]
    fn parse_static_from_bytes() {
        let mut b = vec![0u8; STATIC_SIZE];
        assert!(!StaticInfo::from(&RawStatic::from_bytes(&b)).is_populated());
        put_wstr(&mut b, 0, "1.9");
        put_wstr(&mut b, offset_of!(RawStatic, carModel), "lancia_delta");
        put_wstr(&mut b, offset_of!(RawStatic, track), "monte_carlo");
        put_i32(&mut b, offset_of!(RawStatic, maxRpm), 7200);
        put_f32(&mut b, offset_of!(RawStatic, tyreRadius) + 4, 0.31);
        put_i32(&mut b, offset_of!(RawStatic, isOnline), 1);

        let s = StaticInfo::from(&RawStatic::from_bytes(&b));
        assert!(s.is_populated());
        assert_eq!(s.sm_version, "1.9");
        assert_eq!(s.car_model, "lancia_delta");
        assert_eq!(s.track, "monte_carlo");
        assert_eq!(s.max_rpm, 7200);
        assert_eq!(s.tyre_radius[1], 0.31);
        assert!(s.is_online);
    }

    #[test]
    fn parse_moza_header() {
        use crate::layout::MOZA_HEADER_SIZE;
        let mut b = vec![0u8; MOZA_HEADER_SIZE];
        put_i32(&mut b, 0, 3);
        put_i32(&mut b, 4, 1);
        b[8..8 + 11].copy_from_slice(b"EU Lobby 1 ");
        let m = MozaInfo::from(&RawMozaHeader::from_bytes(&b));
        assert_eq!(m.num_vehicles, 3);
        assert_eq!(m.focus_vehicle, 1);
        assert_eq!(m.server_name, "EU Lobby 1");
        let empty = MozaInfo::from(&RawMozaHeader::from_bytes(&[]));
        assert_eq!(empty, MozaInfo::default());
    }

    #[test]
    fn snapshot_helpers() {
        let mut s = Snapshot::default();
        assert!(!s.is_live());
        s.graphics.status = Status::Live;
        assert!(s.is_live());
        assert_eq!(s.status(), Status::Live);
    }
}
