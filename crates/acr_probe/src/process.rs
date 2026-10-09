//! The target process, opened with `PROCESS_VM_READ | PROCESS_QUERY_INFORMATION` only.
//! There is deliberately no write, allocate, thread or suspend capability in this module.

use crate::mem::{MemRegion, ReadMem};
use anyhow::{bail, Result};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct ModuleEntry {
    pub name: String,
    pub base: usize,
    pub size: usize,
    pub path: String,
}

impl ModuleEntry {
    pub fn contains(&self, addr: usize) -> bool {
        addr >= self.base && addr < self.base + self.size
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ProcessEntry {
    pub pid: u32,
    pub name: String,
}

/// `name+0xRVA` for an address inside a module, else `None`.
pub fn symbolize(modules: &[ModuleEntry], addr: usize) -> Option<String> {
    modules.iter().find(|m| m.contains(addr)).map(|m| format!("{}+{:#x}", m.name, addr - m.base))
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::mem::size_of;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, Process32FirstW, Process32NextW, MODULEENTRY32W,
        PROCESSENTRY32W, TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32, TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Memory::{
        VirtualQueryEx, MEMORY_BASIC_INFORMATION, MEM_COMMIT, MEM_IMAGE, MEM_MAPPED, PAGE_EXECUTE, PAGE_EXECUTE_READ,
        PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_NOACCESS, PAGE_PROTECTION_FLAGS,
        PAGE_READONLY, PAGE_READWRITE, PAGE_WRITECOPY,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
    };

    /// The only access rights ever requested.
    pub const ACCESS: PROCESS_ACCESS_RIGHTS = PROCESS_ACCESS_RIGHTS(PROCESS_VM_READ.0 | PROCESS_QUERY_INFORMATION.0);

    struct Handle(HANDLE);
    // SAFETY: a process handle may be used from any thread; ReadProcessMemory and
    // VirtualQueryEx are thread-safe.
    unsafe impl Send for Handle {}
    unsafe impl Sync for Handle {}

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: the handle came from OpenProcess / CreateToolhelp32Snapshot and is
            // closed exactly once.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    fn wide_to_string(w: &[u16]) -> String {
        let n = w.iter().position(|&c| c == 0).unwrap_or(w.len());
        String::from_utf16_lossy(&w[..n])
    }

    pub fn list_processes() -> Result<Vec<ProcessEntry>> {
        // SAFETY: snapshot handle is wrapped (closed on drop); the entry is correctly sized.
        unsafe {
            let snap = Handle(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)?);
            let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
            let mut out = Vec::new();
            if Process32FirstW(snap.0, &mut e).is_ok() {
                loop {
                    out.push(ProcessEntry { pid: e.th32ProcessID, name: wide_to_string(&e.szExeFile) });
                    if Process32NextW(snap.0, &mut e).is_err() {
                        break;
                    }
                }
            }
            Ok(out)
        }
    }

    pub struct RemoteProcess {
        handle: Handle,
        pub pid: u32,
    }

    impl RemoteProcess {
        pub fn open(pid: u32) -> Result<Self> {
            // SAFETY: plain OpenProcess call; the handle is owned by `Handle`.
            let h = unsafe { OpenProcess(ACCESS, false, pid) }
                .map_err(|e| anyhow::anyhow!("OpenProcess({pid}, VM_READ|QUERY_INFORMATION): {e}"))?;
            Ok(Self { handle: Handle(h), pid })
        }

        pub fn modules(&self) -> Result<Vec<ModuleEntry>> {
            // ERROR_BAD_LENGTH is transient while the target loads modules; retry briefly.
            let mut last = None;
            for _ in 0..8 {
                // SAFETY: snapshot handle is wrapped; entry struct is correctly sized.
                let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, self.pid) };
                let snap = match snap {
                    Ok(s) => Handle(s),
                    Err(e) => {
                        last = Some(e);
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        continue;
                    }
                };
                let mut e = MODULEENTRY32W { dwSize: size_of::<MODULEENTRY32W>() as u32, ..Default::default() };
                let mut out = Vec::new();
                // SAFETY: see above.
                unsafe {
                    if Module32FirstW(snap.0, &mut e).is_ok() {
                        loop {
                            out.push(ModuleEntry {
                                name: wide_to_string(&e.szModule),
                                base: e.modBaseAddr as usize,
                                size: e.modBaseSize as usize,
                                path: wide_to_string(&e.szExePath),
                            });
                            if Module32NextW(snap.0, &mut e).is_err() {
                                break;
                            }
                        }
                    }
                }
                out.sort_by_key(|m| m.base);
                return Ok(out);
            }
            bail!("module snapshot of pid {} failed: {:?}", self.pid, last)
        }
    }

    fn readable(p: PAGE_PROTECTION_FLAGS) -> bool {
        if (p & PAGE_GUARD).0 != 0 || (p & PAGE_NOACCESS).0 != 0 || p == PAGE_EXECUTE {
            return false;
        }
        [
            PAGE_READONLY,
            PAGE_READWRITE,
            PAGE_WRITECOPY,
            PAGE_EXECUTE_READ,
            PAGE_EXECUTE_READWRITE,
            PAGE_EXECUTE_WRITECOPY,
        ]
        .iter()
        .any(|f| (p & *f).0 != 0)
    }

    impl ReadMem for RemoteProcess {
        fn read(&self, addr: usize, buf: &mut [u8]) -> bool {
            if buf.is_empty() {
                return true;
            }
            if addr.checked_add(buf.len()).is_none() {
                return false;
            }
            let mut n = 0usize;
            // SAFETY: the kernel validates the remote range; `buf` is a valid destination.
            let ok = unsafe {
                ReadProcessMemory(self.handle.0, addr as *const _, buf.as_mut_ptr().cast(), buf.len(), Some(&mut n))
            };
            ok.is_ok() && n == buf.len()
        }

        fn regions(&self) -> Vec<MemRegion> {
            let mut out = Vec::new();
            let mut addr = 0usize;
            while addr < 0x7FFF_FFFF_0000 {
                let mut mbi = MEMORY_BASIC_INFORMATION::default();
                // SAFETY: VirtualQueryEx only inspects the target; `mbi` is a valid out-pointer.
                let n = unsafe {
                    VirtualQueryEx(
                        self.handle.0,
                        Some(addr as *const _),
                        &mut mbi,
                        size_of::<MEMORY_BASIC_INFORMATION>(),
                    )
                };
                if n == 0 || mbi.RegionSize == 0 {
                    break;
                }
                let base = mbi.BaseAddress as usize;
                if mbi.State == MEM_COMMIT && readable(mbi.Protect) {
                    let p = mbi.Protect;
                    let writable = [PAGE_READWRITE, PAGE_WRITECOPY, PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY]
                        .iter()
                        .any(|f| (p & *f).0 != 0);
                    let executable = [PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY]
                        .iter()
                        .any(|f| (p & *f).0 != 0);
                    let kind = if mbi.Type == MEM_IMAGE {
                        crate::mem::RegionKind::Image
                    } else if mbi.Type == MEM_MAPPED {
                        crate::mem::RegionKind::Mapped
                    } else {
                        crate::mem::RegionKind::Private
                    };
                    out.push(MemRegion { base, size: mbi.RegionSize, kind, writable, executable });
                }
                addr = base + mbi.RegionSize;
            }
            out
        }
    }
}

