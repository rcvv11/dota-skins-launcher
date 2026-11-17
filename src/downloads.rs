//! Files the launcher fetches by itself on first start: Source2Viewer-CLI
//! (ValveResourceFormat, MIT) for the 3D preview, item colourings and glows.
//! The item catalog is built by gencatalog (app.rs), which downloads the
//! public item schema itself.

use std::io::Read;
use std::path::PathBuf;

/// Latest Windows build of the CLI from the project's GitHub releases.
const VRF_CLI_URL: &str =
    "https://github.com/ValveResourceFormat/ValveResourceFormat/releases/latest/download/cli-windows-x64.zip";

/// Where a downloaded CLI lives: `%APPDATA%\dota-skins\tools\vrf-cli`.
pub fn vrf_cli_dir() -> PathBuf {
    crate::config::app_dir().join("tools").join("vrf-cli")
}

/// Downloads and unpacks the CLI. Returns the path of the exe.
pub fn download_vrf_cli() -> Result<PathBuf, String> {
    let dir = vrf_cli_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let zip = dir.with_extension("zip.part");

    let resp = ureq::get(VRF_CLI_URL)
        .set("User-Agent", "dota-skins-launcher")
        .call()
        .map_err(|e| crate::trf!("не удалось скачать Source2Viewer-CLI: {e}", "could not download Source2Viewer-CLI: {e}"))?;
    let mut bytes = Vec::new();
    resp.into_reader()
        .take(512 * 1024 * 1024)
        .read_to_end(&mut bytes)
        .map_err(|e| crate::trf!("загрузка Source2Viewer-CLI прервалась: {e}", "Source2Viewer-CLI download was interrupted: {e}"))?;
    std::fs::write(&zip, &bytes).map_err(|e| format!("{}: {e}", zip.display()))?;

    // Windows' own PowerShell unpacks zips: no archive library needed.
    use std::os::windows::process::CommandExt;
    let status = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg(format!(
            "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
            zip.display(),
            dir.display()
        ))
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .status()
        .map_err(|e| crate::trf!("не удалось распаковать Source2Viewer-CLI: {e}", "could not unpack Source2Viewer-CLI: {e}"))?;
    let _ = std::fs::remove_file(&zip);
    if !status.success() {
        return Err(crate::tr!("не удалось распаковать Source2Viewer-CLI", "could not unpack Source2Viewer-CLI").into());
    }
    let exe = dir.join(crate::preview::CLI_NAME);
    if exe.is_file() {
        Ok(exe)
    } else {
        Err(crate::trf!("в архиве нет {}", "no {} in the archive", crate::preview::CLI_NAME))
    }
}
