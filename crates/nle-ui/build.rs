use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/rust-cut-pro.ico");
    println!("cargo:rerun-if-changed=assets/rust-cut-pro.rc");
    println!("cargo:rerun-if-changed=assets/icon-128.rgba");

    whisper_vulkan();

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let rc_file = manifest_dir.join("assets").join("rust-cut-pro.rc");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let res_file = out_dir.join("rust-cut-pro.res");

    let Some(rc_exe) = find_rc_exe() else {
        println!("cargo:warning=rc.exe not found; skipping Windows exe icon resource embedding");
        return;
    };

    let status = Command::new(&rc_exe).args(["/nologo", "/fo"]).arg(&res_file).arg(&rc_file).status();

    match status {
        Ok(s) if s.success() && res_file.exists() => {
            println!("cargo:rustc-link-arg-bin=rust-cut-pro={}", res_file.display());
        }
        Ok(s) => {
            println!("cargo:warning=rc.exe exited with {s}; skipping Windows exe icon embedding");
        }
        Err(e) => {
            println!("cargo:warning=failed to run rc.exe ({e}); skipping Windows exe icon embedding");
        }
    }
}

/// GPU captions: whisper.cpp's Vulkan backend (`whisper-vulkan`, on by default).
fn whisper_vulkan() {
    println!("cargo:rerun-if-env-changed=VULKAN_SDK");
    if env::var_os("CARGO_FEATURE_WHISPER_VULKAN").is_none() {
        return;
    }
    match env::var_os("VULKAN_SDK") {
        Some(sdk) if Path::new(&sdk).is_dir() => {}
        Some(sdk) => println!(
            "cargo:warning=VULKAN_SDK points to {} which does not exist; Whisper captions need the Vulkan SDK (https://vulkan.lunarg.com) for GPU builds",
            Path::new(&sdk).display()
        ),
        None => println!(
            "cargo:warning=VULKAN_SDK is not set; install the Vulkan SDK (https://vulkan.lunarg.com) or build with --no-default-features --features whisper for CPU-only captions"
        ),
    }
    // Static MSVC links drop archive members nothing references directly;
    // keep every ggml-vulkan object so the backend registers at startup.
    // The library sits on whisper-rs-sys's link search path.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") && env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/WHOLEARCHIVE:ggml-vulkan.lib");
    }
}

fn find_rc_exe() -> Option<PathBuf> {
    if let Ok(path) = env::var("PATH") {
        for dir in env::split_paths(&path) {
            let candidate = dir.join("rc.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    let arch = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86") => "x86",
        Ok("aarch64") => "arm64",
        _ => "x64",
    };

    for root in [r"C:\Program Files (x86)\Windows Kits\10\bin", r"C:\Program Files\Windows Kits\10\bin"] {
        let root_path = Path::new(root);
        let Ok(entries) = std::fs::read_dir(root_path) else { continue };
        let mut versions: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_dir()).collect();
        versions.sort();
        versions.reverse();
        for ver in versions {
            let candidate = ver.join(arch).join("rc.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    None
}
