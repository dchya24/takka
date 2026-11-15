//! Command surface of the interactive/`run` CLI: the `Commands` state
//! object, the REPL driver, streaming renderer, slash commands, config
//! wizard and the confirmation/question bridge.
//!
//! Layout (mechanical split, no behaviour change): this file holds the
//! entry surface — process-wide render/interaction statics, `QueueMode`,
//! `MidRunKey`, the `Commands` struct, its constructors/builders/getters,
//! and default route methods. The run loop lives in `repl`, terminal
//! output in `render`/`event`, slash commands in `slash`, the config
//! wizard in `config`, model selection in `model`, daemon attachment in
//! `daemon`, and the question/todo/confirmation bridge in `confirm`.
//! `e2e` is the end-to-end smoke suite (test builds only).

mod config;
mod confirm;
mod daemon;
mod event;
mod model;
mod render;
mod repl;
mod slash;

#[cfg(test)]
mod e2e;

pub(crate) use confirm::*;
pub(crate) use event::*;
pub(crate) use render::*;

use anyhow::Result;
use ratatui::style::{Color as RColor, Modifier as RModifier, Style as RStyle};
use ratatui::text::{Line as RLine, Span as RSpan};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use takka_core::sync;
use takka_core::Config;

use crate::widgets::{components, inline};

pub(crate) static ALWAYS_CONFIRM: AtomicBool = AtomicBool::new(false);

pub(crate) static COLOR_ENABLED: AtomicBool = AtomicBool::new(true);

/// True while `Commands::run`'s streaming renderer thread is alive (REPL
/// mode). The question handler only does the consume/re-arm spinner dance
/// when this is set; in TUI mode the progress indicator lives in the app
/// frame instead.
pub(crate) static SPINNER_ACTIVE: AtomicBool = AtomicBool::new(false);

/// True while a blocking prompt (tool confirmation, `question` dialog) owns
/// stdin.
///
/// The REPL reads keys continuously — even during a run, so text typed
/// mid-run can be queued — and in raw mode `event::read()` is unconditional:
/// whichever reader asks first gets the key. That race is what ate the `y`/`a`
/// keys of a confirmation prompt and turned them into "Invalid input". The REPL
/// polls this flag and leaves stdin alone while a prompt is up.
pub(crate) static INTERACTIVE_PROMPT_ACTIVE: AtomicBool = AtomicBool::new(false);

/// RAII guard for [`INTERACTIVE_PROMPT_ACTIVE`].
pub(crate) struct InteractivePromptGuard(());

impl InteractivePromptGuard {
    pub(crate) fn activate() -> Self {
        INTERACTIVE_PROMPT_ACTIVE.store(true, Ordering::SeqCst);
        Self(())
    }
}

impl Drop for InteractivePromptGuard {
    fn drop(&mut self) {
        INTERACTIVE_PROMPT_ACTIVE.store(false, Ordering::SeqCst);
    }
}

/// True while the full-screen TUI owns the terminal. The question (and
/// confirmation) handler checks this to leave / re-enter the alternate
/// screen around its prompt so dialoguer renders on a clean primary
/// screen instead of on top of a frozen TUI frame.
pub(crate) static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Set by the interactive handlers after re-entering the alternate
/// screen; the TUI main loop does a full repaint (`terminal::clear`)
/// before its next draw so dialoguer's artifacts are never left behind
/// (ratatui only diffs against its last frame).
pub(crate) static TUI_NEEDS_REDRAW: AtomicBool = AtomicBool::new(false);

/// Serializes writes to the terminal between the streaming renderer
/// thread (spawned by `Commands::run`) / the TUI main loop and the
/// `question` tool's interactive prompt.
///
/// While the question handler holds this gate the renderer parks instead
/// of touching the terminal, so its `MoveUp`/`Clear` spinner redraws
/// can't land on top of the dialoguer option list — and on Windows the
/// two writers racing over cursor control is what looks like a flickering
/// dim lamp.
pub(crate) static RENDER_GATE: Mutex<()> = Mutex::new(());

