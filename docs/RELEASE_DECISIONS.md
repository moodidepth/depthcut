# Release decisions — v0.1.0

Decided 2026-11-17. Each entry says what was chosen, why, what is in the code
now, and what is still open. "Verified" means it was run on the dev machine
(Windows 11, Ryzen 7 5800X, Arc A750). Anything not run is marked unverified.

---

## 1. Platforms: Windows 10/11 x64 only

**Decision.** Windows 10 1809+ / 11, x86-64, is the only supported release
target. Linux builds from source and is labelled *experimental*. There are no
macOS builds.

**Why.** The dev machine and every hardware path (D3D11VA/QSV decode, QSV
encode, the Arc DX12 workaround) are tested on Windows only. Linux code paths
exist (VA-API, Unix IPC for Discord) but nobody runs them regularly.

**State.** `installer/depthcut.iss` sets `MinVersion=10.0.17763` and x64
only. The README says so at the top.

---

## 2. Distribution: portable zip + Inno Setup installer

**Decision.** Ship both files:

- `Depthcut-vX.Y.Z-win64-portable.zip`: one folder with the exe, VC++
  runtime DLLs, docs and licenses. Unzip and run.
- `Depthcut-vX.Y.Z-win64-setup.exe`: an Inno Setup 6 installer.
  - Installs per user by default (no UAC); the dialog offers an all-users install.
  - Start-menu shortcut and an optional desktop icon.
  - Optional `.rcp` → `Depthcut.Project` association; the app opens `argv[1]`.
  - Uninstaller under *Settings → Apps*.

Plus a `SHA256SUMS.txt`.

**Why.** The zip suits people who don't want an installer. The installer is
needed for the file association and a clean uninstall. Inno Setup is free,
scriptable, and doesn't need WiX/MSI tooling.

**State.**

- `scripts/package_windows.ps1` builds both.
- The `AppId` GUID `{2C8EC40F-E4BE-43BB-9672-FD3DECDD4C97}` must never change, because
  upgrades and the uninstaller are keyed on it.
- The VC++ 2015–2022 runtime (`msvcp140`, `vcruntime140`, `vcruntime140_1`) is
  copied app-local from the Visual Studio redist folder. Microsoft permits this.
- The UCRT and `d3dcompiler_47` are part of Windows 10+.
- **Open:** Inno Setup isn't installed on the dev machine, so the setup.exe has
  **not been built or tested yet** (`winget install JRSoftware.InnoSetup`, then
  rerun the script).

User data is kept on uninstall (see README → *Where your data lives*).

---

## 3. FFmpeg: not bundled; first-launch downloader

**Decision.** Don't ship FFmpeg. Find it in this order:

1. `RCP_FFMPEG` / a custom path
2. next to the exe
3. `%LOCALAPPDATA%\Depthcut\bin`
4. `PATH`
5. winget / Scoop / Chocolatey folders

If none is found, a banner offers **Download FFmpeg Automatically** or **Locate
Existing ffmpeg.exe…**.

**Why.** The FFmpeg builds with QSV, x264 and x265 are GPL. Bundling them would
oblige us to distribute GPL source and makes the license story harder for an
MIT/Apache app. Running FFmpeg as a separate program the user downloads keeps
the two cleanly apart. It also keeps the zip about 100 MB smaller.

**State.**

- Downloads the gyan.dev *release essentials* zip.
- Verifies the SHA-256 published next to it.
- Extracts `ffmpeg.exe`, `ffprobe.exe` and the license to
  `%LOCALAPPDATA%\Depthcut\bin`.
- Shows progress and can be cancelled.

**Verified 2026-11-17:** a real download of 9.0.2-essentials matched its
checksum, extracted, and ran (the `#[ignore]` test
`real_gyan_download_verifies_and_extracts`).

The *essentials* build includes libx264. Whether its Quick Sync encoders work
on Arc **was not checked** (the verified download was a temporary copy). The
user's own *full* build is what the QSV tests ran against. **Open:** if gyan.dev changes its URL layout, the downloader breaks; the
"Locate…" button and manual install remain as fallbacks.

---

## 4. AI Copilot / Gemini: live model list + fallback + File API

**Decision.** Built-in model names are only defaults. Settings fetches the
models your key can actually use (`GET v1beta/models`, filtered to
`generateContent`). It also accepts a custom model ID, and retries once on a
fallback model (default `gemini-flash-latest`) when the main model returns
429/503. Video context goes through the Gemini **File API**:

1. resumable upload
2. poll until `ACTIVE`
3. reference the file by URI
4. delete it afterwards

**Why.** Model names change faster than releases. Hard-coding them meant the
Copilot silently broke. The old inline-base64 video path hit request size
limits.

**State.**

- Implemented, with a **Test Live Connection** button.
- **Verified:** File API upload and polling against the real API.
- **Unverified:** a full `generateContent` round trip with a real key. The API
  returned 503 (overloaded) during testing, so the answer path and the
  fallback-on-503 behaviour have only been exercised by unit tests and that
  503 itself.
- The built-in names `gemini-4-hydrogen/argon/carbon` are **not confirmed to
  exist**. Users should pick from the "Available to your key" list.
