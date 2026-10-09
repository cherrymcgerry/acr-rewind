//! Assetto Corsa Rally game adapter: the only crate that knows about ACR / Unreal Engine
//! internals. Everything game-specific is data-driven from `config/signatures.toml`
//! ([`Signatures`]); anything that can't be resolved is reported, never guessed.
//!
//! - [`pattern`] / [`pe`] / [`resolve`]: signature scanning and engine-global resolution
//!   (pure functions over byte regions + [`mem::Memory`], unit-tested on synthetic buffers).
//! - [`reflection`]: GObjects / FNamePool / UClass / UFunction / FProperty walking.
//! - [`ue_call`]: ProcessEvent invocation with reflection-derived parameter layouts.
//! - [`game`]: the resolved live game, pawn lookup, online-guard signals.
//! - [`backend`]: the [`CarBackend`] trait the hook drives, implemented by
//!   [`CarAvatarAdapter`] (default: dmphysics CarAvatar UFunctions + sim bodies),
//!   [`SimCarAdapter`] (direct sim memory) and [`UeActorAdapter`] (generic fallback).
//! - [`bodies`]: the Kunos rigid-body locator (vtable scan), shared with `acr-probe`.

pub mod backend;
pub mod bodies;
pub mod car_avatar;
pub mod freeze;
pub mod game;
pub mod math;
pub mod mem;
pub mod pattern;
pub mod pe;
pub mod reflection;
pub mod resolve;
pub mod sigs;
pub mod sim_car;
pub mod ue_actor;
pub mod ue_call;

pub use backend::{BackendError, BackendHealth, BlobHistory, CarBackend, CarChange, WriteMode};
pub use car_avatar::CarAvatarAdapter;
pub use game::{Game, InitError, PawnLocator};
pub use rewind_core::CarSnapshot;
pub use sigs::{Signatures, SigsError};
pub use sim_car::SimCarAdapter;
pub use ue_actor::UeActorAdapter;