/// How user text typed while a run is active is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueueMode {
    /// Pesan selama run aktif diantrikan lokal; dikirim sebagai Run
    /// berikutnya.
    #[default]
    FollowUp,
    /// Pesan selama run aktif dikirim langsung sebagai steer ke engine.
    Interrupt,
}

impl QueueMode {
    /// CLI-facing name (matches the `--queue-mode` values).
    pub fn as_str(&self) -> &'static str {
        match self {
            QueueMode::FollowUp => "follow-up",
            QueueMode::Interrupt => "interrupt",
        }
    }

    /// One-line behavioral description used by `/queue` output.
    pub fn description(&self) -> &'static str {
        match self {
            QueueMode::FollowUp => "text typed during a run is queued and sent as the next run",
            QueueMode::Interrupt => {
                "text typed during a run is sent as a steer at the next tool boundary"
            }
        }
    }
}

/// One mid-run reader key event (story 2.1): characters + Enter drive
/// the steer path; Ctrl+C rides the story-1.4 two-stage cancel. The
/// cfg(test) injection feeds these instead of the keyboard.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum MidRunKey {
    Char(char),
    Enter,
    CtrlC,
}

/// Safety valve for the client-issued drain loop: each drained follow-up
/// goes through the full `run()` path, which drains again on completion. In
/// practice the queue is empty by the time nested drains run; the cap only
/// guards against a pathological submit-during-drained-run loop (possible
/// once input-during-run is wired into the TUI).
pub(crate) const MAX_FOLLOW_UP_DRAIN_DEPTH: usize = 8;

