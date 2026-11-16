use anyhow::Result;
use comfy_table::{
    modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL, Cell, Color as TColor, Table,
};
use dialoguer::{theme::ColorfulTheme, Confirm};
use ratatui::style::{Color as RColor, Modifier as RModifier, Style as RStyle};
use ratatui::text::{Line as RLine, Span as RSpan};
use takka_core::Config;

use crate::cli::{ConfigAction, OutputFormat, SkillAction};
use crate::error::CommandError;
use crate::widgets::{components, inline};

use super::*;

impl Commands {
    pub async fn list_tools(&mut self) {
        if let Err(e) = self.ensure_connection().await {
            inline::print_line(&components::error_badge(&e.to_string()));
            return;
        }
        let runtime = self.connection.as_mut().unwrap();
        let reply = match runtime
            .send_and_wait(takka_core::runtime::protocol::Request::ListTools)
            .await
        {
            Ok(reply) => reply,
            Err(e) => {
                inline::print_line(&components::error_badge(&e.to_string()));
                return;
            }
        };

        if let takka_core::Event::ToolList { tools } = reply.event {
            inline::print_blank();
            inline::print_line(&components::section_header(
                "🔧",
                &format!("Available Tools ({})", tools.len()),
                RColor::Cyan,
            ));
            inline::print_blank();
            for t in &tools {
                inline::print_line(&RLine::from(vec![
                    RSpan::raw("  "),
                    RSpan::styled(
                        t.name.clone(),
                        RStyle::default().add_modifier(RModifier::BOLD),
                    ),
                ]));
                inline::print_line(&RLine::from(vec![
                    RSpan::raw("    "),
                    RSpan::raw(t.description.clone()),
                ]));
            }
            inline::print_blank();
        }
    }

    // ── MCP status (for REPL /mcp) ──────────────────────────

    pub fn show_mcp_status(&self) {
        inline::print_blank();

        if self.config.mcp_servers.is_empty() {
            inline::print_line(&components::warning_badge("No MCP servers configured."));
            inline::print_line(&RLine::from(vec![
                RSpan::raw("  Add servers in your config file: "),
                RSpan::raw(Config::config_path().display().to_string()),
            ]));
            inline::print_blank();
            return;
        }

        inline::print_line(&components::section_header(
            "📡",
            &format!("MCP Servers ({})", self.config.mcp_servers.len()),
            RColor::Cyan,
        ));
        inline::print_blank();

        let bold = RStyle::default().add_modifier(RModifier::BOLD);
        for (name, srv) in &self.config.mcp_servers {
            inline::print_line(&RLine::from(vec![
                RSpan::raw("  "),
                RSpan::styled(name.clone(), bold),
            ]));
            if let Some(cmd) = &srv.command {
                inline::print_line(&RLine::from(format!("    Command: {}", cmd)));
            }
            if let Some(args) = &srv.args {
                if !args.is_empty() {
                    inline::print_line(&RLine::from(format!("    Args:    {}", args.join(" "))));
                }
            }
            if let Some(url) = &srv.url {
                inline::print_line(&RLine::from(format!("    URL:     {}", url)));
            }
            inline::print_blank();
        }
    }

    // ── Memory search (for REPL /search) ────────────────────

    /// Render a `/search <query>` result against the current orchestrator's
    /// conversation memory. No-op (with a hint) if the orchestrator hasn't
    /// been initialized yet — i.e. before the first turn.
    pub async fn search_memory_inline(&mut self, query: &str) {
        let query = query.trim();
        if query.is_empty() {
            inline::print_blank();
            inline::print_line(&components::warning_badge("Search query is empty."));
            inline::print_blank();
            return;
        }
        if let Err(error) = self.ensure_connection().await {
            inline::print_line(&components::error_badge(&error.to_string()));
            return;
        }
        let response = self
            .connection
            .as_mut()
            .unwrap()
            .send_and_wait(takka_core::runtime::protocol::Request::SearchMemory {
                query: query.to_string(),
            })
            .await;
        match response {
            Ok(event) => {
                if let takka_core::Event::MemorySearchResult { query, matches } = event.event {
                    inline::print_blank();
                    inline::print_line(&components::section_header(
                        "🔍",
                        &format!("Memory search: \"{}\" — {} match(es)", query, matches.len()),
                        RColor::Cyan,
                    ));
                    inline::print_blank();
                    for (index, hit) in matches.iter().enumerate() {
                        inline::print_line(&RLine::from(format!("  [{}] {}", index + 1, hit.role)));
                        inline::print_line(&RLine::from(format!("      {}", hit.content)));
                    }
                    inline::print_blank();
                }
            }
            Err(error) => inline::print_line(&components::error_badge(&error.to_string())),
        }
    }

