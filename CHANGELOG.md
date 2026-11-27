# Changelog

All notable changes to Depthcut. Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versions follow [SemVer](https://semver.org/).

## [Unreleased]

### NVIDIA and AMD GPUs
- Export and proxies use the hardware encoder of the GPU Depthcut renders on: **NVENC** on
  NVIDIA, **AMF** on AMD, **Quick Sync** on Intel. The other vendors' working encoders are still
  listed (for example on a laptop with two GPUs). Software x264 remains the fallback.
- NVENC exports use `-tune hq`, spatial AQ and a 20-frame lookahead. AV1 quality maps onto
  NVENC's 0–63 `-cq` scale and AMF's 0–255 QP scale. H.264 and AV1 on AMF also set the
  B-frame QP.
- Linux: CUDA (NVDEC) decode on NVIDIA, and VA-API decode on AMD without forcing Intel's
  `iHD` driver. Windows decoding stays on D3D11VA for every vendor.
- The GPU picker prefers any discrete GPU (NVIDIA, AMD or Intel Arc) over an integrated one.
- Tested on Windows 11 with a Ryzen 7 7800X3D + RTX 3080 and a Ryzen 7 5700X + RX 7800 XT, as
  well as the Ryzen 7 5800X + Arc A750 development machine. Linux CUDA / AMD VA-API decode is not
  hardware-tested yet. The start-up probe hides any encoder that fails.

### CPU cores on any processor
- The worker pool now comes from the real core topology instead of a fixed "12 workers, 4 free"
  rule. Whole fast cores are kept free for the UI, audio and GPU driver: 1 on 2–4-core CPUs, 2 on
  5–16, 3 on 17+. On Intel hybrid CPUs these are P-cores. The Ryzen 7 5800X keeps its old split
  (12 workers on CPUs 4–15).
- FFmpeg processes (decode, proxies, export, audio) run only on the worker CPUs. x264, x265,
  Whisper and ONNX thread counts follow the same budget.

## [0.1.0] — 2026-11-27

First public build. Windows 10/11 x64 only; Linux is experimental (build from source); macOS is
not supported yet.

### Name, welcome and alpha labels
- The app is now **Depthcut** (formerly Rust Cut Pro). On first launch the old
  `%APPDATA%\RustCutPro` and `%LOCALAPPDATA%\RustCutPro` folders are renamed to `Depthcut`, and
  saved API keys move to the `Depthcut` Credential Manager entry. The installer upgrades an old
  install in place and removes the old exe, shortcuts and file-association keys. `.rcp` project
  files and FCPXML metadata IDs are unchanged, so old projects open as before.
- A welcome window on first launch (and under *Help → Welcome & Support…*) explains who makes
  Depthcut, asks for support on [Ko-fi](https://ko-fi.com/moodidepth) to keep it free, and lists
  the alpha features.
- Unfinished features are labelled **(alpha)** in menus, panel titles and tooltips.
- Whisper word timestamps (DTW) are on by default on the GPU too.
- Keyboard and menu zoom (`Ctrl+=` / `Ctrl+-`) keeps the playhead in place instead of jumping
  to the start of the timeline.

### Editor
- Magnetic timeline: ripple insert/append/overwrite/connect, blade and blade all, ripple and lift
  delete, trim/slip/slide, compound clips, gaps, adjustment clips, markers, snapping, skimming.
- Undo/redo for every edit, with continuous drags merged into one step. Atomic 30-second autosave
  with crash recovery.
- Retime editor: 25–400 % presets, speed ramps, reverse, hold frames, frame blending and optical flow.
- Keyframe diamonds on every continuous inspector parameter, Bézier easing, animation lane.
- GPU effects, 14 blend modes, `.cube` LUTs, transitions, titles and lower thirds, all in WGSL.
- Several timelines per project, bins, keyword and smart collections, ratings, metadata, list view.
- Source viewer with In/Out marks, three-point edits, subclips and match frame; track targeting,
  lane overwrite/insert, Lift / Extract.
- Secondary storylines, Open in Timeline for compounds, Position tool, auditions, audio lanes by role.
- Multicam: audio / timecode sync, multicam clips, angle viewer, cut-and-switch.
- Motion: point / region / planar trackers, stabilization, scene-cut detection, Speed Warp, AI
  mattes *(alpha)*.
- Motion graphics: graph editor, Bézier roto masks, shapes, rolling credits, title templates.
- Text-based editing *(alpha)*: delete words, fillers and pauses in the transcript panel.

### Colour
- Colour management, stacked corrections (wheels, log wheels, curves), HSL qualifier, window and
  Bézier masks, node compositor graph.
- Grade copy/paste, Paste Attributes, node keyframes, stills gallery with shot match, HDR export.
- GPU scopes: waveform, parade, vectorscope, histogram.

### Interchange and delivery
- FCPXML 1.10 import/export, CMX 3600 EDL, OpenTimelineIO, AAF export *(alpha)*.
- `.rcp` project files open from the command line, or by double-click when the installer's file
  association is enabled.
- Render queue, In/Out export range, deinterlacing, subtitle tracks and WebVTT.

### Montage tools
- **Zoom Punch** (`Z`) and **Hard Zoom Punch** (`Shift+Z`) at the playhead.
- 16 built-in CS2 / Valorant presets (RSMB-style blur, scope pulse, impact shake, bullet-time ramp,
  flow sync, freeze-frame outline, colour looks).
- Beat detection, Sync to Beats (`Alt+S`), Scan VOD for kills / action peaks (`Shift+Alt+K`),
  Gaming 9:16 split-crop (`Alt+9`).
- Timeline Zoom to Fit is now `Ctrl+0`, because `Shift+Z` became Hard Zoom Punch.

### Audio
- Look-ahead auto-ducking: music and SFX duck before the voice starts. It is computed offline, so
  preview and export are identical.
- Bus mixer with submixes, automation and LUFS metering; parametric EQ, dynamics, restoration.
- VST3 and CLAP plug-in inserts *(alpha)*; neural voice isolation (DeepFilterNet 3) *(alpha)*;
  voiceover recording *(alpha)*; stem and audio-only export.
- Vocal Clarity, −14 LUFS normalize with a peak limiter, waveforms.
- Online CC0/CC-BY audio search (Openverse, Internet Archive) with preview, download and automatic
  credits file.

### Captions
- Offline Whisper auto-captions with Vulkan GPU acceleration, word-level highlight styles and
  `.srt` import/export.
- Captions follow the speech they belong to through ripple deletes, trims and reorders. Captions
  over deleted speech are removed.

### AI Editor Copilot (optional)
- Gemini, OpenAI-compatible or local Ollama.
- API keys are kept in **Windows Credential Manager**. A key found in an older plaintext
  `ai_config.toml` is moved there on first launch and removed from the file.
- The Gemini model picker lists the models your key can actually use. It also takes a custom model
  ID and an automatic fallback model for when the chosen one is overloaded (HTTP 429/503).
- **Test Live Connection** button in Settings.
- Video context is uploaded through the Gemini File API: a resumable upload, then waiting until
  the file is ACTIVE. The uploaded file is deleted afterwards.

### First run, packaging and diagnostics
- FFmpeg banner when FFmpeg is missing: **Download FFmpeg Automatically** (gyan.dev build,
  SHA-256 verified, installed per user) or **Locate Existing ffmpeg.exe…**.
- Portable zip and Inno Setup installer (Start menu, optional desktop icon, `.rcp` association,
  uninstaller). VC++ runtime included app-local.
- *Help → About* and *Help → Licenses & Notices…*: version, FFmpeg status, and every third-party
  license (generated from the real dependency graph).
- *Help → Show Log File*: current and previous session logs, with a crash marker detected at the
  next launch.
- Optional Discord Rich Presence. It is silent when Discord isn't running.

### Fixed (bug hunt, 2026-11-17)
- Save now marks the project clean and drops the stale autosave.
- A "Save changes?" prompt appears before New / Open / Demo / FCPXML import / drop, and on quit.
- A recovered autosave stays marked unsaved until you save it.
- Undoing back to the saved state counts as clean.
- Retimed clips: head/tail trims stop at the media bounds.
- Connected clips: a left-edge trim keeps the clip's end in place.

### Known limitations
- Builds are not code-signed yet, so SmartScreen warns on first run (*More info → Run anyway*).
- A GPU driver with Vulkan support is required (`vulkan-1.dll`).
- Features marked **(alpha)** are still in development: AI Copilot, Auto Captions and the
  transcript editor, AI mattes, voice isolation, voiceover recording, AAF export, and audio plug-ins
  (VST3/CLAP).
- Auto Captions on the GPU once dropped words that the CPU caught (2026-11-17, Arc A750). It no
  longer reproduces (re-tested 2026-11-27, CPU and GPU give identical transcripts), but it has
  only been checked on one GPU. If captions skip words, untick **Use GPU** in the Captions panel
  and run again.
- Quick Sync hardware encode/decode needs an Intel GPU. Others use software x264.
- Gemini model availability depends on your key and region. Use *Test Live Connection*.
