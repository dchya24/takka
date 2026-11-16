use super::*;

/// `/timeline` display: current detail level and what it means.
pub(crate) fn show_timeline_status(commands: &Commands) {
    let style = commands.timeline_style();
    inline::print_blank();
    inline::print_line(&Line::from(vec![
        RSpan::raw("Tool timeline: "),
        RSpan::styled(
            style.as_str(),
            RStyle::default()
                .fg(Color::Rgb(64, 224, 208))
                .add_modifier(Modifier::BOLD),
        ),
        RSpan::styled(format!(" — {}", style.description()), RStyle::default()),
    ]));
    inline::print_line(&Line::from(RSpan::styled(
        "  /timeline compact  one row per tool   ·   /timeline detailed  rows + nested detail",
        RStyle::default().add_modifier(Modifier::DIM),
    )));
    inline::print_blank();
}

/// `/queue` display: active mode + pending follow-up queue (index +
/// first 80 characters of each entry). Only reachable while the REPL is
/// idle; input-during-run wiring comes with the TUI phase.
pub(crate) fn show_queue_status(commands: &Commands) {
    let mode = commands.queue_mode_getter();
    inline::print_blank();
    inline::print_line(&Line::from(vec![
        RSpan::raw("Queue mode: "),
        RSpan::styled(
            mode.as_str(),
            RStyle::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        RSpan::raw(format!(" — {}", mode.description())),
    ]));

    let pending = commands.pending_follow_ups();
    if pending.is_empty() {
        inline::print_line(&Line::from(RSpan::styled(
            "Pending follow-ups: (none)",
            RStyle::default().add_modifier(Modifier::DIM),
        )));
    } else {
        inline::print_line(&Line::from(format!(
            "Pending follow-ups: {}",
            pending.len()
        )));
        for (i, text) in pending.iter().enumerate() {
            let preview: String = text.chars().take(80).collect();
            let ellipsis = if text.chars().count() > 80 { "…" } else { "" };
            inline::print_line(&Line::from(vec![
                RSpan::styled(
                    format!("  {}. ", i + 1),
                    RStyle::default().fg(Color::DarkGray),
                ),
                RSpan::raw(format!("{preview}{ellipsis}")),
            ]));
        }
    }
    inline::print_blank();
}

// ── Model info ──────────────────────────────────────────────

pub(crate) struct ModelInfo {
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) api_base: String,
    pub(crate) agent_md_name: Option<String>,
    pub(crate) memory_md_loaded: bool,
    pub(crate) vision_capable: bool,
    pub(crate) active_skill: Option<String>,
    /// Session skill list (per-instance, Fase D) for the banner.
    pub(crate) available_skills: Vec<(String, String)>,
    /// `agent.max_cost_usd` for this session, if configured (H-06).
    pub(crate) max_cost_usd: Option<f64>,
}

pub(crate) fn get_model_info(commands: &Commands) -> ModelInfo {
    let (provider, model, api_base) = commands.model_info();
    let agent_md_name = commands
        .agent_md_path()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string());
    ModelInfo {
        provider,
        model,
        api_base,
        agent_md_name,
        memory_md_loaded: commands.memory_md_loaded(),
        vision_capable: commands.active_model_capabilities().vision,
        active_skill: commands.active_skill_name(),
        available_skills: commands.available_skills(),
        max_cost_usd: commands.max_cost_usd(),
    }
}

// ── Print helpers ───────────────────────────────────────────

