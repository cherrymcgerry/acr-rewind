use crate::CarSnapshot;

/// Fixed-capacity ring buffer of snapshots ordered by strictly increasing `t`.
///
/// When full, pushing overwrites the oldest entry. Logical index 0 is the oldest snapshot.
#[derive(Clone, Debug)]
pub struct RingBuffer {
    buf: Vec<CarSnapshot>,
    capacity: usize,
    /// Physical index of the oldest element.
    head: usize,
    len: usize,
}

impl RingBuffer {
    /// Creates an empty buffer. `capacity` is clamped to at least 2 so interpolation is possible.
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(2);
        Self { buf: Vec::with_capacity(capacity), capacity, head: 0, len: 0 }
    }

    /// Buffer sized for `seconds` of history captured at `tick_hz`.
    pub fn with_duration(seconds: f64, tick_hz: u32) -> Self {
        Self::new((seconds.max(0.0) * f64::from(tick_hz)).ceil() as usize + 1)
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn is_full(&self) -> bool {
        self.len == self.capacity
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.head = 0;
        self.len = 0;
    }

    #[inline]
    fn phys(&self, i: usize) -> usize {
        (self.head + i) % self.capacity
    }

    /// Snapshot at logical index `i` (0 = oldest).
    pub fn get(&self, i: usize) -> Option<&CarSnapshot> {
        (i < self.len).then(|| &self.buf[self.phys(i)])
    }

    pub fn oldest(&self) -> Option<&CarSnapshot> {
        self.get(0)
    }

    pub fn newest(&self) -> Option<&CarSnapshot> {
        self.len.checked_sub(1).and_then(|i| self.get(i))
    }

    /// Iterates oldest to newest.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &CarSnapshot> + ExactSizeIterator + '_ {
        (0..self.len).map(move |i| &self.buf[self.phys(i)])
    }

    /// `(oldest.t, newest.t)`, or `None` if empty.
    pub fn time_range(&self) -> Option<(f64, f64)> {
        Some((self.oldest()?.t, self.newest()?.t))
    }

    /// Covered history in seconds (0 if fewer than two snapshots).
    pub fn duration(&self) -> f64 {
        self.time_range().map_or(0.0, |(a, b)| b - a)
    }

    /// Appends a snapshot, overwriting the oldest one when full.
    ///
    /// If `snap.t` is not after the newest entry (time went backwards, e.g. after a resume),
    /// all entries with `t >= snap.t` are dropped first so ordering stays strictly increasing.
    /// Non-finite times are ignored.
    pub fn push(&mut self, snap: CarSnapshot) {
        if !snap.t.is_finite() {
            return;
        }
        if self.newest().is_some_and(|n| snap.t <= n.t) {
            self.truncate_from(snap.t);
        }
        if self.buf.len() < self.capacity {
            // Still filling for the first time (head is always 0 here).
            debug_assert_eq!(self.head, 0);
            if self.len < self.buf.len() {
                self.buf[self.len] = snap;
            } else {
                self.buf.push(snap);
            }
            self.len += 1;
        } else if self.len < self.capacity {
            let idx = self.phys(self.len);
            self.buf[idx] = snap;
            self.len += 1;
        } else {
            self.buf[self.head] = snap;
            self.head = (self.head + 1) % self.capacity;
        }
    }

    /// Index of the first snapshot with `t > time`.
    fn upper_bound(&self, time: f64) -> usize {
        let (mut lo, mut hi) = (0, self.len);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.buf[self.phys(mid)].t <= time {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Removes every snapshot with `t > time`.
    pub fn truncate_after(&mut self, time: f64) {
        self.len = self.upper_bound(time);
        self.compact_if_empty();
    }

    /// Removes every snapshot with `t >= time`.
    fn truncate_from(&mut self, time: f64) {
        let (mut lo, mut hi) = (0, self.len);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.buf[self.phys(mid)].t < time {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        self.len = lo;
        self.compact_if_empty();
    }

    fn compact_if_empty(&mut self) {
        if self.len == 0 {
            self.clear();
        }
    }

    /// Interpolated snapshot at `time`, clamped to the stored range. `None` if empty.
    ///
    /// The returned snapshot's `t` equals the clamped query time.
    pub fn sample(&self, time: f64) -> Option<CarSnapshot> {
        let (t0, t1) = self.time_range()?;
        if time.is_nan() {
            return None;
        }
        let time = time.clamp(t0, t1);
        let ub = self.upper_bound(time);
        if ub == 0 {
            return self.oldest().copied();
        }
        if ub >= self.len {
            return self.newest().copied();
        }
        let a = &self.buf[self.phys(ub - 1)];
        let b = &self.buf[self.phys(ub)];
        let span = b.t - a.t;
        let alpha = if span > 0.0 { (time - a.t) / span } else { 0.0 };
        let mut s = a.interpolate(b, alpha);
        s.t = time;
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::DVec3;

    fn snap(t: f64) -> CarSnapshot {
        CarSnapshot { t, pos: DVec3::new(t * 10.0, 0.0, 0.0), ..Default::default() }
    }

    fn times(b: &RingBuffer) -> Vec<f64> {
        b.iter().map(|s| s.t).collect()
    }

    #[test]
    fn empty_buffer() {
        let b = RingBuffer::new(4);
        assert!(b.is_empty());
        assert_eq!(b.oldest(), None);
        assert_eq!(b.newest(), None);
        assert_eq!(b.time_range(), None);
        assert_eq!(b.duration(), 0.0);
        assert_eq!(b.sample(1.0), None);
    }

    #[test]
    fn capacity_min_two_and_duration_sizing() {
        assert_eq!(RingBuffer::new(0).capacity(), 2);
        assert_eq!(RingBuffer::with_duration(30.0, 60).capacity(), 1801);
    }

    #[test]
    fn push_and_order() {
        let mut b = RingBuffer::new(4);
        for i in 0..3 {
            b.push(snap(i as f64));
        }
        assert_eq!(times(&b), vec![0.0, 1.0, 2.0]);
        assert_eq!(b.len(), 3);
        assert!(!b.is_full());
    }

    #[test]
    fn wraparound_overwrites_oldest() {
        let mut b = RingBuffer::new(4);
        for i in 0..10 {
            b.push(snap(i as f64));
        }
        assert!(b.is_full());
        assert_eq!(times(&b), vec![6.0, 7.0, 8.0, 9.0]);
        assert_eq!(b.oldest().unwrap().t, 6.0);
        assert_eq!(b.newest().unwrap().t, 9.0);
        assert_eq!(b.get(2).unwrap().t, 8.0);
        assert_eq!(b.get(4), None);
        assert_eq!(b.iter().next_back().unwrap().t, 9.0);
    }

    #[test]
    fn sample_interpolates_across_wrap_boundary() {
        let mut b = RingBuffer::new(4);
        for i in 0..6 {
            b.push(snap(i as f64));
        }
        // Physical layout is now wrapped; 3.5 straddles entries 3 and 4.
        let s = b.sample(3.5).unwrap();
        assert!((s.pos.x - 35.0).abs() < 1e-9);
        assert_eq!(s.t, 3.5);
        let s = b.sample(4.25).unwrap();
        assert!((s.pos.x - 42.5).abs() < 1e-9);
    }

    #[test]
    fn sample_exact_and_clamped() {
        let mut b = RingBuffer::new(8);
        for i in 0..5 {
            b.push(snap(i as f64 * 0.5));
        }
        assert_eq!(b.sample(1.0).unwrap().pos.x, 10.0);
        assert_eq!(b.sample(-5.0).unwrap().t, 0.0);
        assert_eq!(b.sample(99.0).unwrap().t, 2.0);
        assert_eq!(b.sample(99.0).unwrap().pos.x, 20.0);
        assert_eq!(b.sample(f64::NAN), None);
    }

    #[test]
    fn sample_single_entry() {
        let mut b = RingBuffer::new(4);
        b.push(snap(3.0));
        assert_eq!(b.sample(0.0).unwrap().t, 3.0);
        assert_eq!(b.sample(5.0).unwrap().t, 3.0);
    }

    #[test]
    fn sample_irregular_spacing() {
        let mut b = RingBuffer::new(8);
        b.push(snap(0.0));
        b.push(snap(0.1));
        b.push(snap(1.1));
        let s = b.sample(0.6).unwrap();
        assert!((s.pos.x - 6.0).abs() < 1e-9);
    }

    #[test]
    fn truncate_after_removes_future() {
        let mut b = RingBuffer::new(4);
        for i in 0..7 {
            b.push(snap(i as f64));
        }
        b.truncate_after(4.5);
        assert_eq!(times(&b), vec![3.0, 4.0]);
        // Further pushes continue the wrapped layout correctly.
        b.push(snap(5.0));
        b.push(snap(6.0));
        b.push(snap(7.0));
        assert_eq!(times(&b), vec![4.0, 5.0, 6.0, 7.0]);
    }

    #[test]
    fn truncate_after_keeps_exact_match_and_handles_extremes() {
        let mut b = RingBuffer::new(8);
        for i in 0..4 {
            b.push(snap(i as f64));
        }
        b.truncate_after(2.0);
        assert_eq!(times(&b), vec![0.0, 1.0, 2.0]);
        b.truncate_after(100.0);
        assert_eq!(b.len(), 3);
        b.truncate_after(-1.0);
        assert!(b.is_empty());
        b.push(snap(10.0));
        assert_eq!(times(&b), vec![10.0]);
    }

    #[test]
    fn push_backwards_in_time_drops_newer_entries() {
        let mut b = RingBuffer::new(8);
        for i in 0..5 {
            b.push(snap(i as f64));
        }
        b.push(snap(2.5));
        assert_eq!(times(&b), vec![0.0, 1.0, 2.0, 2.5]);
        b.push(snap(2.5));
        assert_eq!(times(&b), vec![0.0, 1.0, 2.0, 2.5]);
        b.push(snap(-1.0));
        assert_eq!(times(&b), vec![-1.0]);
    }

    #[test]
    fn push_ignores_non_finite_time() {
        let mut b = RingBuffer::new(4);
        b.push(snap(f64::NAN));
        b.push(snap(f64::INFINITY));
        assert!(b.is_empty());
    }

    #[test]
    fn partial_refill_after_truncate_before_full() {
        let mut b = RingBuffer::new(4);
        b.push(snap(0.0));
        b.push(snap(1.0));
        b.push(snap(2.0));
        b.truncate_after(0.5);
        b.push(snap(1.5));
        b.push(snap(2.5));
        b.push(snap(3.5));
        b.push(snap(4.5));
        assert_eq!(times(&b), vec![1.5, 2.5, 3.5, 4.5]);
    }

    #[test]
    fn clear_resets() {
        let mut b = RingBuffer::new(3);
        for i in 0..5 {
            b.push(snap(i as f64));
        }
        b.clear();
        assert!(b.is_empty());
        b.push(snap(1.0));
        assert_eq!(times(&b), vec![1.0]);
    }
}
