# Rust Cut Pro

A clean-room, Final Cut Pro-style non-linear video editor written in Rust.
It is tuned for an **AMD Ryzen 7 5800X** (8 cores / 16 threads) with an
**Intel Arc A750** (8 GB VRAM), on Linux (Vulkan + Quick Sync over VA-API
`iHD`) and Windows (DX12 / Vulkan + Quick Sync over D3D11VA).

The editor starts with a built-in demo project (procedural colour bars and
tone), so it works with no media files and without FFmpeg installed.

---

## Workspace

```
rust-cut-pro/
├── crates/nle-core     data model — no GPU, no FFmpeg, fully unit-tested
├── crates/nle-media    decoding, hardware acceleration, audio, thumbnails, proxies
├── crates/nle-render   wgpu/WGSL render engine and hardware export
└── crates/nle-ui       eframe/egui application (binary: rust-cut-pro)
```

There are no circular dependencies: `nle-core` ← `nle-media` ← `nle-render` ← `nle-ui`.

### nle-core

| Module | What it does |
|---|---|
| `timeline` | Magnetic Timeline: the primary storyline is an ordered `Vec<Clip>` with no stored positions, so every edit ripples automatically. Connected clips live inside their parent clip (offset + lane), and compound clips, gap clips and adjustment layers are supported. Operations: insert/append/overwrite/connect, ripple delete, replace with gap, blade/blade all, trim, slip, slide, move/lift, range delete and snapping. |
| `keyframe` | Keyframe curves with cubic Bézier eases, solved by Newton iteration with a bisection fallback. Interpolations: linear, ease in/out, hold and custom Bézier. |
| `speed` | Retiming built from segments, with smoothstep speed ramps integrated analytically. Covers 25/50/200/400 % presets, reverse, hold (freeze) frames, and frame blending / optical-flow modes. |
| `command` | Undoable `Command`s and the `History` (`Vec<AppliedCommand>` undo and redo stacks). Before/after snapshots make redo exact, and continuous drags coalesce into a single undo step. |
| `autosave` | Atomic 30-second autosave: write to a temp file, fsync, rename, and keep a `.prev` rotation. Crash recovery is offered at the next launch. |
| `fcpxml` | FCPXML 1.10 import and export (assets, spine, connected clips, gaps, titles, compound clips, transitions, retiming, keyframes). |
| `effects` | Modular, reorderable clip effect stack (16 GPU effects across Blur, Stylize, Distort, Keying and Color), plus Adjustment clips and Rate Conform quality modes. |
| `captions` | Word-timed subtitle/caption track, active-word highlighting (`CapCut` yellow/green/custom, pill, outline) and `.srt` import / export. |
| `keymap` | Customizable keyboard & mouse (`Middle` / `Back` / `Forward`) bindings with `Final Cut Pro` and `DaVinci Resolve` presets and TOML persistence. |
| `eval` | Resolves the timeline at a given instant into a render graph (layers, adjustment passes, effects, captions and transitions) plus audio spans. |

### nle-media

- **Hardware decode.** Planned per codec and bit depth, in this order: Quick Sync (`h264_qsv` / `hevc_qsv` / `av1_qsv`), then VA-API or D3D11VA, then software. On Linux the QSV child device is `vaapi=va:/dev/dri/renderD128,driver=iHD` → `qsv=hw@va`. On Windows it is `d3d11va=dx:,vendor_id=0x8086` → `qsv=hw@dx`. If any step fails, decoding falls back to software without interrupting the user.
- **Decoders.** Two backends:
  - the default decoder streams raw NV12/P010 from the `ffmpeg` executable;
  - the `ffmpeg` cargo feature enables in-process decoding through `ffmpeg-next`.
- **Threading and decode pipeline.**
  - Bounded ring-buffer frame queues.
  - A "latest wins" frame server with one decode worker per asset.
  - A **rayon pool capped at 12 workers** that always leaves **4 threads** free for the UI, audio and the GPU driver. Workers are pinned to the top CPUs.
- **Audio & Loudness / Beat Analysis.**
  - Kaiser-windowed sinc resampling to **48 kHz stereo f32**, offline mixdown & 16 kHz mono conversion.
  - A 3-band biquad EQ, **Vocal Clarity** chain (80 Hz HPF + 3 kHz presence + limiter), and EBU R128 / BS.1770 integrated loudness (`-14 LUFS` normalize).
  - Sidechain auto-ducking: the dialogue bus ducks music and effects.
  - Spectral-flux onset & tempo/beat detector (`60–200 BPM`) for timeline beat markers.
  - A lock-free `rtrb` ring feeds the `cpal` callback, and the audio clock is the master clock.