pub struct Commands {
    pub(crate) config: Config,
    // orchestrator removed (P1-1 DoD: no Orchestrator::new in CLI —
    // all agent runs go through the takka-runtime daemon).
    pub(crate) connection: Option<crate::client::AgentConnection>,
    pub(crate) runtime_config_path: Option<String>,
    pub(crate) runtime_mode: crate::client::RuntimeMode,
    /// Explicit daemon discovery file for `attach_daemon` (default:
    /// [`crate::client::daemon_info_path`]). Test seam for hermetic e2e
    /// smokes; production builds leave it `None`. Unix-only (daemon mode
    /// is not supported on Windows); `allow(dead_code)` keeps non-unix
    /// builds warning-clean under `-D warnings`.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) daemon_info_path_override: Option<std::path::PathBuf>,
    pub(crate) color_enabled: bool,
    pub(crate) debug_enabled: bool,
    pub(crate) permission_mode: takka_core::PermissionMode,
    /// Path to the `AGENT.md` discovered by walk-up at orchestrator init,
    /// or `None` when no project instructions were found.
    pub(crate) agent_md_path: Option<std::path::PathBuf>,
    /// `true` when at least one persistent memory file (user-global or
    /// project-local) was loaded and folded into the system prompt.
    pub(crate) memory_md_loaded: bool,
    /// Image attachments queued by the `/image <path>` slash command,
    /// to ride along with the next user turn. Drained by `run()` when
    /// the next message is sent.
    pub(crate) pending_attachments: Vec<takka_core::Attachment>,
    /// `true` when running in interactive REPL mode. Controls whether
    /// the `question` tool handler is registered (stdin-based prompts)
    /// vs returning skip-all fallback (non-interactive `takka run`).
    pub(crate) interactive_mode: bool,
    /// Shared skill index for the skill tool, populated at orchestrator init.
    pub(crate) skill_index: Option<std::sync::Arc<std::sync::RwLock<takka_core::SkillIndex>>>,
    /// Skill currently active for THIS session (per-instance state
    /// replacing the removed process-global, Fase D).
    pub(crate) active_skill: Option<String>,
    /// How text typed during an active run is handled: queued locally as a
    /// follow-up (default) or sent immediately as a steer (interrupt).
    pub(crate) queue_mode: QueueMode,
    /// Text submitted while a run was active in FollowUp mode. Drained
    /// after the run's terminal event: each item is sent as the next run
    /// (client-issued; see `drain_follow_ups`).
    pub(crate) pending_follow_ups:
        std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    /// Current nesting depth of `drain_follow_ups` (drained runs drain
    /// again on completion; capped at [`MAX_FOLLOW_UP_DRAIN_DEPTH`]).
    pub(crate) drain_depth: usize,
    /// Session override for `output.show_thoughts` (`--no-thoughts`,
    /// `/thoughts`). `None` means "use the config".
    pub(crate) show_thoughts_override: Option<bool>,
    /// Detail level for the inline tool timeline (`/timeline`).
    pub(crate) timeline_style: TimelineStyle,
    /// Figures from the most recent `run()` — the REPL's end-of-run summary
    /// reads these (elapsed time is measured by the caller, tokens are
    /// estimated from the character counts here).
    pub(crate) last_run: RunStats,
    /// Test-only taps (cfg(test)): e2e tests prove the client-issued drain
    /// contract (runs == queue items + 1; the drained run emits its own
    /// SessionStarted) by counting client-issued Runs and capturing the
    /// lifecycle events of drained runs — which travel through `run()`'s
    /// renderer path and are invisible to per-call callbacks.
    #[cfg(test)]
    pub(crate) test_runs: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    pub(crate) test_events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Test-only tap (cfg(test)): the plain text of the preview lines the
    /// last `run()` printed live, captured from the renderer thread's
    /// `RenderFinal` (its terminal output is not capturable in-process).
    #[cfg(test)]
    pub(crate) test_preview: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    #[cfg(test)]
    pub(crate) test_preview_diverged: std::sync::Arc<std::sync::Mutex<bool>>,
    /// Test tap (cfg(test)): the post-run flush path's last appended lines —
    /// the suffix under the preview, or today's full render on the fallback.
    #[cfg(test)]
    pub(crate) test_final_print: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Test tap (cfg(test)): the interleaved print order of the last run —
    /// `text:<line>` / `tool:<name>` / `thinking` markers from the renderer
    /// (story 1.3 e2e interleave assertion).
    #[cfg(test)]
    pub(crate) test_render_order: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Test sink (cfg(test), story 2.2): per-INSTANCE notice capture for
    /// the mid-run reader's queue/cancel notices — no globals, so
    /// parallel tests never capture each other's stderr writes.
    #[cfg(test)]
    pub(crate) test_mid_run_notice_sink: Option<std::sync::Arc<std::sync::Mutex<Vec<String>>>>,
    /// Test tap (cfg(test)): the mid-run key injection source for story
    /// 2.1 smokes — the reader takes this once instead of the keyboard.
    #[cfg(test)]
    test_mid_run_keys:
        std::sync::Arc<std::sync::Mutex<Option<std::sync::mpsc::Receiver<MidRunKey>>>>,
    /// Test tap (cfg(test)): the mid-run input echo frames (story 2.1).
    #[cfg(test)]
    pub(crate) test_input_echoes: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// cfg(test): per-instance prompt gate for the mid-run reader
    /// (story 2.3 smokes) — parallel e2e gates never collide.
    #[cfg(test)]
    pub(crate) test_prompt_gate: std::sync::Arc<AtomicBool>,
    /// Test tap (cfg(test)): which terminal badge each run's Err branch
    /// rendered (story 1.4: "Cancelled by user" must surface as the
    /// cancel notice; everything else keeps the red error badge).
    #[cfg(test)]
    pub(crate) test_notice: std::sync::Arc<std::sync::Mutex<Vec<RunOutcomeBadge>>>,
    /// Test tap (cfg(test)): how many times a run executed the terminal
    /// cursor-cleanup block — the "status line erased exactly once"
    /// guarantee asserted by the cancel smokes (story 1.4 review).
    #[cfg(test)]
    pub(crate) test_terminal_cleanup_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Test-only cancel flag (cfg(test), story 1.4): a private instance
    /// the cancel smokes flip, so parallel tests in one process never
    /// observe each other's cancel. Production always resolves the
    /// process-wide flag the signal task flips (`run_cancel_flag`).
    #[cfg(test)]
    pub(crate) test_cancel_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl Default for Commands {
    fn default() -> Self {
        Self::new(Config::fallback())
    }
}

impl Commands {
    /// Create Commands without initializing the orchestrator (for config/status/examples)
    pub fn new(config: Config) -> Self {
        Self {
            config,

            connection: None,
            runtime_config_path: None,
            runtime_mode: crate::client::RuntimeMode::Embedded,
            daemon_info_path_override: None,
            color_enabled: true,
            debug_enabled: false,
            permission_mode: takka_core::PermissionMode::Default,
            agent_md_path: None,
            memory_md_loaded: false,
            pending_attachments: Vec::new(),
            interactive_mode: false,
            skill_index: None,
            active_skill: None,
            queue_mode: QueueMode::default(),
            pending_follow_ups: Default::default(),
            drain_depth: 0,
            show_thoughts_override: None,
            timeline_style: TimelineStyle::Detailed,
            last_run: RunStats::default(),
            #[cfg(test)]
            test_runs: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            test_events: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            #[cfg(test)]
            test_preview: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            #[cfg(test)]
            test_preview_diverged: std::sync::Arc::new(std::sync::Mutex::new(false)),
            #[cfg(test)]
            test_final_print: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            #[cfg(test)]
            test_render_order: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            #[cfg(test)]
            test_notice: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            #[cfg(test)]
            test_mid_run_keys: std::sync::Arc::new(std::sync::Mutex::new(None)),
            #[cfg(test)]
            test_input_echoes: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            #[cfg(test)]
            test_prompt_gate: std::sync::Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            test_mid_run_notice_sink: None,
            #[cfg(test)]
            test_terminal_cleanup_count: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(
                0,
            )),
            #[cfg(test)]
            test_cancel_flag: None,
        }
    }