    // ── Plan (for REPL /plan) ───────────────────────────

    /// Plan a goal through `takka_core::PlannerAgent`: ask the model
    /// for a step-by-step plan, render it for the operator, and ask for
    /// approval before executing the steps.
    ///
    /// This replaces the older 'just-prefix-the-prompt' fallback. The
    /// orchestrator and tool registry are reused from the current
    /// session so plan steps run with the same safety + permissions as
    /// regular turns.
    pub async fn plan_inline(&mut self, goal: &str) -> anyhow::Result<()> {
        let goal = goal.trim();
        if goal.is_empty() {
            inline::print_blank();
            inline::print_line(&components::warning_badge("Plan goal is empty."));
            inline::print_blank();
            return Ok(());
        }

        self.plan_and_execute(goal, true).await
    }

    /// `takka run --plan <task>`: one-shot plan-then-execute without
    /// entering interactive mode. Respects the `require_approval` config
    /// setting (defaults to true).
    pub async fn plan_run(&mut self, task: &str) -> anyhow::Result<()> {
        let task = task.trim();
        if task.is_empty() {
            inline::print_blank();
            inline::print_line(&components::warning_badge("Task is empty."));
            inline::print_blank();
            return Ok(());
        }

        // Use require_approval from config (default: true)
        self.plan_and_execute(task, self.config.agent.planner.require_approval)
            .await
    }

