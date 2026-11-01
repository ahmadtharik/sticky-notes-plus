//! Updates from GitHub Releases, driven only from Settings. Nothing is checked
//! in the background and nothing installs until the user asks.
use crate::google_calendar::{HttpRequest, Transport};
use anyhow::{anyhow, ensure, Context};
use serde_json::Value;
use std::path::{Path, PathBuf};

const LATEST_RELEASE: &str =
    "https://api.github.com/repos/ahmadtharik/sticky-notes-plus/releases/latest";
const DOWNLOAD_PREFIX: &str = "https://github.com/ahmadtharik/sticky-notes-plus/releases/download/";
const MSI_ASSET: &str = "StickyNotesPlus.msi";
const CHECKSUM_ASSET: &str = "StickyNotesPlus.msi.sha256";
const FAILURE_FILE: &str = "failed.txt";

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub msi_url: String,
    pub checksum_url: String,
}

fn parse_version(value: &str) -> Option<[u32; 3]> {
    let mut parts = value.strip_prefix('v').unwrap_or(value).split('.');
    let version = [
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ];
    parts.next().is_none().then_some(version)
}

fn asset_url(release: &Value, name: &str) -> anyhow::Result<String> {
    let url = release["assets"]
        .as_array()
        .and_then(|assets| {
            assets
                .iter()
                .find(|asset| asset["name"].as_str() == Some(name))
        })
        .and_then(|asset| asset["browser_download_url"].as_str())
        .ok_or_else(|| anyhow!("The release has no {name}"))?;
    ensure!(
        url.starts_with(DOWNLOAD_PREFIX),
        "Unexpected download address for {name}"
    );
    Ok(url.to_owned())
}

/// The newer release in a latest-release response, or None when already current.
fn newer_release(body: &str, current: &str) -> anyhow::Result<Option<Release>> {
    let release: Value = serde_json::from_str(body).context("Unreadable release information")?;
    let tag = release["tag_name"].as_str().unwrap_or_default();
    let latest = parse_version(tag).ok_or_else(|| anyhow!("Unexpected release tag {tag:?}"))?;
    let current = parse_version(current).ok_or_else(|| anyhow!("Invalid app version"))?;
    if latest <= current {
        return Ok(None);
    }
    Ok(Some(Release {
        version: tag.strip_prefix('v').unwrap_or(tag).to_owned(),
        msi_url: asset_url(&release, MSI_ASSET)?,
        checksum_url: asset_url(&release, CHECKSUM_ASSET)?,
    }))
}

/// None when up to date. A repository with no releases yet answers 404.
pub fn check(transport: &dyn Transport) -> anyhow::Result<Option<Release>> {
    let response = transport.send(HttpRequest {
        method: "GET",
        url: LATEST_RELEASE.to_owned(),
        bearer: None,
        body: None,
    })?;
    match response.status {
        200 => newer_release(&response.body, CURRENT_VERSION),
        404 => Ok(None),
        status => Err(anyhow!("GitHub answered {status}")),
    }
}

/// Reads the digest from `sha256sum`-style text ("<hex>  <file name>").
fn parse_checksum(text: &str) -> Option<[u8; 32]> {
    let hex = text.split_whitespace().next()?;
    if hex.len() != 64 || !hex.is_ascii() {
        return None;
    }
    let mut digest = [0u8; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(digest)
}

/// Only the installed copy may update itself: a development or acceptance
/// build must never replace the user's installed app.
pub fn installed_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let folder = exe.parent()?;
    let programs = folder.parent()?;
    let installed =
        folder.file_name()? == "Sticky Notes Plus" && programs.file_name()? == "Programs";
    installed.then_some(exe)
}

fn work_folder() -> PathBuf {
    std::env::temp_dir().join("StickyNotesPlusUpdate")
}

/// 3010 and 1641 mean installed, with a restart needed or under way.
fn install_succeeded(code: Option<i32>) -> bool {
    matches!(code, Some(0 | 1641 | 3010))
}

/// Why the last install failed, if it did. Clears the leftover files, keeping
/// a failed install's log beside the temp folder for diagnosis.
pub fn take_last_failure() -> Option<String> {
    let folder = work_folder();
    let failure = std::fs::read_to_string(folder.join(FAILURE_FILE)).ok();
    if failure.is_some() {
        let _ = std::fs::rename(
            folder.join("install.log"),
            std::env::temp_dir().join("StickyNotesPlusUpdate-failed.log"),
        );
    }
    let _ = std::fs::remove_dir_all(folder);
    failure
}

