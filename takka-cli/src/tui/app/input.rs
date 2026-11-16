use super::*;

impl App {
    // ── Input editing ────────────────────────────────────────

    /// Insert character at cursor position and update dropdown
    pub fn insert_char(&mut self, c: char) {
        self.input.insert(self.cursor_pos, c);
        self.cursor_pos += c.len_utf8();
        self.update_dropdown();
    }

    /// Delete character before cursor
    pub fn delete_char(&mut self) {
        if self.cursor_pos > 0 {
            let prev = self.input[..self.cursor_pos]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.input.remove(prev);
            self.cursor_pos = prev;
            self.update_dropdown();
        }
    }

    /// Delete character at cursor (Delete key)
    #[allow(dead_code)]
    pub fn delete_char_forward(&mut self) {
        if self.cursor_pos < self.input.len() {
            self.input.remove(self.cursor_pos);
            // cursor_pos stays the same
            self.update_dropdown();
        }
    }

    /// Move cursor left
    pub fn move_cursor_left(&mut self) {
        if self.cursor_pos > 0 {
            self.cursor_pos = self.input[..self.cursor_pos]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
    }

    /// Move cursor right
    pub fn move_cursor_right(&mut self) {
        if self.cursor_pos < self.input.len() {
            self.cursor_pos = self.input[self.cursor_pos..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor_pos + i)
                .unwrap_or(self.input.len());
        }
    }

    /// Move cursor to start
    pub fn move_cursor_start(&mut self) {
        self.cursor_pos = 0;
    }

    /// Move cursor to end
    pub fn move_cursor_end(&mut self) {
        self.cursor_pos = self.input.len();
    }

    // ── Dropdown: @ file search & / commands ────────────────────

    /// The buffer the dropdown engine (and mid-run editing) targets.
    pub(crate) fn active_target(&self) -> InputTarget {
        if self.is_loading {
            InputTarget::MidRun
        } else {
            InputTarget::Idle
        }
    }

    /// (text, cursor) view of the target buffer. The mid-run buffer has no
    /// stored cursor — it is always at the end (story 2.4 contract).
    pub(crate) fn buffer_view(&self, target: InputTarget) -> (&str, usize) {
        match target {
            InputTarget::Idle => (self.input.as_str(), self.cursor_pos),
            InputTarget::MidRun => (self.mid_run_input.as_str(), self.mid_run_input.len()),
        }
    }

    /// Write (text, cursor) back to the target buffer. For the mid-run
    /// buffer the cursor is always the end, so only the text is stored.
    pub(crate) fn buffer_write_back(&mut self, target: InputTarget, text: String, cursor: usize) {
        match target {
            InputTarget::Idle => {
                self.input = text;
                self.cursor_pos = cursor;
            }
            InputTarget::MidRun => {
                self.mid_run_input = text;
            }
        }
    }

    /// Update dropdown state based on the active input buffer and cursor.
    ///
    /// The active buffer is the idle `input` while idle and the mid-run
    /// `mid_run_input` while a run is loading, so completions work in both
    /// states through the same engine (story 3.1).
    pub(crate) fn update_dropdown(&mut self) {
        let target = self.active_target();
        // Copy the view out so `self` can be borrowed again below.
        let (text, cursor) = {
            let (t, c) = self.buffer_view(target);
            (t.to_string(), c)
        };
        self.dropdown = Self::evaluate_dropdown(&text, cursor, || self.skill_pairs());
    }

    /// Pure dropdown evaluator over a (text, cursor) buffer view.
    ///
    /// Rules (mirrors the inline REPL, the reference contract):
    /// - If the buffer starts with `/` and cursor is in the command part →
    ///   command dropdown
    /// - `/models <partial>` or `/m <partial>` → model dropdown
    /// - `/skill <partial>` or `/sk <partial>` → skill follow-up dropdown:
    ///   re-discovered and re-filtered per keystroke so an accepted
    ///   `/skill `/`/sk ` survives narrowing (the recorded idle drift fix)
    /// - If cursor is right after or within an `@...` sequence → file
    ///   dropdown
    /// - Otherwise → no dropdown
    ///
    /// `skill_pairs` is only invoked when a skill trigger needs the
    /// discovery index, so ordinary typing never pays for discovery. An
    /// empty result closes the dropdown: it renders nothing but would
    /// otherwise intercept Enter / arrows it cannot serve (same guard as
    /// the inline REPL).
    pub(crate) fn evaluate_dropdown(
        text: &str,
        cursor: usize,
        skill_pairs: impl FnOnce() -> Vec<(String, String)>,
    ) -> Option<Dropdown> {
        let mut next: Option<Dropdown> = None;

        // 1) Check for `/` command trigger
        if text.starts_with('/') {
            let before_cursor = &text[..cursor];
            if !before_cursor.contains(' ') {
                // Only show command dropdown if cursor is still in the
                // command part (before any space — the command itself)
                let query = &text[1..cursor];
                next = Some(Dropdown::new(DropdownType::Command, query.to_string()));
            } else if let Some(space_pos) = before_cursor.find(' ') {
                let cmd = &text[1..space_pos];
                // 1b) Check for `/models <partial>` or `/m <partial>` model trigger
                if cmd == "models" || cmd == "m" {
                    let query = &text[space_pos + 1..cursor];
                    next = Some(Dropdown::new(DropdownType::Model, query.to_string()));
                }
                // 1c) Check for `/skill <partial>` or `/sk <partial>` skill
                //     follow-up trigger. Discovery returning nothing keeps
                //     the dropdown closed (no empty trap).
                else if cmd == "skill" || cmd == "sk" {
                    let pairs = skill_pairs();
                    if !pairs.is_empty() {
                        let query = &text[space_pos + 1..cursor];
                        next = Some(Dropdown::new_skill(query.to_string(), pairs));
                    }
                }
            }
        }

        // 2) Check for `@` file trigger
        if next.is_none() {
            if let Some(at_pos) = find_at_trigger(text, cursor) {
                let query = &text[at_pos + 1..cursor];
                next = Some(Dropdown::new(DropdownType::File, query.to_string()));
            }
        }

        // 3) Commit, closing empty dropdowns so they cannot swallow keys
        //    (e.g. a `/models <id> ` query matching no model must not eat
        //    the Enter that submits the command).
        next.filter(|dd| !dd.is_empty())
    }

    /// Accept the currently selected dropdown item and insert it into the
    /// active input buffer: the idle buffer while idle, the mid-run buffer
    /// while a run is loading (the cursor always lands at its end). Enter
    /// with an open dropdown fills, never submits/steers.
    pub fn accept_dropdown(&mut self) {
        let target = self.active_target();

        // Take the dropdown up front: every path below either closes it or
        // re-opens it (directory / skill chaining).
        let Some(dropdown) = self.dropdown.take() else {
            return;
        };
        let Some(selected_text) = dropdown.selected_item().map(|s| s.to_string()) else {
            return;
        };

        let mut is_dir = false;
        // The /skill chain re-opens the dropdown after the fill.
        let mut open_skills = false;

        let (mut buf, mut cursor) = {
            let (t, c) = self.buffer_view(target);
            (t.to_string(), c)
        };

        match dropdown.dropdown_type {
            DropdownType::Command => {
                let cmd = selected_text.trim_start_matches('/');
                if cmd == "skill" || cmd == "sk" {
                    // Fill the command, then open the skills dropdown for
                    // its argument (follow-up typing filters through
                    // update_dropdown's skill trigger).
                    buf = "/skill ".to_string();
                    cursor = buf.len();
                    open_skills = true;
                } else {
                    // Replace entire buffer with /command
                    buf = format!("/{} ", selected_text);
                    cursor = buf.len();
                }
            }
            DropdownType::File => {
                is_dir = selected_text.ends_with('/');
                // Replace from @ to cursor with the selected file path
                if let Some(at_pos) = find_at_trigger(&buf, cursor) {
                    let before_at = buf[..at_pos].to_string();
                    let after_cursor = buf[cursor..].to_string();

                    // Add trailing space for files, keep slash for dirs
                    let suffix = if is_dir {
                        // Directory — keep it for further navigation
                        ""
                    } else {
                        // File — add space after
                        " "
                    };

                    buf = format!("{}@{}{}{}", before_at, selected_text, suffix, after_cursor);
                    cursor = at_pos + 1 + selected_text.len() + suffix.len();
                }
            }
            DropdownType::Skill => {
                // Extract skill name from display and set buffer
                let skill_name = dropdown
                    .get_skill_name(&selected_text)
                    .unwrap_or_else(|| selected_text.clone());
                buf = format!("/skill {}", skill_name);
                cursor = buf.len();
            }
            DropdownType::Model => {
                // Extract model ID from display string (e.g. "gpt-4o 👁 [openai]" → "gpt-4o")
                let model_id = dropdown
                    .get_model_id(&selected_text)
                    .unwrap_or_else(|| selected_text.clone());
                // Replace from after "/models " to cursor with model ID
                let space_pos = buf.find(' ').unwrap_or(buf.len());
                let prefix = buf[..=space_pos].to_string(); // includes the space
                let after_cursor = buf[cursor..].to_string();
                buf = format!("{}{} {}", prefix, model_id, after_cursor);
                cursor = prefix.len() + model_id.len() + 1;
            }
        }

        self.buffer_write_back(target, buf, cursor);

        // After accepting:
        // - The /skill chain: open the skills dropdown for its argument
        // - Directories: re-trigger the dropdown to show contents of the
        //   selected dir
        // - Files/commands/models: close dropdown (already taken)
        if open_skills {
            self.open_skill_dropdown();
        } else if is_dir {
            self.update_dropdown();
        }
    }

    /// Move dropdown selection up
    pub fn dropdown_up(&mut self) {
        if let Some(dropdown) = &mut self.dropdown {
            dropdown.select_prev();
        }
    }

    /// Move dropdown selection down
    pub fn dropdown_down(&mut self) {
        if let Some(dropdown) = &mut self.dropdown {
            dropdown.select_next();
        }
    }

    /// Close dropdown
    pub fn close_dropdown(&mut self) {
        self.dropdown = None;
    }

    /// Discover skill (name, description) pairs from the shared Commands
    /// config. When commands are unavailable or the lock is contended
    /// (mid-run the run task may hold it), discovery runs with the default
    /// configuration.
    pub(crate) fn skill_pairs(&self) -> Vec<(String, String)> {
        let discovery_config = self
            .commands
            .as_ref()
            .and_then(|cmds_arc| cmds_arc.try_lock().ok())
            .map(|cmds| takka_core::DiscoveryConfig::from(&cmds.get_config().skills))
            .unwrap_or_default();
        let index = takka_core::discover_skills(&discovery_config);

        let mut skills: Vec<_> = index.all().into_iter().collect();
        skills.sort_by(|a, b| a.name().cmp(b.name()));
        skills
            .into_iter()
            .map(|s| (s.name().to_string(), s.description().to_string()))
            .collect()
    }

    /// Open a skill selection dropdown populated from the discovery index.
    ///
    /// Guard: with nothing discoverable (or discovery unavailable) the
    /// dropdown stays closed — an empty dropdown would render nothing yet
    /// still intercept Enter / arrows (the "empty dropdown trap").
    pub(crate) fn open_skill_dropdown(&mut self) {
        let pairs = self.skill_pairs();
        self.open_skill_dropdown_with(pairs);
    }

    /// Injected-pairs variant of [`Self::open_skill_dropdown`] so the
    /// guard is unit-testable without ambient discovery.
    pub(crate) fn open_skill_dropdown_with(&mut self, pairs: Vec<(String, String)>) {
        if pairs.is_empty() {
            self.dropdown = None;
            return;
        }
        self.dropdown = Some(Dropdown::new_skill(String::new(), pairs));
    }

    // ── History navigation ──────────────────────────────────

    /// Navigate history up (older entries)
    pub fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        if self.history_index == -1 {
            self.saved_input = self.input.clone();
        }
        if (self.history_index as usize) < self.history.len() - 1 {
            self.history_index += 1;
            self.input = self.history[self.history.len() - 1 - self.history_index as usize].clone();
            self.cursor_pos = self.input.len();
            self.dropdown = None;
        }
    }

