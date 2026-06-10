//! Lightweight, cross-platform self-update.
//!
//! On startup we ask GitHub for the latest release. If it is newer than the
//! running version we surface a prompt; if the user accepts we download the
//! single-file binary for this platform and overwrite the running executable
//! in place (the OS keeps the old image mapped until the process exits, so the
//! swap is safe on Windows, macOS and Linux alike).
//!
//! To stay dependency-light the release pipeline publishes a *raw* binary per
//! platform (and an AppImage on Linux), so there is no archive to unpack here —
//! we just fetch one file and hand it to [`self_replace`].

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::worker::Command;

/// GitHub "latest release" endpoint for this project.
const RELEASES_API: &str = "https://api.github.com/repos/Plasmarobo/electronicmail/releases/latest";

/// A newer release discovered on GitHub, ready to download and install.
#[derive(Clone, Debug)]
pub struct ReleaseInfo {
    /// Human-readable version (e.g. "0.2.2"), with any leading `v` stripped.
    pub version: String,
    /// Markdown release notes (may be empty).
    pub notes: String,
    /// Direct download URL for this platform's binary asset.
    pub download_url: String,
    /// The asset file name (used for the temp download path).
    pub asset_name: String,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

/// The release asset name(s) that can update *this* platform, in priority
/// order. Empty on platforms we don't publish binaries for (updater disabled).
const fn platform_assets() -> &'static [&'static str] {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        &["electronicmail-windows-x86_64.exe"]
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        &["electronicmail-linux-x86_64.AppImage"]
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        &["electronicmail-macos-aarch64"]
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        &["electronicmail-macos-x86_64"]
    }
    #[cfg(not(any(
        all(target_os = "windows", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "x86_64"),
    )))]
    {
        &[]
    }
}

/// Spawn a background thread that checks GitHub for a newer release and, if it
/// finds one, asks the worker to surface an update prompt. Network failures are
/// swallowed: a missed update check should never disrupt the app.
pub fn spawn_check(cmd_tx: std::sync::mpsc::Sender<Command>) {
    std::thread::spawn(move || {
        if let Ok(Some(release)) = check() {
            let _ = cmd_tx.send(Command::UpdateFound(release));
        }
    });
}

/// Query GitHub for the latest release and return it when it is newer than the
/// running build and ships a binary for this platform.
pub fn check() -> Result<Option<ReleaseInfo>> {
    let candidates = platform_assets();
    if candidates.is_empty() {
        return Ok(None);
    }

    let release: GhRelease = http_client(Duration::from_secs(15))?
        .get(RELEASES_API)
        .header("Accept", "application/vnd.github+json")
        .send()?
        .error_for_status()?
        .json()
        .context("parsing GitHub release response")?;

    let (Some(latest), Some(current)) = (
        parse_version(&release.tag_name),
        parse_version(env!("CARGO_PKG_VERSION")),
    ) else {
        return Ok(None);
    };
    if latest <= current {
        return Ok(None);
    }

    let Some(asset) = candidates
        .iter()
        .find_map(|name| release.assets.iter().find(|a| a.name == *name))
    else {
        return Ok(None);
    };

    Ok(Some(ReleaseInfo {
        version: release.tag_name.trim_start_matches('v').to_string(),
        notes: release.body.unwrap_or_default(),
        download_url: asset.browser_download_url.clone(),
        asset_name: asset.name.clone(),
    }))
}

/// Download the release binary and overwrite the running executable with it.
///
/// When running as a Linux AppImage we replace the `.AppImage` file itself
/// (pointed at by `$APPIMAGE`); otherwise we hand the fresh binary to
/// [`self_replace::self_replace`], which performs the swap correctly on every
/// platform (including renaming the busy executable on Windows).
pub fn install(release: &ReleaseInfo) -> Result<()> {
    let bytes = download(&release.download_url)?;

    if let Some(appimage) = std::env::var_os("APPIMAGE") {
        let target = PathBuf::from(appimage);
        let tmp = target.with_extension("update-tmp");
        std::fs::write(&tmp, &bytes)
            .with_context(|| format!("writing update to {}", tmp.display()))?;
        set_executable(&tmp)?;
        std::fs::rename(&tmp, &target)
            .with_context(|| format!("replacing {}", target.display()))?;
        return Ok(());
    }

    let tmp = std::env::temp_dir().join(format!("electronicmail-update-{}", release.asset_name));
    std::fs::write(&tmp, &bytes).with_context(|| format!("writing update to {}", tmp.display()))?;
    set_executable(&tmp)?;
    self_replace::self_replace(&tmp).context("replacing the running executable")?;
    let _ = std::fs::remove_file(&tmp);
    Ok(())
}

fn download(url: &str) -> Result<Vec<u8>> {
    let resp = http_client(Duration::from_secs(300))?
        .get(url)
        .send()?
        .error_for_status()?;
    Ok(resp.bytes()?.to_vec())
}

fn http_client(timeout: Duration) -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("electronicmail/", env!("CARGO_PKG_VERSION")))
        .timeout(timeout)
        .build()
        .context("building HTTP client")
}

/// Mark a freshly downloaded file as executable (no-op on Windows).
fn set_executable(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("setting execute permission on {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Parse a `MAJOR.MINOR.PATCH` version (tolerating a leading `v` and any
/// pre-release/build suffix) into a comparable tuple.
fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let core = s.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next().unwrap_or(core);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::parse_version;

    #[test]
    fn parses_plain_and_prefixed() {
        assert_eq!(parse_version("0.2.1"), Some((0, 2, 1)));
        assert_eq!(parse_version("v1.4.0"), Some((1, 4, 0)));
    }

    #[test]
    fn tolerates_suffixes_and_short_forms() {
        assert_eq!(parse_version("v2.0.0-rc1"), Some((2, 0, 0)));
        assert_eq!(parse_version("v3.1"), Some((3, 1, 0)));
        assert_eq!(parse_version("v4"), Some((4, 0, 0)));
    }

    #[test]
    fn ordering_is_semver_like() {
        assert!(parse_version("v0.2.2") > parse_version("v0.2.1"));
        assert!(parse_version("v0.3.0") > parse_version("v0.2.9"));
        assert!(parse_version("v1.0.0") > parse_version("v0.9.9"));
    }
}