pub(crate) fn print_banner(model_info: &ModelInfo, stats: &SessionStats) {
    let cwd = std::env::current_dir()
        .unwrap_or_default()
        .display()
        .to_string();

    inline::print_blank();

    // TAKKA wordmark, drawn with block glyphs. Each row gets its own
    // left-to-right gradient so the block reads as a diagonal sweep.
    pub(crate) const LOGO_ROWS: [&str; 3] = [
        "  █████  ▄▀▀▄  █  █  █  █  ▄▀▀▄",
        "    █    █▄▄█  ███   ███   █▄▄█",
        "    █    █  █  █  █  █  █  █  █",
    ];
    let logo_from = Color::Rgb(255, 105, 180);
    let logo_to = Color::Rgb(64, 224, 208);
    for row in LOGO_ROWS.iter() {
        let title = components::banner_title(row, logo_from, logo_to);
        inline::print_line(&title);
    }
    inline::print_blank();

    let info_lines = vec![
        Line::from(vec![
            RSpan::styled("📂 ", RStyle::default()),
            RSpan::styled("cwd  ", RStyle::default().add_modifier(Modifier::DIM)),
            RSpan::styled(cwd, RStyle::default().fg(Color::Rgb(180, 180, 200))),
        ]),
        Line::from(vec![
            RSpan::styled("⚡ ", RStyle::default()),
            RSpan::styled("model", RStyle::default().add_modifier(Modifier::DIM)),
            RSpan::raw("  "),
            RSpan::styled(
                model_info.provider.clone(),
                RStyle::default()
                    .fg(Color::Rgb(64, 224, 208))
                    .add_modifier(Modifier::BOLD),
            ),
            RSpan::styled(" / ", RStyle::default().add_modifier(Modifier::DIM)),
            RSpan::styled(
                model_info.model.clone(),
                RStyle::default()
                    .fg(Color::Rgb(255, 215, 0))
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            RSpan::styled("💡 ", RStyle::default()),
            RSpan::styled("tip  ", RStyle::default().add_modifier(Modifier::DIM)),
            RSpan::styled("type ", RStyle::default()),
            RSpan::styled(
                "/help",
                RStyle::default()
                    .fg(Color::Rgb(255, 215, 0))
                    .add_modifier(Modifier::BOLD),
            ),
            RSpan::styled(" for commands, ", RStyle::default()),
            RSpan::styled(
                "@",
                RStyle::default()
                    .fg(Color::Rgb(135, 206, 250))
                    .add_modifier(Modifier::BOLD),
            ),
            RSpan::styled(" to reference files", RStyle::default()),
        ]),
    ];

    let mut info_lines = info_lines;
    if model_info.agent_md_name.is_some()
        || model_info.memory_md_loaded
        || model_info.active_skill.is_some()
    {
        let mut spans: Vec<RSpan<'static>> = vec![
            RSpan::styled("🔗 ", RStyle::default()),
            RSpan::styled("ctx  ", RStyle::default().add_modifier(Modifier::DIM)),
        ];
        let mut first = true;
        if let Some(ref name) = model_info.agent_md_name {
            spans.push(RSpan::styled(
                format!("📄 {}", name),
                RStyle::default()
                    .fg(Color::Rgb(176, 196, 222))
                    .add_modifier(Modifier::BOLD),
            ));
            first = false;
        }
        if model_info.memory_md_loaded {
            if !first {
                spans.push(RSpan::styled(
                    "  ·  ",
                    RStyle::default().add_modifier(Modifier::DIM),
                ));
            }
            spans.push(RSpan::styled(
                "🧠 memory.md",
                RStyle::default()
                    .fg(Color::Rgb(176, 196, 222))
                    .add_modifier(Modifier::BOLD),
            ));
            first = false;
        }
        if let Some(ref skill) = model_info.active_skill {
            if !first {
                spans.push(RSpan::styled(
                    "  ·  ",
                    RStyle::default().add_modifier(Modifier::DIM),
                ));
            }
            spans.push(RSpan::styled(
                format!("⚡ skill:{}", skill),
                RStyle::default()
                    .fg(Color::Rgb(241, 196, 15))
                    .add_modifier(Modifier::BOLD),
            ));
        }
        info_lines.push(Line::from(spans));
    }

    let skills = &model_info.available_skills;
    if !skills.is_empty() {
        pub(crate) const MAX_SKILL_NAMES: usize = 5;
        let mut skill_names: Vec<String> = skills
            .iter()
            .take(MAX_SKILL_NAMES)
            .map(|(name, _)| name.clone())
            .collect();
        let remaining = skills.len().saturating_sub(MAX_SKILL_NAMES);
        if remaining > 0 {
            skill_names.push(format!("+{} more", remaining));
        }
        let skill_str = skill_names.join(" · ");
        info_lines.push(Line::from(vec![
            RSpan::styled("📦 ", RStyle::default()),
            RSpan::styled("skills", RStyle::default().add_modifier(Modifier::DIM)),
            RSpan::raw(" "),
            RSpan::styled(skill_str, RStyle::default().fg(Color::Rgb(241, 196, 15))),
        ]));
    }

    let panel_lines = components::panel(
        "Welcome",
        &info_lines,
        components::BoxStyle::Rounded,
        Color::Rgb(100, 100, 140),
    );
    inline::print_lines(&panel_lines);
    inline::print_blank();

    print_status_bar(model_info, stats);
}

