use super::*;

// ── Inner REPL loop ─────────────────────────────────────────
//
// Rendering architecture (MoveUp tracking, encapsulated in RenderState):
//
//   On each keystroke:
//     1. RenderState::clear_input_area() — MoveUp(N) + Clear
//     2. render_prompt_line() → returns line count N
//     3. render_dropdown_lines() → returns line count D
//     4. RenderState::after_render() — stores N+D in prev_lines
//
//   Key invariant: **every** render path MUST leave the cursor on a
//   new line below the rendered content so that MoveUp(N) can
//   correctly return to the top of the input area on the next
//   keystroke.  `render_prompt_line` uses `print_line` (which appends
//   `\r\n`) for the last line, guaranteeing this.
//
//   N counts *physical* terminal rows: long input wraps onto extra
//   rows, so render_prompt_line / render_dropdown_lines return the
//   sum of wrapped row counts (see input_renderer::physical_lines),
//   not the number of logical print_line calls.
//
//   Terminal resize: clears render state + reprints status bar.
//
//   We avoid DECSC/DECRC (SavePosition/RestorePosition) because they
//   are unreliable when the terminal scrolls.  MoveUp tracking is
//   deterministic regardless of scroll state.

/// Main REPL loop. Internal; args grouped per call site.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn repl_loop(
    buffer: &mut InputBuffer,
    dropdown: &mut Option<Dropdown>,
    commands: &mut Commands,
    conversation: &mut Vec<ConversationEntry>,
    current_session: &mut crate::session::Session,
    stats: &SessionStats,
    model_info: &mut ModelInfo,
    toast_manager: &mut ToastManager,
) -> Result<()> {
    let mut render = RenderState::new();
    let mut stdout = io::stdout();

    loop {
        // ── Clean up expired toasts ──
        toast_manager.cleanup();

        // ── Print status bar once per prompt cycle ──
        if render.needs_status_bar {
            print_prompt_status_bar(model_info, stats);

            // Render active toasts
            if toast_manager.has_toasts() {
                let toast_lines = toast_manager.render();
                for line in toast_lines {
                    inline::print_line(&line);
                }
            }

            render.needs_status_bar = false;
        }

        // ── Clear previous input area ──
        render.clear_input_area(&mut stdout)?;

        // ── Render prompt line first ──
        let meta = PromptMetadata::new(model_info.provider.clone(), model_info.model.clone());
        let prompt_lines = input_renderer::render_prompt_line(&meta, buffer, dropdown.is_some());
        render.prev_lines = prompt_lines;

        // ── Render dropdown below input (if active) ──
        if let Some(ref dd) = dropdown {
            if !dd.is_empty() {
                let dd_lines = input_renderer::render_dropdown_lines(dd);
                render.prev_lines += dd_lines;
            }
        }

        // ── Wait for key event ──
        // While a blocking prompt (tool confirmation / `question`) owns stdin we
        // must not read: in raw mode whichever reader asks first wins, and the
        // REPL used to swallow the `y`/`a` keys meant for that prompt. Poll
        // instead of blocking so the flag is noticed before a key is taken, and
        // don't repaint while waiting — the prompt is drawing the screen.
        let event = loop {
            if crate::commands::INTERACTIVE_PROMPT_ACTIVE.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(25));
                continue;
            }
            if event::poll(std::time::Duration::from_millis(50))? {
                break event::read()?;
            }
        };

        match event {
            Event::Key(key) => {
                if !crate::keyboard::should_process_key_kind(key.kind) {
                    continue;
                }

                // Safety net: an empty dropdown renders nothing but would
                // still swallow Enter / Tab / Up / Down below. update_dropdown
                // already closes empty dropdowns; this guards any path that
                // re-creates one (e.g. after accepting an item).
                if dropdown.as_ref().is_some_and(|dd| dd.is_empty()) {
                    *dropdown = None;
                }

                // If dropdown is open, handle dropdown-specific keys first
                if dropdown.is_some() {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Up) => {
                            if let Some(ref mut dd) = dropdown {
                                dd.select_prev();
                            }
                            continue;
                        }
                        (KeyModifiers::NONE, KeyCode::Down) => {
                            if let Some(ref mut dd) = dropdown {
                                dd.select_next();
                            }
                            continue;
                        }
                        (KeyModifiers::NONE, KeyCode::Tab)
                        | (KeyModifiers::NONE, KeyCode::Enter) => {
                            if let Some(_auto_cmd) = accept_dropdown(buffer, dropdown) {
                                // Auto-submit: skill / model selected, execute immediately.
                                // Clear the rendered prompt + dropdown first so the
                                // dropdown disappears the moment the item is accepted;
                                // reset() also prevents the next cycle's MoveUp from
                                // erasing the command output printed below.
                                render.clear_input_area(&mut stdout)?;
                                render.reset();
                                let input = buffer.submit();
                                let dir_name = std::env::current_dir()
                                    .unwrap_or_default()
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("?")
                                    .to_string();
                                inline::print_line(&Line::from(vec![
                                    RSpan::styled(
                                        format!("{}> ", dir_name),
                                        RStyle::default().add_modifier(Modifier::DIM),
                                    ),
                                    RSpan::styled(
                                        input.clone(),
                                        RStyle::default().fg(Color::Rgb(220, 220, 230)),
                                    ),
                                ]));
                                let should_break = handle_input(
                                    &input,
                                    commands,
                                    conversation,
                                    current_session,
                                    stats,
                                    model_info,
                                    toast_manager,
                                )
                                .await;
                                *model_info = get_model_info(commands);
                                render.needs_status_bar = true;
                                if should_break {
                                    return Ok(());
                                }
                                continue;
                            }
                            // Re-evaluate dropdown: if user selected `/skill` the input
                            // is now "/skill " which should trigger the skill dropdown.
                            update_dropdown(buffer, dropdown);
                            continue;
                        }
                        (KeyModifiers::NONE, KeyCode::Esc) => {
                            *dropdown = None;
                            continue;
                        }
                        // Any other key: close dropdown and fall through
                        _ => {
                            *dropdown = None;
                        }
                    }
                }

                // Handle normal input keys
                match (key.modifiers, key.code) {
                    // ── Submit ──
                    (KeyModifiers::NONE, KeyCode::Enter) => {
                        // Clear input area (input + dropdown below)
                        render.clear_input_area(&mut stdout)?;
                        render.reset();
                        *dropdown = None;

                        let input = buffer.submit();
                        if input.is_empty() {
                            inline::print_blank();
                            render.reset();
                            continue;
                        }

                        // Echo the user's input as a permanent line
                        let dir_name = std::env::current_dir()
                            .unwrap_or_default()
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("?")
                            .to_string();
                        inline::print_line(&Line::from(vec![
                            RSpan::styled(
                                format!("{}> ", dir_name),
                                RStyle::default().add_modifier(Modifier::DIM),
                            ),
                            RSpan::styled(
                                input.clone(),
                                RStyle::default().fg(Color::Rgb(220, 220, 230)),
                            ),
                        ]));

                        // Handle input
                        let should_break = handle_input(
                            &input,
                            commands,
                            conversation,
                            current_session,
                            stats,
                            model_info,
                            toast_manager,
                        )
                        .await;

                        // Refresh model_info in case provider changed
                        *model_info = get_model_info(commands);
                        render.needs_status_bar = true;

                        if should_break {
                            return Ok(());
                        }
                    }

                    // ── Exit ──
                    (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
                        render.clear_input_area(&mut stdout)?;
                        return Ok(());
                    }

                    // ── Cancel ──
                    (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                        render.clear_input_area(&mut stdout)?;
                        render.reset();
                        *dropdown = None;
                        buffer.clear();
                        inline::print_blank();
                        inline::print_line(&components::info_badge("Use /quit or Ctrl+D to exit."));
                        inline::print_blank();
                        render.needs_status_bar = true;
                    }

                    // ── Character input (normal + shift/capslock) ──
                    (KeyModifiers::NONE, KeyCode::Char(c))
                    | (KeyModifiers::SHIFT, KeyCode::Char(c)) => {
                        buffer.insert_char(c);
                        buffer.reset_history_browse();
                        update_dropdown(buffer, dropdown);
                    }

                    // ── Shift+Enter for multi-line ──
                    (KeyModifiers::SHIFT, KeyCode::Enter) => {
                        buffer.insert_line_break();
                        buffer.reset_history_browse();
                    }

                    // ── Backspace ──
                    (KeyModifiers::NONE, KeyCode::Backspace) => {
                        // If at start of a line in multi-line mode, merge with previous line
                        if buffer.is_multiline()
                            && buffer.cursor() > 0
                            && buffer.text().as_bytes().get(buffer.cursor() - 1) == Some(&b'\n')
                        {
                            buffer.merge_with_previous_line();
                        } else {
                            buffer.delete_backward();
                        }
                        buffer.reset_history_browse();
                        update_dropdown(buffer, dropdown);
                    }

                    // ── Ctrl+Backspace / Ctrl+W ──
                    (KeyModifiers::CONTROL, KeyCode::Backspace)
                    | (KeyModifiers::CONTROL, KeyCode::Char('w')) => {
                        buffer.delete_word_backward();
                        buffer.reset_history_browse();
                        update_dropdown(buffer, dropdown);
                    }

                    // ── Delete ──
                    (KeyModifiers::NONE, KeyCode::Delete) => {
                        buffer.delete_forward();
                        buffer.reset_history_browse();
                        update_dropdown(buffer, dropdown);
                    }

                    // ── Cursor movement ──
                    (KeyModifiers::NONE, KeyCode::Left) => {
                        buffer.cursor_left();
                    }
                    (KeyModifiers::NONE, KeyCode::Right) => {
                        buffer.cursor_right();
                    }
                    (KeyModifiers::CONTROL, KeyCode::Left) => {
                        buffer.cursor_word_left();
                    }
                    (KeyModifiers::CONTROL, KeyCode::Right) => {
                        buffer.cursor_word_right();
                    }
                    (KeyModifiers::NONE, KeyCode::Home) => {
                        buffer.cursor_home();
                    }
                    (KeyModifiers::NONE, KeyCode::End) => {
                        buffer.cursor_end();
                    }

                    // ── History navigation (only when no dropdown and not multi-line) ──
                    (KeyModifiers::NONE, KeyCode::Up) => {
                        if buffer.is_multiline() {
                            buffer.cursor_up();
                        } else {
                            buffer.history_up();
                        }
                    }
                    (KeyModifiers::NONE, KeyCode::Down) => {
                        if buffer.is_multiline() {
                            buffer.cursor_down();
                        } else {
                            buffer.history_down();
                        }
                    }

                    // ── Ctrl+L — clear screen ──
                    (KeyModifiers::CONTROL, KeyCode::Char('l')) => {
                        render.reset();
                        crossterm::execute!(
                            std::io::stdout(),
                            crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
                            crossterm::cursor::MoveTo(0, 0)
                        )
                        .ok();
                        print_banner(model_info, stats);
                        render.needs_status_bar = true;
                    }

                    // Ignore other key combinations
                    _ => {}
                }
            }
            Event::Resize(_, _) => {
                // Terminal was resized — the previous rendered line
                // count is now stale.  Force a status bar reprint so
                // the user gets a clean visual anchor.
                render.reset();
                render.needs_status_bar = true;
            }
            _ => {}
        }
    }
}

