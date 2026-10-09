//! Tick sources: ProcessEvent detour, a generic ABI-preserving detour thunk (world tick /
//! Kunos sim step), and the 16 ms timer-thread fallback.

use crate::gate::FrameGate;
use crate::runtime;
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, FILETIME};
use windows::Win32::System::Diagnostics::Debug::{GetThreadContext, CONTEXT, CONTEXT_CONTROL_AMD64};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows::Win32::System::Threading::{
    GetCurrentThreadId, GetThreadTimes, OpenThread, ResumeThread, SuspendThread, THREAD_GET_CONTEXT,
    THREAD_QUERY_LIMITED_INFORMATION, THREAD_SUSPEND_RESUME,
};

/// UE runs the game loop on the process's main (first) thread.
static GAME_TID: AtomicU32 = AtomicU32::new(0);
static GATE: Mutex<Option<FrameGate>> = Mutex::new(None);
static PE_TRAMPOLINE: AtomicUsize = AtomicUsize::new(0);
/// Original function behind [`tick_thunk`]; read by the asm via `sym`.
static TICK_ORIG: AtomicUsize = AtomicUsize::new(0);
/// Only the `sim_step` source may skip the original call.
static SKIP_ALLOWED: AtomicBool = AtomicBool::new(false);
/// Set by the `sim_step` freeze lever.
pub static SIM_STEP_FROZEN: AtomicBool = AtomicBool::new(false);

