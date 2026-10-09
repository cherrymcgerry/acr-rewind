//! Raw page layouts, byte-for-byte as the game writes them.
//!
//! Assetto Corsa Rally publishes the ACC / AC Evo variant of the Kunos shared memory pages
//! (ACC Shared Memory Documentation v1.8.12): an 800-byte physics page whose first 416 bytes
//! match original AC, an ACC-shaped graphics page and an AC-shaped static page with ACC
//! additions. Everything game-version-specific about the *layout* lives in this file; unit and
//! semantic quirks live in [`crate::quirks`].
//!
//! All fields are `i32`, `f32` or `u16` (`wchar_t`), so every bit pattern is a valid value.

/// Kernel object names of the three pages.
pub const PHYSICS_TAG: &str = "Local\\acpmf_physics";
pub const GRAPHICS_TAG: &str = "Local\\acpmf_graphics";
pub const STATIC_TAG: &str = "Local\\acpmf_static";

/// Plain-old-data page that can be built from arbitrary bytes.
///
/// # Safety
/// Implementors must be `#[repr(C)]` structs made only of integer/float fields (and arrays of
/// them) so that any byte pattern, including all zeros, is a valid value.
pub unsafe trait RawPage: Copy + 'static {
    const SIZE: usize = std::mem::size_of::<Self>();

    /// Builds a page from `bytes`. Shorter input is zero-extended, longer input is truncated.
    fn from_bytes(bytes: &[u8]) -> Self {
        let mut out = std::mem::MaybeUninit::<Self>::zeroed();
        let n = bytes.len().min(Self::SIZE);
        // SAFETY: `out` is SIZE bytes, n <= SIZE, and the trait contract makes any bytes valid.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), out.as_mut_ptr().cast::<u8>(), n);
            out.assume_init()
        }
    }

    /// Raw bytes of the page.
    fn as_bytes(&self) -> &[u8] {
        // SAFETY: Self is POD without padding-sensitive invariants; reading its bytes is fine.
        unsafe { std::slice::from_raw_parts((self as *const Self).cast::<u8>(), Self::SIZE) }
    }
}

pub type WStr15 = [u16; 15];
pub type WStr33 = [u16; 33];

/// `SPageFilePhysics` (800 bytes).
#[repr(C, packed(4))]
#[derive(Clone, Copy, Debug)]
#[allow(non_snake_case)]
pub struct RawPhysics {
    // Original AC prefix (0..416).
    pub packetId: i32,
    pub gas: f32,
    pub brake: f32,
    pub fuel: f32,
    pub gear: i32,
    pub rpms: i32,
    pub steerAngle: f32,
    pub speedKmh: f32,
    pub velocity: [f32; 3],
    pub accG: [f32; 3],
    pub wheelSlip: [f32; 4],
    pub wheelLoad: [f32; 4],
    pub wheelsPressure: [f32; 4],
    pub wheelAngularSpeed: [f32; 4],
    pub tyreWear: [f32; 4],
    pub tyreDirtyLevel: [f32; 4],
    pub tyreCoreTemperature: [f32; 4],
    pub camberRAD: [f32; 4],
    pub suspensionTravel: [f32; 4],
    pub drs: f32,
    pub tc: f32,
    pub heading: f32,
    pub pitch: f32,
    pub roll: f32,
    pub cgHeight: f32,
    pub carDamage: [f32; 5],
    pub numberOfTyresOut: i32,
    pub pitLimiterOn: i32,
    pub abs: f32,
    pub kersCharge: f32,
    pub kersInput: f32,
    pub autoShifterOn: i32,
    pub rideHeight: [f32; 2],
    pub turboBoost: f32,
    pub ballast: f32,
    pub airDensity: f32,
    pub airTemp: f32,
    pub roadTemp: f32,
    pub localAngularVel: [f32; 3],
    pub finalFF: f32,
    pub performanceMeter: f32,
    pub engineBrake: i32,
    pub ersRecoveryLevel: i32,
    pub ersPowerLevel: i32,
    pub ersHeatCharging: i32,
    pub ersIsCharging: i32,
    pub kersCurrentKJ: f32,
    pub drsAvailable: i32,
    pub drsEnabled: i32,
    pub brakeTemp: [f32; 4],
    pub clutch: f32,
    pub tyreTempI: [f32; 4],
    pub tyreTempM: [f32; 4],
    pub tyreTempO: [f32; 4],
    // ACC / AC Evo additions (416..800).
    pub isAIControlled: i32,
    pub tyreContactPoint: [[f32; 3]; 4],
    pub tyreContactNormal: [[f32; 3]; 4],
    pub tyreContactHeading: [[f32; 3]; 4],
    pub brakeBias: f32,
    pub localVelocity: [f32; 3],
    pub P2PActivations: i32,
    pub P2PStatus: i32,
    /// Documented as float but written as int32.
    pub currentMaxRpm: i32,
    pub mz: [f32; 4],
    pub fx: [f32; 4],
    pub fy: [f32; 4],
    pub slipRatio: [f32; 4],
    pub slipAngle: [f32; 4],
    pub tcinAction: i32,
    pub absInAction: i32,
    pub suspensionDamage: [f32; 4],
    pub tyreTemp: [f32; 4],
    pub waterTemp: f32,
    pub brakePressure: [f32; 4],
    pub frontBrakeCompound: i32,
    pub rearBrakeCompound: i32,
    pub padLife: [f32; 4],
    pub discLife: [f32; 4],
    pub ignitionOn: i32,
    pub starterEngineOn: i32,
    pub isEngineRunning: i32,
    pub kerbVibration: f32,
    pub slipVibrations: f32,
    pub gVibrations: f32,
    pub absVibrations: f32,
}