- OpenAI-compatible and Ollama providers are implemented. Neither was tested
  against a live server for this release.

---

## 5. API keys: Windows Credential Manager

**Decision.** Store keys with the `keyring` crate in Windows Credential Manager,
service `Depthcut`, one entry per provider (`gemini_api_key.Depthcut`,
`openai_api_key.Depthcut`). `%LOCALAPPDATA%\Argon\ai_config.toml` holds no
keys. A key found in an old plaintext config is moved to the credential store
on load and then removed from the file.

**Why.** Plaintext keys in a TOML file are readable by any process and end up
in backups and screenshots.

**State.**

- Implemented and unit-tested (migration test with cleanup).
- If the credential store is unavailable (e.g. Linux without Secret Service),
  the editor falls back to the file and says so in Settings.
- **Verified on the dev machine 2026-11-17:** the first launch of the packaged
  build moved the old plaintext key out of `ai_config.toml` (no `api_key` lines
  left) into Credential Manager (`gemini_api_key.Depthcut`).

---

## 6. Code signing: SmartScreen docs now, signing hooks ready

**Decision.** v0.1.0 ships **unsigned**. The README explains the SmartScreen
*More info → Run anyway* step. `scripts/package_windows.ps1` signs the exe and
the installer automatically when any of these is set:

| Env vars | Method |
|---|---|
| `RCP_TRUSTED_SIGNING_DLIB` + `RCP_TRUSTED_SIGNING_METADATA` | Azure Trusted Signing (`signtool /dlib … /dmdf …`, timestamp `http://timestamp.acs.microsoft.com`) |
| `RCP_SIGN_PFX` (+ `RCP_SIGN_PFX_PASSWORD`) | `.pfx` file |
| `RCP_SIGN_CERT_THUMBPRINT` | certificate in the user store |

`RCP_SIGN_TIMESTAMP_URL` overrides the timestamp server (default DigiCert).
With none of these set, signing is skipped with a message.

**Why.** Azure Trusted Signing (about $10/month) is the cheapest way to build
SmartScreen reputation. It needs identity validation (business, or
individual where available), which is a step for the owner, not the code.

**State.** The hooks are written but **unverified**: no certificate exists to
test them with. `signtool.exe` is present on the dev machine (Windows SDK
10.0.26100).

---

## 7. Discord Rich Presence: App ID slot, silent fallback

**Decision.**

- The Application ID comes from the user's settings, the `RCP_DISCORD_APP_ID`
  environment variable, or an ID baked in at build time
  (`option_env!("RCP_DISCORD_APP_ID")`).
- With no ID, or with Discord not running, presence does nothing and shows no
  errors.
- The large-image asset key is `depthcut`.

**Why.** Users shouldn't need a developer account to see presence. The
maintainer creates one Discord application and bakes its ID into release builds.

**State.**

- Implemented.
- **Open:** the Discord application doesn't exist yet, so no ID is baked in.
  Checklist: [docs/DISCORD.md](DISCORD.md).

---

## Also decided along the way

- **License:** MIT OR Apache-2.0. `THIRD_PARTY_NOTICES.md` is generated from
  the real Windows dependency graph (`scripts/gen_third_party_notices.py`). It
  is shown in *Help → Licenses & Notices…* and shipped in both packages.
  Regenerate it after any dependency change.
- **Vulkan is a hard runtime requirement.** whisper.cpp is built with the Vulkan
  backend, so the exe imports `vulkan-1.dll` and won't start on a PC without a
  Vulkan driver. Every current AMD/NVIDIA/Intel driver installs it, but very old
  or VM GPUs may not. A future option: load Vulkan dynamically, or ship a CPU-only
  Whisper build. This is documented in README → System requirements.
- **CI:** `.github/workflows/ci.yml` (Windows) runs check, test and package on
  every push to main and every PR (branches: `gh workflow run CI --ref <branch>`).
- **Release smoke test:** `crates/nle-render/tests/e2e_smoke.rs` (`#[ignore]`,
  needs TTS, ffmpeg, a GPU and the Whisper base model). It runs import → trim →
  Zoom Punch → Whisper captions → look-ahead ducking → export → ffprobe, and
  measures the duck in the exported audio. **Passed 2026-11-17** on the dev
  machine:
  - export via `av1_qsv`, 9.5 s, 1280×720
  - music −39.1 dB → −52.8 dB under the voice → −39.1 dB after

  The same run found GPU caption word drops ("This is a" and "word." missing).
  **Re-tested 2026-11-27:** the same mix now gives all 14 words on the GPU,
  with DTW word timestamps on and off, and matches the CPU. No code change
  explains it. The PC's CPU hardware fault was fixed in between, which is the
  most likely cause. `crates/nle-media/tests/whisper_backends.rs` compares
  CPU/GPU × DTW recall on longer recordings: 98/98 words on clean speech for
  every config, and identical results on noisy speech. DTW word timing is now
  on by default on the GPU too (`RCP_WHISPER_DTW_GPU=0` turns it off). Captions
  stay marked *alpha*, and *Use GPU* off remains the fallback.