    /// Shared planner logic: create a plan, optionally ask for approval,
    /// render live progress, and execute.
    ///
    /// `ask_approval`: when true, render the plan and prompt with
    /// dialoguer. When false, skip the prompt (auto-approve).
    pub(crate) async fn plan_and_execute(
        &mut self,
        goal: &str,
        ask_approval: bool,
    ) -> anyhow::Result<()> {
        inline::print_blank();
        inline::print_line(&components::section_header(
            "🗺️",
            &format!("Planning: {}", goal),
            RColor::Cyan,
        ));
        inline::print_blank();

        if let Err(e) = self.ensure_connection().await {
            inline::print_line(&components::error_badge(&e.to_string()));
            return Ok(());
        }
        let runtime = self.connection.as_mut().unwrap();

        // Full planner lifecycle on the daemon: LLM plan creation, the
        // approval gate (blocking dialoguer prompt is safe here — the
        // daemon is parked waiting for our ConfirmResponse), then live
        // execution progress.
        runtime
            .send(takka_core::runtime::protocol::Request::Plan {
                goal: goal.to_string(),
                require_approval: ask_approval,
            })
            .await?;

        loop {
            let event = runtime.next_event().await?.event;
            match event {
                takka_core::Event::PlanApprovalRequest { steps, .. } => {
                    let dim = RStyle::default().add_modifier(RModifier::DIM);
                    let bold = RStyle::default().add_modifier(RModifier::BOLD);
                    for (i, step) in steps.iter().enumerate() {
                        inline::print_line(&RLine::from(vec![
                            RSpan::styled(format!("  {:>2}. ", i + 1), dim),
                            RSpan::styled(step.description.clone(), bold),
                        ]));
                    }
                    inline::print_blank();

                    let proceed = if ask_approval {
                        Confirm::with_theme(&ColorfulTheme::default())
                            .with_prompt("Execute this plan?")
                            .default(false)
                            .interact()
                            .unwrap_or(false)
                    } else {
                        inline::print_line(&RLine::from(vec![RSpan::styled(
                            "  Auto-approved (require_approval = false)\n".to_string(),
                            RStyle::default().fg(RColor::DarkGray),
                        )]));
                        true
                    };

                    runtime
                        .send(takka_core::runtime::protocol::Request::ConfirmResponse {
                            request_id: None,
                            approved: proceed,
                        })
                        .await?;

                    if !proceed {
                        inline::print_line(&components::warning_badge(
                            "Plan rejected. Nothing executed.",
                        ));
                        inline::print_blank();
                        return Ok(());
                    }
                }
                takka_core::Event::PlanProgress {
                    step_description,
                    step_status,
                    steps_total,
                    steps_completed,
                    steps_failed,
                    ..
                } => {
                    let icon = match step_status.as_str() {
                        "in_progress" => "▶",
                        "completed" => "✅",
                        "failed" => "❌",
                        _ => "⏳",
                    };
                    let fraction = if steps_total > 0 {
                        steps_completed as f32 / steps_total as f32
                    } else {
                        0.0
                    };
                    let bar = components::labeled_bar(
                        "progress",
                        fraction,
                        20,
                        RColor::Rgb(46, 204, 113),
                        RColor::Rgb(60, 60, 80),
                    );
                    inline::print_line(&RLine::from(vec![
                        RSpan::raw(format!("  {} ", icon)),
                        RSpan::styled(
                            step_description.clone(),
                            RStyle::default().add_modifier(RModifier::DIM),
                        ),
                    ]));
                    inline::print_line(&bar);
                    inline::print_line(&RLine::from(vec![
                        RSpan::raw("     "),
                        RSpan::styled(
                            format!("{} done / {} failed", steps_completed, steps_failed),
                            RStyle::default().fg(RColor::DarkGray),
                        ),
                    ]));
                    inline::print_blank();
                }
                takka_core::Event::Done { result } => {
                    inline::print_line(&components::success_badge(&result));
                    inline::print_blank();
                    return Ok(());
                }
                takka_core::Event::Error { message } => {
                    inline::print_line(&components::error_badge(&message));
                    inline::print_blank();
                    return Ok(());
                }
                _ => {}
            }
        }
    }

    pub fn examples(&self) {
        let yellow_comment = RStyle::default().fg(RColor::Yellow);

        inline::print_blank();
        inline::print_line(&components::section_header(
            "📖",
            "Takka CLI — Usage Examples",
            RColor::Cyan,
        ));
        inline::print_blank();

        let groups: &[(&str, &[&str])] = &[
            (
                "# Run a single task",
                &[
                    "takka run \"list all Rust files\"",
                    "takka run \"create hello.txt with 'hello world'\"",
                    "takka run \"explain the codebase structure\"",
                ],
            ),
            ("# Interactive mode", &["takka interactive", "takka i"]),
            (
                "# Config management",
                &[
                    "takka config init                    # Default config",
                    "takka config init --interactive      # Guided wizard",
                    "takka config init --provider openai  # Quick setup",
                    "takka config show",
                    "takka config show --format table",
                    "takka config edit",
                    "takka config validate",
                    "takka config backup",
                    "takka config export                  # Masked secrets",
                ],
            ),
            ("# Status & info", &["takka status", "takka version"]),
        ];

        for (heading, lines) in groups {
            inline::print_line(&RLine::from(vec![
                RSpan::raw("  "),
                RSpan::styled((*heading).to_string(), yellow_comment),
            ]));
            for cmd in *lines {
                inline::print_line(&RLine::from(format!("  {}", cmd)));
            }
            inline::print_blank();
        }
    }

    // ── Config dispatch ─────────────────────────────────────