/// `SPageFileGraphic`, ACC shape (1588 bytes).
#[repr(C, packed(4))]
#[derive(Clone, Copy, Debug)]
#[allow(non_snake_case)]
pub struct RawGraphics {
    pub packetId: i32,
    /// `AC_STATUS`: 0 off, 1 replay, 2 live, 3 pause.
    pub status: i32,
    /// `AC_SESSION_TYPE`.
    pub session: i32,
    pub currentTime: WStr15,
    pub lastTime: WStr15,
    pub bestTime: WStr15,
    pub split: WStr15,
    pub completedLaps: i32,
    pub position: i32,
    pub iCurrentTime: i32,
    pub iLastTime: i32,
    pub iBestTime: i32,
    pub sessionTimeLeft: f32,
    pub distanceTraveled: f32,
    pub isInPit: i32,
    pub currentSectorIndex: i32,
    pub lastSectorTime: i32,
    pub numberOfLaps: i32,
    pub tyreCompound: WStr33,
    pub replayTimeMultiplier: f32,
    pub normalizedCarPosition: f32,
    pub activeCars: i32,
    pub carCoordinates: [[f32; 3]; 60],
    pub carID: [i32; 60],
    pub playerCarID: i32,
    pub penaltyTime: f32,
    pub flag: i32,
    pub penalty: i32,
    pub idealLineOn: i32,
    pub isInPitLane: i32,
    pub surfaceGrip: f32,
    pub mandatoryPitDone: i32,
    pub windSpeed: f32,
    pub windDirection: f32,
    pub isSetupMenuVisible: i32,
    pub mainDisplayIndex: i32,
    pub secondaryDisplayIndex: i32,
    pub TC: i32,
    pub TCCut: i32,
    pub EngineMap: i32,
    pub ABS: i32,
    pub fuelXLap: f32,
    pub rainLights: i32,
    pub flashingLights: i32,
    pub lightsStage: i32,
    pub exhaustTemperature: f32,
    pub wiperLV: i32,
    pub DriverStintTotalTimeLeft: i32,
    pub DriverStintTimeLeft: i32,
    pub rainTyres: i32,
    pub sessionIndex: i32,
    pub usedFuel: f32,
    pub deltaLapTime: WStr15,
    pub iDeltaLapTime: i32,
    pub estimatedLapTime: WStr15,
    pub iEstimatedLapTime: i32,
    pub isDeltaPositive: i32,
    pub iSplit: i32,
    pub isValidLap: i32,
    pub fuelEstimatedLaps: f32,
    pub trackStatus: WStr33,
    pub missingMandatoryPits: i32,
    pub Clock: f32,
    pub directionLightsLeft: i32,
    pub directionLightsRight: i32,
    pub GlobalYellow: i32,
    pub GlobalYellow1: i32,
    pub GlobalYellow2: i32,
    pub GlobalYellow3: i32,
    pub GlobalWhite: i32,
    pub GlobalGreen: i32,
    pub GlobalChequered: i32,
    pub GlobalRed: i32,
    pub mfdTyreSet: i32,
    pub mfdFuelToAdd: f32,
    pub mfdTyrePressureLF: f32,
    pub mfdTyrePressureRF: f32,
    pub mfdTyrePressureLR: f32,
    pub mfdTyrePressureRR: f32,
    pub trackGripStatus: i32,
    pub rainIntensity: i32,
    pub rainIntensityIn10min: i32,
    pub rainIntensityIn30min: i32,
    pub currentTyreSet: i32,
    pub strategyTyreSet: i32,
    pub gapAhead: i32,
    pub gapBehind: i32,
}