    /// Navigate history down (newer entries)
    pub fn history_down(&mut self) {
        if self.history_index > 0 {
            self.history_index -= 1;
            self.input = self.history[self.history.len() - 1 - self.history_index as usize].clone();
            self.cursor_pos = self.input.len();
        } else if self.history_index == 0 {
            self.history_index = -1;
            self.input = self.saved_input.clone();
            self.cursor_pos = self.input.len();
        }
        self.dropdown = None;
    }

    // ── Scroll ──────────────────────────────────────────────

    /// Scroll messages up
    pub fn scroll_up(&mut self) {
        if self.scroll_offset > 0 {
            self.scroll_offset = self.scroll_offset.saturating_sub(3);
        }
    }

    /// Scroll messages down
    pub fn scroll_down(&mut self) {
        self.scroll_offset += 3;
    }

    // ── Submit ──────────────────────────────────────────────

    /// Submit current input
    /// Mid-run input (story 2.4): while a run is loading, typed text
    /// accumulates in a separate mid-run buffer; Enter submits it through
    /// the run's steering contract (`Commands::steer_text` — Interrupt
    /// steers, FollowUp queues) and the message log surfaces the hand-over.
    pub async fn submit_mid_run(&mut self) {
        let input = self.mid_run_input.trim().to_string();
        if input.is_empty() {
            return;
        }
        self.mid_run_input.clear();
        // The buffer is gone — the dropdown must not survive it.
        self.dropdown = None;

        let Some(commands_arc) = self.commands.clone() else {
            return;
        };
        let mode = commands_arc.lock().await.queue_mode_getter();
        commands_arc.lock().await.steer_text(input.clone()).await;

        let note = match mode {
            crate::commands::QueueMode::Interrupt => {
                format!("\u{21b3} steered in (applies at the next tool boundary): \"{input}\"")
            }
            crate::commands::QueueMode::FollowUp => {
                format!("\u{21b3} queued for the next run: \"{input}\"")
            }
        };
        self.messages.push(Message {
            role: MessageRole::System,
            content: note.clone(),
            timestamp: chrono::Local::now(),
        });
        let _ = self.tx.send(AppMessage::Progress(note));
    }