- **Whisper Auto Captions & Online Audio/SFX.**
  - Offline speech-to-text via `whisper-rs` (`transcribe`), accelerated on Vulkan GPU (Intel Arc) with CPU fallback and GGML model management (`base-q5_1`, `large-v3-turbo-q5_0`).
  - Royalty-free online search (Openverse & Internet Archive), streaming FFmpeg → `rtrb` → `cpal` URL preview (`AudioPreview`), streamed downloads with CC attribution tracking, and optional `yt-dlp` URL import.
- **Media analysis.** Parallel filmstrip thumbnails, RMS/peak waveforms, integrated LUFS loudness, and a background proxy transcoder (QSV first).

### nle-render

All pixel work runs in WGSL compute shaders on the GPU:

- NV12 / P010 → linear RGBA (BT.709, limited range).
- Exposure, white balance, lift/gamma/gain wheels, contrast, saturation and **trilinear `.cube` 3D LUTs**. A before/after split wipe is available for comparison.
- Modular **effect stack** compute shaders (Gaussian/Radial/Directional/Zoom/Motion Blur, Glow, Vignette, Film Grain, Sharpen, Chromatic Aberration, Halation, Pixelate, Lens Distortion, Chroma Key, Hue/Sat/Lightness, Levels), including **Adjustment clip** passes over layers below.
- Transform, crop, Ken Burns, opacity and blend modes (normal, multiply, screen, overlay, add).
- **Transitions:** Cross Dissolve, Dip to Black, Dip to White, Whip Pan (directional blur), Slide, Push and Luma Wipe.
- Slow-motion and retimed footage **Frame Blending** and **block-matching motion-compensated interpolation** (`Optical Flow`).
- Titles, lower thirds, animated text presets, active-word highlighted captions, outline and pill backgrounds.
- NV12 packing for the encoders, with **export** via `av1_qsv`, `hevc_qsv`, `h264_qsv` and `h264_vaapi` (or software `libx264`). When timeline-used online media requires credit (`CC-BY`, etc.), `project_credits.txt` is written automatically beside the exported video.
- Decoded frames stay in VRAM in an **LRU cache capped at 2,560 MB**.

### nle-ui

The UI uses eframe/egui on the wgpu backend. The renderer shares eframe's wgpu
device, so the viewer displays the rendered frame without any copies.

- **Theme:** dark, FCP-style. Background `#18181A`, panels `#222225`, borders `#2C2C30`. Clip colours: video `#0A84FF`, audio `#30D158`, titles & adjustment clips `#BF5AF2`. Snap indicator `#FF9F0A`, playhead `#FF453A`.
- **Browser (`Project Media` & `Online Audio & SFX` tabs):**
  - import local media by drag-and-drop or dialog, or search Openverse & Internet Archive CC0/CC-BY audio with streaming preview and one-click Download & Insert (plus optional `yt-dlp` URL import);
  - search, tags and favourites;
  - hover-scrub filmstrips, which also play in the viewer;
  - an Original/Proxy toggle;
  - proxy creation and a metadata panel (including LUFS loudness and licence/attribution credits).
- **Viewer:** GPU texture with fit/zoom, HH:MM:SS:FF timecode (click it to type a new position), transport controls, audio meters, title/action safe zones, a before/after wipe, and on-screen gizmos for position, scale, rotation and crop.
- **Inspector tabs:**
  - **Video:** compositing, transform, crop/Ken Burns, Rate Conform, speed, and reorderable GPU Effect stack with grouped `+ Add Effect` and keyframable parameters.
  - **Color:** colour wheels, primaries and LUT.
  - **Audio:** volume, pan, role, `-14 LUFS` normalize, Vocal Clarity, `Analyze Beats`, fades, the 3-band EQ with a response curve, and ducking.
  - **Titles & Transitions:** text animations, active-word highlight colour, and text outline controls.

  Every continuous parameter has a keyframe diamond.
- **Timeline:**
  - Primary storyline with connected lanes above (video, titles, purple `fx` adjustment clips) and below (audio), plus a dedicated `CC` Captions row when captions exist (click to seek, double-click to edit).
  - Ruler displays both named **Standard markers** and detected **Beat markers**.
  - Clips show filmstrips and RMS waveforms, with volume lines and fade ramps.
  - Transitions appear on edit points; retime and animation lanes render both clip and effect keyframes.
  - Skimming, snapping, `CC Captions` toggle, and right-click context menus (`Add Adjustment Clip`, `Insert Gap`, `Add Marker`, `Generate Captions…`, `Analyze Beats`).
