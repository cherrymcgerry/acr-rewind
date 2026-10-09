//! Fault-tolerant memory access.
//!
//! Everything that touches game memory goes through [`Memory`], so a bad pointer from a stale
//! or wrong offset turns into `None` instead of an access violation inside the game.
//! [`ProcessMemory`] uses `ReadProcessMemory`/`WriteProcessMemory` on the current process
//! (the kernel validates the range); [`SliceMemory`] is an in-memory fake for tests.
//! [`ScanSource`] adds region enumeration for whole-address-space scans.

use std::sync::Mutex;

pub trait Memory {
    /// Fills `buf` from `addr`. Returns false (and leaves `buf` unspecified) if any byte is
    /// unreadable.
    fn read(&self, addr: usize, buf: &mut [u8]) -> bool;
    /// Writes `data` at `addr`. Returns false if any byte is unwritable.
    fn write(&self, addr: usize, data: &[u8]) -> bool;

    fn read_u8(&self, addr: usize) -> Option<u8> {
        let mut b = [0u8; 1];
        self.read(addr, &mut b).then_some(b[0])
    }
    fn read_u16(&self, addr: usize) -> Option<u16> {
        let mut b = [0u8; 2];
        self.read(addr, &mut b).then(|| u16::from_le_bytes(b))
    }
    fn read_u32(&self, addr: usize) -> Option<u32> {
        let mut b = [0u8; 4];
        self.read(addr, &mut b).then(|| u32::from_le_bytes(b))
    }
    fn read_i32(&self, addr: usize) -> Option<i32> {
        self.read_u32(addr).map(|v| v as i32)
    }
    fn read_u64(&self, addr: usize) -> Option<u64> {
        let mut b = [0u8; 8];
        self.read(addr, &mut b).then(|| u64::from_le_bytes(b))
    }
    fn read_ptr(&self, addr: usize) -> Option<usize> {
        self.read_u64(addr).map(|v| v as usize)
    }
    /// Reads a pointer and rejects null / obviously non-canonical values.
    fn read_valid_ptr(&self, addr: usize) -> Option<usize> {
        self.read_ptr(addr).filter(|&p| is_plausible_ptr(p))
    }
    fn read_f32(&self, addr: usize) -> Option<f32> {
        self.read_u32(addr).map(f32::from_bits)
    }
    fn read_f64(&self, addr: usize) -> Option<f64> {
        self.read_u64(addr).map(f64::from_bits)
    }
    fn read_vec(&self, addr: usize, len: usize) -> Option<Vec<u8>> {
        let mut v = vec![0u8; len];
        self.read(addr, &mut v).then_some(v)
    }

    fn write_u32(&self, addr: usize, v: u32) -> bool {
        self.write(addr, &v.to_le_bytes())
    }
    fn write_i32(&self, addr: usize, v: i32) -> bool {
        self.write(addr, &v.to_le_bytes())
    }
    fn write_f32(&self, addr: usize, v: f32) -> bool {
        self.write(addr, &v.to_le_bytes())
    }
    fn write_f64(&self, addr: usize, v: f64) -> bool {
        self.write(addr, &v.to_le_bytes())
    }
}

impl<M: Memory + ?Sized> Memory for &M {
    fn read(&self, addr: usize, buf: &mut [u8]) -> bool {
        (**self).read(addr, buf)
    }
    fn write(&self, addr: usize, data: &[u8]) -> bool {
        (**self).write(addr, data)
    }
}

impl<M: Memory + ?Sized> Memory for std::sync::Arc<M> {
    fn read(&self, addr: usize, buf: &mut [u8]) -> bool {
        (**self).read(addr, buf)
    }
    fn write(&self, addr: usize, data: &[u8]) -> bool {
        (**self).write(addr, data)
    }
}

/// A committed address range `[base, base + size)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanRegion {
    pub base: usize,
    pub size: usize,
}

/// Memory whose committed, private, read-write regions can be enumerated (heap objects live
/// there). Implemented in-process by [`ProcessMemory`] and externally by `acr-probe`.
pub trait ScanSource: Memory {
    /// Committed `MEM_PRIVATE` regions with plain read-write protection (no guard pages, not
    /// executable), in ascending address order.
    fn scan_regions(&self) -> Vec<ScanRegion>;
}

impl<S: ScanSource + ?Sized> ScanSource for &S {
    fn scan_regions(&self) -> Vec<ScanRegion> {
        (**self).scan_regions()
    }
}

impl<S: ScanSource + ?Sized> ScanSource for std::sync::Arc<S> {
    fn scan_regions(&self) -> Vec<ScanRegion> {
        (**self).scan_regions()
    }
}

/// User-mode x64 pointers live below 0x0000_8000_0000_0000; the low 64 KiB is never mapped.
pub fn is_plausible_ptr(p: usize) -> bool {
    (0x10000..0x0000_8000_0000_0000).contains(&p)
}