/// `SPageFileStatic`, AC shape plus ACC additions (820 bytes).
#[repr(C, packed(4))]
#[derive(Clone, Copy, Debug)]
#[allow(non_snake_case)]
pub struct RawStatic {
    pub smVersion: WStr15,
    pub acVersion: WStr15,
    pub numberOfSessions: i32,
    pub numCars: i32,
    pub carModel: WStr33,
    pub track: WStr33,
    pub playerName: WStr33,
    pub playerSurname: WStr33,
    pub playerNick: WStr33,
    pub sectorCount: i32,
    pub maxTorque: f32,
    pub maxPower: f32,
    pub maxRpm: i32,
    pub maxFuel: f32,
    pub suspensionMaxTravel: [f32; 4],
    pub tyreRadius: [f32; 4],
    pub maxTurboBoost: f32,
    pub deprecated_1: f32,
    pub deprecated_2: f32,
    pub penaltiesEnabled: i32,
    pub aidFuelRate: f32,
    pub aidTireRate: f32,
    pub aidMechanicalDamage: f32,
    pub aidAllowTyreBlankets: f32,
    pub aidStability: f32,
    pub aidAutoClutch: i32,
    pub aidAutoBlip: i32,
    pub hasDRS: i32,
    pub hasERS: i32,
    pub hasKERS: i32,
    pub kersMaxJ: f32,
    pub engineBrakeSettingsCount: i32,
    pub ersPowerControllerCount: i32,
    pub trackSPlineLength: f32,
    pub trackConfiguration: WStr33,
    pub ersMaxJ: f32,
    pub isTimedRace: i32,
    pub hasExtraLap: i32,
    pub carSkin: WStr33,
    pub reversedGridPositions: i32,
    pub PitWindowStart: i32,
    pub PitWindowEnd: i32,
    pub isOnline: i32,
    pub dryTyresName: WStr33,
    pub wetTyresName: WStr33,
}

/// Kernel object name of the Moza telemetry page the game publishes
/// (`apps\python\Moza\Moza_shared_mem.py` in the install).
pub const MOZA_TAG: &str = "Local\\acpmf_Moza";

/// Header of `Local\acpmf_Moza` (pack 4). Only the leading fields the online guard needs;
/// `vehicleInfo[64]` and the trailer follow and are not read.
#[repr(C, packed(4))]
#[derive(Clone, Copy, Debug)]
#[allow(non_snake_case)]
pub struct RawMozaHeader {
    pub numVehicles: i32,
    pub focusVehicle: i32,
    /// NUL-terminated narrow string.
    pub serverName: [u8; 512],
}

// SAFETY: all of these are repr(C) structs of i32/f32/u16/u8 fields and arrays thereof.
unsafe impl RawPage for RawPhysics {}
unsafe impl RawPage for RawGraphics {}
unsafe impl RawPage for RawStatic {}
unsafe impl RawPage for RawMozaHeader {}

pub const PHYSICS_SIZE: usize = 800;
pub const GRAPHICS_SIZE: usize = 1588;
pub const STATIC_SIZE: usize = 820;
pub const MOZA_HEADER_SIZE: usize = 520;

const _: () = assert!(std::mem::size_of::<RawPhysics>() == PHYSICS_SIZE);
const _: () = assert!(std::mem::size_of::<RawGraphics>() == GRAPHICS_SIZE);
const _: () = assert!(std::mem::size_of::<RawStatic>() == STATIC_SIZE);
const _: () = assert!(std::mem::size_of::<RawMozaHeader>() == MOZA_HEADER_SIZE);

