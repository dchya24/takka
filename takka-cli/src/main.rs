#![allow(dead_code)] // widget/TUI library grows ahead of usage; keep for active development

#[cfg(test)]
mod tests;

mod cli;
mod client;
mod commands;
mod confirmation;
mod error;
mod file_ref;
mod input_buffer;
mod input_renderer;
mod interactive;
mod keyboard;
mod session;
mod tui;
mod update;
mod widgets;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, ColorChoice, Command, ConfigAction};
use commands::Commands;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use takka_core::Config;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Shared cancel flag. The signal handler flips this on the first Ctrl+C;
/// the orchestrator's loop boundary picks it up and returns gracefully.
/// A second Ctrl+C escalates to a hard exit.
static CANCEL_FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();

pub(crate) fn cancel_flag() -> Arc<AtomicBool> {
    CANCEL_FLAG
        .get_or_init(|| Arc::new(AtomicBool::new(false)))
        .clone()
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // ── Logging setup ─────────────────────────────────────
    // Two layers with independent filters:
    //   console  → respects RUST_LOG / -v / --debug
    //   file     → always TRACE for our crates, regardless of console
    //              verbosity, so every LLM call + loop iteration is
    //              captured for post-hoc debugging.
    let console_level = match cli.verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let console_filter = if cli.debug {
        EnvFilter::new("debug")
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(console_level))
    };

    // Resolve the file-log destination.
    //   --log-file <PATH>   → use exactly that path
    //   --no-log-file       → disable
    //   debug build, else   → default to ./logs/takka-<ts>.log
    //   release build       → disabled unless --log-file given
    let log_file_path: Option<std::path::PathBuf> = if cli.no_log_file {
        None
    } else if let Some(p) = &cli.log_file {
        Some(std::path::PathBuf::from(p))
    } else if cfg!(debug_assertions) {
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
        Some(std::path::PathBuf::from(format!("logs/takka-{}.log", ts)))
    } else {
        None
    };

    // WorkerGuard keeps the background writer thread alive until main
    // returns; dropping it flushes. Must live as long as we log.
    let mut _log_guard: Option<tracing_appender::non_blocking::WorkerGuard> = None;

    let console_layer = fmt::layer()
        .with_target(cli.debug)
        .with_filter(console_filter);

    let file_layer = match &log_file_path {
        Some(path) => {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    let _ = std::fs::create_dir_all(parent);
                }
            }
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| {
                    anyhow::anyhow!("failed to open log file {}: {}", path.display(), e)
                })?;
            let (writer, guard) = tracing_appender::non_blocking(file);
            _log_guard = Some(guard);

            // File captures our crates at TRACE plus dep warnings,
            // tunable via the RUST_LOG_FILE env var.
            let file_filter = EnvFilter::try_from_env("RUST_LOG_FILE").unwrap_or_else(|_| {
                EnvFilter::new("takka_core=trace,takka_cli=trace,takka=trace,warn")
            });

            eprintln!("📝 takka log → {}", path.display());

            Some(
                fmt::layer()
                    .with_writer(writer)
                    .with_ansi(false)
                    .with_target(true)
                    .with_filter(file_filter),
            )
        }
        None => None,
    };

    tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
        .init();

    if cli.verbose > 0 || cli.debug {
        tracing::info!(
            "takka v{} — verbose:{}, debug:{}, color:{:?}",
            env!("CARGO_PKG_VERSION"),
            cli.verbose,
            cli.debug,
            cli.color,
        );
    }

    // ── Color resolution ───────────────────────────────────
    // Auto leaves the decision to capabilities::should_use_color() which
    // also honors NO_COLOR, TERM=dumb, and the TTY check.
    let color_override = match cli.color {
        ColorChoice::Always => Some(true),
        ColorChoice::Never => Some(false),
        ColorChoice::Auto => None,
    };
    widgets::capabilities::set_color_enabled(color_override);
    let color_enabled = widgets::capabilities::should_use_color();

    // ── Ctrl+C graceful shutdown (background task) ─────────
    // First Ctrl+C: cooperative cancel. The agent loop checks this flag at
    // every turn boundary and returns AgenticError::Cancelled.
    // Second Ctrl+C: force-exit (we may be stuck in a tool that doesn't
    // observe the flag).
    let cancel = cancel_flag();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        if cancel.swap(true, Ordering::SeqCst) {
            // Already cancelled once — hard exit on second Ctrl+C.
            eprintln!("\n⚠ Force-exiting on second Ctrl+C.");
            std::process::exit(130);
        }
        eprintln!("\n⚠ Cancel requested (press Ctrl+C again to force-exit).");
        // Wait for a possible second Ctrl+C.
        tokio::signal::ctrl_c().await.ok();
        eprintln!("\n⚠ Force-exiting on second Ctrl+C.");
        std::process::exit(130);
    });

    // ── Config commands (don't need loaded config) ─────────
    if let Some(Command::Config(action)) = &cli.command {
        if matches!(action, ConfigAction::Path) {
            println!("{}", Config::config_path().display());
            return Ok(());
        }

        if matches!(
            action,
            ConfigAction::Init { .. } | ConfigAction::Reset { .. } | ConfigAction::Migrate { .. }
        ) {
            let fallback_config = Config::fallback();
            let commands = Commands::new(fallback_config)
                .with_color(color_enabled)
                .with_debug(cli.debug);
            return commands.config(action).map_err(|e| anyhow::anyhow!(e));
        }

        if matches!(action, ConfigAction::Validate { .. }) && !Config::config_exists() {
            eprintln!("✗ Config file not found. Run 'takka config init' to create one.");
            std::process::exit(1);
        }
    }

    // ── Load config ────────────────────────────────────────
    let config = if let Some(config_path) = &cli.config {
        Config::load_from_path(config_path)
            .ok_or_else(|| anyhow::anyhow!("Failed to load config from: {}", config_path))?
    } else if let Some(loaded) = Config::load() {
        loaded
    } else {
        match &cli.command {
            Some(Command::Run { .. }) | Some(Command::Interactive) | Some(Command::Tui) | None => {
                use ratatui::style::{Color as RColor, Modifier, Style as RStyle};
                use ratatui::text::{Line, Span as RSpan};
                use widgets::{components, inline};

                inline::print_line(&components::warning_badge(&format!(
                    "Config file not found. Creating default at: {}",
                    Config::config_path().display()
                )));
                inline::print_blank();

                let new_config = Config::fallback();
                if let Err(e) = new_config.save() {
                    inline::print_line(&components::error_badge(&format!(
                        "Failed to create config: {}",
                        e
                    )));
                    std::process::exit(1);
                }

                inline::print_line(&components::success_badge("Default config created."));
                inline::print_line(&components::warning_badge(
                    "No providers or models configured yet.",
                ));
                inline::print_blank();
                inline::print_line(&Line::from(RSpan::raw("  Set up a provider with:")));
                let bold = RStyle::default().add_modifier(Modifier::BOLD);
                let dim = RStyle::default().fg(RColor::DarkGray);
                inline::print_line(&Line::from(vec![
                    RSpan::raw("    "),
                    RSpan::styled("takka config init --interactive", bold),
                    RSpan::styled("   # Guided wizard", dim),
                ]));
                inline::print_line(&Line::from(vec![
                    RSpan::raw("    "),
                    RSpan::styled("takka config init --provider zai", bold),
                    RSpan::styled("  # Quick setup", dim),
                ]));
                inline::print_line(&Line::from(vec![
                    RSpan::raw("    "),
                    RSpan::styled("takka config edit", bold),
                    RSpan::styled("                  # Edit manually", dim),
                ]));
                inline::print_blank();

                new_config
            }
            _ => Config::fallback(),
        }
    };

    let mut commands = Commands::new(config)
        .with_runtime_config_path(cli.config.clone())
        .with_color(color_enabled)
        .with_debug(cli.debug)
        .with_permission_mode(cli.mode.into())
        .with_queue_mode(cli.queue_mode.into())
        .with_runtime_mode(cli.runtime.into());

    // ── Command dispatch ───────────────────────────────────
    match &cli.command {
        Some(Command::Run { task, plan }) => {
            if *plan {
                commands.plan_run(task).await?;
            } else {
                commands.run(task).await?;
            }
        }
        Some(Command::Interactive) => {
            commands = commands.with_interactive_mode(true);
            interactive::run(commands).await?;
        }
        Some(Command::Attach { shutdown }) => {
            if *shutdown {
                commands.attach_daemon(true).await?;
            } else {
                // Attach reuses the regular REPL over the daemon
                // connection (attach_daemon establishes it up front so
                // the REPL's ensure_connection never spawns anything).
                commands = commands.with_interactive_mode(true);
                commands.attach_daemon(false).await?;
                interactive::run(commands).await?;
            }
        }
        Some(Command::Tui) => {
            commands = commands.with_interactive_mode(true);
            tui::run_tui(std::sync::Arc::new(tokio::sync::Mutex::new(commands))).await?;
        }
        Some(Command::Config(action)) => {
            commands.config(action)?;
        }
        Some(Command::Status) => {
            commands.status()?;
        }
        Some(Command::Examples) => {
            commands.examples();
        }
        Some(Command::Skill(action)) => {
            commands.skill_command(action)?;
        }
        Some(Command::Update { check }) => {
            if *check {
                update::check_and_print()?;
            } else {
                update::run_update()?;
            }
        }
        Some(Command::Version) => {
            println!("takka {}", env!("CARGO_PKG_VERSION"));
        }
        None => {
            commands = commands.with_interactive_mode(true);
            interactive::run(commands).await?;
        }
    }

    Ok(())
}