/// In-memory fake address space made of one or more `(base, bytes)` regions.
#[derive(Debug, Default)]
pub struct SliceMemory {
    regions: Mutex<Vec<(usize, Vec<u8>)>>,
}

impl SliceMemory {
    pub fn new(base: usize, data: Vec<u8>) -> Self {
        Self { regions: Mutex::new(vec![(base, data)]) }
    }

    pub fn add_region(&self, base: usize, data: Vec<u8>) {
        let mut r = self.regions.lock().unwrap();
        r.push((base, data));
        r.sort_by_key(|(b, _)| *b);
    }

    /// Removes the region starting at `base` (simulates a freed heap block).
    pub fn remove_region(&self, base: usize) {
        self.regions.lock().unwrap().retain(|(b, _)| *b != base);
    }

    /// Copy of `len` bytes at `addr` (panics if unmapped; test helper).
    pub fn bytes(&self, addr: usize, len: usize) -> Vec<u8> {
        let mut v = vec![0u8; len];
        assert!(self.read(addr, &mut v), "unmapped {addr:#x}");
        v
    }

    fn with_range<R>(&self, addr: usize, len: usize, f: impl FnOnce(&mut [u8]) -> R) -> Option<R> {
        let mut regions = self.regions.lock().ok()?;
        let (base, data) =
            regions.iter_mut().find(|(b, d)| addr >= *b && addr.checked_add(len).is_some_and(|e| e <= *b + d.len()))?;
        let off = addr - *base;
        Some(f(&mut data[off..off + len]))
    }
}

impl Memory for SliceMemory {
    fn read(&self, addr: usize, buf: &mut [u8]) -> bool {
        self.with_range(addr, buf.len(), |src| buf.copy_from_slice(src)).is_some()
    }

    fn write(&self, addr: usize, data: &[u8]) -> bool {
        self.with_range(addr, data.len(), |dst| dst.copy_from_slice(data)).is_some()
    }
}

impl ScanSource for SliceMemory {
    fn scan_regions(&self) -> Vec<ScanRegion> {
        self.regions
            .lock()
            .map(|r| r.iter().map(|(base, d)| ScanRegion { base: *base, size: d.len() }).collect())
            .unwrap_or_default()
    }
}

/// The current process, accessed through the kernel so faults become errors.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessMemory;

#[cfg(windows)]
impl Memory for ProcessMemory {
    fn read(&self, addr: usize, buf: &mut [u8]) -> bool {
        use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
        use windows::Win32::System::Threading::GetCurrentProcess;
        if buf.is_empty() {
            return true;
        }
        if addr.checked_add(buf.len()).is_none() || addr < 0x10000 {
            return false;
        }
        let mut n = 0usize;
        // SAFETY: the kernel validates the source range; `buf` is a valid destination.
        let ok = unsafe {
            ReadProcessMemory(GetCurrentProcess(), addr as *const _, buf.as_mut_ptr().cast(), buf.len(), Some(&mut n))
        };
        ok.is_ok() && n == buf.len()
    }

    fn write(&self, addr: usize, data: &[u8]) -> bool {
        use windows::Win32::System::Diagnostics::Debug::WriteProcessMemory;
        use windows::Win32::System::Threading::GetCurrentProcess;
        if data.is_empty() {
            return true;
        }
        if addr.checked_add(data.len()).is_none() || addr < 0x10000 {
            return false;
        }
        let mut n = 0usize;
        // SAFETY: the kernel validates the destination range; `data` is a valid source.
        let ok = unsafe {
            WriteProcessMemory(GetCurrentProcess(), addr as *const _, data.as_ptr().cast(), data.len(), Some(&mut n))
        };
        ok.is_ok() && n == data.len()
    }
}

#[cfg(not(windows))]
impl Memory for ProcessMemory {
    fn read(&self, _addr: usize, _buf: &mut [u8]) -> bool {
        false
    }
    fn write(&self, _addr: usize, _data: &[u8]) -> bool {
        false
    }
}

#[cfg(windows)]
impl ScanSource for ProcessMemory {
    fn scan_regions(&self) -> Vec<ScanRegion> {
        use windows::Win32::System::Memory::{
            VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT, MEM_PRIVATE, PAGE_READWRITE,
        };
        let mut out = Vec::new();
        let mut addr = 0x10000usize;
        while addr < 0x7FFF_FFFF_0000 {
            let mut mbi = MEMORY_BASIC_INFORMATION::default();
            // SAFETY: VirtualQuery only inspects the address; `mbi` is a valid out-pointer.
            let n = unsafe {
                VirtualQuery(Some(addr as *const _), &mut mbi, std::mem::size_of::<MEMORY_BASIC_INFORMATION>())
            };
            if n == 0 || mbi.RegionSize == 0 {
                break;
            }
            let base = mbi.BaseAddress as usize;
            // Exactly PAGE_READWRITE: excludes guard pages, write-copy and executable memory.
            if mbi.State == MEM_COMMIT && mbi.Type == MEM_PRIVATE && mbi.Protect == PAGE_READWRITE {
                out.push(ScanRegion { base, size: mbi.RegionSize });
            }
            addr = base.saturating_add(mbi.RegionSize);
        }
        out
    }
}

