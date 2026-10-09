//! Development injector: loads `acr_hook.dll` into a running `acr.exe` via
//! `CreateRemoteThread(LoadLibraryW)`. For iterating on the hook without restarting the game
//! through the `dwmapi.dll` proxy.
//!
//! Usage: `injector [path\to\acr_hook.dll] [process.exe]`
//! Defaults: `acr_hook.dll` next to the injector, target `acr.exe`.
//!
//! This only injects into an already-running process. It never launches, installs, or modifies
//! the game.

#![cfg(windows)]

use std::ffi::c_void;
use std::mem::size_of;
use std::path::PathBuf;
use windows::core::{s, w, HSTRING};
use windows::Win32::Foundation::{CloseHandle, FALSE, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{
    VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{
    CreateRemoteThread, GetExitCodeThread, OpenProcess, WaitForSingleObject, LPTHREAD_START_ROUTINE,
    PROCESS_CREATE_THREAD, PROCESS_QUERY_INFORMATION, PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE,
};

fn find_pid(exe: &str) -> Option<u32> {
    let target = exe.to_lowercase();
    // SAFETY: snapshot handle is checked and closed; entry struct is correctly sized.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut found = None;
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let n = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..n]);
                if name.to_lowercase() == target {
                    found = Some(entry.th32ProcessID);
                    break;
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        found
    }
}

fn inject(pid: u32, dll: &HSTRING) -> Result<(), String> {
    let bytes = (dll.len() + 1) * 2; // wide, null-terminated
                                     // SAFETY: each call is checked; the remote allocation is freed and handles are closed.
    unsafe {
        let proc = OpenProcess(
            PROCESS_CREATE_THREAD
                | PROCESS_QUERY_INFORMATION
                | PROCESS_VM_OPERATION
                | PROCESS_VM_WRITE
                | PROCESS_VM_READ,
            FALSE,
            pid,
        )
        .map_err(|e| format!("OpenProcess: {e}"))?;

        let result = (|| {
            let remote = VirtualAllocEx(proc, None, bytes, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
            if remote.is_null() {
                return Err("VirtualAllocEx failed".to_owned());
            }
            let write = WriteProcessMemory(proc, remote, dll.as_ptr() as *const c_void, bytes, None);
            if write.is_err() {
                let _ = VirtualFreeEx(proc, remote, 0, MEM_RELEASE);
                return Err(format!("WriteProcessMemory: {write:?}"));
            }
            let kernel32 = GetModuleHandleW(w!("kernel32.dll")).map_err(|e| format!("kernel32: {e}"))?;
            let load = GetProcAddress(kernel32, s!("LoadLibraryW")).ok_or("LoadLibraryW not found")?;
            let start: LPTHREAD_START_ROUTINE = Some(std::mem::transmute::<
                unsafe extern "system" fn() -> isize,
                unsafe extern "system" fn(*mut c_void) -> u32,
            >(load));
            let thread = CreateRemoteThread(proc, None, 0, start, Some(remote), 0, None)
                .map_err(|e| format!("CreateRemoteThread: {e}"))?;
            let r = wait_thread(thread);
            let _ = CloseHandle(thread);
            let _ = VirtualFreeEx(proc, remote, 0, MEM_RELEASE);
            r
        })();
        let _ = CloseHandle(proc);
        result
    }
}

/// Waits for the remote `LoadLibraryW` and reports whether the module loaded.
unsafe fn wait_thread(thread: HANDLE) -> Result<(), String> {
    // SAFETY: caller passes a valid thread handle.
    unsafe {
        if WaitForSingleObject(thread, 15_000) != WAIT_OBJECT_0 {
            return Err("remote thread timed out".into());
        }
        let mut code = 0u32;
        GetExitCodeThread(thread, &mut code).map_err(|e| format!("GetExitCodeThread: {e}"))?;
        // LoadLibraryW returns the HMODULE truncated to 32 bits; 0 means it failed.
        if code == 0 {
            return Err("remote LoadLibraryW returned NULL (DLL failed to load)".into());
        }
        Ok(())
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dll_path = args.get(1).map(PathBuf::from).unwrap_or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("acr_hook.dll")))
            .unwrap_or_else(|| PathBuf::from("acr_hook.dll"))
    });
    let exe = args.get(2).cloned().unwrap_or_else(|| "acr.exe".into());

    let dll_path = match dll_path.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cannot find {}: {e}", dll_path.display());
            std::process::exit(2);
        }
    };
    let dll = HSTRING::from(dll_path.as_os_str());

    let Some(pid) = find_pid(&exe) else {
        eprintln!("{exe} is not running. Start the game first, then run the injector.");
        std::process::exit(1);
    };
    println!("injecting {} into {exe} (pid {pid})", dll_path.display());
    match inject(pid, &dll) {
        Ok(()) => println!("done; see acr-rewind.log next to the DLL"),
        Err(e) => {
            eprintln!("injection failed: {e}");
            std::process::exit(1);
        }
    }
}