#[cfg(windows)]
#[cfg(all(test, windows))]
pub use imp::ACCESS;
pub use imp::{list_processes, RemoteProcess};

#[cfg(not(windows))]
mod imp {
    use super::*;
    pub struct RemoteProcess {
        pub pid: u32,
    }
    impl RemoteProcess {
        pub fn open(_pid: u32) -> Result<Self> {
            bail!("acr-probe only supports Windows")
        }
        pub fn modules(&self) -> Result<Vec<ModuleEntry>> {
            bail!("acr-probe only supports Windows")
        }
    }
    impl ReadMem for RemoteProcess {
        fn read(&self, _addr: usize, _buf: &mut [u8]) -> bool {
            false
        }
        fn regions(&self) -> Vec<MemRegion> {
            Vec::new()
        }
    }
    pub fn list_processes() -> Result<Vec<ProcessEntry>> {
        bail!("acr-probe only supports Windows")
    }
}

#[cfg(not(windows))]
pub use imp::{list_processes, RemoteProcess};

/// Finds exactly one process named `name` (case-insensitive) unless `pid` is given.
pub fn find_target(name: &str, pid: Option<u32>) -> Result<u32> {
    if let Some(p) = pid {
        return Ok(p);
    }
    let want = name.to_ascii_lowercase();
    let hits: Vec<ProcessEntry> =
        list_processes()?.into_iter().filter(|p| p.name.to_ascii_lowercase() == want).collect();
    match hits.len() {
        0 => bail!("{name} is not running (start the game, or pass --pid / --process)"),
        1 => Ok(hits[0].pid),
        _ => bail!(
            "{} processes named {name}: {:?}; pass --pid",
            hits.len(),
            hits.iter().map(|p| p.pid).collect::<Vec<_>>()
        ),
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn access_mask_is_read_only() {
        use windows::Win32::System::Threading::{PROCESS_QUERY_INFORMATION, PROCESS_VM_READ};
        assert_eq!(ACCESS.0, PROCESS_VM_READ.0 | PROCESS_QUERY_INFORMATION.0);
        assert_eq!(ACCESS.0, 0x0410);
    }

    #[test]
    fn reads_own_process() {
        let me = RemoteProcess::open(std::process::id()).unwrap();
        let value: u64 = 0xA5A5_1234_5678_9ABC;
        let addr = &value as *const u64 as usize;
        assert_eq!(me.read_u64(addr), Some(value));
        assert_eq!(me.read_u64(0x10), None);
        let regions = me.regions();
        assert!(regions.iter().any(|r| r.contains(addr) && r.writable));
        assert!(regions.windows(2).all(|w| w[0].end() <= w[1].base));
        let mods = me.modules().unwrap();
        let exe = std::env::current_exe().unwrap();
        let exe_name = exe.file_name().unwrap().to_string_lossy().to_ascii_lowercase();
        assert!(mods.iter().any(|m| m.name.to_ascii_lowercase() == exe_name));
        assert!(mods.iter().any(|m| m.name.eq_ignore_ascii_case("kernel32.dll")));
        let main = mods.iter().find(|m| m.name.to_ascii_lowercase() == exe_name).unwrap();
        assert_eq!(symbolize(&mods, main.base + 0x10).unwrap(), format!("{}+0x10", main.name));
    }

    #[test]
    fn finds_self_by_name() {
        let exe = std::env::current_exe().unwrap();
        let name = exe.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(find_target(&name, None).unwrap(), std::process::id());
        assert_eq!(find_target("definitely-not-running-xyz.exe", Some(42)).unwrap(), 42);
        assert!(find_target("definitely-not-running-xyz.exe", None).is_err());
    }
}
