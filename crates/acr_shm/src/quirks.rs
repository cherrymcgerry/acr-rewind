//! Assetto Corsa Rally-specific interpretation of raw values.
//!
//! Observed on AC Rally v0.x (see live-telemetry-evo's `acrally.py`): temperatures are Kelvin,
//! `camberRAD`, `rideHeight`, `tyreTempI/M/O` and `tyreWear` are not published, and the static
//! page may stay zeroed until a stage is loaded. Adjust here if a patch changes semantics.

/// Temperatures (tyre core, brake, water, exhaust) are published in Kelvin.
pub const TEMPERATURES_IN_KELVIN: bool = true;

/// Values above this are assumed to be Kelvin even if the flag above is wrong.
const KELVIN_HEURISTIC: f32 = 200.0;

/// Fields present in the layout that AC Rally leaves at zero.
pub const UNPUBLISHED_PHYSICS_FIELDS: &[&str] =
    &["camberRAD", "rideHeight", "tyreTempI", "tyreTempM", "tyreTempO", "tyreWear"];

/// Converts a published temperature to degrees Celsius.
pub fn temp_c(raw: f32) -> f32 {
    if TEMPERATURES_IN_KELVIN && raw > KELVIN_HEURISTIC {
        raw - 273.15
    } else {
        raw
    }
}

/// Converts the published gear (0 = R, 1 = N, 2 = 1st, ...) to -1 = R, 0 = N, 1 = 1st.
pub fn gear(raw: i32) -> i32 {
    raw - 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kelvin_to_celsius() {
        assert!((temp_c(363.15) - 90.0).abs() < 1e-3);
        assert!((temp_c(343.6) - 70.45).abs() < 1e-3);
        assert_eq!(temp_c(0.0), 0.0);
        assert_eq!(temp_c(85.0), 85.0);
    }

    #[test]
    fn gear_mapping() {
        assert_eq!(gear(0), -1);
        assert_eq!(gear(1), 0);
        assert_eq!(gear(2), 1);
    }
}
