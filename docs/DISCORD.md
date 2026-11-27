# Discord Rich Presence

Depthcut can show what you're editing in your Discord status, for example
*"Editing: ace_montage"* / *"Timeline · 14 clips · 02:41 · 1920×1080 60p"*. The
project name and the timeline stats can each be hidden. It is optional and off
until you give it a Discord Application ID.

## How it behaves

- It talks to the Discord desktop app over its local IPC pipe
  (`\\.\pipe\discord-ipc-0..9`). Nothing is sent to any server by Depthcut.
- If Discord isn't running, or no Application ID is set, it does nothing and
  shows no errors. It reconnects on its own if Discord starts later.
- Settings: *Help → Discord Rich Presence…*. They are saved in
  `%APPDATA%\Depthcut\discord.json`.

## Where the Application ID comes from

The editor checks these in order:

1. the ID saved in *Help → Discord Rich Presence…* (`discord.json`)
2. the `RCP_DISCORD_APP_ID` environment variable at run time
3. the ID compiled into the exe: set `RCP_DISCORD_APP_ID` while running
   `cargo build --release` (or `scripts\package_windows.ps1`) and it's baked in
   with `option_env!`

Official release builds should use option 3, so users get presence without
any setup. **No ID is baked in yet.** The project needs a Discord application
first (checklist below).

## One-time Discord application setup (maintainer)

1. Open <https://discord.com/developers/applications> → **New Application** →
   name it **Depthcut**. The name appears in people's status as
   *"Playing Depthcut"*.
2. **General Information:** upload `assets/depthcut-icon.png` as the App
   Icon. Copy the **Application ID**.
3. **Rich Presence → Art Assets:** upload `assets/depthcut-discord.png`
   (1024×1024) with the key **`depthcut`**. This must match the editor's
   default asset key. New assets can take a few minutes to show up.
4. Build the release with the ID:

   ```powershell
   $env:RCP_DISCORD_APP_ID = "<application id>"
   powershell -ExecutionPolicy Bypass -File scripts\package_windows.ps1
   ```

5. Check it: start Discord, start the editor, open a project, and look at your
   profile status.

An Application ID isn't a secret: it ships inside every Discord-enabled app.
There is no client secret involved, and none is needed for Rich Presence.

## Asset checklist

| Asset key | File | Size | Used for |
|---|---|---|---|
| `depthcut` | `assets/depthcut-discord.png` | 1024×1024 | large image in the status |
| (app icon) | `assets/depthcut-icon.png` | | application icon in the portal |

The portable zip and the installer include `assets\depthcut-discord.png`
next to the exe, so the dialog's **Open Icon Folder** button works for users
who set up their own application.
