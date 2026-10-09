//! Injected rewind DLL for Assetto Corsa Rally.
//!
//! `DllMain` only spawns the init thread ([`init::run`]); everything else happens there:
//! config + signatures from the DLL's directory, engine resolution through `acr_ue`, the
//! world-tick hook, the online guard thread and the hudhook overlay. The per-frame logic is
//! the platform-independent [`driver::Driver`].

pub mod config;
pub mod driver;
pub mod engine;
pub mod ffb;
pub mod gate;
pub mod guard;
pub mod input;
pub mod overlay;
pub mod settings;
pub mod validate;

#[cfg(windows)]
mod hooks;
#[cfg(windows)]
mod init;
#[cfg(windows)]
mod runtime;

#[cfg(windows)]
mod entry {
    use std::ffi::c_void;
    use std::path::PathBuf;
    use windows::Win32::Foundation::{BOOL, HMODULE, TRUE};
    use windows::Win32::System::LibraryLoader::{DisableThreadLibraryCalls, GetModuleFileNameW};
    use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

    fn module_dir(h: HMODULE) -> PathBuf {
        let mut buf = vec![0u16; 32768];
        // SAFETY: valid module handle and buffer.
        let n = unsafe { GetModuleFileNameW(h, &mut buf) } as usize;
        let path = PathBuf::from(String::from_utf16_lossy(&buf[..n]));
        path.parent().map(PathBuf::from).unwrap_or_default()
    }

    /// # Safety
    /// Called by the Windows loader.
    #[no_mangle]
    pub unsafe extern "system" fn DllMain(hinst: HMODULE, reason: u32, _reserved: *mut c_void) -> BOOL {
        if reason == DLL_PROCESS_ATTACH {
            // SAFETY: valid module handle from the loader.
            unsafe {
                let _ = DisableThreadLibraryCalls(hinst);
            }
            let dir = module_dir(hinst);
            let h = hinst.0 as usize;
            // The thread starts running once the loader lock is released.
            let _ = std::thread::Builder::new().name("acr-rewind-init".into()).spawn(move || {
                let _ = std::panic::catch_unwind(|| super::init::run(h, dir));
            });
        }
        TRUE
    }
}
