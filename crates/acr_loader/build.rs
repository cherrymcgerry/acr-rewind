//! Emits linker forwarder exports so `dwmapi.dll` (this proxy) re-exports every function of
//! the real system `dwmapi.dll`. Each forwarder names the target by its absolute path
//! (`C:\Windows\System32\dwmapi`), which the loader resolves to the genuine system DLL rather
//! than back to this proxy, so there is no recursion even though both files are `dwmapi.dll`.
//!
//! Export set captured with `dumpbin /exports` of the system DLL (Windows 11 26200).
//!
//! Only the documented public `DwmXxx` functions are forwarded: these are the names a game
//! can import from `dwmapi.dll` by name, and MSVC's `/EXPORT:name=module.func` forwarder is
//! only accepted for symbols it recognises as external imports. The internal `Dwmp*`/`Dll*`
//! entries and the ordinal-only (NONAME) exports are engine/shell internals that applications
//! never import, so leaving them out does not affect the game. If a title is ever found to
//! import one of those, add a runtime-forwarding stub for it here.

fn main() {
    if std::env::var("CARGO_CFG_WINDOWS").is_err() {
        return;
    }

    // Base of the system DLL; used verbatim as the forwarder module name. The absolute path
    // resolves to the real system DLL (not this proxy), so there is no self-recursion.
    const SYS: &str = r"C:\Windows\System32\dwmapi";

    // (name, ordinal) for the documented public exports of the system DLL.
    const NAMED: &[(&str, u32)] = &[
        ("DwmEnableComposition", 102),
        ("DwmAttachMilContent", 116),
        ("DwmDefWindowProc", 117),
        ("DwmDetachMilContent", 118),
        ("DwmEnableBlurBehindWindow", 119),
        ("DwmEnableMMCSS", 120),
        ("DwmExtendFrameIntoClientArea", 121),
        ("DwmFlush", 122),
        ("DwmGetColorizationColor", 123),
        ("DwmGetCompositionTimingInfo", 125),
        ("DwmGetGraphicsStreamClient", 126),
        ("DwmGetGraphicsStreamTransformHint", 129),
        ("DwmGetTransportAttributes", 130),
        ("DwmGetUnmetTabRequirements", 133),
        ("DwmGetWindowAttribute", 134),
        ("DwmInvalidateIconicBitmaps", 146),
        ("DwmIsCompositionEnabled", 149),
        ("DwmModifyPreviousDxFrameDuration", 199),
        ("DwmQueryThumbnailSourceSize", 200),
        ("DwmRegisterThumbnail", 201),
        ("DwmRenderGesture", 202),
        ("DwmSetDxFrameDuration", 203),
        ("DwmSetIconicLivePreviewBitmap", 204),
        ("DwmSetIconicThumbnail", 205),
        ("DwmSetPresentParameters", 206),
        ("DwmSetWindowAttribute", 207),
        ("DwmShowContact", 208),
        ("DwmTetherContact", 209),
        ("DwmTransitionOwnedWindow", 210),
        ("DwmUnregisterThumbnail", 211),
        ("DwmUpdateThumbnailProperties", 212),
    ];

    for (name, ord) in NAMED {
        println!("cargo:rustc-cdylib-link-arg=/EXPORT:{name}={SYS}.{name},@{ord}");
    }
}
