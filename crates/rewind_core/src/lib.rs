//! Platform-independent rewind engine.
//!
//! - [`CarSnapshot`]: one captured physics state of the player car.
//! - [`RingBuffer`]: fixed-capacity history with time-based interpolated sampling.
//! - [`RewindController`]: the `Recording -> Rewinding (mode) -> Resuming -> Recording` state
//!   machine that the game hook drives once per frame with [`ModeInput`].
//! - [`Config`]: serde-deserializable settings (`config/acr-rewind.toml`).
//! - [`Binding`] / [`InputAction`]: the input binding model and its text form.
//!
//! Units are up to the adapter, but the intended convention is SI: metres, m/s, rad/s, seconds.

pub mod bindings;
mod buffer;
mod config;
mod controller;
mod snapshot;

pub use bindings::{Binding, InputAction};
pub use buffer::RingBuffer;
pub use config::{BindingsConfig, BufferConfig, Config, ConfigError, FfbConfig, ModeConfig, ScrubConfig};
pub use controller::{
    Action, CancelReason, ModeEvent, ModeInput, ModeView, RewindController, RewindState, ScrubInput, TickOutput,
    Timeline,
};
pub use snapshot::{CarSnapshot, Inputs, WheelState};

pub use glam::{DQuat, DVec3};
