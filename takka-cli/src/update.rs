//! Self-update mechanism via GitHub Releases.
//!
//! Checks `https://api.github.com/repos/dchya24/agentic/releases/latest`
//! for a newer version, downloads the matching binary asset, and replaces
//! the running executable in-place.

use anyhow::{Context, Result};
use ratatui::style::{Color as RColor, Modifier as RModifier, Style as RStyle};
use ratatui::text::{Line as RLine, Span as RSpan};
use std::env;
use std::fs;
// No direct io imports needed; curl handles download
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::widgets::{components, inline};

// ── Constants ──────────────────────────────────────────────

const REPO_OWNER: &str = "dchya24";
const REPO_NAME: &str = "agentic";
const GITHUB_API: &str = "https://api.github.com";

/// Current version embedded at compile time from `CARGO_PKG_VERSION`.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

// ── Public API ─────────────────────────────────────────────

/// Outcome of an update check (no network I/O performed yet).
pub struct UpdateInfo {
    pub latest_version: String,
    pub download_url: String,
    pub asset_name: String,
    pub release_notes: String,
}

/// Check GitHub for the latest release. Returns `Ok(None)` when already
/// up-to-date.
pub fn check_for_update() -> Result<Option<UpdateInfo>> {
    let latest = fetch_latest_release()?;

    if is_newer(&latest.tag_name, CURRENT_VERSION)? {
        let asset = pick_asset(&latest.assets)?;

        Ok(Some(UpdateInfo {
            latest_version: trim_v_prefix(&latest.tag_name).to_string(),
            download_url: asset.browser_download_url.clone(),
            asset_name: asset.name.clone(),
            release_notes: latest.body.clone().unwrap_or_default(),
        }))
    } else {
        Ok(None)
    }
}

/// Check for update and print result inline (for `takka update --check`).
pub fn check_and_print() -> Result<()> {
    inline::print_blank();
    inline::print_line(&components::section_header(
        "🔄",
        "Checking for updates…",
        RColor::Cyan,
    ));
    inline::print_blank();

    match check_for_update() {
        Ok(Some(info)) => {
            print_update_available(&info);
        }
        Ok(None) => {
            inline::print_line(&components::success_badge(&format!(
                "Already up to date (v{})",
                CURRENT_VERSION,
            )));
        }
        Err(e) => {
            inline::print_line(&components::error_badge(&format!(
                "Failed to check for updates: {}",
                e
            )));
        }
    }

    inline::print_blank();
    Ok(())
}

/// Download and apply the update. Performs network I/O.
pub fn run_update() -> Result<()> {
    inline::print_blank();
    inline::print_line(&components::section_header(
        "🔄",
        "Checking for updates…",
        RColor::Cyan,
    ));
    inline::print_blank();

    // 1. Check
    let info = match check_for_update()? {
        Some(i) => i,
        None => {
            inline::print_line(&components::success_badge(&format!(
                "Already up to date (v{})",
                CURRENT_VERSION,
            )));
            inline::print_blank();
            return Ok(());
        }
    };

    print_update_available(&info);

    // 2. Download
    inline::print_line(&components::info_badge(&format!(
        "Downloading {}…",
        info.asset_name
    )));

    let tmp = download_to_temp(&info.download_url)?;
    inline::print_line(&components::success_badge("Download complete."));

    // 3. Install
    inline::print_line(&components::info_badge("Installing…"));
    install_binary(&tmp)?;

    inline::print_blank();
    inline::print_line(&components::success_badge(&format!(
        "Successfully updated to v{}!",
        info.latest_version,
    )));
    inline::print_line(&RLine::from(vec![
        RSpan::styled("  ", RStyle::default()),
        RSpan::styled(
            "Restart takka to use the new version.",
            RStyle::default()
                .fg(RColor::Yellow)
                .add_modifier(RModifier::BOLD),
        ),
    ]));
    inline::print_blank();

    Ok(())
}

// ── GitHub API types ───────────────────────────────────────

#[derive(serde::Deserialize)]
struct GithubRelease {
    tag_name: String,
    body: Option<String>,
    assets: Vec<GithubAsset>,
}

#[derive(serde::Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

