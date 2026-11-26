//! Release hint for the interactive banner.
//!
//! On REPL start we (1) show a one-line hint when a previous background check
//! found a newer release, and (2) if the last check is older than
//! [`CHECK_INTERVAL_SECS`], refresh it on a detached thread. The REPL never
//! waits on the network: the result of a refresh shows up on the *next*
//! start. Nothing is installed; the hint points at `takka update`.
//!
//! Opt out with `updates.check = false` in the config or by setting
//! `TAKKA_NO_UPDATE_CHECK` (any non-empty value other than `0`). Only the
//! interactive REPL calls this: `run`, the daemon and ACP stay silent and
//! make no request.

use crate::widgets::inline;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Minimum time between network checks.
pub const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;
/// Environment variable that disables the check.
pub const ENV_DISABLE: &str = "TAKKA_NO_UPDATE_CHECK";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Cache {
    /// Unix seconds of the last attempt (successful or not).
    checked_at: u64,
    /// Latest release tag seen, `None` when the attempt failed.
    latest: Option<String>,
}

fn cache_path() -> PathBuf {
    takka_core::config::config_dir().join("update-check.json")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_cache(path: &Path) -> Option<Cache> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn write_cache(path: &Path, cache: &Cache) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string(cache) {
        let _ = std::fs::write(path, json);
    }
}

/// Whether the check is allowed, given the config flag and the env value.
pub fn enabled(config_check: bool, env_value: Option<&str>) -> bool {
    let env_off = env_value.is_some_and(|v| !v.is_empty() && v != "0");
    config_check && !env_off
}

/// A refresh is due when there is no cache, it is unreadable, or the last
/// attempt is older than the interval. A clock that went backwards counts as
/// not due, so a bad clock cannot cause a request on every start.
fn refresh_due(cache: Option<&Cache>, now: u64) -> bool {
    match cache {
        None => true,
        Some(c) => now >= c.checked_at && now - c.checked_at >= CHECK_INTERVAL_SECS,
    }
}

/// The hint line for a cached release, or `None` when it is not newer.
fn hint(cache: Option<&Cache>, current: &str) -> Option<String> {
    let latest = cache?.latest.as_deref()?;
    match crate::update::is_newer(latest, current) {
        Ok(true) => Some(format!(
            "takka {} is available (you have v{current}). Run `takka update`.",
            if latest.starts_with('v') {
                latest.to_string()
            } else {
                format!("v{latest}")
            }
        )),
        _ => None,
    }
}

/// Used by `startup` and tests: hint from the cache at `path`, plus whether a
/// background refresh should be started.
fn plan(path: &Path, now: u64, current: &str) -> (Option<String>, bool) {
    let cache = read_cache(path);
    (
        hint(cache.as_ref(), current),
        refresh_due(cache.as_ref(), now),
    )
}

/// Call once at REPL start. Returns the hint to print (if any) and starts a
/// detached refresh when due. No-op when disabled.
pub fn startup(config_check: bool) -> Option<String> {
    let env = std::env::var(ENV_DISABLE).ok();
    if !enabled(config_check, env.as_deref()) {
        return None;
    }
    let path = cache_path();
    let (line, due) = plan(&path, now_secs(), crate::update::CURRENT_VERSION);
    if due {
        spawn_refresh(path);
    }
    line
}

/// Print the hint as one dim line (call right after the banner).
pub fn print_hint(line: &str) {
    inline::print_line(&Line::from(vec![Span::styled(
        format!("  {line}"),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::DIM),
    )]));
    inline::print_blank();
}

fn spawn_refresh(path: PathBuf) {
    let _ = std::thread::Builder::new()
        .name("takka-update-check".into())
        .spawn(move || {
            let latest = crate::update::latest_tag().ok();
            if latest.is_none() {
                tracing::debug!("update check failed; will retry after the interval");
            }
            write_cache(
                &path,
                &Cache {
                    checked_at: now_secs(),
                    latest,
                },
            );
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(checked_at: u64, latest: Option<&str>) -> Cache {
        Cache {
            checked_at,
            latest: latest.map(Into::into),
        }
    }

    #[test]
    fn enabled_honours_config_and_env() {
        assert!(enabled(true, None));
        assert!(enabled(true, Some("")));
        assert!(enabled(true, Some("0")));
        assert!(!enabled(true, Some("1")));
        assert!(!enabled(true, Some("yes")));
        assert!(!enabled(false, None));
    }

    #[test]
    fn refresh_due_rules() {
        assert!(refresh_due(None, 1_000));
        let c = cache(1_000, None);
        assert!(!refresh_due(Some(&c), 1_000 + CHECK_INTERVAL_SECS - 1));
        assert!(refresh_due(Some(&c), 1_000 + CHECK_INTERVAL_SECS));
        // Clock moved backwards: do not hammer the API.
        assert!(!refresh_due(Some(&c), 500));
    }

    #[test]
    fn hint_only_for_a_newer_release() {
        let newer = cache(0, Some("v9.9.9"));
        let line = hint(Some(&newer), "0.7.0").unwrap();
        assert!(
            line.contains("v9.9.9") && line.contains("takka update"),
            "{line}"
        );
        assert!(hint(Some(&cache(0, Some("v0.7.0"))), "0.7.0").is_none());
        assert!(hint(Some(&cache(0, Some("v0.1.0"))), "0.7.0").is_none());
        assert!(hint(Some(&cache(0, None)), "0.7.0").is_none());
        assert!(hint(None, "0.7.0").is_none());
        // Garbage tags never produce a hint.
        assert!(hint(Some(&cache(0, Some("nightly"))), "0.7.0").is_none());
        // A tag without the `v` prefix is normalised in the message.
        assert!(hint(Some(&cache(0, Some("9.9.9"))), "0.7.0")
            .unwrap()
            .contains("v9.9.9"));
    }

    #[test]
    fn cache_round_trips_and_bad_files_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/update-check.json");
        assert_eq!(read_cache(&path), None);
        write_cache(&path, &cache(42, Some("v1.2.3")));
        assert_eq!(read_cache(&path), Some(cache(42, Some("v1.2.3"))));
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(read_cache(&path), None);
    }

    #[test]
    fn plan_combines_hint_and_due() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        // No cache: nothing to show, refresh due.
        assert_eq!(plan(&path, 100, "0.7.0"), (None, true));
        // Fresh cache with a newer release: show it, no refresh yet.
        write_cache(&path, &cache(100, Some("v9.9.9")));
        let (line, due) = plan(&path, 200, "0.7.0");
        assert!(line.is_some());
        assert!(!due);
        // Stale cache: still show the (possibly outdated) hint and refresh.
        let (line, due) = plan(&path, 100 + CHECK_INTERVAL_SECS, "0.7.0");
        assert!(line.is_some());
        assert!(due);
    }
}
