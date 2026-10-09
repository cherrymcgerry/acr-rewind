//! Opening and reading the named file mappings.

use crate::layout::{
    RawGraphics, RawMozaHeader, RawPage, RawPhysics, RawStatic, GRAPHICS_TAG, MOZA_TAG, PHYSICS_TAG, STATIC_TAG,
};
use crate::pages::{Graphics, MozaInfo, Physics, Snapshot, StaticInfo};

#[derive(Debug, thiserror::Error)]
pub enum ShmError {
    /// The mapping does not exist: the game is not running (or hasn't created it yet).
    #[error("shared memory page {0} not found (is Assetto Corsa Rally running?)")]
    NotRunning(&'static str),
    #[error("failed to map {page}: {message}")]
    Os { page: &'static str, message: String },
    #[error("shared memory is only available on Windows")]
    Unsupported,
}

/// Handle to all three pages. Holding it keeps the mappings alive even after the game exits,
/// so watch [`Physics::packet_id`] to detect staleness.
pub struct SharedMemory {
    physics: imp::Mapping,
    graphics: imp::Mapping,
    static_: imp::Mapping,
}

/// Retries for a physics read that raced with a game write.
const TORN_READ_RETRIES: usize = 3;

impl SharedMemory {
    /// Opens all three pages read-only.
    pub fn open() -> Result<Self, ShmError> {
        Ok(Self {
            physics: imp::Mapping::open(PHYSICS_TAG)?,
            graphics: imp::Mapping::open(GRAPHICS_TAG)?,
            static_: imp::Mapping::open(STATIC_TAG)?,
        })
    }

    pub fn read_raw_physics(&self) -> RawPhysics {
        let mut page: RawPhysics = self.physics.read();
        for _ in 0..TORN_READ_RETRIES {
            let again: RawPhysics = self.physics.read();
            if { again.packetId } == { page.packetId } {
                break;
            }
            page = again;
        }
        page
    }

    pub fn read_raw_graphics(&self) -> RawGraphics {
        self.graphics.read()
    }

    pub fn read_raw_static(&self) -> RawStatic {
        self.static_.read()
    }

    pub fn physics(&self) -> Physics {
        Physics::from(&self.read_raw_physics())
    }

    pub fn graphics(&self) -> Graphics {
        Graphics::from(&self.read_raw_graphics())
    }

    pub fn static_info(&self) -> StaticInfo {
        StaticInfo::from(&self.read_raw_static())
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot { physics: self.physics(), graphics: self.graphics(), static_info: self.static_info() }
    }
}

/// Handle to the optional `Local\acpmf_Moza` page (opened separately: it may not exist).
pub struct MozaPage {
    page: imp::Mapping,
}

impl MozaPage {
    pub fn open() -> Result<Self, ShmError> {
        Ok(Self { page: imp::Mapping::open(MOZA_TAG)? })
    }

    pub fn read(&self) -> MozaInfo {
        MozaInfo::from(&self.page.read::<RawMozaHeader>())
    }
}

#[cfg(windows)]
mod imp {
    use super::{RawPage, ShmError};
    use std::ffi::c_void;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, HANDLE};
    use windows::Win32::System::Memory::{
        MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, VirtualQuery, FILE_MAP_READ, MEMORY_BASIC_INFORMATION,
        MEMORY_MAPPED_VIEW_ADDRESS,
    };

    pub struct Mapping {
        handle: HANDLE,
        view: MEMORY_MAPPED_VIEW_ADDRESS,
        len: usize,
    }

    // SAFETY: the view is read-only and only accessed through volatile copies.
    unsafe impl Send for Mapping {}
    unsafe impl Sync for Mapping {}

    impl Mapping {
        pub fn open(tag: &'static str) -> Result<Self, ShmError> {
            let name: Vec<u16> = tag.encode_utf16().chain(std::iter::once(0)).collect();
            // SAFETY: `name` is a valid NUL-terminated wide string that outlives the call.
            let handle = unsafe { OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR(name.as_ptr())) }.map_err(|e| {
                if e.code() == ERROR_FILE_NOT_FOUND.to_hresult() {
                    ShmError::NotRunning(tag)
                } else {
                    ShmError::Os { page: tag, message: e.message() }
                }
            })?;
            // SAFETY: `handle` is a valid mapping handle; 0 bytes maps the whole section.
            let view = unsafe { MapViewOfFile(handle, FILE_MAP_READ, 0, 0, 0) };
            if view.Value.is_null() {
                let err = windows::core::Error::from_win32();
                // SAFETY: handle was returned by OpenFileMappingW above.
                unsafe {
                    let _ = CloseHandle(handle);
                }
                return Err(ShmError::Os { page: tag, message: err.message() });
            }
            let mut info = MEMORY_BASIC_INFORMATION::default();
            // SAFETY: `info` is a valid out-pointer of the stated size.
            let n = unsafe {
                VirtualQuery(
                    Some(view.Value as *const c_void),
                    &mut info,
                    std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
                )
            };
            let len = if n == 0 { 0 } else { info.RegionSize };
            Ok(Self { handle, view, len })
        }

        /// Copies the page out of the mapping. Bytes beyond the mapped size read as zero.
        pub fn read<T: RawPage>(&self) -> T {
            let n = self.len.min(T::SIZE);
            let mut buf = vec![0u8; T::SIZE];
            let src = self.view.Value as *const u8;
            for (i, b) in buf.iter_mut().enumerate().take(n) {
                // SAFETY: i < len, the size of the committed view region; the game writes
                // concurrently, so read volatile to avoid the compiler assuming stability.
                *b = unsafe { std::ptr::read_volatile(src.add(i)) };
            }
            T::from_bytes(&buf)
        }
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            // SAFETY: view and handle were obtained in `open` and are released exactly once.
            unsafe {
                let _ = UnmapViewOfFile(self.view);
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::layout::PHYSICS_SIZE;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows::Win32::System::Memory::{
        CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_WRITE, PAGE_READWRITE,
    };

    #[test]
    fn missing_mapping_reports_not_running() {
        let err = imp::Mapping::open("Local\\acr_rewind_test_does_not_exist").err().unwrap();
        assert!(matches!(err, ShmError::NotRunning(_)));
    }

    #[test]
    fn reads_live_named_mapping() {
        let tag: &'static str = Box::leak(format!("Local\\acr_rewind_test_{}", std::process::id()).into_boxed_str());
        let name: Vec<u16> = tag.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: plain Win32 calls with valid arguments; resources are released below.
        unsafe {
            let h = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                0,
                PHYSICS_SIZE as u32,
                PCWSTR(name.as_ptr()),
            )
            .unwrap();
            let view = MapViewOfFile(h, FILE_MAP_WRITE, 0, 0, 0);
            let p = view.Value as *mut u8;
            p.cast::<i32>().write(77);
            p.add(16).cast::<i32>().write(4); // gear raw 4 = 3rd
            p.add(20).cast::<i32>().write(5000);

            let m = imp::Mapping::open(tag).unwrap();
            let raw: RawPhysics = m.read();
            let phys = Physics::from(&raw);
            assert_eq!(phys.packet_id, 77);
            assert_eq!(phys.gear, 3);
            assert_eq!(phys.rpm, 5000);

            p.cast::<i32>().write(78);
            assert_eq!({ m.read::<RawPhysics>().packetId }, 78);

            drop(m);
            let _ = UnmapViewOfFile(view);
            let _ = CloseHandle(h);
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{RawPage, ShmError};

    pub struct Mapping;

    impl Mapping {
        pub fn open(_tag: &'static str) -> Result<Self, ShmError> {
            Err(ShmError::Unsupported)
        }

        pub fn read<T: RawPage>(&self) -> T {
            T::from_bytes(&[])
        }
    }
}