// ── Input handling ──────────────────────────────────────────

/// Handle submitted input. Returns true if the loop should break (quit).
pub(crate) async fn handle_input(
    input: &str,
    commands: &mut Commands,
    conversation: &mut Vec<ConversationEntry>,
    current_session: &mut crate::session::Session,
    stats: &SessionStats,
    model_info: &ModelInfo,
    toast_manager: &mut ToastManager,
) -> bool {
    // Handle slash commands
    if input.starts_with('/') {
        if let Some(action) = handle_slash_command(input) {
            return handle_repl_action(
                &action,
                commands,
                conversation,
                current_session,
                stats,
                model_info,
                toast_manager,
            )
            .await;
        }
        return false;
    }

    // Handle plain text shortcuts
    match input.to_lowercase().as_str() {
        "exit" | "quit" | "q" => return true,
        "help" | "h" => print_help(),
        "new" | "n" => {
            if !current_session.messages.is_empty() {
                let _ = crate::session::save(current_session);
            }
            let cwd = std::env::current_dir()
                .unwrap_or_default()
                .display()
                .to_string();
            *current_session =
                crate::session::create(&cwd, &model_info.provider, &model_info.model);
            conversation.clear();
            stats.reset();
            if let Err(e) = commands.restart_session().await {
                tracing::warn!(error = %e, "session reset through runtime failed");
            }

            crossterm::execute!(
                std::io::stdout(),
                crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
                crossterm::cursor::MoveTo(0, 0)
            )
            .ok();
            print_banner(model_info, stats);
            inline::print_blank();
            inline::print_line(&components::success_badge("New session started."));
            inline::print_blank();
            print_status_bar(model_info, stats);

            // Show toast notification
            toast_manager.add(Toast::success("New session started"));
        }
        _ => {
            process_message(
                input,
                commands,
                conversation,
                current_session,
                stats,
                model_info,
            )
            .await;
        }
    }

    false
}