- **Auto Captions & Whisper (`Ctrl+Shift+C`)** & **Keyboard/Mouse Shortcuts (`Ctrl+,` / `F1`):**
  - Download/delete Whisper GGML models, pick language or auto-detect, toggle Vulkan GPU acceleration, import/export `.srt`, edit segments (with automatic word retiming) and style captions live.
  - Customize all key and mouse (`Middle`, `Back`, `Forward`) bindings, switch between `Final Cut Pro` and `DaVinci Resolve` presets, view live conflict warnings, and autosave to `config/keybindings.toml`.

---

## System packages

### Linux (Ubuntu 24.04 / Debian 13)

```bash
# Rust
curl https://sh.rustup.rs -sSf | sh

# GPU + Quick Sync (Intel Arc): Mesa Vulkan driver, iHD VA-API driver, oneVPL runtime
sudo apt install mesa-vulkan-drivers vulkan-tools intel-media-va-driver-non-free \
                 libvpl2 libmfx-gen1.2 vainfo

# FFmpeg with QSV/VA-API (decode, proxies, export)
sudo apt install ffmpeg

# Build dependencies for eframe/winit, cpal and rfd
sudo apt install build-essential pkg-config libasound2-dev libudev-dev \
                 libxkbcommon-dev libwayland-dev libx11-dev libxcursor-dev \
                 libxrandr-dev libxi-dev libgtk-3-dev

# Optional: in-process decoding (cargo feature `ffmpeg`, built on ffmpeg-next 9.1,
# which supports the FFmpeg versions distros ship; tested with FFmpeg 9.0.2)
sudo apt install libavcodec-dev libavformat-dev libavutil-dev libswscale-dev \
                 libavfilter-dev libavdevice-dev clang libclang-dev
```

- **Fedora:** `intel-media-driver`, `libvpl`, `oneVPL-intel-gpu`, `ffmpeg` (RPM Fusion), `alsa-lib-devel`, `gtk3-devel`, `vulkan-loader`.
- **Arch:** `intel-media-driver`, `vpl-gpu-rt`, `vulkan-intel`, `ffmpeg`, `alsa-lib`, `gtk3`.

Check Quick Sync with `vainfo --display drm --device /dev/dri/renderD128`. It should list the `iHD` driver and the AV1/HEVC/H.264 profiles. Your user must be in the `render` and `video` groups.

### Windows 10/11

