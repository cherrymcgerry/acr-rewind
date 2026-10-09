//! `dwmapi.dll` proxy. Dropped next to `acr.exe`, the game loads this instead of the system
//! DLL; every `Dwm*` export is a linker forwarder to the real `C:\Windows\System32\dwmapi.dll`
//! (see `build.rs`), so the game behaves normally. On attach we also load `acr_hook.dll` from
//! our own directory, which starts the rewind mod.
//!
//! If `acr_hook.dll` is missing or fails to load, the game still runs (the forwarders keep
//! working); we only log to a sibling file since no console is attached.

#![cfg(windows)]

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{BOOL, HMODULE, TRUE};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, LoadLibraryW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

const HOOK_DLL: &str = "acr_hook.dll";

/// Directory this proxy DLL was loaded from.
fn self_dir() -> Option<PathBuf> {
    let mut module = HMODULE::default();
    // SAFETY: the address belongs to this module, so the returned handle is valid.
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(self_dir as *const u16),
            &mut module,
        )
        .ok()?;
    }
    let mut buf = vec![0u16; 32768];
    // SAFETY: valid module handle and buffer.
    let n = unsafe { GetModuleFileNameW(module, &mut buf) } as usize;
    if n == 0 {
        return None;
    }
    PathBuf::from(String::from_utf16_lossy(&buf[..n])).parent().map(PathBuf::from)
}

fn log(dir: &Path, msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("acr-loader.log")) {
        let _ = writeln!(f, "{msg}");
    }
}

fn load_hook() {
    let Some(dir) = self_dir() else {
        return;
    };
    let path = dir.join(HOOK_DLL);
    let wide = HSTRING::from(path.as_os_str());
    // SAFETY: valid null-terminated path. The handle is intentionally leaked so the hook stays
    // resident for the life of the process.
    match unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) } {
        Ok(_) => log(&dir, &format!("loaded {}", path.display())),
        Err(e) => log(&dir, &format!("failed to load {}: {e}", path.display())),
    }
}

/// # Safety
/// Called by the Windows loader.
#[no_mangle]
pub unsafe extern "system" fn DllMain(_hinst: HMODULE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        load_hook();
    }
    TRUE
}