// ── REPL actions ────────────────────────────────────────────

pub(crate) enum ReplAction {
    Quit,
    NewSession,
    Config,
    History,
    Tools,
    Stats,
    Provider(String),
    Models,
    ModelsSwitch(String),
    Sessions,
    SessionsResume(String),
    Mcp,
    Plan(String),
    Skills,
    SkillsLoad(String),
    Search(String),
    Image(String),
    QueueShow,
    QueueSet(QueueMode),
    ThoughtsToggle,
    TimelineShow,
    TimelineSet(TimelineStyle),
}

pub(crate) fn handle_slash_command(input: &str) -> Option<ReplAction> {
    let parts: Vec<&str> = input.splitn(2, ' ').collect();
    let cmd = parts[0];
    let arg = parts
        .get(1)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    match cmd {
        "/quit" | "/q" | "/exit" => Some(ReplAction::Quit),
        "/help" | "/h" => {
            print_help();
            None
        }
        "/new" | "/n" | "/clear" | "/cls" => Some(ReplAction::NewSession),
        "/config" | "/cfg" => Some(ReplAction::Config),
        "/history" | "/hist" => Some(ReplAction::History),
        "/tools" | "/t" => Some(ReplAction::Tools),
        "/stats" => Some(ReplAction::Stats),
        "/mcp" => Some(ReplAction::Mcp),
        "/provider" if !arg.is_empty() => Some(ReplAction::Provider(arg)),
        "/provider" => {
            inline::print_blank();
            inline::print_line(&components::warning_badge("Usage: /provider <name>"));
            inline::print_blank();
            None
        }
        "/models" | "/m" if !arg.is_empty() => Some(ReplAction::ModelsSwitch(arg)),
        "/models" | "/m" => Some(ReplAction::Models),
        "/sessions" | "/ss" if !arg.is_empty() => Some(ReplAction::SessionsResume(arg)),
        "/sessions" | "/ss" => Some(ReplAction::Sessions),
        "/plan" if !arg.is_empty() => Some(ReplAction::Plan(arg)),
        "/plan" => {
            inline::print_blank();
            inline::print_line(&components::warning_badge("Usage: /plan <goal>"));
            inline::print_blank();
            None
        }
        "/skills" | "/skill" | "/sk" if !arg.is_empty() => Some(ReplAction::SkillsLoad(arg)),
        "/skills" => Some(ReplAction::Skills),
        "/skill" | "/sk" => Some(ReplAction::Skills),
        "/search" | "/find" if !arg.is_empty() => Some(ReplAction::Search(arg)),
        "/search" | "/find" => {
            inline::print_blank();
            inline::print_line(&components::warning_badge(
                "Usage: /search <query>  (case-insensitive substring match over conversation memory)",
            ));
            inline::print_blank();
            None
        }
        "/image" | "/img" if !arg.is_empty() => Some(ReplAction::Image(arg)),
        "/image" | "/img" => {
            inline::print_blank();
            inline::print_line(&components::warning_badge(
                "Usage: /image <path | data: url | http(s) url>",
            ));
            inline::print_blank();
            None
        }
        "/queue" if !arg.is_empty() => match arg.as_str() {
            "interrupt" => Some(ReplAction::QueueSet(QueueMode::Interrupt)),
            "follow-up" => Some(ReplAction::QueueSet(QueueMode::FollowUp)),
            _ => {
                inline::print_blank();
                inline::print_line(&components::warning_badge(
                    "Usage: /queue [follow-up | interrupt]",
                ));
                inline::print_blank();
                None
            }
        },
        "/queue" => Some(ReplAction::QueueShow),
        "/thoughts" | "/thinking" => Some(ReplAction::ThoughtsToggle),
        "/timeline" if !arg.is_empty() => match arg.as_str() {
            "detail" | "detailed" | "full" => {
                Some(ReplAction::TimelineSet(TimelineStyle::Detailed))
            }
            "compact" | "short" | "min" => Some(ReplAction::TimelineSet(TimelineStyle::Compact)),
            _ => {
                inline::print_blank();
                inline::print_line(&components::warning_badge(
                    "Usage: /timeline [detailed | compact]",
                ));
                inline::print_blank();
                None
            }
        },
        "/timeline" => Some(ReplAction::TimelineShow),
        _ => {
            inline::print_blank();
            inline::print_line(&components::error_badge(&format!(
                "Unknown command: {}",
                cmd
            )));
            inline::print_line(&Line::from(vec![
                RSpan::raw("  Type "),
                RSpan::styled(
                    "/help",
                    RStyle::default()
                        .fg(Color::Rgb(255, 215, 0))
                        .add_modifier(Modifier::BOLD),
                ),
                RSpan::raw(" for available commands."),
            ]));
            inline::print_blank();
            None
        }
    }
}