/// Decodes a NUL-terminated narrow (UTF-8 / Latin-1-ish) buffer.
pub fn cstr(buf: &[u8]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// Decodes a NUL-terminated UTF-16 buffer.
pub fn wstr(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::offset_of;

    #[test]
    fn physics_offsets() {
        assert_eq!(offset_of!(RawPhysics, gear), 16);
        assert_eq!(offset_of!(RawPhysics, velocity), 32);
        assert_eq!(offset_of!(RawPhysics, wheelAngularSpeed), 104);
        assert_eq!(offset_of!(RawPhysics, suspensionTravel), 184);
        assert_eq!(offset_of!(RawPhysics, heading), 208);
        assert_eq!(offset_of!(RawPhysics, localAngularVel), 296);
        assert_eq!(offset_of!(RawPhysics, brakeTemp), 348);
        assert_eq!(offset_of!(RawPhysics, clutch), 364);
        assert_eq!(offset_of!(RawPhysics, isAIControlled), 416);
        assert_eq!(offset_of!(RawPhysics, localVelocity), 568);
        assert_eq!(offset_of!(RawPhysics, currentMaxRpm), 588);
        assert_eq!(offset_of!(RawPhysics, tyreTemp), 696);
        assert_eq!(offset_of!(RawPhysics, brakePressure), 716);
        assert_eq!(offset_of!(RawPhysics, gVibrations), 792);
    }

    #[test]
    fn graphics_offsets() {
        assert_eq!(offset_of!(RawGraphics, status), 4);
        assert_eq!(offset_of!(RawGraphics, completedLaps), 132);
        assert_eq!(offset_of!(RawGraphics, tyreCompound), 176);
        // 2 bytes of alignment padding after the 33-wchar string.
        assert_eq!(offset_of!(RawGraphics, replayTimeMultiplier), 244);
        assert_eq!(offset_of!(RawGraphics, carCoordinates), 256);
        assert_eq!(offset_of!(RawGraphics, playerCarID), 1216);
        assert_eq!(offset_of!(RawGraphics, isValidLap), 1408);
        assert_eq!(offset_of!(RawGraphics, gapBehind), 1584);
    }

    #[test]
    fn static_offsets() {
        assert_eq!(offset_of!(RawStatic, carModel), 68);
        assert_eq!(offset_of!(RawStatic, sectorCount), 400);
        assert_eq!(offset_of!(RawStatic, maxRpm), 412);
        assert_eq!(offset_of!(RawStatic, ersMaxJ), 592);
        assert_eq!(offset_of!(RawStatic, isOnline), 684);
        assert_eq!(offset_of!(RawStatic, wetTyresName), 754);
    }

    #[test]
    fn from_bytes_short_input_zero_extends() {
        let mut bytes = vec![0u8; 8];
        bytes[0..4].copy_from_slice(&7i32.to_le_bytes());
        bytes[4..8].copy_from_slice(&0.5f32.to_le_bytes());
        let p = RawPhysics::from_bytes(&bytes);
        assert_eq!({ p.packetId }, 7);
        assert_eq!({ p.gas }, 0.5);
        assert_eq!({ p.absVibrations }, 0.0);
    }

    #[test]
    fn from_bytes_roundtrip_and_long_input() {
        let mut bytes: Vec<u8> = (0..PHYSICS_SIZE + 100).map(|i| (i % 251) as u8).collect();
        let p = RawPhysics::from_bytes(&bytes);
        assert_eq!(p.as_bytes(), &bytes[..PHYSICS_SIZE]);
        bytes.truncate(PHYSICS_SIZE);
        assert_eq!(RawPhysics::from_bytes(&bytes).as_bytes(), &bytes[..]);
    }

    #[test]
    fn wide_strings() {
        let mut buf = [0u16; 33];
        for (i, c) in "ford_fiesta".encode_utf16().enumerate() {
            buf[i] = c;
        }
        assert_eq!(wstr(&buf), "ford_fiesta");
        assert_eq!(wstr(&[0u16; 15]), "");
        let full: Vec<u16> = "abc".encode_utf16().collect();
        assert_eq!(wstr(&full), "abc");
    }
}
