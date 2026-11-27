# Changelog

All notable changes to Depthcut. Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versions follow [SemVer](https://semver.org/).

## [0.1.0] — unreleased

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
- 16 GPU effects, colour wheels, `.cube` LUTs, 7 transitions, titles and lower thirds, all in WGSL.
- FCPXML 1.10 import/export. `.rcp` project files can be opened from the command line, or by
  double-clicking them when the installer's file association is enabled.

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
- 3-band EQ, Vocal Clarity, −14 LUFS normalize with a peak limiter, waveforms.
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