/// Downloads and verifies the installer. Call on a thread with WinRT
/// initialized.
pub fn download(release: &Release) -> anyhow::Result<PathBuf> {
    let transport = crate::google_calendar::WinRtTransport::new()?;
    let checksum = transport.download(&release.checksum_url)?;
    let expected = parse_checksum(&String::from_utf8_lossy(&checksum))
        .ok_or_else(|| anyhow!("The release checksum is unreadable"))?;
    let msi = transport.download(&release.msi_url)?;
    ensure!(
        crate::google_calendar::sha256(&msi) == expected,
        "The download is damaged"
    );
    let folder = work_folder();
    std::fs::create_dir_all(&folder)?;
    let path = folder.join(MSI_ASSET);
    std::fs::write(&path, msi)?;
    Ok(path)
}

/// Starts a temporary copy of this program to install once the app has exited.
/// The caller exits right after this returns.
pub fn start_install(msi: &Path, installed: &Path) -> anyhow::Result<()> {
    let helper = work_folder().join("updater.exe");
    std::fs::copy(installed, &helper)?;
    std::process::Command::new(helper)
        .arg("--apply-update")
        .arg(msi)
        .arg(installed)
        .arg(std::process::id().to_string())
        .spawn()?;
    Ok(())
}

/// `--apply-update <msi> <exe> <pid>`: waits for the app to exit, installs
/// silently, and reopens the app whether or not the install worked.
pub fn apply(args: &[String]) -> anyhow::Result<()> {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
    };
    let [msi, exe, pid] = args else {
        return Err(anyhow!("Expected an installer, program and process id"));
    };
    let pid: u32 = pid.parse()?;
    unsafe {
        let process = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if !process.is_null() {
            WaitForSingleObject(process, 30_000);
            CloseHandle(process);
        }
    }
    let folder = work_folder();
    let status = std::process::Command::new("msiexec")
        .args(["/i", msi, "/qn", "/norestart", "/l*v"])
        .arg(folder.join("install.log"))
        .status();
    let failure = match status {
        Ok(status) if install_succeeded(status.code()) => None,
        Ok(status) => Some(format!("code {}", status.code().unwrap_or(-1))),
        Err(error) => Some(error.to_string()),
    };
    if let Some(failure) = failure {
        let _ = std::fs::write(folder.join(FAILURE_FILE), failure);
    }
    std::process::Command::new(exe).spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(tag: &str, prefix: &str) -> String {
        serde_json::json!({
            "tag_name": tag,
            "assets": [
                {"name": MSI_ASSET, "browser_download_url": format!("{prefix}{tag}/{MSI_ASSET}")},
                {"name": CHECKSUM_ASSET, "browser_download_url": format!("{prefix}{tag}/{CHECKSUM_ASSET}")},
            ],
        })
        .to_string()
    }

    #[test]
    fn only_a_newer_release_is_offered() {
        let release = newer_release(&body("v0.10.0", DOWNLOAD_PREFIX), "0.9.1")
            .unwrap()
            .unwrap();
        assert_eq!(release.version, "0.10.0");
        assert!(release.msi_url.ends_with("/v0.10.0/StickyNotesPlus.msi"));
        assert_eq!(
            newer_release(&body("v0.9.1", DOWNLOAD_PREFIX), "0.9.1").unwrap(),
            None
        );
        assert_eq!(
            newer_release(&body("v0.9.0", DOWNLOAD_PREFIX), "0.9.1").unwrap(),
            None
        );
    }

    #[test]
    fn malformed_tags_and_foreign_downloads_are_rejected() {
        assert!(newer_release(&body("nightly", DOWNLOAD_PREFIX), "0.1.0").is_err());
        assert!(newer_release(&body("v1.2", DOWNLOAD_PREFIX), "0.1.0").is_err());
        assert!(newer_release(&body("v9.0.0", "https://example.com/"), "0.1.0").is_err());
        let no_assets = serde_json::json!({"tag_name": "v9.0.0", "assets": []}).to_string();
        assert!(newer_release(&no_assets, "0.1.0").is_err());
    }

    #[test]
    fn reboot_required_installs_are_successes() {
        for code in [0, 1641, 3010] {
            assert!(install_succeeded(Some(code)), "{code}");
        }
        for code in [Some(1), Some(1603), None] {
            assert!(!install_succeeded(code), "{code:?}");
        }
    }

    #[test]
    fn checksum_text_is_parsed_strictly() {
        let hex = "ab".repeat(32);
        assert_eq!(
            parse_checksum(&format!("{hex}  StickyNotesPlus.msi\n")),
            Some([0xab; 32])
        );
        assert_eq!(parse_checksum("abcd"), None);
        assert_eq!(parse_checksum(&"zz".repeat(32)), None);
        assert_eq!(parse_checksum(""), None);
    }
}
