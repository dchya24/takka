use super::*;

impl App {
    // ── G-06: /search <query> ─────────────────────────────────

    /// Search conversation history for matching messages
    pub(crate) fn handle_search(&mut self, query: &str) {
        let query = query.trim();
        if query.is_empty() {
            self.messages.push(Message {
                role: MessageRole::System,
                content: "Usage: `/search <query>` — Search conversation history.".into(),
                timestamp: chrono::Local::now(),
            });
            return;
        }

        let query_lower = query.to_lowercase();
        let mut hits: Vec<(usize, &str, &str)> = Vec::new(); // (turn_index, role, snippet)

        for (i, msg) in self.messages.iter().enumerate() {
            let content_lower = msg.content.to_lowercase();
            if content_lower.contains(&query_lower) {
                let role_label = match msg.role {
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                    MessageRole::System => "system",
                    MessageRole::Error => "error",
                    MessageRole::Tool => "tool",
                    MessageRole::ToolResult => "tool-result",
                    MessageRole::ToolError => "tool-error",
                    MessageRole::ToolActivity => "tool-activity",
                };
                hits.push((i, role_label, &msg.content));
            }
        }

        if hits.is_empty() {
            self.messages.push(Message {
                role: MessageRole::System,
                content: format!("No matches found for \"{}\"", query),
                timestamp: chrono::Local::now(),
            });
            return;
        }

        // Format top results (max 5) with surrounding context
        let mut result_text = format!(
            "**Search Results for \"{}\"** — {} match(es)\n\n",
            query,
            hits.len()
        );
        for (idx, (turn, role, content)) in hits.iter().take(5).enumerate() {
            // Extract a snippet around the match
            let content_lower = content.to_lowercase();
            let match_pos = content_lower.find(&query_lower).unwrap_or(0);
            let start = content
                .char_indices()
                .take_while(|(i, _)| *i < match_pos.saturating_sub(60))
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            let end_pos = match_pos + query.len() + 80;
            let end = content
                .char_indices()
                .take_while(|(i, _)| *i < end_pos)
                .last()
                .map(|(i, c)| i + c.len_utf8())
                .unwrap_or(content.len());
            let snippet = &content[start..end.min(content.len())];
            let prefix = if start > 0 { "..." } else { "" };
            let suffix = if end < content.len() { "..." } else { "" };

            result_text.push_str(&format!(
                "{}. **Turn {}** ({})\n   `{}{}{}`\n\n",
                idx + 1,
                turn + 1,
                role,
                prefix,
                snippet.trim(),
                suffix
            ));
        }

        if hits.len() > 5 {
            result_text.push_str(&format!("...and {} more matches\n", hits.len() - 5));
        }

        self.messages.push(Message {
            role: MessageRole::System,
            content: result_text,
            timestamp: chrono::Local::now(),
        });
    }

    // ── G-07: /image <path> ────────────────────────────────────

