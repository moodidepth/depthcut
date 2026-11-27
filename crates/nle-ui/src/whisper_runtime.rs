//! Starting without the Vulkan runtime, and a command-line Whisper self-test.
//!
//! whisper.cpp's Vulkan backend imports three functions from `vulkan-1.dll`
//! directly (the rest go through vulkan.hpp's dynamic dispatcher), which made
//! the DLL a load-time dependency: on a PC without the Vulkan loader (VMs,
//! Windows Server, the basic display driver) the exe would not start at all.
//! `build.rs` links it with `/DELAYLOAD:vulkan-1.dll`, and the hooks below
//! catch the failed load: `vkGetInstanceProcAddr` then resolves to a stub
//! that reports Vulkan 1.0, so ggml's own instance init throws "Vulkan 1.2
//! required" inside its `try` and Whisper carries on on the CPU.
//!
//! `RCP_SIMULATE_NO_VULKAN=1` takes the same path on a PC that has Vulkan.
//! `depthcut.exe --whisper-selftest [model.bin]` transcribes two seconds of
//! tone and prints the backend, to check all of this without the UI.

#![cfg_attr(not(all(windows, target_env = "msvc", feature = "whisper-vulkan")), allow(dead_code))]

#[cfg(all(windows, target_env = "msvc", feature = "whisper-vulkan"))]
mod hooks {
    use std::ffi::{c_char, c_void, CStr};
    use std::ptr::null;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Set once the real loader failed to load (or its absence is simulated).
    pub static LOADER_MISSING: AtomicBool = AtomicBool::new(false);

    // <delayimp.h>
    const DLI_NOTE_PRE_LOAD_LIBRARY: u32 = 1;
    const DLI_FAIL_LOAD_LIB: u32 = 3;
    const DLI_FAIL_GET_PROC: u32 = 4;

    #[repr(C)]
    pub struct DelayLoadProc {
        import_by_name: i32,
        /// `LPCSTR` name, or the ordinal when `import_by_name` is 0.
        name: *const c_char,
    }

    #[repr(C)]
    pub struct DelayLoadInfo {
        cb: u32,
        pidd: *const c_void,
        ppfn: *mut *const c_void,
        dll: *const c_char,
        dlp: DelayLoadProc,
        hmod_cur: *mut c_void,
        pfn_cur: *const c_void,
        last_error: u32,
    }

    pub type PfnDliHook = unsafe extern "system" fn(u32, *mut DelayLoadInfo) -> *const c_void;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetModuleHandleW(name: *const u16) -> *mut c_void;
    }

    unsafe fn is_vulkan(info: &DelayLoadInfo) -> bool {
        !info.dll.is_null() && unsafe { CStr::from_ptr(info.dll) }.to_bytes().eq_ignore_ascii_case(b"vulkan-1.dll")
    }

    /// Stand-in module for the missing DLL: the exe itself (it exports no
    /// Vulkan symbols, so every lookup lands in [`failure_hook`]).
    fn stand_in() -> *const c_void {
        LOADER_MISSING.store(true, Ordering::Relaxed);
        // SAFETY: a null name returns the exe's own module handle.
        unsafe { GetModuleHandleW(null()) as *const c_void }
    }

    unsafe extern "system" fn enumerate_instance_version(version: *mut u32) -> i32 {
        if !version.is_null() {
            // SAFETY: Vulkan passes a valid out-pointer.
            unsafe { *version = 1 << 22 }; // VK_API_VERSION_1_0
        }
        0 // VK_SUCCESS
    }

    unsafe extern "system" fn get_instance_proc_addr(_instance: *mut c_void, name: *const c_char) -> *const c_void {
        if !name.is_null() && unsafe { CStr::from_ptr(name) }.to_bytes() == b"vkEnumerateInstanceVersion" {
            enumerate_instance_version as *const c_void
        } else {
            null()
        }
    }

    /// `vkGetPhysicalDeviceFeatures2` / `vkCmdCopyBuffer`: unreachable without
    /// an instance; return nothing.
    unsafe extern "system" fn no_op() {}

    unsafe extern "system" fn failure_hook(notify: u32, info: *mut DelayLoadInfo) -> *const c_void {
        // SAFETY: the delay-load helper passes a valid DelayLoadInfo.
        let Some(info) = (unsafe { info.as_ref() }) else { return null() };
        if !unsafe { is_vulkan(info) } {
            return null();
        }
        match notify {
            DLI_FAIL_LOAD_LIB => stand_in(),
            DLI_FAIL_GET_PROC if info.dlp.import_by_name != 0 && !info.dlp.name.is_null() => {
                if unsafe { CStr::from_ptr(info.dlp.name) }.to_bytes() == b"vkGetInstanceProcAddr" {
                    get_instance_proc_addr as *const c_void
                } else {
                    no_op as *const c_void
                }
            }
            _ => null(),
        }
    }

    unsafe extern "system" fn notify_hook(notify: u32, info: *mut DelayLoadInfo) -> *const c_void {
        let Some(info) = (unsafe { info.as_ref() }) else { return null() };
        if notify == DLI_NOTE_PRE_LOAD_LIBRARY && unsafe { is_vulkan(info) } && std::env::var_os("RCP_SIMULATE_NO_VULKAN").is_some_and(|v| v == "1") {
            return stand_in();
        }
        null()
    }

    // The delay-load helper (delayimp.lib) reads these two; defining them here
    // replaces its null defaults.
    #[no_mangle]
    #[used]
    pub static __pfnDliFailureHook2: PfnDliHook = failure_hook;
    #[no_mangle]
    #[used]
    pub static __pfnDliNotifyHook2: PfnDliHook = notify_hook;
}

/// Whether `vulkan-1.dll` was missing (or simulated missing) when Whisper
/// first touched it.
pub fn vulkan_loader_missing() -> bool {
    #[cfg(all(windows, target_env = "msvc", feature = "whisper-vulkan"))]
    {
        hooks::LOADER_MISSING.load(std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(not(all(windows, target_env = "msvc", feature = "whisper-vulkan")))]
    {
        false
    }
}

/// `--whisper-selftest [model]`: transcribe 2 s of tone, print the backend
/// and exit. `None` when the flag isn't on the command line.
pub fn selftest_from_args() -> Option<i32> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("--whisper-selftest") {
        return None;
    }
    let model = args.next().filter(|a| !a.is_empty()).map(std::path::PathBuf::from).unwrap_or_else(|| nle_media::paths::models_dir().join("ggml-base-q5_1.bin"));
    let pcm: Vec<f32> = (0..32_000).map(|i| 0.1 * (std::f32::consts::TAU * 220.0 * i as f32 / 16_000.0).sin()).collect();
    let opts = nle_media::transcribe::TranscribeOptions::new(model.clone());
    let res =
        nle_media::transcribe::transcribe(&pcm, &opts, std::sync::Arc::new(|_| {}), std::sync::Arc::new(|_| {}), std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)));
    let (line, code) = match res {
        Ok(t) => (
            format!(
                "whisper selftest: OK | backend: {} | fallback: {} | vulkan-1.dll missing: {} | {} word(s) | model {}",
                t.backend,
                t.fallback_reason.as_deref().unwrap_or("none"),
                vulkan_loader_missing(),
                t.words.len(),
                model.display()
            ),
            0,
        ),
        Err(e) => (format!("whisper selftest: FAILED: {e} | vulkan-1.dll missing: {}", vulkan_loader_missing()), 1),
    };
    println!("{line}");
    Some(code)
}
