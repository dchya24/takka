//! Terminal capability detection.
//!
//! Decides whether output should include ANSI styling based on:
//! - `NO_COLOR` environment variable (https://no-color.org/)
//! - `TERM=dumb`
//! - Whether stdout is a TTY (so piped output stays clean)
//! - An explicit override set via [`set_color_enabled`] (e.g. from `--color` flag)
//!
//! The decision is cached on first read (env vars don't change at runtime),
//! but the explicit override always wins when set.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicI8, Ordering};
use std::sync::OnceLock;

/// Override state for color decisions.
///   -1 = no override (auto-detect)
///    0 = forced off
///    1 = forced on
static COLOR_OVERRIDE: AtomicI8 = AtomicI8::new(-1);

/// Auto-detected color decision, computed once.
static AUTO_COLOR: OnceLock<bool> = OnceLock::new();

/// Auto-detected TTY decision, computed once.
static AUTO_TTY: OnceLock<bool> = OnceLock::new();

/// Force color on (`true`) or off (`false`). Pass `None` to fall back to
/// auto-detection. Typically called once during startup from the `--color`
/// CLI flag resolution.
pub fn set_color_enabled(enabled: Option<bool>) {
    let value = match enabled {
        None => -1i8,
        Some(false) => 0,
        Some(true) => 1,
    };
    COLOR_OVERRIDE.store(value, Ordering::Relaxed);
}

/// Should output use ANSI color/styling?
///
/// Order of precedence:
///   1. Explicit override from [`set_color_enabled`].
///   2. `NO_COLOR` env var (any non-empty value disables color).
///   3. `TERM=dumb` disables color.
///   4. Stdout TTY check (piped output gets no color).
pub fn should_use_color() -> bool {
    match COLOR_OVERRIDE.load(Ordering::Relaxed) {
        0 => return false,
        1 => return true,
        _ => {}
    }

    *AUTO_COLOR.get_or_init(|| {
        // NO_COLOR: any non-empty value disables color.
        if std::env::var_os("NO_COLOR")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            return false;
        }

        // TERM=dumb: classic signal of a non-styled terminal.
        if std::env::var("TERM").map(|t| t == "dumb").unwrap_or(false) {
            return false;
        }

        // Piped output: skip styling so logs/grep stay clean.
        is_stdout_tty()
    })
}

/// Is stdout a TTY? Cached after first call.
pub fn is_stdout_tty() -> bool {
    *AUTO_TTY.get_or_init(|| std::io::stdout().is_terminal())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_forces_decision() {
        set_color_enabled(Some(true));
        assert!(should_use_color());
        set_color_enabled(Some(false));
        assert!(!should_use_color());
        // Reset to auto for other tests.
        set_color_enabled(None);
    }
}
