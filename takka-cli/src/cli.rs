use clap::{Parser, Subcommand};

/// AI agent orchestration command-line interface
#[derive(Parser)]
#[command(
    name = "takka",
    version,
    about = "AI agent orchestration CLI — run tasks, manage config, chat interactively",
    long_about = "takka — agent yang tak berhenti bekerja\n\
        \n\
        AI agent orchestration CLI\n\
        \n\
        Run coding tasks through an AI agent with access to shell tools.\n\
        Supports multiple LLM providers (OpenAI, Anthropic, Z.ai),\n\
        streaming output, interactive mode, and MCP servers.\n\
        \n\
        Quick start:\n\
        \n  \
          takka init                           # Interactive setup wizard\n  \
          takka run \"list files\"               # Run a single task\n  \
          takka interactive                    # Start REPL\n\
        "
)]
pub struct Cli {
    /// Set a custom config file path
    #[arg(short, long, global = true, value_name = "PATH")]
    pub config: Option<String>,

    /// Verbose output (repeat for more: -v, -vv, -vvv)
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Enable debug output (shows tool calls, raw API requests)
    #[arg(long, global = true)]
    pub debug: bool,

    /// Control colored output
    #[arg(long, global = true, value_name = "WHEN", default_value = "auto")]
    pub color: ColorChoice,

    /// Permission mode for tool execution
    ///
    /// - default: ask for medium+ risk actions, allow reads (recommended)
    /// - plan:    read-only mode; deny all writes and commands
    /// - yolo:    auto-approve everything except hard-blocked patterns
    #[arg(long, global = true, value_name = "MODE", default_value = "default")]
    pub mode: PermissionModeArg,

    /// How user text typed while a run is active is handled
    ///
    /// - follow-up: locally queued; sent as the next run after the
    ///   current one finishes (default)
    /// - interrupt: sent to the engine as a steer; injected at the next
    ///   tool boundary of the running task
    #[arg(long, global = true, value_name = "MODE", default_value = "follow-up")]
    pub queue_mode: QueueModeArg,

    /// Write all tracing logs (incl. every LLM request/response and agent
    /// loop iteration) to a file. In debug builds this defaults to
    /// `./logs/takka-<timestamp>.log`; in release it is off unless given.
    #[arg(long, global = true, value_name = "PATH")]
    pub log_file: Option<String>,

    /// Disable automatic file logging (overrides the debug-build default).
    #[arg(long, global = true)]
    pub no_log_file: bool,

    /// Runtime connection mode
    ///
    /// - embedded: run the agent engine in-process (default; no separate
    ///   takka-runtime binary needed)
    /// - spawn:    spawn the takka-runtime daemon subprocess (JSONL over
    ///   stdio; honors TAKKA_RUNTIME_BIN)
    /// - daemon:   attach to a running `takka-runtime --daemon` broker
    ///   over its unix socket; auto-spawns one when none is running
    #[arg(long, global = true, value_name = "MODE", default_value = "embedded")]
    pub runtime: RuntimeModeArg,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// CLI representation of [`takka_core::PermissionMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum PermissionModeArg {
    Default,
    Plan,
    Yolo,
}

impl From<PermissionModeArg> for takka_core::PermissionMode {
    fn from(arg: PermissionModeArg) -> Self {
        match arg {
            PermissionModeArg::Default => takka_core::PermissionMode::Default,
            PermissionModeArg::Plan => takka_core::PermissionMode::Plan,
            PermissionModeArg::Yolo => takka_core::PermissionMode::Yolo,
        }
    }
}

/// CLI representation of [`crate::commands::QueueMode`] (how user text
/// typed while a run is active is handled).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum QueueModeArg {
    /// Locally queue the text; send it as the next run (default)
    #[default]
    FollowUp,
    /// Send it to the engine as a steer at the next tool boundary
    Interrupt,
}

impl From<QueueModeArg> for crate::commands::QueueMode {
    fn from(arg: QueueModeArg) -> Self {
        match arg {
            QueueModeArg::FollowUp => crate::commands::QueueMode::FollowUp,
            QueueModeArg::Interrupt => crate::commands::QueueMode::Interrupt,
        }
    }
}