#[cfg(not(windows))]
impl ScanSource for ProcessMemory {
    fn scan_regions(&self) -> Vec<ScanRegion> {
        Vec::new()
    }
}

/// Returns true if `[addr, addr+len)` is committed and readable (no guard pages).
#[cfg(windows)]
pub fn is_readable_range(addr: usize, len: usize) -> bool {
    use windows::Win32::System::Memory::{
        VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
        PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE, PAGE_WRITECOPY,
    };
    let Some(end) = addr.checked_add(len) else {
        return false;
    };
    let mut cur = addr;
    while cur < end {
        let mut mbi = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: VirtualQuery only inspects the address; `mbi` is a valid out-pointer.
        let n =
            unsafe { VirtualQuery(Some(cur as *const _), &mut mbi, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) };
        if n == 0 || mbi.State != MEM_COMMIT {
            return false;
        }
        let p = mbi.Protect;
        if (p & PAGE_GUARD).0 != 0 || (p & PAGE_NOACCESS).0 != 0 {
            return false;
        }
        let readable = [
            PAGE_READONLY,
            PAGE_READWRITE,
            PAGE_WRITECOPY,
            PAGE_EXECUTE_READ,
            PAGE_EXECUTE_READWRITE,
            PAGE_EXECUTE_WRITECOPY,
        ]
        .iter()
        .any(|f| (p & *f).0 != 0);
        if !readable {
            return false;
        }
        cur = mbi.BaseAddress as usize + mbi.RegionSize;
    }
    true
}

#[cfg(not(windows))]
pub fn is_readable_range(_addr: usize, _len: usize) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_memory_reads_and_writes() {
        let m = SliceMemory::new(0x1000, vec![0; 16]);
        assert!(m.write_u32(0x1004, 0xAABBCCDD));
        assert_eq!(m.read_u32(0x1004), Some(0xAABBCCDD));
        assert_eq!(m.read_u16(0x1004), Some(0xCCDD));
        assert!(m.read_u64(0x100C).is_none(), "crosses end");
        assert!(m.read_u8(0xFFF).is_none());
        m.add_region(0x9000, vec![1, 2, 3]);
        assert_eq!(m.read_u8(0x9002), Some(3));
        assert!(m.write_f64(0x1008, 1.5));
        assert_eq!(m.read_f64(0x1008), Some(1.5));
    }

    #[test]
    fn plausible_pointers() {
        assert!(!is_plausible_ptr(0));
        assert!(!is_plausible_ptr(0xFFFF));
        assert!(is_plausible_ptr(0x7FF6_0000_0000));
        assert!(!is_plausible_ptr(0xFFFF_8000_0000_0000));
    }

    #[cfg(windows)]
    #[test]
    fn process_memory_is_fault_tolerant() {
        let m = ProcessMemory;
        let value: u64 = 0x1122_3344_5566_7788;
        let addr = &value as *const u64 as usize;
        assert_eq!(m.read_u64(addr), Some(value));
        assert_eq!(m.read_u64(0), None);
        assert_eq!(m.read_u64(0x10), None);
        assert_eq!(m.read_u64(0x7FFF_FFFF_0000), None, "unmapped");
        let mut target = [0u8; 4];
        let taddr = target.as_mut_ptr() as usize;
        assert!(m.write_u32(taddr, 7));
        // SAFETY: `target` is a live local; volatile because the write bypassed the compiler.
        let after = unsafe { std::ptr::read_volatile(&target) };
        assert_eq!(u32::from_le_bytes(after), 7);
        assert!(!m.write_u32(0x10, 7));
        assert!(is_readable_range(addr, 8));
        assert!(!is_readable_range(0x10, 8));
    }

    #[cfg(windows)]
    #[test]
    fn process_scan_regions_cover_heap_but_not_code() {
        let heap = vec![0u8; 0x10_0000];
        let h = heap.as_ptr() as usize;
        let regions = ProcessMemory.scan_regions();
        assert!(regions.windows(2).all(|w| w[0].base + w[0].size <= w[1].base));
        let inside = |a: usize| regions.iter().any(|r| a >= r.base && a < r.base + r.size);
        assert!(inside(h), "heap allocation is private RW");
        let code = process_scan_regions_cover_heap_but_not_code as *const () as usize;
        assert!(!inside(code), "image code is excluded");
    }

    #[test]
    fn slice_memory_regions_sorted_and_removable() {
        let m = SliceMemory::new(0x9000, vec![0; 16]);
        m.add_region(0x1000, vec![0; 32]);
        assert_eq!(
            m.scan_regions(),
            vec![ScanRegion { base: 0x1000, size: 32 }, ScanRegion { base: 0x9000, size: 16 }]
        );
        m.remove_region(0x1000);
        assert!(m.read_u8(0x1000).is_none());
        assert_eq!(m.scan_regions().len(), 1);
    }
}
