use super::*;

impl App {
    /// Get model info from commands
    pub fn model_info(&self) -> (String, String, String) {
        match &self.commands {
            Some(cmds_arc) => {
                if let Ok(cmds) = cmds_arc.try_lock() {
                    cmds.model_info()
                } else {
                    ("none".into(), "none".into(), "-".into())
                }
            }
            None => ("none".into(), "none".into(), "-".into()),
        }
    }

    /// Save current session to disk
    pub(crate) fn save_session(&mut self) {
        // Push accumulated messages to session before saving
        for msg in &self.messages {
            let role = match msg.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                _ => continue,
            };
            // Check if message already exists in session (avoid duplicates)
            let content_preview = if msg.content.len() > 50 {
                &msg.content[..50]
            } else {
                &msg.content
            };
            let already_exists = self.session.messages.iter().any(|m| {
                m.content.len() >= 50 && m.content.starts_with(content_preview)
                    || m.content == msg.content
            });
            if !already_exists {
                session::push_message(&mut self.session, role, &msg.content);
            }
        }
        if let Err(e) = session::save(&self.session) {
            self.messages.push(Message {
                role: MessageRole::System,
                content: format!("⚠ Could not save session: {}", e),
                timestamp: chrono::Local::now(),
            });
        }
    }

    /// Start a new session (reset all state)
    pub(crate) async fn new_session(&mut self) {
        // Save current session first
        if !self.session.messages.is_empty() {
            self.save_session();
        }

        // Reset all state
        let (provider, model, _) = self.model_info();
        let cwd = std::env::current_dir()
            .unwrap_or_default()
            .display()
            .to_string();
        self.session = session::create(&cwd, &provider, &model);
        self.messages.clear();
        self.messages.push(Message {
            role: MessageRole::System,
            content: "New session started.".into(),
            timestamp: chrono::Local::now(),
        });
        self.current_response.clear();
        self.scroll_offset = 0;
        self.stats.reset();
        self.image_attachment = None;

        // Restart the orchestrator session through the runtime client.
        if let Some(cmds_arc) = &self.commands {
            if let Ok(mut cmds) = cmds_arc.try_lock() {
                if let Err(e) = cmds.restart_session().await {
                    self.messages.push(Message {
                        role: MessageRole::System,
                        content: format!("⚠ Session reset failed: {}", e),
                        timestamp: chrono::Local::now(),
                    });
                }
            }
        }
    }

    /// Resume a session by loading its history
    pub(crate) async fn resume_session(&mut self, session_id: &str) {
        match session::load(session_id) {
            Ok(loaded) => {
                // Save current session first
                if !self.session.messages.is_empty() {
                    self.save_session();
                }

                // Restore loaded session
                self.messages.clear();
                for msg in &loaded.messages {
                    let role = match msg.role.as_str() {
                        "user" => MessageRole::User,
                        "assistant" => MessageRole::Assistant,
                        "system" => MessageRole::System,
                        _ => MessageRole::System,
                    };
                    let timestamp = chrono::DateTime::parse_from_rfc3339(&msg.timestamp)
                        .map(|dt| dt.with_timezone(&chrono::Local))
                        .unwrap_or_else(|_| chrono::Local::now());
                    self.messages.push(Message {
                        role,
                        content: msg.content.clone(),
                        timestamp,
                    });
                }
                self.session = loaded;
                self.session_view = None;
                self.scroll_offset = 0;
                self.stats.reset();

                self.messages.push(Message {
                    role: MessageRole::System,
                    content: format!(
                        "Resumed session: {} ({} messages)",
                        self.session.title,
                        self.session.messages.len()
                    ),
                    timestamp: chrono::Local::now(),
                });

                // Restart the orchestrator session through the runtime.
                if let Some(cmds_arc) = &self.commands {
                    if let Ok(mut cmds) = cmds_arc.try_lock() {
                        if let Err(e) = cmds.restart_session().await {
                            self.messages.push(Message {
                                role: MessageRole::Error,
                                content: format!("⚠ Session reset failed: {}", e),
                                timestamp: chrono::Local::now(),
                            });
                        }
                    }
                }
            }
            Err(e) => {
                self.messages.push(Message {
                    role: MessageRole::Error,
                    content: format!("Failed to load session: {}", e),
                    timestamp: chrono::Local::now(),
                });
            }
        }
    }

    /// Switch model by name (partial match)
    pub(crate) async fn switch_model(&mut self, name: &str) {
        match &self.commands {
            Some(cmds_arc) => {
                let mut cmds = cmds_arc.lock().await;
                match cmds.switch_model(name) {
                    Ok((provider, model)) => {
                        // Update session with new model
                        self.session.provider = provider.clone();
                        self.session.model = model.clone();
                        self.messages.push(Message {
                            role: MessageRole::System,
                            content: format!("Switched to {} / {}", provider, model),
                            timestamp: chrono::Local::now(),
                        });
                    }
                    Err(e) => {
                        self.messages.push(Message {
                            role: MessageRole::Error,
                            content: format!("Failed to switch model: {}", e),
                            timestamp: chrono::Local::now(),
                        });
                    }
                }
            }
            None => {
                self.messages.push(Message {
                    role: MessageRole::Error,
                    content: "Commands not initialized".into(),
                    timestamp: chrono::Local::now(),
                });
            }
        }
    }

    // ── G-11: Context indicator state ─────────────────────────

    /// Check for context files (AGENT.md, memory.md) and return
    /// a formatted indicator string for the header bar.
    pub fn context_indicators(&self) -> String {
        let mut indicators = Vec::new();

        if let Some(cmds_arc) = &self.commands {
            if let Ok(cmds) = cmds_arc.try_lock() {
                if cmds.agent_md_path().is_some() {
                    indicators.push("📄 AGENT.md");
                }
                if cmds.memory_md_loaded() {
                    indicators.push("🧠 memory.md");
                }
            }
        }

        if indicators.is_empty() {
            String::new()
        } else {
            format!(" {} ", indicators.join(" "))
        }
    }

    /// Open session list view
    pub(crate) fn open_sessions(&mut self) {
        match session::list() {
            Ok(summaries) => {
                self.session_view = Some(SessionView {
                    summaries,
                    selected: 0,
                    filter: String::new(),
                });
            }
            Err(e) => {
                self.messages.push(Message {
                    role: MessageRole::Error,
                    content: format!("Failed to list sessions: {}", e),
                    timestamp: chrono::Local::now(),
                });
            }
        }
    }

    /// Handle key events in session view mode
    pub(crate) async fn handle_session_view_key(&mut self, key: crossterm::event::KeyEvent) {
        let view = match &mut self.session_view {
            Some(v) => v,
            None => return,
        };

        match key.code {
            KeyCode::Up if view.selected > 0 => {
                view.selected -= 1;
            }
            KeyCode::Down if view.selected + 1 < view.summaries.len() => {
                view.selected += 1;
            }
            KeyCode::Enter => {
                if let Some(summary) = view.summaries.get(view.selected) {
                    let id = summary.id.clone();
                    let _ = view; // Release borrow before resuming
                    self.resume_session(&id).await;
                }
            }
            KeyCode::Esc => {
                self.session_view = None;
            }
            _ => {}
        }
    }
}