/// Which runtime connection mode to use
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum RuntimeModeArg {
    /// Run the agent engine in-process (default; single binary)
    #[default]
    Embedded,
    /// Spawn the takka-runtime daemon subprocess
    Spawn,
    /// Run the agent through a long-lived daemon broker (unix socket;
    /// auto-spawns one when none is running)
    Daemon,
}

impl From<RuntimeModeArg> for crate::client::RuntimeMode {
    fn from(arg: RuntimeModeArg) -> Self {
        match arg {
            RuntimeModeArg::Embedded => crate::client::RuntimeMode::Embedded,
            RuntimeModeArg::Spawn => crate::client::RuntimeMode::Spawn,
            RuntimeModeArg::Daemon => crate::client::RuntimeMode::Daemon,
        }
    }
}

/// When to use colors
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorChoice {
    /// Auto-detect based on terminal
    Auto,
    /// Always use colors
    Always,
    /// Never use colors
    Never,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run a single task through the AI agent
    #[command(long_about = "Run a single task through the AI agent.\n\
        \n\
        The agent will analyze the task, select appropriate tools,\n\
        and execute the plan. Streaming output is shown in real-time.\n\
        \n\
        Use --plan to decompose the task into steps, show the plan,\n\
        and execute approved steps via the planner agent.\n\
        \n\
        Examples:\n  \
          takka run \"list all Rust files\"\n  \
          takka run --plan \"refactor the auth module\"\n  \
          takka run --mode plan \"explain the codebase\"")]
    Run {
        /// The task description
        task: String,

        /// Plan-then-execute: decompose the task into steps, show the plan,
        /// ask for approval, and execute the approved steps.
        ///
        /// Shorthand for `takka run --mode plan`, but explicitly invokes
        /// the planner agent rather than just denying writes.
        #[arg(long)]
        plan: bool,
    },

    /// Start interactive mode (REPL)
    #[command(
        alias = "i",
        long_about = "Start an interactive REPL session.\n\
        \n\
        Enter a persistent chat session where you can send multiple\n\
        messages, switch providers, manage config, and more.\n\
        \n\
        Slash commands: /help, /config, /provider, /model, /clear, /save, /tools, /quit"
    )]
    Interactive,

    /// Start TUI mode (full-screen interactive)
    #[command(long_about = "Start a full-screen TUI session.\n\
        \n\
        Features:\n\
        - Rich markdown rendering\n\
        - Animated progress indicators\n\
        - Dropdown for / commands\n\
        - Dropdown for @ file completion\n\
        - Scrollable message history\n\
        \n\
        Keybindings: /help, Ctrl+C to cancel, Ctrl+D to quit")]
    Tui,

    /// Attach to a running takka daemon
    #[command(long_about = "Attach this CLI to an already-running takka daemon\n\
        (takka-runtime --daemon) over its unix socket.\n\
        \n\
        Without --shutdown this drops into the regular interactive REPL,\n\
        routed through the daemon's engine — the daemon keeps the session\n\
        alive across attach/detach cycles, so a detached run survives and\n\
        reattaching adopts it.\n\
        \n\
        Examples:\n  \
          takka attach                # Attach to the running daemon\n  \
          takka attach --shutdown     # Ask the daemon to shut down")]
    Attach {
        /// Ask the daemon to shut down instead of attaching
        #[arg(long)]
        shutdown: bool,
    },

    /// Show current status (provider, model, connection)
    #[command(long_about = "Show current agent status.\n\
        \n\
        Displays active provider, model, config file location,\n\
        and whether the config is valid.")]
    Status,

    /// Show usage examples
    #[command(long_about = "Show usage examples for common tasks.\n\
        \n\
        Displays a curated list of example commands to help\n\
        you get started with the CLI.")]
    Examples,

    /// Configuration management
    #[command(subcommand)]
    Config(ConfigAction),

    /// Skill management
    #[command(subcommand)]
    Skill(SkillAction),

    /// Check for updates and install if available
    #[command(long_about = "Check GitHub for a newer release and install it.\n\
        \n\
        Uses the GitHub Releases API to find the latest version.\n\
        When a newer version is found, downloads the binary and\n\
        replaces the running executable in-place.\n\
        \n\
        Use --check to only check without installing.\n\
        \n\
        Examples:\n  \
          takka update              # Check and install\n  \
          takka update --check      # Check only")]
    Update {
        /// Only check for updates, don't install
        #[arg(long)]
        check: bool,
    },

    /// Show version information
    #[command(alias = "v")]
    Version,
}