    /// Attach an image for the next message (vision models)
    pub(crate) async fn handle_image(&mut self, path: &str) {
        let path = path.trim();
        if path.is_empty() {
            // Clear any pending attachment
            if let Some(cmds_arc) = &self.commands {
                let mut cmds = cmds_arc.lock().await;
                let count = cmds.pending_attachment_count();
                if count > 0 {
                    cmds.drain_pending_attachments();
                    self.image_attachment = None;
                    self.messages.push(Message {
                        role: MessageRole::System,
                        content: "Cleared pending image attachment.".into(),
                        timestamp: chrono::Local::now(),
                    });
                } else {
                    self.messages.push(Message {
                        role: MessageRole::System,
                        content: "Usage: `/image <path>` — Attach an image (for vision models)."
                            .into(),
                        timestamp: chrono::Local::now(),
                    });
                }
            }
            return;
        }

        // Use commands to validate and queue the image
        if let Some(cmds_arc) = &self.commands {
            let mut cmds = cmds_arc.lock().await;
            // Check vision capability first
            let caps = cmds.active_model_capabilities();
            if !caps.vision {
                self.messages.push(Message {
                    role: MessageRole::Error,
                    content: "Active model does not support image input.\nSwitch with `/models` to a vision-capable model first.".into(),
                    timestamp: chrono::Local::now(),
                });
                return;
            }

            // Load the image
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
                    let source = format!("{}", att.source);
                    let bytes = att.size_bytes;
                    let mime = att.mime_type.clone();
                    // Store filename for display in status bar
                    let display_name = std::path::Path::new(path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.to_string());
                    self.image_attachment = Some(display_name);
                    cmds.queue_attachment(att);
                    self.messages.push(Message {
                        role: MessageRole::System,
                        content: format!(
                            "📷 Attached: {} ({} bytes, {})\nImage will be sent with your next message.",
                            source,
                            bytes,
                            if mime.is_empty() { "unknown" } else { mime.as_str() }
                        ),
                        timestamp: chrono::Local::now(),
                    });
                }
                Err(e) => {
                    self.messages.push(Message {
                        role: MessageRole::Error,
                        content: format!("Failed to attach image: {}", e),
                        timestamp: chrono::Local::now(),
                    });
                }
            }
        } else {
            self.messages.push(Message {
                role: MessageRole::Error,
                content: "Commands not initialized.".into(),
                timestamp: chrono::Local::now(),
            });
        }
    }

    // ── G-08: /provider <name> ─────────────────────────────────

    /// List available providers
    pub(crate) async fn handle_provider_list(&mut self) {
        let providers_text = match &self.commands {
            Some(cmds_arc) => {
                let cmds = cmds_arc.lock().await;
                let config = &cmds.config_ref();
                let mut lines = String::from("**Providers:**\n\n");
                let active = config.active_provider().map(|p| p.name.clone());
                for provider in &config.providers {
                    let is_active = active.as_deref() == Some(&provider.name);
                    let marker = if is_active { " ●" } else { "" };
                    let model_count = provider.models.len();
                    lines.push_str(&format!(
                        "- **{}** ({} model(s)){}\n",
                        provider.name, model_count, marker
                    ));
                }
                lines.push_str("\nUse `/provider <name>` to switch provider.");
                lines
            }
            None => "Commands not initialized.".into(),
        };
        self.messages.push(Message {
            role: MessageRole::System,
            content: providers_text,
            timestamp: chrono::Local::now(),
        });
    }

    /// Switch to a different provider
    pub(crate) async fn handle_provider_switch(&mut self, name: &str) {
        // Find the default model of the target provider and switch to it
        let result = match &self.commands {
            Some(cmds_arc) => match cmds_arc.try_lock() {
                Ok(cmds) => {
                    let config = cmds.config_ref();
                    let provider = config
                        .providers
                        .iter()
                        .find(|p| p.name.to_lowercase() == name.to_lowercase());
                    match provider {
                        Some(p) => {
                            let default_model = p
                                .models
                                .first()
                                .map(|m| m.display_name.as_deref().unwrap_or(&m.model))
                                .unwrap_or(name)
                                .to_string();
                            Some(default_model)
                        }
                        None => None,
                    }
                }
                Err(_) => None,
            },
            None => None,
        };

        match result {
            Some(model_name) => {
                self.switch_model(&model_name).await;
            }
            None => {
                self.messages.push(Message {
                    role: MessageRole::Error,
                    content: format!(
                        "Provider '{}' not found. Use `/provider` to see available providers.",
                        name
                    ),
                    timestamp: chrono::Local::now(),
                });
            }
        }
    }

    // ── G-09: /mcp ─────────────────────────────────────────────

    /// Show MCP server status
    pub(crate) async fn handle_mcp_status(&mut self) {
        let status_text = match &self.commands {
            Some(cmds_arc) => {
                let cmds = cmds_arc.lock().await;
                let config = cmds.config_ref();
                if config.mcp_servers.is_empty() {
                    "No MCP servers configured.\nAdd servers in your config file.".to_string()
                } else {
                    let mut lines = format!("**MCP Servers ({})**\n\n", config.mcp_servers.len());
                    for (name, srv) in &config.mcp_servers {
                        let has_command = srv.command.is_some();
                        let has_url = srv.url.is_some();
                        let status = if has_command || has_url {
                            "✓ configured"
                        } else {
                            "✗ incomplete"
                        };
                        lines.push_str(&format!("- **{}** — {}", name, status));
                        if let Some(ref cmd) = srv.command {
                            lines.push_str(&format!("\n  Command: `{}`", cmd));
                            if let Some(ref args) = srv.args {
                                if !args.is_empty() {
                                    lines.push_str(&format!(" {}", args.join(" ")));
                                }
                            }
                        }
                        if let Some(ref url) = srv.url {
                            lines.push_str(&format!("\n  URL: `{}`", url));
                        }
                        lines.push('\n');
                    }
                    lines
                }
            }
            None => "Commands not initialized.".into(),
        };
        self.messages.push(Message {
            role: MessageRole::System,
            content: status_text,
            timestamp: chrono::Local::now(),
        });
    }

    // ── Skills ─────────────────────────────────────────────────

    /// List all indexed skills
    pub(crate) async fn handle_skill_list(&mut self) {
        match &self.commands {
            Some(cmds_arc) => {
                let cmds = cmds_arc.lock().await;
                let discovery_config: takka_core::DiscoveryConfig =
                    takka_core::DiscoveryConfig::from(&cmds.get_config().skills);
                let index = takka_core::discover_skills(&discovery_config);

                if index.is_empty() {
                    self.messages.push(Message {
                        role: MessageRole::System,
                        content: "No skills found.\n\nCreate one: `takka skill create <name>`"
                            .into(),
                        timestamp: chrono::Local::now(),
                    });
                    return;
                }

                let mut skills: Vec<_> = index.all().into_iter().collect();
                skills.sort_by(|a, b| a.name().cmp(b.name()));

                let mut lines = format!("**Indexed Skills ({})**\n\n", skills.len());
                for skill in &skills {
                    lines.push_str(&format!(
                        "- **{}** — {}\n",
                        skill.name(),
                        skill.description()
                    ));
                    lines.push_str(&format!("  *Path: `{}`*\n", skill.dir.display()));
                }

                if !index.blocked().is_empty() {
                    lines.push_str("\n**Blocked:**\n");
                    for name in index.blocked() {
                        lines.push_str(&format!("- ✗ {}\n", name));
                    }
                }

                self.messages.push(Message {
                    role: MessageRole::System,
                    content: lines,
                    timestamp: chrono::Local::now(),
                });
            }
            None => {
                self.messages.push(Message {
                    role: MessageRole::Error,
                    content: "Commands not initialized.".into(),
                    timestamp: chrono::Local::now(),
                });
            }
        }
    }

    /// Load, display, and activate a skill
    pub(crate) async fn handle_skill_load(&mut self, name: &str) {
        match &self.commands {
            Some(cmds_arc) => {
                let mut cmds = cmds_arc.lock().await;
                match cmds.load_and_activate_skill(name).await {
                    Ok(body) => {
                        let mut lines = format!("✅ **Skill '{}' activated** — instructions injected into agent context.\n\n", name);

                        // Preview first 10 lines
                        lines.push_str("📖 Preview:\n\n");
                        let preview: Vec<&str> = body.lines().take(10).collect();
                        for line in &preview {
                            lines.push_str(&format!("{}\n", line));
                        }
                        if body.lines().count() > 10 {
                            lines.push_str(&format!(
                                "... ({} more lines)\n",
                                body.lines().count() - 10
                            ));
                        }

                        lines.push_str("\n💡 *Now send a message — the skill instructions will be included as context.*");

                        self.messages.push(Message {
                            role: MessageRole::System,
                            content: lines,
                            timestamp: chrono::Local::now(),
                        });
                    }
                    Err(e) => {
                        self.messages.push(Message {
                            role: MessageRole::Error,
                            content: format!("Failed to activate skill '{}': {}", name, e),
                            timestamp: chrono::Local::now(),
                        });
                    }
                }
            }
            None => {
                self.messages.push(Message {
                    role: MessageRole::Error,
                    content: "Commands not initialized.".into(),
                    timestamp: chrono::Local::now(),
                });
            }
        }
    }

    // ── G-10: /plan <goal> ────────────────────────────────────

    /// Generate and display a structured plan for the given goal.
    /// Sends the goal as a special planning request to the LLM with a
    /// planning-focused system prompt, streams the response normally.
    pub(crate) async fn handle_plan(&mut self, goal: &str) {
        let goal = goal.trim();
        if goal.is_empty() {
            self.messages.push(Message {
                role: MessageRole::System,
                content: "Usage: `/plan <goal>` — Generate a structured plan.".into(),
                timestamp: chrono::Local::now(),
            });
            return;
        }

        // Add plan request message
        self.messages.push(Message {
            role: MessageRole::User,
            content: format!("/plan {}", goal),
            timestamp: chrono::Local::now(),
        });
        self.stats.messages_sent += 1;

        // Start loading
        self.is_loading = true;
        self.progress.start();
        self.loading_started = Some(Instant::now());
        self.current_response.clear();

        // Clone Arc<Mutex<Commands>> for the spawned task
        let commands_arc = match &self.commands {
            Some(c) => c.clone(),
            None => {
                self.is_loading = false;
                self.messages.push(Message {
                    role: MessageRole::Error,
                    content: "Commands not initialized.".into(),
                    timestamp: chrono::Local::now(),
                });
                return;
            }
        };

        let tx = self.tx.clone();
        let plan_goal = goal.to_string();

        tokio::spawn(async move {
            // Prepend a planning system instruction to guide the response
            let plan_prompt = format!(
                "You are a planning assistant. The user will describe a goal.\
                \nRespond with a structured plan:\
                \n1. **Understanding** — Briefly restate the goal\
                \n2. **Approach** — High-level strategy\
                \n3. **Steps** — Numbered, actionable steps\
                \n4. **Considerations** — Risks, edge cases, dependencies\
                \nKeep it concise and practical.\
                \n\nGoal: {}",
                plan_goal
            );

            let event_tx = tx.clone();
            let mut commands = commands_arc.lock().await;
            let result = commands
                .run_with_callbacks(
                    &plan_prompt,
                    |chunk| {
                        let _ = tx.send(AppMessage::StreamChunk(chunk.to_string()));
                    },
                    move |event| match event {
                        takka_core::Event::ToolCall {
                            tool_name,
                            arguments,
                        } => {
                            let _ = event_tx.send(AppMessage::ToolCall {
                                name: tool_name,
                                arguments,
                            });
                        }
                        takka_core::Event::ToolStarted { .. } => {
                            // ToolCall (just above) already surfaces the
                            // running marker — ToolStart is a no-op here.
                        }
                        takka_core::Event::ToolDelta {
                            tool_name, delta, ..
                        } => {
                            let _ = event_tx.send(AppMessage::ToolDelta {
                                name: tool_name,
                                delta,
                            });
                        }
                        takka_core::Event::ToolOutput {
                            tool_name, output, ..
                        } => {
                            let body = match &output {
                                serde_json::Value::String(s) => s.clone(),
                                other => other.to_string(),
                            };
                            let is_error = body.starts_with("Tool error")
                                || body.starts_with("Blocked:")
                                || body.starts_with("Skipped:");
                            let _ = event_tx.send(AppMessage::ToolResult {
                                name: tool_name,
                                output,
                                is_error,
                            });
                        }
                        takka_core::Event::Error { message } => {
                            let _ = event_tx.send(AppMessage::Error(message));
                        }
                        takka_core::Event::CostLimitExceeded {
                            max_cost_usd,
                            spent_usd,
                        } => {
                            let _ = event_tx.send(AppMessage::CostLimit {
                                max_cost_usd,
                                spent_usd,
                            });
                        }
                        takka_core::Event::Usage {
                            prompt_tokens,
                            completion_tokens,
                            ..
                        } => {
                            let _ = event_tx.send(AppMessage::Usage {
                                input: prompt_tokens,
                                output: completion_tokens,
                            });
                        }
                        takka_core::Event::Thinking { .. } => {
                            // Skip — text was already streamed via on_chunk.
                            // A separate Thought message would duplicate content.
                        }
                        _ => {}
                    },
                )
                .await;
            tracing::debug!(
                ok = result.is_ok(),
                "handle_plan: run_with_callbacks completed"
            );
            match result {
                Ok(response) => {
                    tracing::debug!(len = response.len(), "handle_plan: sending TaskComplete");
                    if let Err(e) = tx.send(AppMessage::TaskComplete(response)) {
                        tracing::warn!(error = %e, "handle_plan: failed to send TaskComplete");
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, "handle_plan: sending Error");
                    if let Err(send_err) = tx.send(AppMessage::Error(e.to_string())) {
                        tracing::warn!(error = %send_err, "handle_plan: failed to send Error");
                    }
                }
            }
            // Lock is dropped here; commands_arc stays alive for future use.
        });
    }

    /// Handle slash commands
    pub(crate) async fn handle_slash_command(&mut self, input: &str) {
        let parts: Vec<&str> = input.splitn(2, ' ').collect();
        let cmd = parts[0];
        let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");

        match cmd {
            "/help" | "/h" => {
                self.messages.push(Message {
                    role: MessageRole::System,
                    content: r#"**Available Commands:**

| Command | Alias | Description |
|---------|-------|-------------|
| `/help` | `/h` | Show this help |
| `/new` | `/n` | Start new session |
| `/clear` | `/cls` | Start new session (alias) |
| `/sessions` | `/ss` | List & resume sessions |
| `/models` | `/m` | Switch model |
| `/provider` | | Switch provider |
| `/search` | `/s`, `/find` | Search conversation history |
| `/image` | `/img` | Attach image |
| `/skills` | `/sk` | List indexed skills |
| `/skills <name>` | `/sk <name>` | Load and display a skill |
| `/skill` | | Open skill selection dropdown |
| `/skill <name>` | | Load and activate a skill |
| `/plan` | `/p` | Generate a structured plan |
| `/config` | `/cfg` | Show configuration |
| `/tools` | `/t` | List available tools |
| `/history` | `/hist` | Show message history |
| `/stats` | | Show statistics |
| `/mcp` | | Show MCP server status |
| `/quit` | `/q` | Exit TUI |

**Tips:**
- Type `/` to see command dropdown
- Type `@` anywhere to browse files
- Use ↑/↓ to navigate history
- Use PageUp/PageDown to scroll
- Press Ctrl+C / Esc to cancel"#
                        .into(),
                    timestamp: chrono::Local::now(),
                });
            }
            "/new" | "/n" => {
                self.new_session().await;
            }
            "/clear" | "/cls" => {
                self.new_session().await;
            }
            "/sessions" | "/ss" if !arg.is_empty() => {
                // Resume specific session by ID or index
                self.resume_session(arg).await;
            }
            "/sessions" | "/ss" => {
                self.open_sessions();
            }
            "/models" | "/m" if !arg.is_empty() => {
                self.switch_model(arg).await;
            }
            "/models" | "/m" => {
                // Show models list - open a message with available models
                let models_text = match &self.commands {
                    Some(cmds_arc) => {
                        if let Ok(cmds) = cmds_arc.try_lock() {
                            let (provider, model, _) = cmds.model_info();
                            format!(
                                "**Current Model:** {} / {}\n\nUse `/models <name>` to switch.",
                                provider, model
                            )
                        } else {
                            "**(busy)**".into()
                        }
                    }
                    None => "Commands not initialized.".into(),
                };
                self.messages.push(Message {
                    role: MessageRole::System,
                    content: models_text,
                    timestamp: chrono::Local::now(),
                });
            }
            "/search" | "/s" | "/find" => {
                self.handle_search(arg);
            }
            "/image" | "/img" => {
                self.handle_image(arg).await;
            }
            "/provider" if !arg.is_empty() => {
                self.handle_provider_switch(arg).await;
            }
            "/provider" => {
                self.handle_provider_list().await;
            }
            "/mcp" => {
                self.handle_mcp_status().await;
            }
            "/skills" | "/sk" | "/skill" if !arg.is_empty() => {
                self.handle_skill_load(arg).await;
            }
            "/skills" | "/sk" => {
                self.handle_skill_list().await;
            }
            "/skill" => {
                self.open_skill_dropdown();
            }
            "/plan" | "/p" if !arg.is_empty() => {
                self.handle_plan(arg).await;
            }
            "/plan" | "/p" => {
                self.messages.push(Message {
                    role: MessageRole::System,
                    content: "Usage: `/plan <goal>` — Generate a structured plan.".into(),
                    timestamp: chrono::Local::now(),
                });
            }
            "/quit" | "/q" | "/exit" => {
                self.should_quit = true;
            }
            "/config" | "/cfg" => {
                let config_text = match &self.commands {
                    Some(cmds_arc) => match cmds_arc.try_lock() {
                        Ok(cmds) => {
                            let cfg = cmds.config_ref();
                            let mut lines = String::from("**Configuration:**\n\n");
                            lines.push_str(&format!(
                                "- Config path: `{}`\n",
                                takka_core::Config::config_path().display()
                            ));
                            if let Some(p) = cfg.active_provider() {
                                lines.push_str(&format!("- Provider: **{}**\n", p.name));
                                lines.push_str(&format!("- API Base: `{}`\n", p.api_base));
                                if p.api_key.is_empty() {
                                    lines.push_str("- API Key: ✗ not set\n");
                                } else {
                                    let masked = format!(
                                        "{}...{}",
                                        &p.api_key[..4.min(p.api_key.len())],
                                        &p.api_key[p.api_key.len().saturating_sub(4)..]
                                    );
                                    lines.push_str(&format!("- API Key: `{}`\n", masked));
                                }
                                if let Some(m) = p.models.first() {
                                    lines.push_str(&format!(
                                        "- Model: `{}` (temp: {}, max_tokens: {})\n",
                                        m.model, m.temperature, m.max_tokens
                                    ));
                                }
                            } else {
                                lines.push_str("- No provider configured.\n");
                            }
                            lines
                        }
                        Err(_) => "**(busy)**".into(),
                    },
                    None => "Commands not initialized.".into(),
                };
                self.messages.push(Message {
                    role: MessageRole::System,
                    content: config_text,
                    timestamp: chrono::Local::now(),
                });
            }
            "/tools" | "/t" => {
                let tools_text = {
                    let registry = takka_core::ToolRegistry::new();
                    for tool in takka_core::tools::builtin_tools() {
                        registry.register(tool);
                    }
                    let tool_list = registry.list();
                    let mut lines = format!("**Available Tools ({})**\n\n", tool_list.len());
                    for t in &tool_list {
                        lines.push_str(&format!("- **{}** — {}\n", t.name, t.description));
                        if !t.parameters.is_empty() {
                            let params: Vec<String> = t
                                .parameters
                                .keys()
                                .map(|p| {
                                    let required = t.required.contains(p);
                                    format!("{}{}", p, if required { "*" } else { "" })
                                })
                                .collect();
                            lines.push_str(&format!("  Params: `{}`\n", params.join(", ")));
                        }
                    }
                    lines
                };
                self.messages.push(Message {
                    role: MessageRole::System,
                    content: tools_text,
                    timestamp: chrono::Local::now(),
                });
            }
            "/history" | "/hist" => {
                let history_text = if self.history.is_empty() {
                    "No command history yet.".to_string()
                } else {
                    self.history
                        .iter()
                        .enumerate()
                        .map(|(i, h)| format!("{}. {}", i + 1, h))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                self.messages.push(Message {
                    role: MessageRole::System,
                    content: format!("**Command History:**\n\n{}", history_text),
                    timestamp: chrono::Local::now(),
                });
            }
            "/stats" => {
                let (provider, model, _) = self.model_info();
                let in_tok = self.stats.format_tokens(self.stats.tokens_input);
                let out_tok = self.stats.format_tokens(self.stats.tokens_output);
                self.messages.push(Message {
                    role: MessageRole::System,
                    content: format!(
                        "**Session Statistics:**\n\n- Provider: {}\n- Model: {}\n- Messages sent: {}\n- Tool calls: {}\n- Tokens: {} in / {} out\n- Session ID: {}",
                        provider, model, self.stats.messages_sent, self.stats.tool_calls,
                        in_tok, out_tok, &self.session.id[..self.session.id.len().min(20)]
                    ),
                    timestamp: chrono::Local::now(),
                });
            }
            _ => {
                self.messages.push(Message {
                    role: MessageRole::Error,
                    content: format!(
                        "Unknown command: `{}`\nType `/help` for available commands.",
                        cmd
                    ),
                    timestamp: chrono::Local::now(),
                });
            }
        }
    }
}