    pub fn with_runtime_config_path(mut self, path: Option<String>) -> Self {
        self.runtime_config_path = path;
        self
    }

    pub fn with_runtime_mode(mut self, mode: crate::client::RuntimeMode) -> Self {
        self.runtime_mode = mode;
        self
    }

    /// Point `attach_daemon` at an explicit discovery file (test seam for
    /// hermetic daemon smokes; unix-only — the non-unix build never reads
    /// the field, hence the field-level `allow(dead_code)` above).
    #[cfg(test)]
    pub(crate) fn with_daemon_info_path(mut self, path: std::path::PathBuf) -> Self {
        self.daemon_info_path_override = Some(path);
        self
    }

    /// Install a private cancel flag for the hermetic cancel smokes
    /// (story 1.4): runs then watch THIS flag instead of the process
    /// global, so flipping it mid-run cannot cancel a concurrently
    /// running test's run (tests share one process; the process flag is
    /// process-global).
    /// Per-instance prompt gate for the mid-run reader (cfg(test),
    /// story 2.3): the smoke flips this flag instead of the global.
    #[cfg(test)]
    pub(crate) fn with_mid_run_prompt_gate(mut self, gate: std::sync::Arc<AtomicBool>) -> Self {
        self.test_prompt_gate = gate;
        self
    }

