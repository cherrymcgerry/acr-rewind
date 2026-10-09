//! Read-only memory abstraction shared by every scanner: [`ReadMem`] (implemented by the
//! remote process and by [`FakeMem`] for tests), region filtering, chunking for parallel
//! scans, and a page cache that adapts any [`ReadMem`] to `acr_ue`'s [`Memory`] trait.

use acr_ue::mem::Memory;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;

pub const PAGE: usize = 0x1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RegionKind {
    Private,
    Image,
    Mapped,
}

/// A committed, readable range with uniform attributes (one `VirtualQueryEx` result).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct MemRegion {
    pub base: usize,
    pub size: usize,
    pub kind: RegionKind,
    pub writable: bool,
    pub executable: bool,
}

impl MemRegion {
    pub fn end(&self) -> usize {
        self.base + self.size
    }

    #[cfg(test)]
    pub fn contains(&self, addr: usize) -> bool {
        addr >= self.base && addr < self.end()
    }
}

/// Read-only view of an address space. Implementations never write.
pub trait ReadMem: Sync {
    /// Fills `buf` from `addr`; false if any byte is unreadable.
    fn read(&self, addr: usize, buf: &mut [u8]) -> bool;
    /// Committed, readable regions in ascending address order.
    fn regions(&self) -> Vec<MemRegion>;

    #[cfg(test)]
    fn read_u64(&self, addr: usize) -> Option<u64> {
        let mut b = [0u8; 8];
        self.read(addr, &mut b).then(|| u64::from_le_bytes(b))
    }
}

impl<R: ReadMem + ?Sized> ReadMem for &R {
    fn read(&self, addr: usize, buf: &mut [u8]) -> bool {
        (**self).read(addr, buf)
    }
    fn regions(&self) -> Vec<MemRegion> {
        (**self).regions()
    }
}

/// Which regions a scan visits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct RegionFilter {
    pub private: bool,
    pub image: bool,
    pub mapped: bool,
    /// Live simulation state and heap pointers are always in writable pages.
    pub writable_only: bool,
    pub executable: bool,
}

impl RegionFilter {
    /// Private + image data pages that the game can write.
    pub const DATA: Self = Self { private: true, image: true, mapped: false, writable_only: true, executable: false };

    pub fn accepts(&self, r: &MemRegion) -> bool {
        let kind = match r.kind {
            RegionKind::Private => self.private,
            RegionKind::Image => self.image,
            RegionKind::Mapped => self.mapped,
        };
        kind && (!self.writable_only || r.writable) && (self.executable || !r.executable)
    }

    pub fn apply(&self, regions: &[MemRegion]) -> Vec<MemRegion> {
        regions.iter().copied().filter(|r| self.accepts(r)).collect()
    }
}

/// One unit of parallel work: owns `[base, base+len)` but reads `read_len >= len` bytes so
/// values straddling the boundary are still seen exactly once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Job {
    pub base: usize,
    pub len: usize,
    pub read_len: usize,
}

pub fn chunk_jobs(regions: &[MemRegion], chunk: usize, overlap: usize) -> Vec<Job> {
    let chunk = chunk.max(PAGE);
    let mut jobs = Vec::new();
    for r in regions {
        let mut at = r.base;
        while at < r.end() {
            let len = chunk.min(r.end() - at);
            let read_len = (len + overlap).min(r.end() - at);
            jobs.push(Job { base: at, len, read_len });
            at += len;
        }
    }
    jobs
}