pub(crate) fn print_turn_separator(model_info: &ModelInfo) {
    let git_branch = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let b = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if b.is_empty() || b == "HEAD" {
                    None
                } else {
                    Some(b)
                }
            } else {
                None
            }
        });

    let mut spans = vec![
        RSpan::raw("  "),
        RSpan::styled(
            format!("⚡ {}/{}", model_info.provider, model_info.model),
            RStyle::default().fg(Color::Rgb(255, 215, 0)),
        ),
    ];

    if let Some(ref branch) = git_branch {
        spans.push(RSpan::styled(
            format!(" 📌 {}", branch),
            RStyle::default()
                .fg(Color::Rgb(135, 206, 250))
                .add_modifier(Modifier::DIM),
        ));
    }

    inline::print_line(&Line::from(spans));
}

pub(crate) fn print_status_bar(model_info: &ModelInfo, stats: &SessionStats) {
    let in_tok = stats.format_tokens(stats.total_input_tokens());
    let out_tok = stats.format_tokens(stats.total_output_tokens());

    let sep = RSpan::styled("  │  ", RStyle::default().fg(Color::Rgb(60, 60, 80)));

    inline::print_line(&Line::from(vec![
        RSpan::raw("  "),
        RSpan::styled(
            format!("⚡ {}", model_info.provider),
            RStyle::default().fg(Color::Rgb(255, 215, 0)),
        ),
        RSpan::styled(
            format!("/{}", model_info.model),
            RStyle::default()
                .fg(Color::Rgb(241, 196, 15))
                .add_modifier(Modifier::DIM),
        ),
        if model_info.vision_capable {
            RSpan::styled("  👁", RStyle::default().fg(Color::Rgb(135, 206, 250)))
        } else {
            RSpan::raw("")
        },
        sep.clone(),
        RSpan::styled(
            format!("📊 {} ↑ / {} ↓", in_tok, out_tok),
            RStyle::default().fg(Color::Rgb(186, 85, 211)),
        ),
    ]));
    print_budget_chip(model_info, stats);

    let has_agent_md = model_info.agent_md_name.is_some();
    let has_skill = model_info.active_skill.is_some();
    if has_agent_md || model_info.memory_md_loaded || has_skill {
        let mut chips: Vec<RSpan<'static>> = vec![RSpan::raw("  ")];
        if let Some(ref name) = model_info.agent_md_name {
            chips.push(RSpan::styled(
                format!("📄 {}", name),
                RStyle::default().fg(Color::Rgb(176, 196, 222)),
            ));
        }
        if model_info.memory_md_loaded {
            if has_agent_md {
                chips.push(RSpan::styled(
                    "  ·  ",
                    RStyle::default().fg(Color::Rgb(60, 60, 80)),
                ));
            }
            chips.push(RSpan::styled(
                "🧠 memory.md",
                RStyle::default().fg(Color::Rgb(176, 196, 222)),
            ));
        }
        if let Some(ref skill) = model_info.active_skill {
            if has_agent_md || model_info.memory_md_loaded {
                chips.push(RSpan::styled(
                    "  ·  ",
                    RStyle::default().fg(Color::Rgb(60, 60, 80)),
                ));
            }
            chips.push(RSpan::styled(
                format!("⚡ skill:{}", skill),
                RStyle::default()
                    .fg(Color::Rgb(241, 196, 15))
                    .add_modifier(Modifier::BOLD),
            ));
        }
        inline::print_line(&Line::from(chips));
    }

    inline::print_line(&components::dashed_separator(Color::Rgb(60, 60, 80)));
    inline::print_blank();
}