#[derive(Subcommand)]
pub enum SkillAction {
    /// List all indexed skills
    #[command(long_about = "List all indexed skills with name, description, and source path.")]
    List,

    /// Show skill details
    #[command(long_about = "Show detailed information about a specific skill.")]
    Info {
        /// Name of the skill to inspect
        name: String,
    },

    /// Create a new skill
    #[command(long_about = "Scaffold a new skill directory and SKILL.md template.")]
    Create {
        /// Name for the new skill (lowercase a-z, 0-9, hyphens, 1-64 chars)
        name: String,
        /// Create in user-global directory (~/.config/agentic/skills/) instead of project-local
        #[arg(long)]
        global: bool,
    },
}

#[derive(Subcommand)]
pub enum ConfigAction {
    /// Initialize a new configuration file
    #[command(long_about = "Create a new configuration file.\n\
        \n\
        Without flags, creates a default OpenAI-compatible config.\n\
        Use --interactive for a guided setup wizard.\n\
        Use --provider for quick setup with a specific provider.\n\
        \n\
        Examples:\n  \
          takka config init                    # Default config\n  \
          takka config init --interactive      # Guided wizard\n  \
          takka config init --provider openai  # Quick OpenAI setup")]
    Init {
        /// Run interactive setup wizard
        #[arg(long)]
        interactive: bool,

        /// Quick setup for a specific provider (openai, anthropic, zai)
        #[arg(long, value_name = "NAME")]
        provider: Option<String>,
    },

    /// Show current configuration
    #[command(long_about = "Display the current configuration.\n\
        \n\
        Supports multiple output formats for scripting and inspection.")]
    Show {
        /// Output format
        #[arg(long, value_name = "FORMAT", default_value = "json")]
        format: OutputFormat,
    },

    /// Edit configuration in default editor ($EDITOR)
    Edit,

    /// Validate configuration file
    #[command(long_about = "Check the configuration for errors and warnings.\n\
        \n\
        Use --verbose for detailed information about each check.")]
    Validate {
        /// Show detailed validation output
        #[arg(long)]
        verbose: bool,
    },

    /// Create a timestamped backup of the config file
    #[command(long_about = "Create a backup of the current config file.\n\
        \n\
        Backups are stored in ~/.config/takka/backups/ with timestamps.")]
    Backup,

    /// Restore configuration from a backup file
    #[command(long_about = "Restore config from a backup file.\n\
        \n\
        Provide the path to a backup file to restore from.")]
    Restore {
        /// Path to backup file to restore
        file: String,
    },

    /// Export configuration (secrets masked)
    #[command(long_about = "Export configuration in a shareable format.\n\
        \n\
        API keys and other secrets are masked for safe sharing.")]
    Export,

    /// Import configuration from a file
    #[command(long_about = "Import a configuration from a JSON file.\n\
        \n\
        The file should follow the takka config schema.")]
    Import {
        /// Path to config file to import
        file: String,
    },

    /// Get configuration file path
    Path,

    /// Migrate legacy agentic user data (~/.config/agentic) to takka (~/.config/takka)
    #[command(
        long_about = "Move legacy `agentic` user data to the new takka location.\n\
        \n\
        Moves config.json, sessions/, and memory.md from\
         ~/.config/agentic/ to ~/.config/takka/, after an interactive\
         y/N confirmation.\n\
        \n\
        Refuses to run when ~/.config/takka/config.json already exists\
         (use --force to overwrite). Safe to run twice: with nothing left\
         at the legacy path it reports there is nothing to migrate.\n\
        \n\
        Examples:\n  \
          takka config migrate              # Confirm, then move\n  \
          takka config migrate --yes        # Skip the confirmation\n  \
          takka config migrate --force      # Overwrite an existing new config"
    )]
    Migrate {
        /// Skip the interactive y/N confirmation prompt
        #[arg(long)]
        yes: bool,

        /// Proceed even if ~/.config/takka/config.json already exists
        #[arg(long)]
        force: bool,
    },

    /// Reset configuration to defaults
    #[command(long_about = "Reset the configuration file to defaults.\n\
        \n\
        This will overwrite your current config. Use --force to skip confirmation.")]
    Reset {
        /// Skip confirmation prompt
        #[arg(long)]
        force: bool,
    },
}

/// Output format for config show
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    Json,
    Table,
}