/// Handle a ReplAction. Returns true if the loop should break (quit).
pub(crate) async fn handle_repl_action(
    action: &ReplAction,
    commands: &mut Commands,
    conversation: &mut Vec<ConversationEntry>,
    current_session: &mut crate::session::Session,
    stats: &SessionStats,
    model_info: &ModelInfo,
    toast_manager: &mut ToastManager,
) -> bool {
    match action {
        ReplAction::Quit => return true,

        ReplAction::NewSession => {
            if !current_session.messages.is_empty() {
                if let Err(e) = crate::session::save(current_session) {
                    inline::print_line(&components::warning_badge(&format!(
                        "Could not auto-save session: {}",
                        e
                    )));
                }
            }
            let cwd = std::env::current_dir()
                .unwrap_or_default()
                .display()
                .to_string();
            *current_session =
                crate::session::create(&cwd, &model_info.provider, &model_info.model);
            conversation.clear();
            stats.reset();
            if let Err(e) = commands.restart_session().await {
                tracing::warn!(error = %e, "session reset through runtime failed");
            }

            crossterm::execute!(
                std::io::stdout(),
                crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
                crossterm::cursor::MoveTo(0, 0)
            )
            .ok();
            print_banner(model_info, stats);
            inline::print_blank();
            inline::print_line(&components::success_badge("New session started."));
            inline::print_blank();
            print_status_bar(model_info, stats);

            // Show toast notification
            toast_manager.add(Toast::success("New session started"));
        }

        ReplAction::Config => {
            commands.config_show_inline();
        }

        ReplAction::History => {
            show_history(conversation);
        }

        ReplAction::Tools => {
            commands.list_tools().await;
        }

        ReplAction::Stats => {
            show_stats(stats, model_info);
        }

        ReplAction::Sessions => {
            show_sessions();
        }

        ReplAction::SessionsResume(id) => {
            match crate::session::load(id) {
                Ok(loaded) => {
                    if !current_session.messages.is_empty() {
                        let _ = crate::session::save(current_session);
                    }
                    conversation.clear();
                    for msg in &loaded.messages {
                        conversation.push(ConversationEntry {
                            role: msg.role.clone(),
                            content: msg.content.clone(),
                            timestamp: chrono::DateTime::parse_from_rfc3339(&msg.timestamp)
                                .map(|dt| dt.with_timezone(&chrono::Local))
                                .unwrap_or_else(|_| chrono::Local::now()),
                        });
                    }
                    *current_session = loaded;
                    if let Err(e) = commands.restart_session().await {
                        tracing::warn!(error = %e, "session reset through runtime failed");
                    }
                    stats.reset();

                    crossterm::execute!(
                        std::io::stdout(),
                        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
                        crossterm::cursor::MoveTo(0, 0)
                    )
                    .ok();
                    print_banner(model_info, stats);
                    inline::print_blank();
                    inline::print_line(&components::success_badge(&format!(
                        "Resumed: {} ({} messages)",
                        current_session.title,
                        current_session.messages.len()
                    )));
                    inline::print_blank();
                    print_status_bar(model_info, stats);

                    // Show toast notification
                    toast_manager.add(Toast::success(format!(
                        "Session resumed: {}",
                        current_session.title
                    )));
                }
                Err(e) => {
                    inline::print_blank();
                    inline::print_line(&components::error_badge(&format!(
                        "Failed to load session: {}",
                        e
                    )));
                    inline::print_blank();

                    // Show toast notification
                    toast_manager.add(Toast::error(format!("Failed to load session: {}", e)));
                }
            }
        }

        ReplAction::Provider(name) => {
            inline::print_blank();
            inline::print_line(&components::warning_badge(
                "Provider switching not yet supported in REPL.",
            ));
            inline::print_line(&Line::from(vec![
                RSpan::raw("  Use: "),
                RSpan::styled(
                    "takka config edit",
                    RStyle::default().add_modifier(Modifier::BOLD),
                ),
                RSpan::raw(" to change providers."),
            ]));
            inline::print_blank();
            let _ = name;
        }

        ReplAction::Models => {
            // dialoguer needs cooked mode (it controls the terminal itself)
            disable_raw_mode().ok();
            let result = commands.pick_model_interactive_inline();
            enable_raw_mode().ok();
            if let Some((provider, model)) = result {
                inline::print_blank();
                inline::print_line(&components::success_badge(&format!(
                    "Switched to {} / {}",
                    provider, model
                )));
                inline::print_blank();
                print_status_bar(model_info, stats);

                // Show toast notification
                toast_manager.add(Toast::success(format!(
                    "Switched to {} / {}",
                    provider, model
                )));
            }
        }

        ReplAction::ModelsSwitch(name) => {
            match commands.switch_model(name) {
                Ok((provider, model)) => {
                    inline::print_blank();
                    inline::print_line(&components::success_badge(&format!(
                        "Switched to {} / {}",
                        provider, model
                    )));
                    inline::print_blank();

                    // Show toast notification
                    toast_manager.add(Toast::success(format!(
                        "Switched to {} / {}",
                        provider, model
                    )));
                }
                Err(e) => {
                    inline::print_blank();
                    inline::print_line(&components::error_badge(&e.to_string()));
                    inline::print_line(&Line::from(vec![
                        RSpan::raw("  Use "),
                        RSpan::styled("/models", RStyle::default().add_modifier(Modifier::BOLD)),
                        RSpan::raw(" to see available models."),
                    ]));
                    inline::print_blank();

                    // Show toast notification
                    toast_manager.add(Toast::error(e.to_string()));
                }
            }
        }

        ReplAction::Mcp => {
            commands.show_mcp_status();
        }

        ReplAction::Plan(goal) => {
            conversation.push(ConversationEntry {
                role: "user".into(),
                content: format!("[plan] {}", goal),
                timestamp: chrono::Local::now(),
            });
            stats.increment_messages();

            print_turn_separator(model_info);
            let start = Instant::now();
            if let Err(e) = commands.plan_inline(goal).await {
                inline::print_blank();
                inline::print_line(&components::error_badge(&e.to_string()));
                inline::print_blank();
            } else {
                let elapsed = start.elapsed();
                conversation.push(ConversationEntry {
                    role: "assistant".into(),
                    content: format!("(plan executed in {:.1}s)", elapsed.as_secs_f64()),
                    timestamp: chrono::Local::now(),
                });
                print_response_summary(stats);
            }
        }

        ReplAction::Skills => {
            commands.skill_command(&SkillAction::List).ok();
        }

        ReplAction::SkillsLoad(name) => {
            use ratatui::style::{Color, Modifier, Style};
            use ratatui::text::{Line, Span};

            inline::print_blank();
            inline::print_line(&components::section_header(
                "⚡",
                &format!("Activating skill: {}", name),
                Color::Rgb(255, 215, 0),
            ));
            inline::print_blank();

            // Load & activate: inject instructions into orchestrator context
            match commands.load_and_activate_skill(name).await {
                Ok(body) => {
                    // Success — show activation badge + preview
                    inline::print_line(&Line::from(vec![
                        Span::styled("  ✅ ", Style::default().fg(Color::Rgb(46, 204, 113))),
                        Span::styled(
                            format!(
                                "Skill '{}' activated — instructions injected into agent context.",
                                name
                            ),
                            Style::default()
                                .fg(Color::Rgb(180, 180, 200))
                                .add_modifier(Modifier::DIM),
                        ),
                    ]));
                    inline::print_blank();
                    inline::print_line(&Line::from(vec![Span::styled(
                        "  📖 Preview:",
                        Style::default()
                            .fg(Color::Rgb(241, 196, 15))
                            .add_modifier(Modifier::BOLD),
                    )]));
                    let preview: Vec<&str> = body.lines().take(5).collect();
                    for line in &preview {
                        inline::print_line(&Line::from(vec![
                            Span::raw("     "),
                            Span::styled(*line, Style::default().fg(Color::Rgb(180, 180, 200))),
                        ]));
                    }
                    if body.lines().count() > 5 {
                        inline::print_line(&Line::from(vec![
                            Span::raw("     "),
                            Span::styled("...", Style::default().fg(Color::Rgb(100, 100, 120))),
                        ]));
                    }
                    inline::print_blank();
                    inline::print_line(&Line::from(vec![
                        Span::raw("  💡 "),
                        Span::styled(
                            "Now send a message — the skill instructions will be included as context.",
                            Style::default()
                                .fg(Color::Rgb(100, 100, 120))
                                .add_modifier(Modifier::DIM),
                        ),
                    ]));
                }
                Err(e) => {
                    inline::print_line(&components::warning_badge(&format!(
                        "Failed to activate skill '{}': {}",
                        name, e
                    )));
                }
            }
            inline::print_blank();
        }

        ReplAction::Search(query) => {
            commands.search_memory_inline(query).await;
        }

        ReplAction::Image(path) => {
            commands.attach_image_inline(path);
        }

        ReplAction::QueueShow => {
            show_queue_status(commands);
        }

        ReplAction::QueueSet(mode) => {
            commands.set_queue_mode(*mode);
            inline::print_blank();
            inline::print_line(&components::success_badge(&format!(
                "Queue mode set to {} — {}",
                mode.as_str(),
                mode.description()
            )));
            show_queue_status(commands);
        }

        ReplAction::TimelineShow => {
            show_timeline_status(commands);
        }

        ReplAction::TimelineSet(style) => {
            commands.set_timeline_style(*style);
            inline::print_blank();
            inline::print_line(&components::success_badge(&format!(
                "Timeline set to {} — {}",
                style.as_str(),
                style.description()
            )));
            inline::print_blank();
        }

        ReplAction::ThoughtsToggle => {
            let enabled = !commands.show_thoughts();
            commands.set_show_thoughts(enabled);
            inline::print_blank();
            inline::print_line(&components::success_badge(if enabled {
                "Reasoning shown — the model's pre-tool thinking is printed DIM."
            } else {
                "Reasoning hidden — only answers and tool activity are shown."
            }));
            inline::print_blank();
        }
    }

    false
}