pub(crate) fn print_prompt_status_bar(model_info: &ModelInfo, stats: &SessionStats) {
    let in_tok = stats.format_tokens(stats.total_input_tokens());
    let out_tok = stats.format_tokens(stats.total_output_tokens());
    let total_tok = stats.total_input_tokens() + stats.total_output_tokens();
    let total_formatted = stats.format_tokens(total_tok);

    let sep = RSpan::styled(" \u{2502} ", RStyle::default().fg(Color::Rgb(60, 60, 80)));

    let cwd = std::env::current_dir()
        .unwrap_or_default()
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?")
        .to_string();

    let mut spans: Vec<RSpan<'static>> = vec![
        RSpan::raw("  "),
        RSpan::styled(
            format!("\u{1f4c2} {}", cwd),
            RStyle::default().fg(Color::Rgb(180, 180, 200)),
        ),
    ];

    spans.push(sep.clone());
    spans.push(RSpan::styled(
        format!(
            "\u{1f4ca} \u{2191}{} \u{2193}{} ({})",
            in_tok, out_tok, total_formatted
        ),
        RStyle::default().fg(Color::Rgb(186, 85, 211)),
    ));

    // Add cache hit ratio if available
    let cache_ratio = stats.cache_hit_ratio();
    if cache_ratio > 0.0 {
        spans.push(sep.clone());
        spans.push(RSpan::styled(
            format!("\u{1f4e6} {:.0}%", cache_ratio * 100.0),
            RStyle::default().fg(Color::Rgb(46, 204, 113)),
        ));
    }

    // H-06: remaining budget for this run. Hidden when no ceiling is set.
    if let Some(chip) = budget_chip(model_info.max_cost_usd, stats.run_cost_usd()) {
        spans.push(sep.clone());
        spans.push(RSpan::styled(chip, budget_chip_style()));
    }

    inline::print_line(&Line::from(spans));
}

/// One budget chip span handler for both status bars.
fn budget_chip_style() -> RStyle {
    RStyle::default().fg(Color::Rgb(241, 196, 15))
}

/// `agent.max_cost_usd` chip for the banner status bar: a second line under
/// the model/token row, so the banner keeps its width when no ceiling is
/// configured.
fn print_budget_chip(model_info: &ModelInfo, stats: &SessionStats) {
    if let Some(chip) = budget_chip(model_info.max_cost_usd, stats.run_cost_usd()) {
        inline::print_line(&Line::from(vec![
            RSpan::raw("  "),
            RSpan::styled(chip, budget_chip_style()),
        ]));
    }
}

pub(crate) fn print_response_summary(stats: &SessionStats) {
    let in_tok = stats.format_tokens(stats.total_input_tokens());
    let out_tok = stats.format_tokens(stats.total_output_tokens());

    let sep = RSpan::styled(" │ ", RStyle::default().fg(Color::Rgb(60, 60, 80)));

    // The per-run summary above already carries the "done" outcome; this line
    // is the running session total (tokens are estimates, ~4 chars/token).
    inline::print_line(&Line::from(vec![
        RSpan::raw("  "),
        RSpan::styled("📊 session", RStyle::default().add_modifier(Modifier::DIM)),
        sep.clone(),
        RSpan::styled(
            format!("↑{} ↓{}", in_tok, out_tok),
            RStyle::default().fg(Color::Rgb(186, 85, 211)),
        ),
    ]));
    inline::print_blank();
}

pub(crate) fn print_help() {
    let help_md = r#"## 📖 Commands

**Slash commands:**
- `/help`              Show this help
- `/new`               Start a new session (clears conversation)
- `/config`            Show current configuration
- `/history`           Show conversation history
- `/tools`             List available tools
- `/stats`             Show session statistics
- `/thoughts`          Toggle inline model reasoning (thinking blocks)
- `/timeline`          Show/set tool timeline detail (`detailed` | `compact`)
- `/mcp`               Show MCP server status
- `/skills`            List all indexed skills
- `/skills <name>`     Load and display a skill
- `/skill`             Open skill selection dropdown
- `/skill <name>`      Load and activate a skill
- `/sessions`          List previous sessions
- `/sessions <id>`     Resume a previous session
- `/plan <goal>`       Create a plan for a goal
- `/search <query>`    Search conversation memory (case-insensitive)
- `/image <path>`      Attach image for next turn (path | data: | http(s) URL)
- `/queue`             Show queue mode + pending follow-ups
- `/queue <mode>`      Switch queue mode: follow-up (default) | interrupt
- `/provider <name>`   Switch provider (not yet supported)
- `/models`            Pick model interactively
- `/models <name>`     Switch to model by name (supports auto-complete)
- `/quit`              Exit interactive mode

**Shortcuts:**
- `help`, `h`          Show help
- `new`, `n`           New session
- `exit`, `q`          Exit

**Completion & Hints:**
- `/` → Dropdown with command list + descriptions
- `@` → Dropdown with file list + icons
- Tab/Enter → Accept dropdown selection
- ↑/↓ → Navigate dropdown or history
- Esc → Close dropdown

**Tips:**
- Type any text to send as a task to the AI agent
- Use `/skill <name>` or `/skills <name>` to load a skill before starting a task
- Ctrl+C to cancel, Ctrl+D to exit
"#;

    inline::print_blank();
    inline::print_line(&components::section_header(
        "📖",
        "Help",
        Color::Rgb(64, 224, 208),
    ));
    inline::print_blank();

    let md = crate::widgets::markdown::MarkdownContent::parse(help_md);
    inline::print_lines(&md.lines);
    inline::print_blank();
}