thread_local! {
    static IN_TICK: Cell<bool> = const { Cell::new(false) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TickSource {
    ProcessEvent,
    SimStep,
    Pattern,
    Vtable,
    Timer,
}

pub fn init_gate(use_present: bool) {
    if let Ok(mut g) = GATE.lock() {
        *g = Some(FrameGate::new(use_present));
    }
}

fn current_tid() -> u32 {
    // SAFETY: no preconditions.
    unsafe { GetCurrentThreadId() }
}

fn frame_due() -> bool {
    let Ok(mut g) = GATE.try_lock() else {
        return false;
    };
    g.as_mut().is_some_and(|g| g.should_tick(crate::overlay::present_count(), Instant::now()))
}

fn run_tick_guarded() {
    if IN_TICK.with(Cell::get) {
        return;
    }
    if !frame_due() {
        return;
    }
    IN_TICK.with(|c| c.set(true));
    runtime::tick(current_tid() == GAME_TID.load(Ordering::Relaxed));
    IN_TICK.with(|c| c.set(false));
}

// ---- Threads ----------------------------------------------------------------------------

fn threads_of_process() -> Vec<u32> {
    let pid = std::process::id();
    let mut out = Vec::new();
    // SAFETY: Toolhelp snapshot APIs with a correctly sized THREADENTRY32.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else {
            return out;
        };
        let mut te = THREADENTRY32 { dwSize: std::mem::size_of::<THREADENTRY32>() as u32, ..Default::default() };
        if Thread32First(snap, &mut te).is_ok() {
            loop {
                if te.th32OwnerProcessID == pid {
                    out.push(te.th32ThreadID);
                }
                if Thread32Next(snap, &mut te).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// The thread with the earliest creation time: the main thread, which UE uses as its game
/// thread.
pub fn detect_game_thread() -> Option<u32> {
    let mut best: Option<(u64, u32)> = None;
    for tid in threads_of_process() {
        // SAFETY: handle opened with query rights and closed below; out-params are valid.
        unsafe {
            let Ok(h) = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, false, tid) else {
                continue;
            };
            let (mut c, mut e, mut k, mut u) =
                (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
            if GetThreadTimes(h, &mut c, &mut e, &mut k, &mut u).is_ok() {
                let t = (u64::from(c.dwHighDateTime) << 32) | u64::from(c.dwLowDateTime);
                if best.is_none_or(|(bt, _)| t < bt) {
                    best = Some((t, tid));
                }
            }
            let _ = CloseHandle(h);
        }
    }
    let tid = best.map(|(_, t)| t)?;
    GAME_TID.store(tid, Ordering::Relaxed);
    Some(tid)
}

#[repr(C, align(16))]
struct AlignedContext(CONTEXT);

/// Suspends every other thread, verifies none is executing inside `[target, target+len)`,
/// runs `f`, and resumes them. Retries a few times if a thread is in the patch window.
fn with_threads_suspended<R>(target: usize, len: usize, f: impl FnOnce() -> R) -> Result<R, String> {
    let me = current_tid();
    let mut f = Some(f);
    for _attempt in 0..20 {
        let mut suspended = Vec::new();
        let mut busy = false;
        for tid in threads_of_process().into_iter().filter(|&t| t != me) {
            // SAFETY: handles are opened with suspend/context rights, resumed and closed below.
            unsafe {
                let Ok(h) = OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT, false, tid) else {
                    continue;
                };
                if SuspendThread(h) == u32::MAX {
                    let _ = CloseHandle(h);
                    continue;
                }
                let mut ctx = AlignedContext(CONTEXT { ContextFlags: CONTEXT_CONTROL_AMD64, ..Default::default() });
                if GetThreadContext(h, &mut ctx.0).is_ok() {
                    let ip = ctx.0.Rip as usize;
                    if ip >= target && ip < target + len {
                        busy = true;
                    }
                }
                suspended.push(h);
            }
        }
        let result = if busy { None } else { f.take().map(|f| f()) };
        for h in suspended {
            // SAFETY: each handle was suspended above.
            unsafe {
                ResumeThread(h);
                let _ = CloseHandle(h);
            }
        }
        if let Some(r) = result {
            return Ok(r);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Err("a thread kept executing the hook target".into())
}

/// Hooks `target`. `publish` receives the trampoline before the hook goes live, so the
/// detour never sees an unset original.
fn install_detour(target: usize, detour: usize, publish: impl FnOnce(usize)) -> Result<usize, String> {
    // SAFETY: `target` was resolved and sanity-checked as a function in the game module;
    // the detour has a compatible ABI. Enabling happens with other threads suspended.
    unsafe {
        let d = retour::RawDetour::new(target as *const (), detour as *const ())
            .map_err(|e| format!("detour create at {target:#x}: {e}"))?;
        let tramp = d.trampoline() as *const () as usize;
        publish(tramp);
        with_threads_suspended(target, 16, || d.enable())?.map_err(|e| format!("detour enable at {target:#x}: {e}"))?;
        // Never unhooked: the DLL stays loaded for the life of the process.
        std::mem::forget(d);
        Ok(tramp)
    }
}

// ---- ProcessEvent -----------------------------------------------------------------------

type ProcessEventFn = unsafe extern "C" fn(usize, usize, *mut u8);

unsafe extern "C" fn process_event_detour(obj: usize, func: usize, params: *mut u8) {
    if current_tid() == GAME_TID.load(Ordering::Relaxed) {
        run_tick_guarded();
    }
    let tramp = PE_TRAMPOLINE.load(Ordering::Acquire);
    // SAFETY: the trampoline executes the original ProcessEvent prologue with the same ABI.
    unsafe {
        let f: ProcessEventFn = std::mem::transmute::<usize, ProcessEventFn>(tramp);
        f(obj, func, params)
    }
}

pub fn install_process_event(pe: usize) -> Result<(), String> {
    // Our own UFunction calls go through the trampoline so they bypass the detour.
    install_detour(pe, process_event_detour as *const () as usize, |tramp| {
        PE_TRAMPOLINE.store(tramp, Ordering::Release);
        acr_ue::ue_call::set_process_event_target(tramp);
    })
    .map(|_| ())
}

/// Lets UFunction calls use ProcessEvent without hooking it (non-ProcessEvent tick sources).
pub fn use_process_event_unhooked(pe: usize) {
    acr_ue::ue_call::set_process_event_target(pe);
}

// ---- Generic tick thunk -----------------------------------------------------------------

extern "C" fn tick_thunk_pre() -> u8 {
    run_tick_guarded();
    u8::from(SKIP_ALLOWED.load(Ordering::Relaxed) && SIM_STEP_FROZEN.load(Ordering::Relaxed))
}

/// Saves the four integer and four vector argument registers, runs [`tick_thunk_pre`], then
/// either tail-jumps to the original (arguments and stack untouched) or returns 0 when the
/// sim step is frozen. Skipping assumes the hooked step's return value is unused.
#[unsafe(naked)]
unsafe extern "C" fn tick_thunk() {
    core::arch::naked_asm!(
        "push rcx",
        "push rdx",
        "push r8",
        "push r9",
        "sub rsp, 0x68",
        "movdqu xmmword ptr [rsp + 0x20], xmm0",
        "movdqu xmmword ptr [rsp + 0x30], xmm1",
        "movdqu xmmword ptr [rsp + 0x40], xmm2",
        "movdqu xmmword ptr [rsp + 0x50], xmm3",
        "call {pre}",
        "movdqu xmm0, xmmword ptr [rsp + 0x20]",
        "movdqu xmm1, xmmword ptr [rsp + 0x30]",
        "movdqu xmm2, xmmword ptr [rsp + 0x40]",
        "movdqu xmm3, xmmword ptr [rsp + 0x50]",
        "add rsp, 0x68",
        "pop r9",
        "pop r8",
        "pop rdx",
        "pop rcx",
        "test al, al",
        "jnz 2f",
        "jmp qword ptr [rip + {orig}]",
        "2:",
        "xor eax, eax",
        "ret",
        pre = sym tick_thunk_pre,
        orig = sym TICK_ORIG,
    );
}

/// Detours `target` with the tick thunk. `allow_skip` enables the sim-step freeze.
pub fn install_tick_thunk(target: usize, allow_skip: bool) -> Result<(), String> {
    SKIP_ALLOWED.store(allow_skip, Ordering::SeqCst);
    let tramp = install_detour(target, tick_thunk as *const () as usize, |t| TICK_ORIG.store(t, Ordering::SeqCst))?;
    tracing::info!("tick thunk installed at {target:#x} (trampoline {tramp:#x}, skip allowed: {allow_skip})");
    Ok(())
}

// ---- Timer fallback ---------------------------------------------------------------------

pub fn spawn_timer() {
    std::thread::Builder::new()
        .name("acr-rewind-timer".into())
        .spawn(|| loop {
            std::thread::sleep(Duration::from_millis(16));
            if !IN_TICK.with(Cell::get) {
                runtime::tick(false);
            }
        })
        .ok();
}

/// `sim_step` freeze lever: makes the thunk skip the Kunos step.
pub struct SimStepFreezer;

impl acr_ue::freeze::Freezer for SimStepFreezer {
    fn name(&self) -> &'static str {
        "sim_step"
    }
    fn set(&mut self, _pawn: Option<usize>, frozen: bool) -> Result<(), acr_ue::BackendError> {
        SIM_STEP_FROZEN.store(frozen, Ordering::SeqCst);
        Ok(())
    }
}