    pub fn config(&self, action: &ConfigAction) -> Result<()> {
        match action {
            ConfigAction::Show { format } => self.config_show(*format)?,
            ConfigAction::Init {
                interactive,
                provider,
            } => self.config_init(*interactive, provider.as_deref())?,
            ConfigAction::Edit => self.config_edit()?,
            ConfigAction::Validate { verbose } => self.config_validate(*verbose)?,
            ConfigAction::Reset { force } => self.config_reset(*force)?,
            ConfigAction::Path => self.config_path()?,
            ConfigAction::Backup => self.config_backup()?,
            ConfigAction::Restore { file } => self.config_restore(file)?,
            ConfigAction::Export => self.config_export()?,
            ConfigAction::Import { file } => self.config_import(file)?,
            ConfigAction::Migrate { yes, force } => self.config_migrate(*yes, *force)?,
        }
        Ok(())
    }

    // ── Skill command dispatch ────────────────────────────────

    pub fn skill_command(&self, action: &SkillAction) -> Result<()> {
        match action {
            SkillAction::List => self.skill_list()?,
            SkillAction::Info { name } => self.skill_info(name)?,
            SkillAction::Create { name, global } => self.skill_create(name, *global)?,
        }
        Ok(())
    }

    pub(crate) fn skill_list(&self) -> Result<()> {
        let discovery_config: takka_core::DiscoveryConfig =
            takka_core::DiscoveryConfig::from(&self.config.skills);
        let index = takka_core::discover_skills(&discovery_config);

        if index.is_empty() {
            println!();
            println!("  No skills found.");
            println!();
            println!("  Create one:  takka skill create <name>");
            println!();
            return Ok(());
        }

        println!();
        println!("  Skills");

        let mut skills: Vec<_> = index.all().into_iter().collect();
        skills.sort_by(|a, b| a.name().cmp(b.name()));

        for skill in &skills {
            println!();
            println!("  📦 {}", skill.name());
            println!("     {}", skill.description());
            println!("     Path: {}", skill.dir.display());
        }

        if !index.blocked().is_empty() {
            println!();
            println!("  Blocked:");
            for name in index.blocked() {
                println!("     ✗ {}", name);
            }
        }

        println!();
        Ok(())
    }

    pub(crate) fn skill_info(&self, name: &str) -> Result<()> {
        let discovery_config: takka_core::DiscoveryConfig =
            takka_core::DiscoveryConfig::from(&self.config.skills);
        let index = takka_core::discover_skills(&discovery_config);

        let skill = match index.get(name) {
            Some(s) => s,
            None => {
                eprintln!("✗ Skill '{}' not found.", name);
                std::process::exit(1);
            }
        };

        println!();
        println!("  📦 {} — {}", skill.name(), skill.description());
        println!("     Path: {}", skill.dir.display());
        println!("     SKILL.md size: {} bytes", skill.content.len());
        println!();

        // Show preview (first 20 lines of body)
        let preview_lines: Vec<&str> = skill.body.lines().take(20).collect();
        if !preview_lines.is_empty() {
            println!("  Preview:");
            for line in &preview_lines {
                println!("    {}", line);
            }
            if skill.body.lines().count() > 20 {
                println!("    ... ({} more lines)", skill.body.lines().count() - 20);
            }
        }

        // List other files in the skill directory
        if let Ok(entries) = std::fs::read_dir(&skill.dir) {
            let files: Vec<_> = entries
                .flatten()
                .filter(|e| e.path().is_file())
                .map(|e| e.path())
                .filter(|p| p.file_name().and_then(|n| n.to_str()) != Some("SKILL.md"))
                .collect();
            if !files.is_empty() {
                println!();
                println!("  Referenced files:");
                for f in &files {
                    println!(
                        "     📄 {}",
                        f.file_name().unwrap_or_default().to_string_lossy()
                    );
                }
            }
        }

        println!();
        Ok(())
    }

