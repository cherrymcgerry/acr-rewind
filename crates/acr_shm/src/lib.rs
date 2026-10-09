//! Read-only access to the Assetto Corsa Rally shared memory pages
//! (`Local\acpmf_physics`, `Local\acpmf_graphics`, `Local\acpmf_static`).
//!
//! - [`layout`]: raw `#[repr(C, packed(4))]` page structs (the part to fix if a patch moves fields).
//! - [`quirks`]: AC Rally value semantics (Kelvin temperatures, gear offset, unpublished fields).
//! - [`pages`]: decoded, safe types ([`Physics`], [`Graphics`], [`StaticInfo`], [`Status`]).
//! - [`SharedMemory`]: opens the mappings and produces [`Snapshot`]s.

pub mod layout;
pub mod pages;
pub mod quirks;
mod reader;

pub use pages::{car_position, CarPosSource, Graphics, MozaInfo, Physics, Snapshot, StaticInfo, Status, WheelPhysics};
pub use reader::{MozaPage, SharedMemory, ShmError};
