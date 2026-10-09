//! The game-facing interface the hook drives once per frame.

use rewind_core::CarSnapshot;
use std::collections::VecDeque;

/// How much of a snapshot to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteMode {
    /// While scrubbing: pose only, velocities zeroed so an unfrozen sim doesn't drift.
    Pose,
    /// On release: full state (pose, velocities, wheels, drivetrain, raw blobs).
    Full,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum BackendError {
    #[error("player car not found")]
    NoCar,
    #[error("memory write failed at {0:#x}")]
    Write(usize),
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Call(String),
}

/// What changed about the player car since the last frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CarChange {
    Same,
    /// A different car object (stage load / restart): history is invalid.
    Changed,
    Lost,
}

pub trait CarBackend: Send {
    fn name(&self) -> &'static str;

    /// Re-locates the player car (cheap when unchanged). Call once per frame before reads.
    fn refresh(&mut self) -> CarChange;

    /// Captures the live state, or `None` if the car can't be read right now.
    fn read_state(&mut self) -> Option<CarSnapshot>;

    fn write_state(&mut self, snap: &CarSnapshot, mode: WriteMode) -> Result<(), BackendError>;

    /// Freezes or unfreezes the simulation of the player car / world.
    fn freeze(&mut self, frozen: bool) -> Result<(), BackendError>;

    /// True if the backend can freeze at all (otherwise the car is only held by writing the
    /// pose every frame).
    fn can_freeze(&self) -> bool;

    /// The controller stored the last captured snapshot under timeline time `t`.
    fn on_recorded(&mut self, _t: f64) {}

    /// History after `t` was discarded (resume) or everything was cleared (`None`).
    fn on_truncate(&mut self, _t: Option<f64>) {}

    /// One-line description for logs / overlay.
    fn describe(&self) -> String {
        self.name().to_owned()
    }

    /// Hands over the body tracker (and its locked car) to a replacement backend, leaving
    /// this one without a car.
    fn take_body_tracker(&mut self) -> Option<crate::sim_car::BodyTracker> {
        None
    }

    /// Snapshot of the car lock for the status heartbeat.
    fn health(&self) -> BackendHealth {
        BackendHealth::default()
    }
}

/// What the status heartbeat reports about a backend.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackendHealth {
    /// The player car is located (pawn / sim bodies).
    pub car_locked: bool,
    /// Simulated bodies currently tracked (0 if the backend has none).
    pub bodies: usize,
    /// Most recent locate / scan failure, if any.
    pub last_error: Option<String>,
}

/// Raw memory blobs captured alongside snapshots, keyed by timeline time.
#[derive(Debug, Default)]
pub struct BlobHistory {
    capacity: usize,
    entries: VecDeque<(f64, Vec<u8>)>,
    pending: Option<Vec<u8>>,
}

impl BlobHistory {
    pub fn new(capacity: usize) -> Self {
        Self { capacity: capacity.max(1), entries: VecDeque::new(), pending: None }
    }

    /// Holds a blob captured during `read_state` until the controller assigns its time.
    pub fn stage(&mut self, blob: Vec<u8>) {
        self.pending = Some(blob);
    }

    pub fn commit(&mut self, t: f64) {
        if let Some(b) = self.pending.take() {
            while self.entries.back().is_some_and(|(last, _)| *last >= t - 1e-9) {
                self.entries.pop_back();
            }
            if self.entries.len() == self.capacity {
                self.entries.pop_front();
            }
            self.entries.push_back((t, b));
        }
    }

    /// Latest blob at or before `t`.
    pub fn at_or_before(&self, t: f64) -> Option<&[u8]> {
        self.entries.iter().rev().find(|(bt, _)| *bt <= t + 1e-9).map(|(_, b)| b.as_slice())
    }

    pub fn truncate_after(&mut self, t: f64) {
        while self.entries.back().is_some_and(|(bt, _)| *bt > t + 1e-9) {
            self.entries.pop_back();
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.pending = None;
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_history_stage_commit_lookup() {
        let mut h = BlobHistory::new(3);
        h.commit(0.0);
        assert!(h.is_empty(), "nothing staged");
        for (i, t) in [0.0, 0.1, 0.2, 0.3].iter().enumerate() {
            h.stage(vec![i as u8]);
            h.commit(*t);
        }
        assert_eq!(h.len(), 3, "capacity evicts oldest");
        assert_eq!(h.at_or_before(0.25), Some(&[2u8][..]));
        assert_eq!(h.at_or_before(0.3), Some(&[3u8][..]));
        assert_eq!(h.at_or_before(0.05), None);
        h.truncate_after(0.15);
        assert_eq!(h.len(), 1);
        assert_eq!(h.at_or_before(10.0), Some(&[1u8][..]));
    }

    #[test]
    fn commit_after_resume_drops_future() {
        let mut h = BlobHistory::new(10);
        for (i, t) in [0.0, 0.1, 0.2].iter().enumerate() {
            h.stage(vec![i as u8]);
            h.commit(*t);
        }
        // Clock jumped back to 0.1 (resume) and a new capture arrives at 0.1.
        h.stage(vec![9]);
        h.commit(0.1);
        assert_eq!(h.len(), 2);
        assert_eq!(h.at_or_before(1.0), Some(&[9u8][..]));
        h.clear();
        assert!(h.is_empty());
    }
}