/// Reads `buf.len()` bytes at `addr`; on failure retries page by page. Returns the readable
/// `(start, end)` ranges of `buf` (merged).
pub fn read_segments<R: ReadMem + ?Sized>(mem: &R, addr: usize, buf: &mut [u8]) -> Vec<(usize, usize)> {
    if buf.is_empty() {
        return Vec::new();
    }
    if mem.read(addr, buf) {
        return vec![(0, buf.len())];
    }
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut off = 0usize;
    while off < buf.len() {
        let page_end = ((addr + off) / PAGE + 1) * PAGE - addr;
        let end = page_end.min(buf.len());
        if mem.read(addr + off, &mut buf[off..end]) {
            match out.last_mut() {
                Some(last) if last.1 == off => last.1 = end,
                _ => out.push((off, end)),
            }
        }
        off = end;
    }
    out
}

/// In-memory fake address space.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct FakeMem {
    regions: Vec<(MemRegion, Vec<u8>)>,
}

#[cfg(test)]
impl FakeMem {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, base: usize, data: Vec<u8>, kind: RegionKind, writable: bool) {
        let r = MemRegion { base, size: data.len(), kind, writable, executable: false };
        self.regions.push((r, data));
        self.regions.sort_by_key(|(r, _)| r.base);
    }

    pub fn put(&mut self, addr: usize, bytes: &[u8]) {
        let (r, d) = self.regions.iter_mut().find(|(r, _)| r.contains(addr)).expect("unmapped fake address");
        let off = addr - r.base;
        d[off..off + bytes.len()].copy_from_slice(bytes);
    }

    pub fn put_u64(&mut self, addr: usize, v: u64) {
        self.put(addr, &v.to_le_bytes());
    }
}

#[cfg(test)]
impl ReadMem for FakeMem {
    fn read(&self, addr: usize, buf: &mut [u8]) -> bool {
        let Some((r, d)) = self.regions.iter().find(|(r, _)| r.contains(addr)) else {
            return false;
        };
        let off = addr - r.base;
        match d.get(off..off + buf.len()) {
            Some(src) => {
                buf.copy_from_slice(src);
                true
            }
            None => false,
        }
    }

    fn regions(&self) -> Vec<MemRegion> {
        self.regions.iter().map(|(r, _)| *r).collect()
    }
}

/// Adapts a [`ReadMem`] to `acr_ue`'s [`Memory`] with a page cache (reflection walks touch the
/// same pages over and over; one syscall per page instead of per field). Writes always fail.
pub struct PageCache<R: ReadMem> {
    inner: R,
    pages: Mutex<HashMap<usize, Option<Box<[u8]>>>>,
    max_pages: usize,
}

impl<R: ReadMem> PageCache<R> {
    pub fn new(inner: R, max_pages: usize) -> Self {
        Self { inner, pages: Mutex::new(HashMap::new()), max_pages: max_pages.max(16) }
    }

    pub fn inner(&self) -> &R {
        &self.inner
    }

    fn with_page<T>(&self, page: usize, f: impl FnOnce(&[u8]) -> T) -> Option<T> {
        let mut pages = self.pages.lock().ok()?;
        if !pages.contains_key(&page) {
            if pages.len() >= self.max_pages {
                pages.clear();
            }
            let mut buf = vec![0u8; PAGE].into_boxed_slice();
            let ok = self.inner.read(page, &mut buf);
            pages.insert(page, ok.then_some(buf));
        }
        pages.get(&page)?.as_deref().map(f)
    }
}

impl<R: ReadMem> Memory for PageCache<R> {
    fn read(&self, addr: usize, buf: &mut [u8]) -> bool {
        if buf.is_empty() {
            return true;
        }
        if addr < 0x10000 || addr.checked_add(buf.len()).is_none() {
            return false;
        }
        if buf.len() > 16 * PAGE {
            return self.inner.read(addr, buf);
        }
        let mut done = 0usize;
        while done < buf.len() {
            let a = addr + done;
            let page = a & !(PAGE - 1);
            let off = a - page;
            let n = (PAGE - off).min(buf.len() - done);
            let ok = self.with_page(page, |p| buf[done..done + n].copy_from_slice(&p[off..off + n]));
            if ok.is_none() {
                return false;
            }
            done += n;
        }
        true
    }

