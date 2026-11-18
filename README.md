# Rust Cut Pro

A fast, GPU-accelerated video editor for gaming montages and short-form edits,
written in Rust. It has a magnetic timeline in the style of Final Cut Pro, plus
montage tools built for CS2 and Valorant clips: zoom punches, beat sync, kill
scanning, 9:16 reframing, Whisper auto-captions and an optional AI Copilot.

> **Status: v0.1.0, early release.** Windows 10/11 x64 is the supported
> platform. Linux builds from source but is experimental. macOS is not supported.

---

## Install (Windows)

1. Download `RustCutPro-v0.1.0-win64-setup.exe` (installer) **or**
   `RustCutPro-v0.1.0-win64-portable.zip` (unzip anywhere and run `rust-cut-pro.exe`).
   You can check the download against `SHA256SUMS.txt`:
   `Get-FileHash .\RustCutPro-v0.1.0-win64-portable.zip`
2. **SmartScreen warning.** Builds are **not code-signed yet**, so Windows may say
   *"Windows protected your PC"*. Click **More info → Run anyway**. If you'd rather
   not, build from source (see [Build & run](#build--run)).
3. **First launch: FFmpeg.** Rust Cut Pro uses FFmpeg to import, proxy and export
   video, but does not ship it. If FFmpeg isn't found, a banner at the top offers:
   - **Download FFmpeg Automatically**: fetches the gyan.dev "release essentials"
     build (about 100 MB), checks its SHA-256 and puts it in
     `%LOCALAPPDATA%\RustCutPro\bin`. That build is GPL-licensed; its license is
     saved next to it.
   - **Locate Existing ffmpeg.exe…**: use one you already have.
   - Or install it yourself (`winget install Gyan.FFmpeg`) and restart.

   Without FFmpeg the editor still opens and the demo project plays, but you
   can't import real media or export.

The installer adds a Start-menu shortcut, an optional desktop icon and an
optional `.rcp` file association (double-click a project to open it), plus a
normal uninstaller under *Settings → Apps*.

### System requirements

| | Minimum | Tested on |
|---|---|---|
| OS | Windows 10 1809+ or Windows 11, 64-bit | Windows 11 Pro |
| GPU | DirectX 12 GPU **with a Vulkan 1.2 driver** (the exe needs `vulkan-1.dll`, which every current AMD / NVIDIA / Intel driver installs) | Intel Arc A750 8 GB |
| CPU | 4 cores, x86-64 | Ryzen 7 5800X |
| RAM | 8 GB (16 GB for 1440p/4K timelines or the large Whisper model) | 32 GB |
| Disk | 200 MB for the app, plus about 100 MB for FFmpeg, 60 MB to 550 MB per Whisper model, and space for proxies | |

Hardware video encode/decode (Quick Sync) is used on Intel GPUs. Other GPUs fall
back to software encoding (`libx264`), which works but is slower.

**If the app won't start:** update your graphics driver first (a missing
`vulkan-1.dll` means no Vulkan driver is installed). The VC++ runtime DLLs are
already included next to the exe.

### Where your data lives

| What | Where |
|---|---|
| Autosaves, logs, keybindings, proxies, Discord settings | `%APPDATA%\RustCutPro\` |
| Whisper models, downloaded audio, downloaded FFmpeg (`bin\`) | `%LOCALAPPDATA%\RustCutPro\` |
| AI Copilot settings | `%LOCALAPPDATA%\Argon\ai_config.toml` (no keys in it) |
| AI API keys | **Windows Credential Manager** → *Windows Credentials* → entries starting `RustCutPro` |

Uninstalling leaves these in place so a reinstall keeps your settings. To remove
everything, delete those two `RustCutPro` folders and the `RustCutPro` entries in
Credential Manager.

**Logs.** *Help → Show Log File* opens the log folder. It holds this session's
log and the previous session's. If the app crashed, the next launch notices
and tells you where the log is. Please attach it to bug reports.

---

## Features

- **Magnetic timeline.** Ripple editing, connected clips, compound clips, gaps,
  adjustment clips, blade/trim/slip/slide, markers, snapping, skimming, and
  unlimited undo with 30-second crash-safe autosave. Also:
  - **Secondary storylines** (`Ctrl+G`): connected clips grouped into a
    storyline that ripples on its own and can hold transitions.
  - **Open in Timeline**: double-click a compound clip or storyline to edit
    inside it. A breadcrumb (or `Ctrl+[`) steps back out, and every inner edit
    is one normal undo step that resizes the container.
  - **Position tool** (`P`): moves clips without rippling and leaves gaps.
  - Hold **`` ` ``** (backtick) while moving, trimming or deleting a storyline
    clip to leave its connected clips where they are.
  - **`Ctrl+Alt`+click** a connected clip to move its connection point.
  - **Auditions**: several takes in one slot, cycled with `Ctrl+←/→` and
    finalized with `Shift+Alt+Y`.
  - **Audio lanes by role** (`Ctrl+Shift+L`): audio clips below the storyline
    are grouped into collapsible Dialogue / Music / Effects bands.
- **Montage tools.**
  - **Zoom Punch** (`Z`) and **Hard Zoom Punch** (`Shift+Z`) at the playhead.
  - **16 built-in CS2 / Valorant presets**: RSMB-style motion blur, AWP/Operator
    scope pulse, kill-impact shake, bullet-time ramps, flow sync, freeze-frame
    outline and colour looks.
  - Beat detection with **Sync to Beats** (`Alt+S`), **Scan VOD for kills /
    action peaks** (`Shift+Alt+K`) and **Gaming 9:16 split-crop** (`Alt+9`).
- **Keyframes everywhere.** Every continuous parameter has a keyframe diamond in
  the inspector. Bézier easing; an animation lane (`Ctrl+V`) and a retime editor
  (`Ctrl+R`) with speed ramps, reverse, hold frames, frame blending and optical
  flow.
- **GPU effects and colour.** 20 effects (five blurs, glow, vignette, sharpen,
  grain, lens distortion, chroma key, hue/sat/lightness, levels…), 14 blend
  modes, colour wheels, `.cube` LUTs, 7 transitions, titles. Everything is
  rendered in WGSL compute shaders.
- **Audio.** EQ, Vocal Clarity, −14 LUFS normalize, and **look-ahead
  auto-ducking**: music ducks *before* the voice starts. Preview and export
  sound identical.
- **Auto Captions** (`Ctrl+Shift+C`). Offline Whisper speech-to-text with Vulkan
  GPU acceleration, word-by-word highlight styles, and `.srt` import/export.
- **AI Editor Copilot** (`Ctrl+Shift+A`). Optional. Ask for edit suggestions
  using **Google Gemini**, **OpenAI** or a local **Ollama** model. Your key is
  stored in Windows Credential Manager, not in a text file. Settings has
  **Test Live Connection** and lists the Gemini models your key can use, with
  an automatic fallback model when the main one is overloaded.
- **Online audio & SFX.** Search Openverse and Internet Archive CC0/CC-BY audio
  in the app, preview it, and insert it. Credits are written next to your export.
- **Export.** AV1 / HEVC / H.264 via Quick Sync, or software x264.
- **Discord Rich Presence** (optional, *Help → Discord Rich Presence…*). Shows
  what you're editing; it does nothing if Discord isn't running. See
  [docs/DISCORD.md](docs/DISCORD.md).
- **Licenses.** *Help → Licenses & Notices…* shows every open-source license
  used in the app.

---

## Keyboard shortcuts

Every binding can be changed in *Help → Keyboard Shortcuts* (`Ctrl+,` / `F1`),
including extra mouse buttons (`Middle`, `Back`, `Forward`). There are three
presets: **Argon Montage** (the default, below), **Final Cut Pro** and
**DaVinci Resolve**. Changes are saved to `%APPDATA%\RustCutPro\config\keybindings.toml`.

| Keys | Action | Group |
|---|---|---|
| **Space** | Play / Pause | Playback |
| **J** | Shuttle Reverse 1x/2x/4x (J) | Playback |
| **K** | Pause / Hold for Frame Step (K) | Playback |
| **L** | Shuttle Forward 1x/2x/4x (L) | Playback |
| **←** | Previous Frame | Playback |
| **→** | Next Frame | Playback |
| **Shift+←** | Back 10 Frames | Playback |
| **Shift+→** | Forward 10 Frames | Playback |
| **↑** | Previous Edit Point | Playback |
| **↓** | Next Edit Point | Playback |
| **Home** | Go to Start | Playback |
| **End** | Go to End | Playback |
| **A** | Select Tool | Tools |
| **T** | Trim Tool | Tools |
| **Ctrl+Shift+B** | Blade Tool | Tools |
| **R** | Range Tool | Tools |
| **P** | Position Tool (no ripple) | Tools |
| **C / Ctrl+B / B** | Split Clip at Playhead | Edit |
| **Shift+B** | Blade All at Playhead | Edit |
| **Q** | Ripple Trim Start to Playhead (Q) | Edit |
| **W** | Ripple Trim End to Playhead (W) | Edit |
| **Shift+Delete / Shift+Backspace** | Ripple Delete | Edit |
| **Delete / Backspace** | Lift Delete (Leave Gap) | Edit |
| **Alt+G** | Insert Gap at Playhead | Edit |
| **Ctrl+Z** | Undo | Edit |
| **Ctrl+Shift+Z** | Redo | Edit |
| **Ctrl+A** | Select All | Edit |
| **Esc** | Clear Selection | Edit |
| **V** | Enable / Disable Clip | Edit |
| **Ctrl+T** | Add Cross Dissolve | Edit |
| **Alt+A** | Add Adjustment Clip | Edit |
| **Alt+C** | New Compound Clip | Edit |
| **Shift+Alt+C** | Break Apart Clip Items | Edit |
| **Ctrl+G** | Create Storyline from selected connected clips | Edit |
| **Ctrl+[** | Back Out of Compound / Storyline | Edit |
| **Ctrl+Shift+Y** | Duplicate as Audition | Edit |
| **Ctrl+→** | Next Audition Pick | Edit |
| **Ctrl+←** | Previous Audition Pick | Edit |
| **Shift+Alt+Y** | Finalize Audition | Edit |
| **Shift+H** | Hold Frame | Edit |
| **E** | Append Browser Clip | Edit |
| **Alt+W** | Insert Browser Clip | Edit |
| **Alt+Q** | Connect Browser Clip | Edit |
| **D** | Overwrite with Browser Clip | Edit |
| **Alt+K** | Add Keyframe | Animation |
| **Ctrl+V** | Show Video Animation | Animation |
| **Ctrl+R** | Show Retime Editor | Animation |
| **M** | Add Marker | Markers |
| **Alt+M** | Add Named / Colored Marker | Markers |
| **Alt+B** | Analyze Beats | Markers |
| **Shift+Alt+K** | Scan VOD for Kills / Action Peaks | Montage |
| **Alt+S** | Sync Clips & Velocity to Beats | Montage |
| **Alt+9** | Gaming 9:16 Split-Crop | Montage |
| **Z** | Zoom Punch at Playhead | Montage |
| **Shift+Z** | Hard Zoom Punch (hold) | Montage |
| **Ctrl+Shift+A** | AI Editor Copilot | AI |
| **S / N** | Snapping On/Off | View |
| **Shift+S** | Skimming On/Off | View |
| **Ctrl+= / Ctrl++** | Zoom In | View |
| **Ctrl+-** | Zoom Out | View |
| **Ctrl+0** | Zoom to Fit | View |
| **Ctrl+Shift+L** | Show Audio Lanes by Role | View |
| **Ctrl+Shift+C** | Auto Captions | Captions |
| **Ctrl+I** | Import Media | File |
| **Ctrl+Shift+I** | Import FCPXML | File |
| **Ctrl+O** | Open Project | File |
| **Ctrl+S** | Save | File |
| **Ctrl+Shift+S** | Save As | File |
| **Ctrl+E** | Export Movie | File |
| **Ctrl+, / F1** | Keyboard Shortcuts | Help |

Mouse:
- Scroll pans the timeline. Ctrl+scroll zooms around the cursor.
- Click the ruler to move the playhead; drag it to scrub.
- Double-click a browser clip to append it.
- Drag browser clips onto the storyline to insert them, or above/below it to connect them.
- Right-click a clip for the clip menu (transitions, retime, compound, storyline, audition, lift…).
- Double-click a compound clip or storyline to open it in the timeline. Use the breadcrumb above the timeline to step back out.
- Hold `` ` `` (backtick) while dragging or deleting to keep connected clips in place.
- `Ctrl+Alt`+click a connected clip to move its connection point to the click.
- Click a role band header (audio lanes mode) to collapse or expand it.
- Drag keyframe diamonds in the animation lane to retime them.
- Double-click a colour wheel to reset it.

> **Note on Ctrl+V:** egui's winit backend turns Ctrl+V into a *paste* event and drops the key. On Windows the editor reads the key state directly, so the shortcut always works. On Linux it relies on the paste event, so it only fires when the clipboard holds text. The same toggle is in the timeline toolbar (**Animation**) and in **View → Video Animation**.

---

## For developers

Everything below covers building from source and how the code is laid out.
The editor was tuned for an **AMD Ryzen 7 5800X** with an **Intel Arc A750**, on Windows
(DX12/Vulkan + Quick Sync over D3D11VA) and Linux (Vulkan + Quick Sync over VA-API `iHD`).

### Workspace

```
rust-cut-pro/
├── crates/nle-core     data model — no GPU, no FFmpeg, fully unit-tested
├── crates/nle-media    decoding, hardware acceleration, audio, thumbnails, proxies
├── crates/nle-render   wgpu/WGSL render engine and hardware export
└── crates/nle-ui       eframe/egui application (binary: rust-cut-pro)
```

There are no circular dependencies: `nle-core` ← `nle-media` ← `nle-render` ← `nle-ui`.

#### nle-core

| Module | What it does |
|---|---|
| `timeline` | Magnetic Timeline: the primary storyline is an ordered `Vec<Clip>` with no stored positions, so every edit ripples automatically. Connected clips live inside their parent clip (offset + lane + connection point), and compound clips, secondary storylines (compound clips flagged `storyline`), auditions, gap clips and adjustment layers are supported. Operations: insert/append/overwrite/connect, ripple delete, replace with gap, blade/blade all, trim, slip, slide, move/lift, position (no ripple), range delete, hold-connections and snapping. Every operation also runs inside a nested container via `Command::in_path`. |
| `focus` | Step-in editing ("Open in Timeline"): keeps the real timeline while a compound clip / storyline is shown, routes edits, undo and redo into it, and backs out when an undo removes the opened container. |
| `keyframe` | Keyframe curves with cubic Bézier eases, solved by Newton iteration with a bisection fallback. Interpolations: linear, ease in/out, hold and custom Bézier. |
| `speed` | Retiming built from segments, with smoothstep speed ramps integrated analytically. Covers 25/50/200/400 % presets, reverse, hold (freeze) frames, and frame blending / optical-flow modes. |
| `command` | Undoable `Command`s and the `History` (`Vec<AppliedCommand>` undo and redo stacks). Before/after snapshots make redo exact, and continuous drags coalesce into a single undo step. |
| `autosave` | Atomic 30-second autosave: write to a temp file, fsync, rename, and keep a `.prev` rotation. Crash recovery is offered at the next launch. |
| `fcpxml` | FCPXML 1.10 import and export (assets, spine, connected clips, gaps, titles, compound clips, transitions, retiming, keyframes). |
| `effects` | Modular, reorderable clip effect stack (20 GPU effects across Blur, Sharpen, Light, Stylize, Distort, Color, Film Look, Motion, Keying, Glitch & Impact and Reframe), plus Adjustment clips and Rate Conform quality modes. |
| `captions` | Word-timed subtitle/caption track, active-word highlighting (`CapCut` yellow/green/custom, pill, outline) and `.srt` import / export. |
| `keymap` | Customizable keyboard & mouse (`Middle` / `Back` / `Forward`) bindings with `Final Cut Pro` and `DaVinci Resolve` presets and TOML persistence. |
| `eval` | Resolves the timeline at a given instant into a render graph (layers, adjustment passes, effects, captions and transitions) plus audio spans. |

#### nle-media

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

#### nle-render

All pixel work runs in WGSL compute shaders on the GPU:

- NV12 / P010 → working RGBA16F (display-encoded BT.709 values, limited-range expanded). Exposure and white balance convert to linear light internally.
- Exposure, white balance, lift/gamma/gain wheels, contrast, saturation and **trilinear `.cube` 3D LUTs**. A before/after split wipe is available for comparison.
- Modular **effect stack** compute shaders for 20 effects: Gaussian/Radial/Directional/Zoom/Motion Blur, Glow, Vignette, Sharpen, Film Grain, Halation, Camera Shake, RGB Split Glitch, Impact Flash, Neon Outline, Pixelate, Lens Distortion (with chromatic aberration), Chroma Key, Hue/Sat/Lightness, Levels and 9:16 Gaming Reframe, including **Adjustment clip** passes over layers below.
- Transform, crop, Ken Burns, opacity and 14 blend modes: Normal, Multiply, Screen, Overlay, Add, Soft Light, Hard Light, Color Dodge, Color Burn, Darken, Lighten, Difference, Exclusion and Luminosity. They round-trip through FCPXML `adjust-blend`.
- **Transitions:** Cross Dissolve, Dip to Black, Dip to White, Whip Pan (directional blur), Slide, Push and Luma Wipe.
- Slow-motion and retimed footage **Frame Blending** and **block-matching motion-compensated interpolation** (`Optical Flow`).
- Titles, lower thirds, animated text presets, active-word highlighted captions, outline and pill backgrounds.
- NV12 packing for the encoders, with **export** via `av1_qsv`, `hevc_qsv`, `h264_qsv` and `h264_vaapi` (or software `libx264`). When timeline-used online media requires credit (`CC-BY`, etc.), `project_credits.txt` is written automatically beside the exported video.
- Decoded frames stay in VRAM in an **LRU cache capped at 2,560 MB**.

#### nle-ui

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
  - Secondary storylines (grey top bar), compound / audition badges, a breadcrumb while stepped into a compound clip, the Position tool, and an **Audio Lanes** toggle that groups audio below the storyline into collapsible role bands.
- **Auto Captions & Whisper (`Ctrl+Shift+C`)** & **Keyboard/Mouse Shortcuts (`Ctrl+,` / `F1`):**
  - Download/delete Whisper GGML models, pick language or auto-detect, toggle Vulkan GPU acceleration, import/export `.srt`, edit segments (with automatic word retiming) and style captions live.
  - Customize all key and mouse (`Middle`, `Back`, `Forward`) bindings, switch between `Final Cut Pro` and `DaVinci Resolve` presets, view live conflict warnings, and autosave to `config/keybindings.toml`.

---

### System packages

#### Linux (Ubuntu 24.04 / Debian 13)

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

#### Windows 10/11

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

### Build & run

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
| `RCP_DATA_DIR` | Override `%LOCALAPPDATA%\RustCutPro` (where the downloaded FFmpeg goes). |
| `RCP_DISCORD_APP_ID` | Discord application ID. At build time it bakes the ID into the exe; at run time it overrides it. |

Autosaves go to `%APPDATA%\RustCutPro\autosave` on Windows and `~/.local/share/RustCutPro/autosave` on Linux (`config\keybindings.toml`, `logs\` and `proxies\` live beside it). Whisper `models\`, `downloads\` and the downloaded FFmpeg `bin\` live under `%LOCALAPPDATA%\RustCutPro` (Linux: `~/.local/share/rust-cut-pro`).

#### Release packaging (Windows)

```powershell
powershell -ExecutionPolicy Bypass -File scripts\package_windows.ps1   # add -SkipBuild to reuse target\release
```

This writes `dist\RustCutPro-v<version>-win64-portable.zip`, the installer `dist\RustCutPro-v<version>-win64-setup.exe`
(only when [Inno Setup 6](https://jrsoftware.org/isinfo.php) is installed: `winget install JRSoftware.InnoSetup`)
and `dist\SHA256SUMS.txt`. The exe is signed only if a certificate is configured through environment variables
(see the script header and [docs/RELEASE_DECISIONS.md](docs/RELEASE_DECISIONS.md)). After a dependency change,
regenerate the license file with `python scripts/gen_third_party_notices.py`.

---

### Tests

`cargo test --workspace` runs these suites:

- **`nle-core/tests/timeline_tests.rs`:** ripple insert (including in the middle of a clip), blade, gap behaviour, compound make/break, Bézier evaluation, speed-ramp mapping and its inverse, retime splits, undo/redo (exact restore and coalescing), autosave atomicity and recovery, and an FCPXML byte-stable round trip.
- **`nle-core/tests/storyline_tests.rs`:** secondary storylines (create, independent ripple, transitions inside), nested `in_path` edits resizing their container, Position moves leaving gaps, hold-connections, connection points, auditions (add/cycle/finalize) and their `.rcp` / FCPXML round trips. The `focus` module's unit tests cover step-in editing, nested levels and undo across levels.
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
- **Release smoke test** (manual, `#[ignore]`): `nle-render/tests/e2e_smoke.rs` exercises the whole pipeline on generated media. It synthesizes speech with Windows TTS, then imports → trims → applies Zoom Punch → runs Whisper auto-captions → applies look-ahead ducking → exports → validates with ffprobe, and measures the duck in the exported audio. It needs ffmpeg, a GPU and `ggml-base-q5_1.bin` in the models folder:

  ```powershell
  cargo test -p nle-render --features nle-media/whisper-vulkan --test e2e_smoke -- --ignored --nocapture
  ```

**Verified on** Ryzen 7 5800X + Arc A750 under Windows 11, with FFmpeg 9.0.2 (gyan.dev full build):
- every suite passes;
- Quick Sync decode works for H.264, HEVC Main10 and AV1;
- proxies use `h264_qsv`;
- exports work through `av1_qsv`, `hevc_qsv`, `h264_qsv` and `libx264`.

The `ffmpeg` feature (`ffmpeg-next` 9.1) was built and tested against the same FFmpeg 9.0.2, using the winget `Gyan.FFmpeg.Shared` package. The Windows section above has the commands.

---

### Implementation notes

- **Intel Arc on DX12:** creating an `Rgba8UnormSrgb` *view* of a storage-capable `Rgba8Unorm` texture loses the device. The display output is therefore copied into a separate sRGB texture for egui instead of being reinterpreted (see `Renderer::make_display`).
- **Optional libav decoder:** the `ffmpeg` feature (`nle-media/src/decode/libav.rs`) uses FFmpeg's generic hwaccel (D3D11VA / VA-API `iHD`) and transfers frames to NV12/P010. It needs FFmpeg dev libraries and libclang to build. The default CLI decoder covers the full QSV → VA-API/D3D11VA → software chain.
- **Quick Sync on Windows (FFmpeg 8/9):**
  - `hwupload=extra_hw_frames=N` fails on D3D11 with `E_INVALIDARG`, so Windows uses a plain `hwupload,format=qsv`.
  - Output colour flags (`-colorspace` etc.) make FFmpeg insert an auto-scale filter that cannot accept QSV frames. BT.709 tags are therefore set with `setparams` before the upload.
  - QSV decode uses `-hwaccel_output_format qsv` with an explicit `hwdownload`. Without it, HEVC 10-bit and AV1 fail format negotiation.
