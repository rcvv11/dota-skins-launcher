# Dota Skins Launcher

**English** | [Русский](README.ru.md)

A skin changer for Dota 2.

> ⚠️ **Risks.** In memory mode the launcher injects a DLL into `dota2.exe`, and for items in empty
> slots it changes `gameinfo_branchspecific.gi` and `dota.signatures`. This breaks the Steam / Dota 2
> rules and can get your account banned (VAC). Use at your own risk, preferably on a separate
> account. Verifying the game files in Steam restores the original game.

---

## Features

- **Presets** of items for every hero, plus general items (couriers, wards, creeps, towers, emblems,
  weather, terrain, map effects, HUD, loading screens and more).
- **Picking inside Dota.** Items can be picked right in the game client.

- **Two modes.** DLL injection (to change items inside the running game) or plain VPK overrides.

## Building and running

### Step 1. Install Rust

1. Download and run the installer from <https://rustup.rs> (`rustup-init.exe`).
2. If the installer offers to install Visual Studio, choose
   **3) Don't install the prerequisites**: the project does not need it. Then press `Enter`:
   the default settings are fine.
3. Open a new PowerShell window and run:

   ```powershell
   rustup default stable-x86_64-pc-windows-gnu
   ```

### Step 2. Install a C++ compiler (MinGW-w64)

It is needed to build `changer.dll`.

1. Download `w64devkit-x64-<version>.7z.exe` from
   <https://github.com/skeeto/w64devkit/releases> and run it. It is a self-extracting archive:
   choose `C:\` and the files end up in `C:\w64devkit`.
2. Add `C:\w64devkit\bin` to the `PATH` environment variable:
   Start → "Edit the system environment variables" → "Environment Variables" → `Path` → "New".
3. Open a new PowerShell window and check: `g++ --version` should print a version.

### Step 3. Get the project

Use **Code → Download ZIP** on the repository page (and unpack it), or git:

```powershell
git clone <url> dota-skins-launcher
```

### Step 4. Build

Open PowerShell in the project folder and run:

```powershell
powershell -ExecutionPolicy Bypass -File build.ps1
```

The script checks that Rust and the compiler are installed, builds the launcher and `changer.dll`
and puts everything into the `dist\` folder.

### Step 5. Run

1. Start `dist\dota-skins-launcher.exe`. It asks for administrator rights right away: they are
   needed to connect to Dota.
2. **On first start** the launcher downloads everything it needs by itself:
   - the item catalog (from Valve's public item schema, about a minute);
   - Source2Viewer-CLI (about 65 MB), for the 3D preview, item colourings and glows.
   The progress is shown in the status bar at the bottom of the window.
3. In the settings, set the path to `dota2.exe`:
   `...\steamapps\common\dota 2 beta\game\bin\win64\dota2.exe`.
4. Press "LAUNCH DOTA 2". The launcher starts the game and connects to it.
5. In Dota, hover over an item card and press "SELECT". Keep the launcher open: it handles
   the presses.

Some things take effect only after Dota is restarted: items for empty slots (the item table is
read when the game starts), weather, and turning off model merging (`autoexec.cfg`).


## Third-party components

- [ValveResourceFormat / Source2Viewer-CLI](https://github.com/ValveResourceFormat/ValveResourceFormat)
  (MIT): not part of the repository, the launcher downloads it itself.
- [SteamDatabase/GameTracking-Dota2](https://github.com/SteamDatabase/GameTracking-Dota2) and
  [odota/dotaconstants](https://github.com/odota/dotaconstants): data sources for gencatalog.
- Dota 2, its models, effects and item names belong to Valve Corporation. This project is not
  affiliated with Valve and does not distribute its files.