    pub(crate) fn skill_create(&self, name: &str, global: bool) -> Result<()> {
        // Validate name
        let name_re = regex::Regex::new(r"^[a-z0-9-]{1,64}$").unwrap();
        if !name_re.is_match(name) {
            eprintln!(
                "✗ Invalid skill name '{}': must be 1-64 chars, lowercase a-z, 0-9, hyphens only",
                name
            );
            std::process::exit(1);
        }

        let base_dir = if global {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            std::path::PathBuf::from(home)
                .join(".config")
                .join("takka")
                .join("skills")
        } else {
            // Project-local: .agentic/skills/ in cwd (or walk-up)
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let mut dir = find_or_create_dir(&cwd, ".agentic");
            dir.push("skills");
            dir
        };

        let skill_dir = base_dir.join(name);
        if skill_dir.exists() {
            eprintln!(
                "✗ Skill '{}' already exists at {}",
                name,
                skill_dir.display()
            );
            std::process::exit(1);
        }

        std::fs::create_dir_all(&skill_dir)
            .map_err(|e| anyhow::anyhow!("Failed to create skill directory: {}", e))?;

        let template = format!(
            r"---
name: {name}
description: What this skill does and when to use it.
---

# {name}

## Setup
(optional setup instructions)

## Usage
Instructions the agent follows when this skill is loaded.
"
        );

        std::fs::write(skill_dir.join("SKILL.md"), &template)
            .map_err(|e| anyhow::anyhow!("Failed to write SKILL.md: {}", e))?;

        println!();
        println!("  ✅ Skill '{}' created at:", name);
        println!("     {}", skill_dir.display());
        println!();
        println!("  Edit SKILL.md to add instructions, then run:");
        println!("     takka skill list");
        println!();

        Ok(())
    }

    /// Name of the skill active for this session, if any.
    pub fn active_skill_name(&self) -> Option<String> {
        self.active_skill.clone()
    }