/// Fetch the latest release from GitHub.
fn fetch_latest_release() -> Result<GithubRelease> {
    let url = format!(
        "{}/repos/{}/{}/releases/latest",
        GITHUB_API, REPO_OWNER, REPO_NAME
    );

    let output = run_curl(&url, "GET")?;
    let release: GithubRelease =
        serde_json::from_str(&output).context("Failed to parse GitHub release response")?;

    Ok(release)
}

/// Use `curl` to perform HTTP requests. Returns stdout (response body).
fn run_curl(url: &str, method: &str) -> Result<String> {
    let mut cmd = std::process::Command::new("curl");
    cmd.arg("-s")
        .arg("-f") // fail on HTTP errors
        .arg("-L") // follow redirects
        .arg("-X")
        .arg(method)
        .arg("--connect-timeout")
        .arg("10")
        .arg("--max-time")
        .arg("30")
        .arg("-H")
        .arg("Accept: application/vnd.github+json")
        .arg("-H")
        .arg("User-Agent: takka-cli")
        .arg(url);

    // Optionally use GITHUB_TOKEN if available (higher rate limits).
    if let Ok(token) = env::var("GITHUB_TOKEN").or_else(|_| env::var("GH_TOKEN")) {
        cmd.arg("-H")
            .arg(format!("Authorization: Bearer {}", token));
    }

    let output = cmd.output().context("Failed to run curl")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("curl failed: {}", stderr.trim());
    }

    Ok(String::from_utf8(output.stdout)?)
}

/// Use `curl` to download a file to a temp path, with progress output.
fn download_to_temp(url: &str) -> Result<PathBuf> {
    let tmp_dir = std::env::temp_dir().join("agentic-update");
    fs::create_dir_all(&tmp_dir)?;
    let tmp_path = tmp_dir.join("agentic-new");

    let mut cmd = std::process::Command::new("curl");
    cmd.arg("-f")
        .arg("-L")
        .arg("--progress-bar")
        .arg("--connect-timeout")
        .arg("10")
        .arg("--max-time")
        .arg("300") // 5 min for large binaries
        .arg("-o")
        .arg(&tmp_path)
        .arg(url);

    if let Ok(token) = env::var("GITHUB_TOKEN").or_else(|_| env::var("GH_TOKEN")) {
        cmd.arg("-H")
            .arg(format!("Authorization: Bearer {}", token));
    }

    let status = cmd.status().context("Failed to run curl for download")?;

    if !status.success() {
        anyhow::bail!("Download failed");
    }

    // Verify the file exists and has content.
    let meta = fs::metadata(&tmp_path).context("Downloaded file not found")?;
    if meta.len() < 1000 {
        fs::remove_file(&tmp_path).ok();
        anyhow::bail!("Downloaded file is too small (likely an error page)");
    }

    Ok(tmp_path)
}

/// Replace the running binary with the downloaded one.
fn install_binary(tmp_path: &Path) -> Result<()> {
    let current_exe = env::current_exe().context("Cannot determine current executable path")?;

    // Make the new binary executable. Unix-only: Windows does not
    // model execute permissions, and `Permissions::from_mode` does not
    // exist there.
    #[cfg(unix)]
    fs::set_permissions(tmp_path, fs::Permissions::from_mode(0o755))
        .context("Failed to set executable permissions")?;

    // If we can write directly to the current exe path, do a simple rename.
    // On Linux this works even while the binary is running (inode-based).
    if let Err(e) = fs::rename(tmp_path, &current_exe) {
        tracing::debug!("Direct rename failed ({}), using copy+unlink fallback", e);

        // Fallback: copy to new path, then rename over original.
        let backup_path = current_exe.with_extension("bak");
        fs::copy(&current_exe, &backup_path).context("Failed to backup current binary")?;
        fs::copy(tmp_path, &current_exe).context("Failed to copy new binary")?;
        fs::remove_file(tmp_path).ok();
        fs::remove_file(&backup_path).ok();
    }

    Ok(())
}

// ── Version comparison ─────────────────────────────────────

/// Returns `true` if `remote_tag` (e.g. "v0.3.0") is newer than
/// `local_version` (e.g. "0.2.0").
fn is_newer(remote_tag: &str, local_version: &str) -> Result<bool> {
    let remote = parse_version(trim_v_prefix(remote_tag))?;
    let local = parse_version(local_version)?;

    Ok(remote > local)
}