    pub fn insert_mid_run_char(&mut self, c: char) {
        self.mid_run_input.push(c);
        // Mid-run completions (story 3.1): the same dropdown engine that
        // serves the idle buffer re-evaluates over the mid-run buffer.
        self.update_dropdown();
    }

    /// Delete the last character of the mid-run buffer and re-evaluate the
    /// dropdown — correction parity with the idle buffer, so an accepted
    /// fill can be walked back (and its dropdown restored).
    pub fn delete_mid_run_char(&mut self) {
        self.mid_run_input.pop();
        self.update_dropdown();
    }

    pub async fn submit(&mut self) {
        let input = self.input.trim().to_string();
        if input.is_empty() {
            return;
        }

        // Add to history
        if !self.history.is_empty() || self.history.last() != Some(&input) {
            self.history.push(input.clone());
        }
        self.history_index = -1;

        // Clear input
        self.input.clear();
        self.cursor_pos = 0;
        self.dropdown = None;
        self.image_attachment = None;

        // Handle slash commands
        if input.starts_with('/') {
            self.handle_slash_command(&input).await;
            return;
        }

        // Add user message
        self.messages.push(Message {
            role: MessageRole::User,
            content: input.clone(),
            timestamp: chrono::Local::now(),
        });
        self.stats.messages_sent += 1;

        // Push to session
        session::push_message(&mut self.session, "user", &input);

        // Start loading
        self.is_loading = true;
        self.progress.start();
        self.loading_started = Some(Instant::now());
        self.current_response.clear();

        // Run task in background (clone Arc<Mutex<Commands>>, never take)
        let tx = self.tx.clone();
        let task = input.clone();

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

        tokio::spawn(async move {
            let _ = tx.send(AppMessage::Progress("Thinking...".into()));

            // Pipe orchestrator events to the same channel so the
            // message log shows tool calls / results inline. Cloning
            // the sender into the closure is cheap (Arc internally).
            let event_tx = tx.clone();

            let mut commands = commands_arc.lock().await;
            let result = commands
                .run_with_callbacks(
                    &task,
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
                "spawned task: run_with_callbacks completed"
            );
            match result {
                Ok(response) => {
                    tracing::debug!(len = response.len(), "spawned task: sending TaskComplete");
                    if let Err(e) = tx.send(AppMessage::TaskComplete(response)) {
                        tracing::warn!(error = %e, "spawned task: failed to send TaskComplete");
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, "spawned task: sending Error");
                    if let Err(send_err) = tx.send(AppMessage::Error(e.to_string())) {
                        tracing::warn!(error = %send_err, "spawned task: failed to send Error");
                    }
                }
            }
            // Lock is dropped here; commands_arc stays alive for future use.
        });
    }
}