// ── Message processing ────────────────────────────────────

/// Process a single user message through the agent.
pub(crate) async fn process_message(
    input: &str,
    commands: &mut Commands,
    conversation: &mut Vec<ConversationEntry>,
    current_session: &mut crate::session::Session,
    stats: &SessionStats,
    model_info: &ModelInfo,
) {
    conversation.push(ConversationEntry {
        role: "user".into(),
        content: input.to_string(),
        timestamp: chrono::Local::now(),
    });
    stats.increment_messages();

    crate::session::push_message(current_session, "user", input);

    print_turn_separator(model_info);
    inline::print_line(&components::dotted_separator(Color::Rgb(60, 60, 80)));
    inline::print_blank();

    let start = Instant::now();
    // Ctrl+C: main.rs's signal task flips the shared CANCEL_FLAG; the
    // shared run loop (`client::run_turn`) forwards it as
    // `Request::Cancel`, the engine routes it into the orchestrator's
    // cancel handle, and the run unwinds at the next turn boundary.
    let result = commands.run(input).await;

    if let Err(e) = result {
        inline::print_blank();
        inline::print_line(&components::error_badge(&e.to_string()));
        inline::print_blank();
    } else {
        let elapsed = start.elapsed();
        // Real provider usage when the runtime reported it, otherwise the
        // ~4-chars-per-token estimate.
        let run = commands.last_run_stats();
        let measured = run.tokens_are_measured();
        let (input_tokens, output_tokens) = run.tokens();
        stats.add_input_tokens(input_tokens);
        stats.add_output_tokens(output_tokens);
        stats.add_cache_read_tokens(run.cache_read_tokens);
        stats.add_cache_creation_tokens(run.cache_creation_tokens);
        stats.add_tool_calls(run.tool_calls);
        // H-06: the ceiling bounds one run, so the budget chip reads this.
        stats.set_run_cost_usd(run.cost_usd);

        conversation.push(ConversationEntry {
            role: "assistant".into(),
            content: format!("(response in {:.1}s)", elapsed.as_secs_f64()),
            timestamp: chrono::Local::now(),
        });

        crate::session::push_message(
            current_session,
            "assistant",
            &format!("(response in {:.1}s)", elapsed.as_secs_f64()),
        );
        let _ = crate::session::save(current_session);

        // One-line run summary: outcome, wall time, tools, this run's tokens.
        let done_line = spinner::run_summary_line(
            elapsed.as_millis(),
            input_tokens,
            output_tokens,
            run.tool_calls,
            measured,
        );
        inline::print_line(&done_line);
        inline::print_blank();

        print_response_summary(stats);
    }
}