    fn write(&self, _addr: usize, _data: &[u8]) -> bool {
        false
    }
}

/// Uncached [`Memory`] view of a [`ReadMem`] (for live values that must not be stale).
pub struct Live<R: ReadMem>(pub R);

impl<R: ReadMem> Memory for Live<R> {
    fn read(&self, addr: usize, buf: &mut [u8]) -> bool {
        buf.is_empty() || (addr >= 0x10000 && self.0.read(addr, buf))
    }

    fn write(&self, _addr: usize, _data: &[u8]) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(base: usize, size: usize, kind: RegionKind, writable: bool, executable: bool) -> MemRegion {
        MemRegion { base, size, kind, writable, executable }
    }

    #[test]
    fn filter_regions() {
        let rs = [
            region(0x1000, 0x1000, RegionKind::Private, true, false),
            region(0x2000, 0x1000, RegionKind::Image, false, true),
            region(0x3000, 0x1000, RegionKind::Image, true, false),
            region(0x4000, 0x1000, RegionKind::Mapped, true, false),
            region(0x5000, 0x1000, RegionKind::Private, false, false),
        ];
        let kept: Vec<usize> = RegionFilter::DATA.apply(&rs).iter().map(|r| r.base).collect();
        assert_eq!(kept, vec![0x1000, 0x3000]);
        let all = RegionFilter { mapped: true, writable_only: false, executable: true, ..RegionFilter::DATA };
        assert_eq!(all.apply(&rs).len(), 5);
    }

    #[test]
    fn jobs_cover_regions_with_overlap() {
        let rs = [region(0x10000, 0x2800, RegionKind::Private, true, false)];
        let jobs = chunk_jobs(&rs, 0x1000, 0x18);
        assert_eq!(jobs.len(), 3);
        assert_eq!(jobs[0], Job { base: 0x10000, len: 0x1000, read_len: 0x1018 });
        assert_eq!(jobs[2], Job { base: 0x12000, len: 0x800, read_len: 0x800 });
        let total: usize = jobs.iter().map(|j| j.len).sum();
        assert_eq!(total, 0x2800);
    }

    #[test]
    fn segments_skip_unreadable_pages() {
        let mut m = FakeMem::new();
        m.add(0x10000, vec![1; 0x1000], RegionKind::Private, true);
        m.add(0x12000, vec![2; 0x1000], RegionKind::Private, true);
        let mut buf = vec![0u8; 0x3000];
        let segs = read_segments(&m, 0x10000, &mut buf);
        assert_eq!(segs, vec![(0, 0x1000), (0x2000, 0x3000)]);
        assert_eq!(buf[0x2000], 2);
        let mut buf = vec![0u8; 0x800];
        assert_eq!(read_segments(&m, 0x10400, &mut buf), vec![(0, 0x800)]);
    }

    #[test]
    fn page_cache_reads_across_pages_and_never_writes() {
        let mut m = FakeMem::new();
        let data: Vec<u8> = (0..0x3000).map(|i| (i % 251) as u8).collect();
        m.add(0x20000, data.clone(), RegionKind::Private, true);
        let c = PageCache::new(&m, 2);
        let mut buf = vec![0u8; 0x1800];
        assert!(Memory::read(&c, 0x20F00, &mut buf));
        assert_eq!(&buf[..], &data[0xF00..0xF00 + 0x1800]);
        assert_eq!(c.read_u32(0x20004), Some(u32::from_le_bytes(data[4..8].try_into().unwrap())));
        assert!(!Memory::read(&c, 0x23000, &mut [0u8; 4]));
        assert!(!c.write(0x20000, &[0]));
        assert!(!c.write_u32(0x20000, 5));
        assert_eq!(c.read_u8(0x20000), Some(0));
        let live = Live(&m);
        assert!(!live.write_u32(0x20000, 5));
        assert_eq!(live.read_u8(0x20001), Some(1));
    }
}