- Rust via <https://rustup.rs> (MSVC toolchain, with Visual Studio Build Tools and the "Desktop development with C++" workload).
- **Whisper + Vulkan build toolchain (default `whisper-vulkan` feature):** requires CMake (`cmake`), LLVM (`libclang` via `LIBCLANG_PATH`), and the **Vulkan SDK** (`VULKAN_SDK`). The repository includes [`.cargo/config.toml`](.cargo/config.toml) (setting default fallback paths with `force = false`) and [`Directory.Build.props`](Directory.Build.props) (redirecting MSBuild `.tlog` / marker files into `%TEMP%\rcp-tlog` so `whisper-rs-sys` Vulkan shader generation stays well within Windows' 260-character `MAX_PATH` limit). To build without Whisper, pass `--no-default-features` to `nle-ui`.
- An up-to-date **Intel Arc graphics driver** (it provides DX12, Vulkan and Quick Sync).
- **FFmpeg** with QSV support on `PATH` (for example the gyan.dev "full" build: `winget install Gyan.FFmpeg`). The editor finds FFmpeg even if Explorer still has an old `PATH` (for example, right after `winget install`). It looks next to the exe, then on `PATH`, then in the winget, Scoop and Chocolatey folders. You can also point `RCP_FFMPEG` / `RCP_FFPROBE` at the executables.
- Optional, for the `ffmpeg` feature: the FFmpeg 9 **shared** build, which includes headers and `.lib` files, plus LLVM for libclang. Steps:

  1. `winget install Gyan.FFmpeg.Shared LLVM.LLVM`
  2. Build and run from PowerShell:

     ```powershell
     $ff = (Get-ChildItem "$env:LOCALAPPDATA\Microsoft\WinGet\Packages\Gyan.FFmpeg.Shared*\ffmpeg-*-shared").FullName
     $env:FFMPEG_DIR    = $ff
     $env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"
     $env:PATH          = "$ff\bin;$env:PATH"   # avcodec-63.dll etc. must be found at run time
     cargo run --release -p nle-ui --features ffmpeg
     ```

  The plain `Gyan.FFmpeg` package (the "full" build) is just the executables, which is all the default build needs. It does not include headers or `.lib` files. To run the exe without that `PATH` line, copy the DLLs from `$ff\bin` next to `target\release\rust-cut-pro.exe`.

Without FFmpeg the editor still runs: the demo media and the GPU pipeline work, but imports of real files, proxies and export need the `ffmpeg` binary.

---

## Build & run

```bash
cargo run --release -p nle-ui            # the editor (binary: rust-cut-pro)
cargo check --workspace
cargo test --workspace                   # core, media, render (incl. GPU) tests
cargo run --release -p nle-ui --features ffmpeg   # in-process libav decoding
```

Environment variables:

| Variable | Effect |
|---|---|
| `RCP_WGPU_BACKEND=vulkan\|dx12\|gl` | Force a wgpu backend. The default is DX12 → Vulkan on Windows and Vulkan on Linux. |
| `RCP_FFMPEG`, `RCP_FFPROBE` | Paths to the FFmpeg tools. |
| `RUST_LOG=debug` | Verbose logging. |

Autosaves go to `%APPDATA%\RustCutPro\autosave` on Windows and `~/.local/share/RustCutPro/autosave` on Linux (`config\keybindings.toml`, `models\`, `downloads\`, and `proxies\` live beside it under the same app data directory).

---

## Keyboard shortcuts

Shortcuts are customizable in **Help → Keyboard Shortcuts** (`Ctrl+,` / `F1`), supporting **Final Cut Pro** and **DaVinci Resolve** presets plus extra mouse buttons (`Mouse:Middle`, `Mouse:Back`, `Mouse:Forward`). Default **Final Cut Pro** bindings:

| Key | Action |
|---|---|
| **Space** | Play / Pause |
| **J / K / L** | Play reverse / Stop / Play forward. Press J or L again for 2×, 4×, 8×. |
| **← / →** | Previous / next frame |
| **Shift+← / →** | Back / forward 10 frames |
| **↑ / ↓** | Previous / next edit point |
| **Home / End** | Go to start / end |
| **A** | Select tool: click to select, drag to move/reorder, drag edges to ripple trim |
| **T** | Trim tool: edges ripple trim, the middle slips, Alt+middle slides |
| **B** | Blade tool: click to cut. Shift+click blades every lane. |
| **R** | Range selection tool |
| **E / W / Q / D** | Append / Insert / Connect / Overwrite the browser selection |
| **C / Ctrl+B** | Split the selected / hovered clip at the playhead |
| **Q / W** | Ripple-trim the clip's start / end to the playhead |
| **J / K / L** | Shuttle reverse / pause / forward (1× → 2× → 4×); hold **K** and tap **J**/**L** to step one frame |
| **Ctrl+Shift+B** | Blade all at playhead |
| **Delete / Backspace** | Lift delete — leaves a gap (or deletes the selected range) |
| **Shift+Delete** | Ripple delete |
| **Alt+G** | New compound clip from the selection |
| **Shift+Alt+G** | Break apart clip items |
| **Alt+A** | Add Adjustment Clip |
| **V** | Enable / disable the selected clips |
| **Ctrl+T** | Add a Cross Dissolve to the selected edit points |
| **Ctrl+R** | Show / hide the retime editor. Click a segment for 25/50/100/200/400 %, ramps, splitting, reverse and hold. |
| **Shift+H** | Hold (freeze) frame at the playhead, 2 s |
| **Alt+K** | Add keyframe at playhead |
| **Ctrl+V** | Show / hide the video animation (keyframe) lane |
| **M** | Add marker at playhead |
| **Alt+B** | Analyze beats on the selected clip |
| **Ctrl+Shift+C** | Open Auto Captions & Whisper |
| **S** (or **N**) | Magnetic snapping on / off (`Snap: ON/OFF` badge; `♪ Peaks` adds audio-transient snap points) |
| **Shift+S** | Skimming on / off |
| **Shift+Z** | Zoom the timeline to fit |
| **Ctrl+= / Ctrl+-** | Zoom the timeline in / out (Ctrl+scroll also works) |
| **Ctrl+Z** | Undo |
| **Ctrl+Shift+Z** | Redo |
| **Ctrl+A** | Select all clips |
| **Esc** | Clear the selection and range |
| **Ctrl+I** | Import media (or drag files onto the window) |
| **Ctrl+Shift+I** | Import FCPXML (you can also drop a `.fcpxml` file) |
| **Ctrl+O / Ctrl+S / Ctrl+Shift+S** | Open / Save / Save As (`.rcp` project) |
| **Ctrl+E** | Export a movie (AV1/HEVC/H.264 Quick Sync, H.264 VA-API, x264) |
| **Ctrl+, / F1** | Customize keyboard & mouse shortcuts window |

Mouse:
- Scroll pans the timeline. Ctrl+scroll zooms around the cursor.
- Click the ruler to move the playhead; drag it to scrub.
- Double-click a browser clip to append it.
- Drag browser clips onto the storyline to insert them, or above/below it to connect them.
- Right-click a clip for the clip menu (transitions, retime, compound, lift…).
- Drag keyframe diamonds in the animation lane to retime them.
- Double-click a colour wheel to reset it.

> **Note on Ctrl+V:** egui's winit backend turns Ctrl+V into a *paste* event and drops the key. On Windows the editor reads the key state directly, so the shortcut always works. On Linux it relies on the paste event, so it only fires when the clipboard holds text. The same toggle is in the timeline toolbar (**Animation**) and in **View → Video Animation**.

---

## Tests

`cargo test --workspace` runs these suites:

- **`nle-core/tests/timeline_tests.rs`:** ripple insert (including in the middle of a clip), blade, gap behaviour, compound make/break, Bézier evaluation, speed-ramp mapping and its inverse, retime splits, undo/redo (exact restore and coalescing), autosave atomicity and recovery, and an FCPXML byte-stable round trip.
- **`nle-media`:** thread budget, decode planning, ffprobe parsing, the frame queue, resampling, EQ response, ducking and the mixer.
- **`nle-render`:**
  - every WGSL shader validated with naga;
  - LUT parsing and sampling, transforms, the LRU cache and export arguments;
  - an end-to-end GPU test (`tests/gpu.rs`), skipped automatically when no adapter is present. It covers the YUV round trip, dissolve, grading with the wipe, opacity, frame blending and optical flow, every transition, titles (glyphs lit and centred) and the display path.
- **Real-media tests** run automatically when `ffmpeg`/`ffprobe` are found, and are skipped otherwise:
  - `nle-media/tests/ffmpeg_pipeline.rs` generates a test clip, then checks probing, hardware/software decode (full and downscaled), 48 kHz audio decode, filmstrip thumbnails and the proxy transcode.
    It also encodes HEVC Main10 and AV1 sources with the Arc's Quick Sync encoders and checks that they decode in hardware (P010 / NV12). This runs only where those encoders work.
  - `nle-render/tests/export_ffmpeg.rs` exports a two-clip timeline with a dissolve through **every encoder FFmpeg reports** (`av1_qsv`, `hevc_qsv`, `h264_qsv`, `h264_vaapi`, `libx264`). Each output is verified with ffprobe for size, duration and audio track.
  - A hardware-encoder failure is excused only if FFmpeg also cannot use that encoder directly. Otherwise the test fails.

**Verified on** Ryzen 7 5800X + Arc A750 under Windows 11, with FFmpeg 9.0.2 (gyan.dev full build):
- every suite passes;
- Quick Sync decode works for H.264, HEVC Main10 and AV1;
- proxies use `h264_qsv`;
- exports work through `av1_qsv`, `hevc_qsv`, `h264_qsv` and `libx264`.

The `ffmpeg` feature (`ffmpeg-next` 9.1) was built and tested against the same FFmpeg 9.0.2, using the winget `Gyan.FFmpeg.Shared` package. The Windows section above has the commands.

---

## Implementation notes

- **Intel Arc on DX12:** creating an `Rgba8UnormSrgb` *view* of a storage-capable `Rgba8Unorm` texture loses the device. The display output is therefore copied into a separate sRGB texture for egui instead of being reinterpreted (see `Renderer::make_display`).
- **Optional libav decoder:** the `ffmpeg` feature (`nle-media/src/decode/libav.rs`) uses FFmpeg's generic hwaccel (D3D11VA / VA-API `iHD`) and transfers frames to NV12/P010. It needs FFmpeg dev libraries and libclang to build. The default CLI decoder covers the full QSV → VA-API/D3D11VA → software chain.
- **Quick Sync on Windows (FFmpeg 8/9):**
  - `hwupload=extra_hw_frames=N` fails on D3D11 with `E_INVALIDARG`, so Windows uses a plain `hwupload,format=qsv`.
  - Output colour flags (`-colorspace` etc.) make FFmpeg insert an auto-scale filter that cannot accept QSV frames. BT.709 tags are therefore set with `setparams` before the upload.
  - QSV decode uses `-hwaccel_output_format qsv` with an explicit `hwdownload`. Without it, HEVC 10-bit and AV1 fail format negotiation.