    /// Per-instance notice-capture sink for the mid-run reader's queue
    /// and cancel notices (cfg(test), story 2.2).
    #[cfg(test)]
    pub(crate) fn with_mid_run_notice_sink(
        mut self,
        sink: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> Self {
        self.test_mid_run_notice_sink = Some(sink);
        self
    }

    /// Inject the story-2.1 mid-run key source (cfg(test) only): the
    /// reader thread's keyboard-feed replacement for deterministic smokes.
    #[cfg(test)]
    pub(crate) fn with_mid_run_keys(self, source: std::sync::mpsc::Receiver<MidRunKey>) -> Self {
        *self.test_mid_run_keys.lock().unwrap() = Some(source);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_cancel_flag(
        mut self,
        flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        self.test_cancel_flag = Some(flag);
        self
    }

    /// The cancel flag handle the active run watches (story 1.4): the
    /// process-wide flag the signal task flips, or the test-injected
    /// instance under cfg(test).
    pub(crate) fn run_cancel_flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        #[cfg(test)]
        {
            if let Some(flag) = &self.test_cancel_flag {
                return flag.clone();
            }
        }
        crate::cancel_flag()
    }

    pub fn with_color(mut self, enabled: bool) -> Self {
        self.color_enabled = enabled;
        COLOR_ENABLED.store(enabled, Ordering::Relaxed);
        self
    }

    /// Get a reference to the config
    pub(crate) fn get_config(&self) -> &Config {
        &self.config
    }

    pub fn with_interactive_mode(mut self, enabled: bool) -> Self {
        self.interactive_mode = enabled;
        self
    }

    pub fn with_debug(mut self, enabled: bool) -> Self {
        self.debug_enabled = enabled;
        self
    }

    /// Attach the input watcher's shared state so the spinner ticker
    /// can render the live input buffer below the progress line.
    pub fn with_permission_mode(mut self, mode: takka_core::PermissionMode) -> Self {
        self.permission_mode = mode;
        self
    }

    /// Select how text typed during an active run is handled.
    pub fn with_queue_mode(mut self, mode: QueueMode) -> Self {
        self.queue_mode = mode;
        self
    }

    /// One-time session metadata init: filesystem-derived state that
    /// was previously set inside the removed `ensure_orchestrator`.
    pub(crate) fn init_session_metadata(&mut self) {
        if self.agent_md_path.is_some() || self.memory_md_loaded {
            return; // already initialized
        }
        let cwd = std::env::current_dir().unwrap_or_default();
        self.agent_md_path = takka_core::load_project_instructions(&cwd).map(|(path, _)| path);
        self.memory_md_loaded = takka_core::assemble_memory_section(&cwd).is_some();
    }

    // ── Status ──────────────────────────────────────────────

    pub fn status(&self) -> Result<()> {
        let config_path = Config::config_path();
        let config_exists = config_path.exists();

        println!();
        print_info("Takka CLI Status");
        println!("  Config file: {}", config_path.display());
        if config_exists {
            print_success("  Config file: exists");
        } else {
            print_warning("  Config file: not found");
        }

        if self.config.providers.is_empty() {
            print_warning("  No providers configured");
        } else {
            for (i, p) in self.config.providers.iter().enumerate() {
                println!();
                print_info(&format!("Provider #{}: {}", i + 1, p.name));
                println!("    Type:     {}", p.provider_type);
                println!("    API Base: {}", p.api_base);
                if p.api_key.is_empty() {
                    print_warning("    API Key:  ✗ not set");
                } else {
                    print_success("    API Key:  ✓ configured");
                }
                if !p.models.is_empty() {
                    println!("    Models:");
                    for m in &p.models {
                        let display = m.display_name.as_deref().unwrap_or(&m.model);
                        println!("      • {} ({})", display, m.model);
                    }
                }
            }
        }
        println!();
        Ok(())
    }

    // ── Inline config display (for REPL /config) ──────────

    pub fn config_show_inline(&self) {
        inline::print_blank();
        let key_w = 10;
        if let Some(p) = self.config.active_provider() {
            inline::print_line(&components::kv_line(
                "Provider",
                &format!("{} ({})", p.name, p.provider_type),
                key_w,
                RColor::Reset,
            ));
            inline::print_line(&components::kv_line(
                "API Base",
                &p.api_base,
                key_w,
                RColor::Reset,
            ));
            if p.api_key.is_empty() {
                inline::print_line(&components::kv_line(
                    "API Key",
                    "not set",
                    key_w,
                    RColor::Red,
                ));
            } else {
                let masked = format!(
                    "{}...{}",
                    &p.api_key[..4.min(p.api_key.len())],
                    &p.api_key[p.api_key.len().saturating_sub(4)..]
                );
                inline::print_line(&components::kv_line(
                    "API Key",
                    &masked,
                    key_w,
                    RColor::Green,
                ));
            }
            if let Some(m) = p.models.first() {
                inline::print_line(&components::kv_line(
                    "Model",
                    &format!(
                        "{} (temp: {}, max_tokens: {})",
                        m.model, m.temperature, m.max_tokens
                    ),
                    key_w,
                    RColor::Reset,
                ));
            }
        } else {
            inline::print_line(&components::warning_badge("No provider configured."));
        }
        inline::print_line(&components::kv_line(
            "Config",
            &Config::config_path().display().to_string(),
            key_w,
            RColor::Reset,
        ));
        inline::print_blank();
    }

    // ── List tools (for REPL /tools) ────────────────────────

    /// Path to the `AGENT.md` that was loaded into the system prompt for
    /// this session, or `None` when no project instructions were found.
    /// Set during the first `ensure_orchestrator()` call.
    pub fn agent_md_path(&self) -> Option<&std::path::Path> {
        self.agent_md_path.as_deref()
    }

    /// `true` when at least one persistent memory file (user-global or
    /// project-local) was loaded into the system prompt.
    pub fn memory_md_loaded(&self) -> bool {
        self.memory_md_loaded
    }

    /// Override `output.show_thoughts` for this session (`--no-thoughts`).
    pub fn with_show_thoughts(mut self, value: Option<bool>) -> Self {
        self.show_thoughts_override = value;
        self
    }

    /// Whether inline runs print the model's pre-tool reasoning.
    pub fn show_thoughts(&self) -> bool {
        self.show_thoughts_override
            .unwrap_or(self.config.output.show_thoughts)
    }

    /// Toggle reasoning output from the REPL (`/thoughts`).
    pub fn set_show_thoughts(&mut self, enabled: bool) {
        self.show_thoughts_override = Some(enabled);
    }

    /// Detail level of the inline tool timeline.
    pub fn timeline_style(&self) -> TimelineStyle {
        self.timeline_style
    }

    /// Choose the timeline detail level (`/timeline`).
    pub fn set_timeline_style(&mut self, style: TimelineStyle) {
        self.timeline_style = style;
    }

    /// Figures from the most recent [`Commands::run`]: elapsed time is the
    /// caller's, tokens are estimated from the character counts recorded here.
    pub fn last_run_stats(&self) -> RunStats {
        self.last_run
    }

    /// Resolve the active model's capabilities (vision / tools /
    /// streaming). Reads the per-model override first, then falls back
    /// to the built-in lookup.
    pub fn active_model_capabilities(&self) -> takka_core::ModelCapabilities {
        if let Some(model) = self.config.active_model() {
            return model.effective_capabilities();
        }
        takka_core::ModelCapabilities::default()
    }

    /// Drain the queue of `/image`-attached payloads. The queue is
    /// per-command-run — once attached to a turn, the buffer empties.
    pub fn drain_pending_attachments(&mut self) -> Vec<takka_core::Attachment> {
        std::mem::take(&mut self.pending_attachments)
    }

    /// Number of images queued for the next turn (for status display).
    #[allow(dead_code)]
    pub fn pending_attachment_count(&self) -> usize {
        self.pending_attachment_count_inner()
    }

    pub(crate) fn pending_attachment_count_inner(&self) -> usize {
        self.pending_attachments.len()
    }

    /// Queue an already-loaded attachment for the next turn.
    /// Used by the TUI `/image` handler that loads the file itself.
    pub fn queue_attachment(&mut self, att: takka_core::Attachment) {
        self.pending_attachments.push(att);
    }

    /// Read-only reference to the current configuration.
    /// Used by TUI commands that need provider/model lists.
    pub fn config_ref(&self) -> &Config {
        &self.config
    }

    /// Render the `/image <path>` slash command: load the file, validate
    /// it (size cap + MIME), and queue it for the next user turn. The
    /// next call to `run()` will attach all queued images to the
    /// outgoing message.
    pub fn attach_image_inline(&mut self, path: &str) {
        let path = path.trim();
        if path.is_empty() {
            inline::print_blank();
            inline::print_line(&components::warning_badge("Usage: /image <path>"));
            inline::print_blank();
            return;
        }

        // Pre-flight capability check so the user sees the failure now,
        // not on the next turn.
        let caps = self.active_model_capabilities();
        if !caps.vision {
            inline::print_blank();
            inline::print_line(&components::error_badge(
                "Active model does not support image input.",
            ));
            inline::print_line(&RLine::from(vec![
                RSpan::raw("  Switch with "),
                RSpan::styled("/models", RStyle::default().add_modifier(RModifier::BOLD)),
                RSpan::raw(" to a vision-capable model first."),
            ]));
            inline::print_blank();
            return;
        }

        let limits = takka_core::AttachmentLimits::default();
        let result = if path.starts_with("http://")
            || path.starts_with("https://")
            || path.starts_with("data:")
        {
            takka_core::attachments::load_image_from_url(path, limits)
        } else {
            takka_core::attachments::load_image_from_path(path, limits)
        };

        match result {
            Ok(att) => {
                let bytes = att.size_bytes;
                let mime = att.mime_type.clone();
                let source = format!("{}", att.source);
                self.pending_attachments.push(att);
                inline::print_blank();
                inline::print_line(&components::success_badge(&format!(
                    "Attached: {} ({} bytes, {})",
                    source,
                    bytes,
                    if mime.is_empty() {
                        "remote"
                    } else {
                        mime.as_str()
                    }
                )));
                inline::print_line(&RLine::from(vec![
                    RSpan::raw("  "),
                    RSpan::styled(
                        format!(
                            "{} image(s) queued for next turn.",
                            self.pending_attachments.len()
                        ),
                        RStyle::default().add_modifier(RModifier::DIM),
                    ),
                ]));
                inline::print_blank();
            }
            Err(e) => {
                inline::print_blank();
                inline::print_line(&components::error_badge(&format!(
                    "Failed to attach: {}",
                    e
                )));
                inline::print_blank();
            }
        }
    }

    /// Restart the agent session in-place: drops the conversation memory,
    /// clears any pending cancel flag, drops accumulated event handlers,
    /// and resets cumulative cost. The provider, tool registry, system
    /// prompt, AGENT.md and persistent memory section all stay loaded —
    /// the user does NOT pay the re-init cost (or another wizard prompt).
    ///
    /// No-op when the orchestrator hasn't been initialized yet.
    pub async fn restart_session(&mut self) -> Result<()> {
        self.ensure_connection().await?;
        let runtime = self.connection.as_mut().unwrap();
        let event = runtime
            .send_and_wait(takka_core::runtime::protocol::Request::ResetSession)
            .await?;
        match event.event {
            takka_core::Event::SessionReset => Ok(()),
            takka_core::Event::Error { message } => Err(anyhow::anyhow!(message)),
            other => Err(anyhow::anyhow!("unexpected reset response: {other:?}")),
        }
    }

    // ── Follow-up queue (QueueMode) ───────────────────────

    /// Active queue mode (how text typed during a run is handled).
    pub fn queue_mode_getter(&self) -> QueueMode {
        self.queue_mode
    }

    /// Switch the queue mode at runtime (the `/queue` slash command).
    pub fn set_queue_mode(&mut self, mode: QueueMode) {
        self.queue_mode = mode;
    }

    /// Text queued in FollowUp mode awaiting the next drain.
    pub fn pending_follow_ups(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::VecDeque<String>> {
        sync::lock(&self.pending_follow_ups)
    }

    /// Snapshot of the pending follow-ups (story 2.4 tests): avoids a
    /// live guard crossing an async boundary.
    pub fn pending_follow_ups_snapshot(&self) -> Vec<String> {
        sync::lock(&self.pending_follow_ups)
            .iter()
            .cloned()
            .collect()
    }

    /// Programmatic entrypoint for user text typed while a run may be
    /// active (unit-tested now; the TUI wires it up in a later phase —
    /// inline REPL input-during-run is out of scope).
    ///
    /// Returns `Ok(true)` when the text was accepted (steered or queued).
    ///
    /// - Interrupt mode: fire-and-forget steer to the engine (injected at
    ///   the next tool boundary; requires an active run, else the engine
    ///   answers `Error { "no active run" }` on the event stream).
    /// - FollowUp mode: pushed onto the local queue; sent as the next run
    ///   once the current one reaches its terminal event.
    pub(crate) async fn submit_user_text(&mut self, text: String) -> Result<bool> {
        match self.queue_mode {
            QueueMode::Interrupt => {
                let conn = self
                    .connection
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("not connected"))?;
                conn.steer(text).await?;
                Ok(true)
            }
            QueueMode::FollowUp => {
                sync::lock(&self.pending_follow_ups).push_back(text);
                Ok(true)
            }
        }
    }

    /// After a run's terminal event: drain the follow-up queue — send each
    /// queued item as the next run (client-issued, loop, depth-guarded).
    ///
    /// Drained items go through the full `run()` path, which drains again
    /// on completion; `drain_depth` keeps that nesting bounded (see
    /// [`MAX_FOLLOW_UP_DRAIN_DEPTH`]). Leftover items past the cap stay
    /// queued for the next drain.
    ///
    /// Returns a boxed future: `run → drain → run` is recursive at the
    /// type level, and async fns refuse unboxed recursion (the runtime
    /// bound is the depth guard, not the type).
    pub(crate) fn drain_follow_ups(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move {
            if self.drain_depth >= MAX_FOLLOW_UP_DRAIN_DEPTH {
                return Ok(());
            }
            self.drain_depth += 1;
            let result = self.drain_follow_ups_loop().await;
            self.drain_depth -= 1;
            result
        })
    }

