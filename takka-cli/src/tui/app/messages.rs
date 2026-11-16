use std::time::Instant;

use super::*;

impl App {
    /// Process pending messages from async tasks
    pub fn process_messages(&mut self) {
        // ── Watchdog: auto-reset loading if the agent hangs silently ──
        if self.is_loading {
            if let Some(started) = self.loading_started {
                if started.elapsed() > LOADING_TIMEOUT {
                    tracing::warn!(
                        elapsed_ms = started.elapsed().as_millis(),
                        "Loading watchdog triggered — no response in {}s",
                        LOADING_TIMEOUT.as_secs()
                    );
                    self.is_loading = false;
                    self.loading_started = None;
                    self.progress.stop();
                    // A mid-run dropdown must not leak into the idle state.
                    self.dropdown = None;
                    self.messages.push(Message {
                        role: MessageRole::Error,
                        content: format!(
                            "⚠ Agent did not respond within {} seconds. \
                             Try again or use Ctrl+C to cancel.",
                            LOADING_TIMEOUT.as_secs()
                        ),
                        timestamp: chrono::Local::now(),
                    });
                }
            }
        }

        if let Some(rx) = &mut self.rx {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    AppMessage::StreamChunk(chunk) => {
                        self.current_response.push_str(&chunk);
                    }
                    AppMessage::TaskComplete(result) => {
                        self.is_loading = false;
                        self.loading_started = None;
                        self.progress.stop();
                        // A mid-run dropdown must not leak into the idle state.
                        self.dropdown = None;
                        self.stats.messages_sent += 1;

                        let content = if self.current_response.is_empty() {
                            result
                        } else {
                            std::mem::take(&mut self.current_response)
                        };

                        self.messages.push(Message {
                            role: MessageRole::Assistant,
                            content,
                            timestamp: chrono::Local::now(),
                        });
                        // H-06: refresh the run's spend so the header's budget
                        // chip reflects what the ceiling just measured.
                        if let Some(cmds) = self.commands.as_ref().and_then(|c| c.try_lock().ok()) {
                            self.stats.run_cost_usd = cmds.last_run_stats().cost_usd;
                        }
                    }
                    AppMessage::Error(err) => {
                        self.is_loading = false;
                        self.loading_started = None;
                        self.progress.stop();
                        // A mid-run dropdown must not leak into the idle state.
                        self.dropdown = None;
                        self.current_response.clear();

                        // "Cancelled by user" (AgenticError::Cancelled —
                        // story 1.4) is a desired outcome: system notice,
                        // not a red error line (same decision as the
                        // inline REPL's cancel notice).
                        if err.contains("Cancelled by user") {
                            self.messages.push(Message {
                                role: MessageRole::System,
                                content: "Run cancelled".to_string(),
                                timestamp: chrono::Local::now(),
                            });
                        } else {
                            self.messages.push(Message {
                                role: MessageRole::Error,
                                content: format!("Error: {}", err),
                                timestamp: chrono::Local::now(),
                            });
                        }
                    }
                    AppMessage::Progress(msg) => {
                        self.progress.set_message(msg);
                    }
                    AppMessage::ToolCall { name, arguments } => {
                        self.stats.tool_calls += 1;
                        // Stash the structured payload as JSON in the
                        // message content so the renderer can decode it.
                        // We use a sentinel envelope so render code can
                        // tell tool messages apart by the role enum.
                        let payload = serde_json::json!({
                            "name": name,
                            "arguments": arguments,
                        });
                        self.messages.push(Message {
                            role: MessageRole::Tool,
                            content: payload.to_string(),
                            timestamp: chrono::Local::now(),
                        });
                    }
                    AppMessage::ToolResult {
                        name,
                        output,
                        is_error,
                    } => {
                        let payload = serde_json::json!({
                            "name": name,
                            "output": output,
                        });
                        self.messages.push(Message {
                            role: if is_error {
                                MessageRole::ToolError
                            } else {
                                MessageRole::ToolResult
                            },
                            content: payload.to_string(),
                            timestamp: chrono::Local::now(),
                        });
                    }
                    AppMessage::Thought(content) => {
                        // Display the LLM's thinking/explanation as an assistant message
                        if !content.is_empty() {
                            self.messages.push(Message {
                                role: MessageRole::Assistant,
                                content,
                                timestamp: chrono::Local::now(),
                            });
                        }
                    }
                    AppMessage::ToolDelta { name, delta } => {
                        if !delta.trim().is_empty() {
                            self.messages.push(Message {
                                role: MessageRole::ToolActivity,
                                content: format!("[{}]\n{}", name, delta),
                                timestamp: chrono::Local::now(),
                            });
                        }
                    }
                    AppMessage::PlanProgress {
                        goal: _,
                        current_step,
                        step_status,
                        total: _,
                        completed: _,
                        failed: _,
                        pending: _,
                    } => {
                        self.progress
                            .set_message(format!("Plan: {} — {}", current_step, step_status,));
                    }
                    AppMessage::CostLimit {
                        max_cost_usd,
                        spent_usd,
                    } => {
                        // A budget stop is a policy outcome, not a failure:
                        // say so, and say how to lift it.
                        self.messages.push(Message {
                            role: MessageRole::System,
                            content: crate::widgets::budget::stop_notice(max_cost_usd, spent_usd),
                            timestamp: chrono::Local::now(),
                        });
                    }
                    AppMessage::Usage { input, output } => {
                        // Real provider numbers — the status bar used to sit at
                        // 0/0 because nothing ever incremented these.
                        self.stats.tokens_input = self.stats.tokens_input.saturating_add(input);
                        self.stats.tokens_output = self.stats.tokens_output.saturating_add(output);
                    }
                }
            }
        }
    }

    /// Tick for animations
    pub fn tick(&mut self) {
        if self.last_tick.elapsed() >= Duration::from_millis(100) {
            self.progress.tick();
            self.last_tick = Instant::now();
        }
    }
}