    /// Available skills for this session (name, description) from the
    /// discovered index — per-instance replacement for the removed
    /// global `list_skills`.
    pub fn available_skills(&self) -> Vec<(String, String)> {
        match &self.skill_index {
            Some(index) => index
                .read()
                .unwrap()
                .all()
                .into_iter()
                .map(|s| (s.name().to_string(), s.description().to_string()))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Load a skill by name and inject its instructions into the orchestrator's
    /// conversation context as a system message. The next LLM call will see the
    /// skill instructions alongside the user's message.
    ///
    /// Returns the skill body content on success.
    pub async fn load_and_activate_skill(&mut self, name: &str) -> Result<String, String> {
        if let Err(e) = self.ensure_connection().await {
            return Err(e.to_string());
        }
        let runtime = self.connection.as_mut().unwrap();
        let reply = runtime
            .send_and_wait(takka_core::runtime::protocol::Request::SkillActivate {
                name: name.to_string(),
            })
            .await
            .map_err(|e| e.to_string())?;

        match reply.event {
            takka_core::Event::SkillActivatedResult {
                skill,
                activated,
                message,
                content,
            } => {
                self.active_skill = if activated { Some(skill) } else { None };
                match message {
                    Some(m) => Err(m),
                    None => Ok(content),
                }
            }
            other => Err(format!("unexpected runtime reply: {other:?}")),
        }
    }

    // ── Config show (json or table) ─────────────────────────

    pub(crate) fn config_show(&self, format: OutputFormat) -> Result<()> {
        match format {
            OutputFormat::Json => {
                let json = serde_json::to_string_pretty(&self.config)
                    .map_err(|e| CommandError::Config(e.to_string()))?;
                println!("{}", json);
            }
            OutputFormat::Table => {
                self.config_show_table()?;
            }
        }
        Ok(())
    }

    pub(crate) fn config_show_table(&self) -> Result<()> {
        println!();

        // Providers table
        let mut table = Table::new();
        table
            .load_preset(UTF8_FULL)
            .apply_modifier(UTF8_ROUND_CORNERS)
            .set_header(vec![
                Cell::new("Provider").fg(TColor::Cyan),
                Cell::new("Type").fg(TColor::Cyan),
                Cell::new("API Base").fg(TColor::Cyan),
                Cell::new("Key").fg(TColor::Cyan),
                Cell::new("Models").fg(TColor::Cyan),
            ]);

        for p in &self.config.providers {
            let key_status = if p.api_key.is_empty() {
                Cell::new("✗").fg(TColor::Red)
            } else if p.api_key.starts_with('$') {
                Cell::new(format!("env:{}", p.api_key)).fg(TColor::Yellow)
            } else {
                Cell::new(format!(
                    "{}...{}",
                    &p.api_key[..4.min(p.api_key.len())],
                    &p.api_key[p.api_key.len().saturating_sub(4)..]
                ))
                .fg(TColor::Green)
            };

            let models: String = p
                .models
                .iter()
                .map(|m| m.display_name.as_deref().unwrap_or(&m.model))
                .collect::<Vec<_>>()
                .join(", ");

            table.add_row(vec![
                Cell::new(&p.name),
                Cell::new(&p.provider_type),
                Cell::new(&p.api_base),
                key_status,
                Cell::new(if models.is_empty() { "none" } else { &models }),
            ]);
        }

        inline::print_line(&RLine::from(vec![
            RSpan::raw("  "),
            RSpan::styled(
                "Providers:",
                RStyle::default().add_modifier(RModifier::BOLD),
            ),
        ]));
        println!("{table}");

        // Safety table
        let mut safety_table = Table::new();
        safety_table
            .load_preset(UTF8_FULL)
            .apply_modifier(UTF8_ROUND_CORNERS)
            .set_header(vec![
                Cell::new("Setting").fg(TColor::Cyan),
                Cell::new("Value").fg(TColor::Cyan),
            ]);

        safety_table.add_row(vec![
            Cell::new("Auto-approve low risk"),
            Cell::new(if self.config.safety.auto_approve_low_risk {
                "yes"
            } else {
                "no"
            }),
        ]);

        let blocked = if self.config.safety.blocked_commands.is_empty() {
            "none".to_string()
        } else {
            self.config.safety.blocked_commands.join(", ")
        };
        safety_table.add_row(vec![Cell::new("Blocked commands"), Cell::new(blocked)]);

        inline::print_line(&RLine::from(vec![
            RSpan::raw("  "),
            RSpan::styled("Safety:", RStyle::default().add_modifier(RModifier::BOLD)),
        ]));
        println!("{safety_table}");

        // Output table
        let mut output_table = Table::new();
        output_table
            .load_preset(UTF8_FULL)
            .apply_modifier(UTF8_ROUND_CORNERS)
            .set_header(vec![
                Cell::new("Setting").fg(TColor::Cyan),
                Cell::new("Value").fg(TColor::Cyan),
            ]);

        output_table.add_row(vec![
            Cell::new("Color"),
            Cell::new(if self.config.output.color {
                "yes"
            } else {
                "no"
            }),
        ]);
        output_table.add_row(vec![
            Cell::new("Stream"),
            Cell::new(if self.config.output.stream {
                "yes"
            } else {
                "no"
            }),
        ]);
        output_table.add_row(vec![
            Cell::new("Show thoughts"),
            Cell::new(if self.config.output.show_thoughts {
                "yes"
            } else {
                "no"
            }),
        ]);
        output_table.add_row(vec![
            Cell::new("Show tool calls"),
            Cell::new(if self.config.output.show_tool_calls {
                "yes"
            } else {
                "no"
            }),
        ]);

        inline::print_line(&RLine::from(vec![
            RSpan::raw("  "),
            RSpan::styled("Output:", RStyle::default().add_modifier(RModifier::BOLD)),
        ]));
        println!("{output_table}");

        // MCP servers
        if !self.config.mcp_servers.is_empty() {
            let mut mcp_table = Table::new();
            mcp_table
                .load_preset(UTF8_FULL)
                .apply_modifier(UTF8_ROUND_CORNERS)
                .set_header(vec![
                    Cell::new("Name").fg(TColor::Cyan),
                    Cell::new("Command").fg(TColor::Cyan),
                ]);
            for (name, srv) in &self.config.mcp_servers {
                mcp_table.add_row(vec![
                    Cell::new(name),
                    Cell::new(srv.command.as_deref().unwrap_or("")),
                ]);
            }
            inline::print_line(&RLine::from(vec![
                RSpan::raw("  "),
                RSpan::styled(
                    "MCP Servers:",
                    RStyle::default().add_modifier(RModifier::BOLD),
                ),
            ]));
            println!("{mcp_table}");
        }

        Ok(())
    }
}