    pub(crate) async fn drain_follow_ups_loop(&mut self) -> Result<()> {
        loop {
            let next = sync::lock(&self.pending_follow_ups).pop_front();
            let Some(next) = next else { break };
            self.run(&next).await?;
        }
        Ok(())
    }
}

// ── QueueMode / follow-up queue unit tests ──────────────

#[cfg(test)]
mod queue_mode_tests {
    use super::*;

    #[test]
    fn queue_mode_default_is_follow_up() {
        assert_eq!(QueueMode::default(), QueueMode::FollowUp);
    }

    #[test]
    fn queue_mode_labels_match_cli_flag_values() {
        // `/queue <mode>` and `--queue-mode <MODE>` must agree on names.
        assert_eq!(QueueMode::FollowUp.as_str(), "follow-up");
        assert_eq!(QueueMode::Interrupt.as_str(), "interrupt");
    }

    #[test]
    fn set_queue_mode_switches_mode() {
        let mut commands = Commands::new(Config::fallback());
        assert_eq!(commands.queue_mode_getter(), QueueMode::FollowUp);
        commands.set_queue_mode(QueueMode::Interrupt);
        assert_eq!(commands.queue_mode_getter(), QueueMode::Interrupt);
        commands.set_queue_mode(QueueMode::FollowUp);
        assert_eq!(commands.queue_mode_getter(), QueueMode::FollowUp);
    }