/// Parse "1.2.3" into (1, 2, 3).
fn parse_version(v: &str) -> Result<(u32, u32, u32)> {
    let parts: Vec<&str> = v.split('.').collect();
    if parts.len() < 3 {
        anyhow::bail!("Invalid version format: {}", v);
    }
    Ok((
        parts[0].parse().context("Invalid major version")?,
        parts[1].parse().context("Invalid minor version")?,
        parts[2].parse().context("Invalid patch version")?,
    ))
}

fn trim_v_prefix(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
}

// ── Asset selection ────────────────────────────────────────

/// Pick the right asset for the current platform.
fn pick_asset(assets: &[GithubAsset]) -> Result<&GithubAsset> {
    let target = asset_target_name();

    // Exact match first.
    if let Some(a) = assets.iter().find(|a| a.name == target) {
        return Ok(a);
    }

    // Fuzzy match: look for a Linux x86_64 asset.
    let candidates: Vec<&GithubAsset> = assets
        .iter()
        .filter(|a| {
            a.name.contains("linux") && a.name.contains("x86_64") || a.name.contains("x86-64")
        })
        .collect();

    if let Some(a) = candidates.first() {
        return Ok(a);
    }

    // Last resort: first non-.sha256 asset.
    if let Some(a) = assets.iter().find(|a| !a.name.ends_with(".sha256")) {
        return Ok(a);
    }

    anyhow::bail!(
        "No compatible binary found for platform '{}'. Available: {}",
        target,
        assets
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Build expected asset name like `agentic-linux-x86_64`.
/// Includes the OS so assets are unambiguous across platforms
/// (e.g. `agentic-x86_64` alone would collide between Linux and
/// macOS x86_64 builds).
fn asset_target_name() -> String {
    format!("agentic-{}-{}", env::consts::OS, env::consts::ARCH)
}

// ── Display helpers ────────────────────────────────────────

fn print_update_available(info: &UpdateInfo) {
    let bold = RStyle::default().add_modifier(RModifier::BOLD);

    inline::print_line(&RLine::from(vec![
        RSpan::styled("  ", RStyle::default()),
        RSpan::styled("Current: ", RStyle::default().add_modifier(RModifier::DIM)),
        RSpan::styled(
            format!("v{}", CURRENT_VERSION),
            RStyle::default().fg(RColor::Yellow),
        ),
        RSpan::raw("  →  "),
        RSpan::styled("Latest: ", RStyle::default().add_modifier(RModifier::DIM)),
        RSpan::styled(format!("v{}", info.latest_version), bold.fg(RColor::Green)),
    ]));

    if !info.release_notes.is_empty() {
        inline::print_blank();
        // Show first 5 lines of release notes.
        let notes: Vec<&str> = info.release_notes.lines().take(5).collect();
        for line in notes {
            inline::print_line(&RLine::from(vec![
                RSpan::styled("    ", RStyle::default()),
                RSpan::styled(
                    line.to_string(),
                    RStyle::default()
                        .fg(RColor::Rgb(180, 180, 200))
                        .add_modifier(RModifier::DIM),
                ),
            ]));
        }
    }

    inline::print_blank();
}

// ── Tests ──────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_version() {
        assert_eq!(parse_version("0.2.0").unwrap(), (0, 2, 0));
        assert_eq!(parse_version("1.10.3").unwrap(), (1, 10, 3));
    }

    #[test]
    fn test_is_newer() {
        assert!(is_newer("v0.3.0", "0.2.0").unwrap());
        assert!(!is_newer("v0.2.0", "0.2.0").unwrap());
        assert!(!is_newer("v0.1.0", "0.2.0").unwrap());
        assert!(is_newer("v1.0.0", "0.9.9").unwrap());
    }

    #[test]
    fn test_trim_v_prefix() {
        assert_eq!(trim_v_prefix("v0.2.0"), "0.2.0");
        assert_eq!(trim_v_prefix("0.2.0"), "0.2.0");
    }

    #[test]
    fn test_asset_target_name() {
        let name = asset_target_name();
        assert!(name.starts_with("agentic-"));
        // Must be OS-qualified so platform assets never collide.
        let os = name.trim_start_matches("agentic-");
        assert!(os.contains('-'));
    }
}