pub(crate) fn show_stats(stats: &SessionStats, model_info: &ModelInfo) {
    let cwd = std::env::current_dir()
        .unwrap_or_default()
        .display()
        .to_string();

    let in_tok = stats.total_input_tokens();
    let out_tok = stats.total_output_tokens();
    let total_tok = in_tok + out_tok;

    inline::print_blank();
    inline::print_line(&components::section_header(
        "📊",
        "Session Statistics",
        Color::Rgb(64, 224, 208),
    ));
    inline::print_blank();

    inline::print_line(&components::subsection_header(
        "Session",
        Color::Rgb(255, 215, 0),
    ));
    inline::print_line(&components::kv_line(
        "Duration",
        &stats.elapsed_str(),
        12,
        Color::Rgb(46, 204, 113),
    ));
    inline::print_line(&components::kv_line(
        "Messages",
        &format!("{}", stats.messages_sent()),
        12,
        Color::Rgb(255, 215, 0),
    ));
    inline::print_line(&components::kv_line(
        "Tool calls",
        &format!("{}", stats.tool_calls()),
        12,
        Color::Rgb(135, 206, 250),
    ));
    inline::print_blank();

    inline::print_line(&components::subsection_header(
        "Model",
        Color::Rgb(255, 215, 0),
    ));
    inline::print_line(&components::kv_badge(
        "Provider",
        &model_info.provider,
        12,
        Color::Rgb(255, 255, 255),
        Color::Rgb(155, 89, 182),
    ));
    inline::print_line(&components::kv_badge(
        "Model",
        &model_info.model,
        12,
        Color::Rgb(255, 255, 255),
        Color::Rgb(52, 152, 219),
    ));
    inline::print_line(&components::kv_line(
        "API Base",
        &model_info.api_base,
        12,
        Color::Rgb(180, 180, 200),
    ));
    inline::print_blank();

    inline::print_line(&components::subsection_header(
        "Token Usage",
        Color::Rgb(255, 215, 0),
    ));

    if total_tok > 0 {
        let in_ratio = in_tok as f32 / total_tok as f32;
        let out_ratio = out_tok as f32 / total_tok as f32;
        inline::print_line(&components::labeled_bar(
            "Input",
            in_ratio,
            30,
            Color::Rgb(46, 204, 113),
            Color::Rgb(50, 50, 60),
        ));
        inline::print_line(&components::labeled_bar(
            "Output",
            out_ratio,
            30,
            Color::Rgb(231, 76, 60),
            Color::Rgb(50, 50, 60),
        ));
    } else {
        inline::print_line(&Line::from(vec![
            RSpan::styled(
                "  Input:        ",
                RStyle::default().add_modifier(Modifier::DIM),
            ),
            RSpan::styled(
                "— no data yet —",
                RStyle::default().add_modifier(Modifier::DIM),
            ),
        ]));
    }
    inline::print_line(&Line::from(vec![
        RSpan::styled(
            "  Total:        ",
            RStyle::default().add_modifier(Modifier::DIM),
        ),
        RSpan::styled(
            format!("{} tokens", stats.format_tokens(total_tok)),
            RStyle::default()
                .fg(Color::Rgb(255, 215, 0))
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    inline::print_blank();

    let cache_read = stats.total_cache_read_tokens();
    let cache_created = stats.total_cache_creation_tokens();
    if cache_read > 0 || cache_created > 0 {
        inline::print_line(&components::subsection_header(
            "Prompt Cache",
            Color::Rgb(46, 204, 113),
        ));
        inline::print_line(&components::kv_line(
            "Cache read",
            &format!("{} tokens", stats.format_tokens(cache_read)),
            14,
            Color::Rgb(46, 204, 113),
        ));
        inline::print_line(&components::kv_line(
            "Cache created",
            &format!("{} tokens", stats.format_tokens(cache_created)),
            14,
            Color::Rgb(52, 152, 219),
        ));
        let ratio = stats.cache_hit_ratio();
        inline::print_line(&components::kv_line(
            "Hit ratio",
            &format!("{:.0}%", ratio * 100.0),
            14,
            Color::Rgb(241, 196, 15),
        ));
        inline::print_blank();
    }

    inline::print_line(&components::subsection_header(
        "Environment",
        Color::Rgb(255, 215, 0),
    ));
    inline::print_line(&components::kv_line(
        "Working dir",
        &cwd,
        12,
        Color::Rgb(180, 180, 200),
    ));
    inline::print_blank();
    inline::print_line(&components::dashed_separator(Color::Rgb(60, 60, 80)));
    inline::print_blank();
}

pub(crate) fn show_history(conversation: &[ConversationEntry]) {
    inline::print_blank();
    if conversation.is_empty() {
        inline::print_line(&components::warning_badge(
            "No messages in this session yet.",
        ));
        inline::print_blank();
        return;
    }

    inline::print_line(&components::section_header(
        "📜",
        &format!("Conversation History ({} messages)", conversation.len()),
        Color::Rgb(64, 224, 208),
    ));
    inline::print_blank();

    for (i, entry) in conversation.iter().enumerate() {
        let time = entry.timestamp.format("%H:%M:%S");
        let (icon, badge_bg) = match entry.role.as_str() {
            "user" => ("👤", Color::Rgb(52, 152, 219)),
            "assistant" => ("🤖", Color::Rgb(46, 204, 113)),
            _ => ("💬", Color::Rgb(241, 196, 15)),
        };
        let content_preview = if entry.content.len() > 120 {
            format!("{}...", &entry.content[..117])
        } else {
            entry.content.clone()
        };
        inline::print_line(&Line::from(vec![
            RSpan::raw("  "),
            RSpan::styled(
                format!(" {} ", icon),
                RStyle::default()
                    .fg(Color::Rgb(255, 255, 255))
                    .bg(badge_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            RSpan::raw(" "),
            RSpan::styled(
                format!("#{:02}", i + 1),
                RStyle::default()
                    .fg(Color::Rgb(180, 180, 200))
                    .add_modifier(Modifier::BOLD),
            ),
            RSpan::raw(" "),
            RSpan::styled(
                format!("[{}]", time),
                RStyle::default().add_modifier(Modifier::DIM),
            ),
            RSpan::raw("  "),
            RSpan::raw(content_preview),
        ]));
    }
    inline::print_blank();
}

pub(crate) fn print_goodbye(stats: &SessionStats) {
    let in_tok = stats.format_tokens(stats.total_input_tokens());
    let out_tok = stats.format_tokens(stats.total_output_tokens());

    inline::print_blank();

    let cache_read = stats.total_cache_read_tokens();
    let cache_created = stats.total_cache_creation_tokens();
    let has_cache = cache_read > 0 || cache_created > 0;

    let mut summary_lines = vec![
        Line::from(vec![
            RSpan::styled("💬 ", RStyle::default()),
            RSpan::styled("Messages ", RStyle::default().add_modifier(Modifier::DIM)),
            RSpan::styled(
                format!("{}", stats.messages_sent()),
                RStyle::default()
                    .fg(Color::Rgb(135, 206, 250))
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            RSpan::styled("⏱ ", RStyle::default()),
            RSpan::styled("Duration ", RStyle::default().add_modifier(Modifier::DIM)),
            RSpan::styled(
                stats.elapsed_str(),
                RStyle::default()
                    .fg(Color::Rgb(46, 204, 113))
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            RSpan::styled("📊 ", RStyle::default()),
            RSpan::styled("Tokens   ", RStyle::default().add_modifier(Modifier::DIM)),
            RSpan::styled(
                format!("{} ↑", in_tok),
                RStyle::default().fg(Color::Rgb(46, 204, 113)),
            ),
            RSpan::raw(" / "),
            RSpan::styled(
                format!("{} ↓", out_tok),
                RStyle::default().fg(Color::Rgb(231, 76, 60)),
            ),
        ]),
    ];

    if has_cache {
        summary_lines.push(Line::from(vec![
            RSpan::styled("📦 ", RStyle::default()),
            RSpan::styled("Cache    ", RStyle::default().add_modifier(Modifier::DIM)),
            RSpan::styled(
                format!("💰 {} rd", stats.format_tokens(cache_read)),
                RStyle::default()
                    .fg(Color::Rgb(46, 204, 113))
                    .add_modifier(Modifier::BOLD),
            ),
            RSpan::raw(" / "),
            RSpan::styled(
                format!("✏️ {} cr", stats.format_tokens(cache_created)),
                RStyle::default()
                    .fg(Color::Rgb(52, 152, 219))
                    .add_modifier(Modifier::BOLD),
            ),
            RSpan::raw("  "),
            RSpan::styled(
                format!("({:.0}% hit)", stats.cache_hit_ratio() * 100.0),
                RStyle::default().fg(Color::Rgb(241, 196, 15)),
            ),
        ]));
    }

    let panel_lines = components::panel(
        "Session Summary",
        &summary_lines,
        components::BoxStyle::Rounded,
        Color::Rgb(100, 100, 140),
    );
    inline::print_lines(&panel_lines);
    inline::print_blank();

    let goodbye = components::gradient_text(
        "  👋 See you next time!",
        Color::Rgb(255, 105, 180),
        Color::Rgb(64, 224, 208),
    );
    inline::print_line(&goodbye);
    inline::print_blank();
}

pub(crate) fn show_sessions() {
    inline::print_blank();

    let sessions = match crate::session::list() {
        Ok(s) => s,
        Err(e) => {
            inline::print_line(&components::error_badge(&format!(
                "Failed to list sessions: {}",
                e
            )));
            inline::print_blank();
            return;
        }
    };

    if sessions.is_empty() {
        inline::print_line(&components::warning_badge("No previous sessions found."));
        inline::print_blank();
        return;
    }

    inline::print_line(&components::section_header(
        "📜",
        &format!("Sessions ({})", sessions.len()),
        Color::Rgb(64, 224, 208),
    ));
    inline::print_blank();

    let bold = RStyle::default().add_modifier(Modifier::BOLD);
    let dim = RStyle::default().add_modifier(Modifier::DIM);

    for (i, s) in sessions.iter().enumerate().take(20) {
        let time = crate::session::format_relative_time(&s.updated_at);

        let title = if s.title.is_empty() {
            "Untitled"
        } else {
            &s.title
        };

        inline::print_line(&Line::from(vec![
            RSpan::styled(format!("  {:2}. ", i + 1), dim),
            RSpan::styled(title.to_string(), bold),
            RSpan::styled(format!("  {} msgs", s.message_count), dim),
            RSpan::raw("  "),
            RSpan::styled(time, RStyle::default().fg(Color::Rgb(135, 206, 250))),
        ]));
        inline::print_line(&Line::from(vec![
            RSpan::styled("      ", RStyle::default()),
            RSpan::styled(s.id.to_string(), dim),
            RSpan::styled(
                format!(" · {} · {}/{}", s.directory, s.provider, s.model),
                dim,
            ),
        ]));
        inline::print_blank();
    }

    if sessions.len() > 20 {
        inline::print_line(&Line::from(vec![RSpan::styled(
            format!("  ... and {} more", sessions.len() - 20),
            dim,
        )]));
        inline::print_blank();
    }

    inline::print_line(&Line::from(vec![
        RSpan::styled("💡 ", RStyle::default()),
        RSpan::styled("Tip: ", RStyle::default().add_modifier(Modifier::DIM)),
        RSpan::raw("Use "),
        RSpan::styled(
            "/sessions <id>",
            RStyle::default()
                .fg(Color::Rgb(255, 215, 0))
                .add_modifier(Modifier::BOLD),
        ),
        RSpan::raw(" to resume a session"),
    ]));
    inline::print_blank();
}