    #[tokio::test]
    async fn submit_user_text_follow_up_queues_without_connection() {
        // FollowUp is a purely local queue: no engine connection needed.
        let mut commands = Commands::new(Config::fallback());
        commands
            .submit_user_text("queued".to_string())
            .await
            .expect("follow-up submit should always succeed");
        let queued: Vec<_> = commands.pending_follow_ups().iter().cloned().collect();
        assert_eq!(queued, vec!["queued".to_string()]);
    }

    #[tokio::test]
    async fn submit_user_text_follow_up_preserves_fifo_order() {
        let mut commands = Commands::new(Config::fallback());
        commands
            .submit_user_text("first".to_string())
            .await
            .unwrap();
        commands
            .submit_user_text("second".to_string())
            .await
            .unwrap();
        let queued: Vec<_> = commands.pending_follow_ups().iter().cloned().collect();
        assert_eq!(queued, vec!["first".to_string(), "second".to_string()]);
    }

    #[tokio::test]
    async fn submit_user_text_interrupt_requires_a_connection() {
        // Interrupt submits a steer to the engine — impossible unconnected.
        let mut commands = Commands::new(Config::fallback()).with_queue_mode(QueueMode::Interrupt);
        let err = commands
            .submit_user_text("hello".to_string())
            .await
            .expect_err("interrupt submit without connection must fail");
        assert!(err.to_string().contains("not connected"));
        // …and nothing was silently queued either.
        assert!(commands.pending_follow_ups().is_empty());
    }

    #[tokio::test]
    async fn drain_follow_ups_with_empty_queue_is_a_noop() {
        let mut commands = Commands::new(Config::fallback());
        commands
            .drain_follow_ups()
            .await
            .expect("empty drain should be a noop");
        assert_eq!(commands.test_runs.load(Ordering::Relaxed), 0);
    }
}
