mod daemon;
mod stdio_transport;

use takka_core::runtime::engine::RuntimeEngine;
use stdio_transport::StdioTransport;
use tracing_subscriber::EnvFilter;

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|argument| argument == "--daemon") {
        run_daemon(&args);
    }

    // Legacy path: one run over stdin/stdout JSON Lines (unchanged).
    let mut engine = RuntimeEngine::new(StdioTransport::new());
    engine.run();
}

/// `--daemon [--socket-path P] [--info-path P]` — hand-parsed (the
/// runtime binary deliberately has no clap dependency). Diverges on
/// every path, hence `!`.
#[cfg(unix)]
fn run_daemon(args: &[String]) -> ! {
    let config = parse_daemon_flags(args).unwrap_or_else(|message| {
        eprintln!("{message}");
        std::process::exit(2);
    });
    match daemon::serve(config) {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            eprintln!("daemon error: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(unix))]
fn run_daemon(args: &[String]) -> ! {
    let _ = args;
    eprintln!("{}", daemon::not_supported_error());
    std::process::exit(1);
}

/// Default socket lives next to the discovery file so both disappear
/// together; the pid suffix keeps parallel daemons from colliding.
#[cfg(unix)]
fn daemon_config_dir() -> std::path::PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => std::path::PathBuf::from(home)
            .join(".config")
            .join("agentic"),
        None => std::env::temp_dir().join("agentic-daemon"),
    }
}

#[cfg(unix)]
fn parse_daemon_flags(args: &[String]) -> Result<daemon::DaemonConfig, String> {
    let mut socket_path =
        daemon_config_dir().join(format!("agentic-daemon-{}.sock", std::process::id()));
    let mut info_path = daemon_config_dir().join("daemon.json");

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--daemon" => {}
            "--socket-path" => {
                index += 1;
                socket_path = flag_value(args, index, "--socket-path")?;
            }
            "--info-path" => {
                index += 1;
                info_path = flag_value(args, index, "--info-path")?;
            }
            other => return Err(format!("unknown daemon flag: {other}")),
        }
        index += 1;
    }
    Ok(daemon::DaemonConfig {
        socket_path,
        info_path,
    })
}

#[cfg(unix)]
fn flag_value(args: &[String], index: usize, flag: &str) -> Result<std::path::PathBuf, String> {
    args.get(index)
        .map(std::path::PathBuf::from)
        .ok_or_else(|| format!("missing value for {flag}"))
}
