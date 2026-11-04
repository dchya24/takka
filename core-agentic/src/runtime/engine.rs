use super::protocol::{
    ConnectionCapabilities, InitOverrides, NotificationKind, ProtocolEvent, Request, PROTOCOL_NAME,
    PROTOCOL_VERSION,
};
use super::transport::Transport;
use crate::events::Event;
use crate::providers::{LLMProvider, OpenAIProvider};
use crate::{
    Config, Orchestrator, QuestionAnswer, QuestionHandler, QuestionPrompt, SkillTool,
    SpawnSubagentTool, TodoChangeHandler, TodoItem, ToolDeps, ToolRegistry,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

/// Single choke point for every engine event write: opt-out filtering is
/// applied here AND only here. The `NotificationKind` allowlist guarantees
/// interactive gates and per-request responses are never filtered — an
/// opt-out cannot kill a run in progress. Clones share the same opt-out
/// set, so filtering is global across the whole engine (handler sites
/// included).
pub struct EventSink<T: Transport> {
    transport: Arc<T>,
    opt_outs: Arc<Mutex<std::collections::HashSet<NotificationKind>>>,
}

impl<T: Transport> EventSink<T> {
    pub fn new(
        transport: Arc<T>,
        opt_outs: Arc<Mutex<std::collections::HashSet<NotificationKind>>>,
    ) -> Self {
        Self {
            transport,
            opt_outs,
        }
    }

    /// Replace the opt-out set (connection-level, set at `Init`).
    pub fn set_opt_outs(&self, kinds: &[NotificationKind]) {
        *self.opt_outs.lock().unwrap() = kinds.iter().copied().collect();
    }

    fn suppressed(&self, event: &Event) -> bool {
        let kind = match event {
            Event::Thinking { .. } => NotificationKind::Thinking,
            Event::AssistantDelta { .. } => NotificationKind::AssistantDelta,
            Event::Warning { .. } => NotificationKind::Warning,
            Event::TodoChanged { .. } => NotificationKind::TodoChanged,
            _ => return false,
        };
        self.opt_outs.lock().unwrap().contains(&kind)
    }

    pub fn send(&self, request_id: Option<String>, event: Event) {
        if self.suppressed(&event) {
            return;
        }
        if let Err(error) = self
            .transport
            .write_event(&ProtocolEvent::new(request_id, event))
        {
            tracing::warn!(%error, "failed to write runtime event");
        }
    }
}

impl<T: Transport> Clone for EventSink<T> {
    fn clone(&self) -> Self {
        Self {
            transport: self.transport.clone(),
            opt_outs: self.opt_outs.clone(),
        }
    }
}

pub struct RuntimeEngine<T: Transport> {
    transport: Arc<T>,
    sink: EventSink<T>,
    orchestrator: Option<Arc<Orchestrator>>,
    /// Strict initialization gate: only a successful `Request::Init` (or
    /// an embedded `with_session`/`with_provider` constructor) opens the
    /// resource requests below.
    initialized: bool,
    /// Capabilities declared at `Init` — immutable afterwards.
    capabilities: ConnectionCapabilities,
    /// Daemon shared secret (Fase 3a). `None` (default) = non-daemon:
    /// `Init` tokens are ignored entirely (jalur lama).
    daemon_token: Option<String>,
    /// Adopt-on-re-init (Fase 3a, daemon-only): ketika true, `Init`
    /// kedua yang kompatibel meng-adopt session berjalan alih-alih
    /// ditolak strict. Default `false` — embedded/spawned tidak berubah.
    adopt_on_reinit: bool,
    /// Overrides yang BERJALAN: disimpan saat init pertama sukses saja
    /// (adopt tidak menimpa) — pembanding untuk mismatch config.
    current_overrides: InitOverrides,
    /// Discovered skill index — populated by [`Self::initialize`], used
    /// by `Request::SkillActivate` (resolve + inject).
    skill_index: Option<Arc<std::sync::RwLock<crate::SkillIndex>>>,
    /// Steer queue (Fase 2b): `Request::Steer` pushes here while a run
    /// is active; the orchestrator's steering source drains the whole
    /// FIFO at each iteration boundary. One shared `Arc` connects the
    /// engine arm to the orchestrator hook, wherever it was built.
    steer_queue: Arc<Mutex<std::collections::VecDeque<String>>>,
    running: Arc<AtomicBool>,
    current_request_id: Arc<Mutex<Option<String>>>,
    pending_question: Arc<Mutex<Option<mpsc::Sender<Vec<QuestionAnswer>>>>>,
    pending_confirmation: Arc<Mutex<Option<mpsc::Sender<bool>>>>,
    run_threads: Vec<std::thread::JoinHandle<()>>,
}

impl<T: Transport> RuntimeEngine<T> {
    pub fn new(transport: T) -> Self {
        let transport = Arc::new(transport);
        let sink = EventSink::new(
            transport.clone(),
            Arc::new(Mutex::new(std::collections::HashSet::new())),
        );
        Self {
            transport,
            sink,
            orchestrator: None,
            initialized: false,
            capabilities: ConnectionCapabilities::default(),
            daemon_token: None,
            adopt_on_reinit: false,
            current_overrides: InitOverrides::default(),
            skill_index: None,
            steer_queue: Arc::new(Mutex::new(std::collections::VecDeque::new())),
            running: Arc::new(AtomicBool::new(false)),
            current_request_id: Arc::new(Mutex::new(None)),
            pending_question: Arc::new(Mutex::new(None)),
            pending_confirmation: Arc::new(Mutex::new(None)),
            run_threads: Vec::new(),
        }
    }

    pub fn with_session(transport: T, provider: Arc<dyn LLMProvider>, tools: ToolRegistry) -> Self {
        let mut engine = Self::new(transport);
        let mut orchestrator = Orchestrator::new(provider, tools);
        engine.install_steering_source(&mut orchestrator);
        engine.orchestrator = Some(Arc::new(orchestrator));
        engine.initialized = true;
        engine
    }

    /// Declare connection capabilities for an embedded session — the
    /// in-process equivalent of the wire `Init.capabilities`. Must be
    /// called before `run()`; capabilities stay immutable once the loop
    /// serves requests (a second init is rejected).
    pub fn with_capabilities(mut self, capabilities: ConnectionCapabilities) -> Self {
        self.sink.set_opt_outs(&capabilities.opt_out_notify);
        self.capabilities = capabilities;
        self
    }

    /// Declare the daemon shared secret (Fase 3a). `None` (default)
    /// berarti engine bukan daemon: token pada `Init` di-ignored dan
    /// jalur strict lama tetap berlaku.
    pub fn set_daemon_token(&mut self, token: Option<String>) {
        self.daemon_token = token;
    }

    /// Enable adopt-on-re-init (Fase 3a, daemon-only): a second
    /// compatible `Init` adopts the running session instead of being
    /// rejected. Default `false`.
    pub fn set_adopt_on_reinit(&mut self, enabled: bool) {
        self.adopt_on_reinit = enabled;
    }

    pub fn with_provider(transport: T, provider: Arc<dyn LLMProvider>) -> Self {
        let mut engine = Self::new(transport);
        let question_handler = RuntimeQuestionHandler {
            sink: engine.sink.clone(),
            current_request_id: engine.current_request_id.clone(),
            pending: engine.pending_question.clone(),
        };
        let todo_handler = RuntimeTodoHandler {
            sink: engine.sink.clone(),
            current_request_id: engine.current_request_id.clone(),
        };
        let mut deps = ToolDeps::new();
        deps.question_handler = Some(Arc::new(question_handler));
        deps.todo_handler = Some(Box::new(todo_handler));
        let tools = ToolRegistry::new();
        for tool in crate::tools::builtin_tools_with_deps(deps) {
            tools.register(tool);
        }
        let mut orchestrator = Orchestrator::new(provider, tools);
        let sink = engine.sink.clone();
        let current_request_id = engine.current_request_id.clone();
        let pending_confirmation = engine.pending_confirmation.clone();
        orchestrator.set_confirmation_handler(move |request| {
            let (sender, receiver) = mpsc::channel();
            *pending_confirmation.lock().unwrap() = Some(sender);
            let request_id = current_request_id.lock().unwrap().clone();
            sink.send(
                request_id,
                Event::ConfirmationRequest {
                    action: request.action,
                    description: request.description,
                    risk_level: request.risk_level.as_str().to_string(),
                },
            );
            receiver.recv().unwrap_or(false)
        });
        engine.install_steering_source(&mut orchestrator);
        engine.orchestrator = Some(Arc::new(orchestrator));
        engine.initialized = true;
        engine
    }

    pub fn run(&mut self) {
        self.emit(
            None,
            Event::Ready {
                protocol: PROTOCOL_NAME.to_string(),
                version: PROTOCOL_VERSION,
            },
        );

        loop {
            let request = match self.transport.read_request() {
                Ok(Some(request)) => request,
                Ok(None) => break,
                Err(error) => {
                    self.emit(
                        None,
                        Event::Error {
                            message: format!("protocol_error: {error}"),
                        },
                    );
                    continue;
                }
            };
            if request.v != PROTOCOL_VERSION {
                self.emit(
                    Some(request.id),
                    Event::Error {
                        message: format!("unsupported protocol version: {}", request.v),
                    },
                );
                continue;
            }

            match request.request {
                Request::Init {
                    overrides,
                    capabilities,
                    daemon_token,
                } => {
                    // Daemon token (Fase 3a): dicek SEBELUM jalur
                    // init/adopt mana pun — token salah tidak pernah
                    // menyentuh session.
                    if let Some(expected) = &self.daemon_token {
                        if daemon_token.as_deref() != Some(expected.as_str()) {
                            self.emit(
                                Some(request.id),
                                Event::Error {
                                    message: "unauthorized".to_string(),
                                },
                            );
                            continue;
                        }
                    }
                    if self.initialized && !self.adopt_on_reinit {
                        self.emit(
                            Some(request.id),
                            Event::Error {
                                message: "already initialized".to_string(),
                            },
                        );
                        continue;
                    }
                    if self.initialized {
                        // Adopt path (daemon-only): frontend yang reconnect
                        // mengambil alih session berjalan. Caps dan opt-out
                        // TIDAK ditimpa — engine berjalan tetap menang
                        // (opt-out baru di-ignore, bukan fatal).
                        if !self.capabilities.compatible_caps(&capabilities) {
                            self.emit(
                                Some(request.id),
                                Event::Error {
                                    message: "capability 'experimental_api' required".to_string(),
                                },
                            );
                            continue;
                        }
                        if self.current_overrides != overrides {
                            self.emit(
                                Some(request.id),
                                Event::Error {
                                    message:
                                        "daemon already running with different config; restart daemon or match its config"
                                            .to_string(),
                                },
                            );
                            continue;
                        }
                        let session_id = self.orchestrator.as_ref().and_then(|o| o.session_id());
                        self.emit(
                            Some(request.id.clone()),
                            Event::InitOk {
                                protocol: PROTOCOL_NAME.to_string(),
                                version: PROTOCOL_VERSION,
                            },
                        );
                        self.emit(Some(request.id), Event::SessionAdopted { session_id });
                        continue;
                    }
                    match self.initialize(overrides.clone()) {
                        Ok(()) => {
                            self.current_overrides = overrides;
                            self.capabilities = capabilities.clone();
                            self.sink.set_opt_outs(&capabilities.opt_out_notify);
                            self.initialized = true;
                            self.emit(
                                Some(request.id),
                                Event::InitOk {
                                    protocol: PROTOCOL_NAME.to_string(),
                                    version: PROTOCOL_VERSION,
                                },
                            );
                        }
                        Err(message) => self.emit(Some(request.id), Event::Error { message }),
                    }
                }
                Request::Run { task, attachments } => {
                    if !self.initialized {
                        self.emit(
                            Some(request.id),
                            Event::Error {
                                message: "not initialized".to_string(),
                            },
                        );
                        continue;
                    }
                    self.start_run(request.id, task, attachments);
                }
                Request::Cancel => {
                    if let Some(orchestrator) = self.orchestrator.as_ref() {
                        orchestrator.cancel_handle().store(true, Ordering::SeqCst);
                    }
                }
                Request::Steer { content } => {
                    if !self.initialized {
                        self.emit(
                            Some(request.id),
                            Event::Error {
                                message: "not initialized".to_string(),
                            },
                        );
                    } else if !self.running.load(Ordering::SeqCst) {
                        self.emit(
                            Some(request.id),
                            Event::Error {
                                message: "no active run".to_string(),
                            },
                        );
                    } else {
                        self.steer_queue.lock().unwrap().push_back(content.clone());
                        self.emit(Some(request.id), Event::SteerQueued { content });
                    }
                }
                Request::ResetSession => {
                    if let Some(orchestrator) = self.orchestrator.as_ref() {
                        orchestrator.clear_memory();
                        orchestrator.reset_cancel();
                        orchestrator.clear_event_handlers();
                        self.emit(Some(request.id), Event::SessionReset);
                    } else {
                        self.emit(
                            Some(request.id),
                            Event::Error {
                                message: "not initialized".to_string(),
                            },
                        );
                    }
                }
                Request::SearchMemory { query } => {
                    let Some(orchestrator) = self.orchestrator.as_ref() else {
                        self.emit(
                            Some(request.id),
                            Event::Error {
                                message: "not initialized".to_string(),
                            },
                        );
                        continue;
                    };
                    let matches = orchestrator
                        .search_memory(&query)
                        .into_iter()
                        .map(|(role, content)| crate::events::MemorySearchMatch {
                            role: role.as_str().to_string(),
                            content,
                        })
                        .collect();
                    self.emit(
                        Some(request.id),
                        Event::MemorySearchResult { query, matches },
                    );
                }
                Request::AddSystemMessage { content } => {
                    if let Some(orchestrator) = self.orchestrator.as_ref() {
                        orchestrator.add_system_message(content);
                    } else {
                        self.emit(
                            Some(request.id),
                            Event::Error {
                                message: "not initialized".to_string(),
                            },
                        );
                    }
                }
                Request::ListTools => {
                    let Some(orchestrator) = self.orchestrator.as_ref() else {
                        self.emit(
                            Some(request.id),
                            Event::Error {
                                message: "not initialized".to_string(),
                            },
                        );
                        continue;
                    };
                    let tools = orchestrator
                        .tool_registry()
                        .list()
                        .into_iter()
                        .map(|schema| crate::events::ToolInfo {
                            name: schema.name,
                            description: schema.description,
                        })
                        .collect();
                    self.emit(Some(request.id), Event::ToolList { tools });
                }
                Request::SkillActivate { name } => {
                    self.handle_skill_activate(request.id, name);
                }
                Request::Plan {
                    goal,
                    require_approval,
                } => {
                    if !self.initialized {
                        self.emit(
                            Some(request.id),
                            Event::Error {
                                message: "not initialized".to_string(),
                            },
                        );
                        continue;
                    }
                    if !self.capabilities.experimental_api {
                        self.emit(
                            Some(request.id),
                            Event::Error {
                                message: "capability 'experimental_api' required".to_string(),
                            },
                        );
                        continue;
                    }
                    self.handle_plan(request.id, goal, require_approval);
                }
                Request::Shutdown => break,
                Request::QuestionResponse { answers, .. } => {
                    if let Some(sender) = self.pending_question.lock().unwrap().take() {
                        let _ = sender.send(answers);
                    } else {
                        self.emit(
                            Some(request.id),
                            Event::Warning {
                                message: "no question response is pending".to_string(),
                            },
                        );
                    }
                }
                Request::ConfirmResponse { approved, .. } => {
                    if let Some(sender) = self.pending_confirmation.lock().unwrap().take() {
                        let _ = sender.send(approved);
                    } else {
                        self.emit(
                            Some(request.id),
                            Event::Warning {
                                message: "no confirmation response is pending".to_string(),
                            },
                        );
                    }
                }
            }
        }

        for handle in self.run_threads.drain(..) {
            let _ = handle.join();
        }
    }

    fn initialize(&mut self, overrides: super::protocol::InitOverrides) -> Result<(), String> {
        let config = match overrides.config_path.as_deref() {
            Some(path) => Config::load_from_path(path)
                .ok_or_else(|| format!("failed to load config from: {path}"))?,
            None => Config::load().unwrap_or_else(Config::fallback),
        };
        let provider_config = config
            .to_provider_config()
            .ok_or_else(|| "no provider configured".to_string())?;
        let model = overrides
            .model
            .clone()
            .unwrap_or_else(|| provider_config.default_model.clone());
        let provider: Arc<dyn LLMProvider> = Arc::new(OpenAIProvider::new(provider_config));

        let mut deps = self.runtime_tool_deps();
        deps.url_policy = config.url_policy();
        let tools = ToolRegistry::new();
        for tool in crate::tools::builtin_tools_with_deps(deps) {
            tools.register(tool);
        }
        tools.register(Box::new(
            SpawnSubagentTool::new(provider.clone(), tools.clone(), model.clone())
                .with_mode(overrides.permission_mode.unwrap_or_default()),
        ));

        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let discovery_config: crate::DiscoveryConfig = (&config.skills).into();
        let skill_index = Arc::new(std::sync::RwLock::new(crate::discover_skills(
            &discovery_config,
        )));
        self.skill_index = Some(skill_index.clone());
        tools.register(Box::new(SkillTool::new(skill_index.clone())));

        let project_instructions = crate::load_project_instructions(&cwd).map(|(_, text)| text);
        let skills_section = {
            let index = skill_index.read().unwrap();
            let pairs: Vec<(&str, &str)> = index
                .all()
                .iter()
                .map(|skill| (skill.name(), skill.description()))
                .collect();
            crate::skills_system_section(&pairs)
        };
        let prompt_override = overrides
            .system_prompt
            .as_deref()
            .or(config.system_prompt.as_deref());
        let mut system_prompt = crate::assemble_system_prompt(
            None,
            project_instructions.as_deref(),
            skills_section.as_deref(),
            prompt_override,
        );
        if let Some(memory) = crate::assemble_memory_section(&cwd) {
            system_prompt.push_str("\n\n---\n# Persistent Memory\n\n");
            system_prompt.push_str(&memory);
        }

        let mut orchestrator = Orchestrator::new(provider, tools);
        orchestrator.set_model(model);
        orchestrator.set_system_prompt(system_prompt);
        orchestrator.set_permission_mode(overrides.permission_mode.unwrap_or_default());
        if config.agent.auto_compact_with_llm {
            orchestrator.set_auto_compact_with_llm(true);
        }
        if let Some(summarizer) = config.agent.summarizer_model {
            orchestrator.set_summarizer_model(summarizer);
        }
        if let Some(max_iterations) = config.agent.max_iterations {
            orchestrator.set_max_iterations(max_iterations);
        }
        self.install_confirmation_handler(&mut orchestrator);
        self.install_steering_source(&mut orchestrator);
        self.orchestrator = Some(Arc::new(orchestrator));
        Ok(())
    }

    /// Bind the engine's steer queue to an orchestrator (Fase 2b): the
    /// steering source drains the whole FIFO at every iteration boundary
    /// (Fase 2b orchestrator poll). One shared `Arc` queue is used
    /// across the engine field and this closure.
    fn install_steering_source(&self, orchestrator: &mut Orchestrator) {
        let queue = self.steer_queue.clone();
        orchestrator
            .set_steering_source(Box::new(move || queue.lock().unwrap().drain(..).collect()));
    }

    fn runtime_tool_deps(&self) -> ToolDeps {
        let mut deps = ToolDeps::new();
        deps.question_handler = Some(Arc::new(RuntimeQuestionHandler {
            sink: self.sink.clone(),
            current_request_id: self.current_request_id.clone(),
            pending: self.pending_question.clone(),
        }));
        deps.todo_handler = Some(Box::new(RuntimeTodoHandler {
            sink: self.sink.clone(),
            current_request_id: self.current_request_id.clone(),
        }));
        deps
    }

    fn install_confirmation_handler(&self, orchestrator: &mut Orchestrator) {
        let sink = self.sink.clone();
        let current_request_id = self.current_request_id.clone();
        let pending_confirmation = self.pending_confirmation.clone();
        orchestrator.set_confirmation_handler(move |request| {
            let (sender, receiver) = mpsc::channel();
            *pending_confirmation.lock().unwrap() = Some(sender);
            let request_id = current_request_id.lock().unwrap().clone();
            sink.send(
                request_id,
                Event::ConfirmationRequest {
                    action: request.action,
                    description: request.description,
                    risk_level: request.risk_level.as_str().to_string(),
                },
            );
            receiver.recv().unwrap_or(false)
        });
    }

    /// `Request::SkillActivate`: resolve the skill from the daemon's
    /// discovered index, inject its instructions as a system message,
    /// and report the outcome.
    fn handle_skill_activate(&mut self, request_id: String, name: String) {
        let Some(orchestrator) = self.orchestrator.as_ref() else {
            self.emit(
                Some(request_id),
                Event::Error {
                    message: "not initialized".to_string(),
                },
            );
            return;
        };
        let Some(index) = self.skill_index.as_ref() else {
            self.emit(
                Some(request_id),
                Event::SkillActivatedResult {
                    skill: name.clone(),
                    activated: false,
                    message: Some("no skills discovered in this session".to_string()),
                    content: String::new(),
                },
            );
            return;
        };

        let skill = index.read().unwrap().get(&name).cloned();
        match skill {
            None => {
                self.emit(
                    Some(request_id),
                    Event::SkillActivatedResult {
                        skill: name.clone(),
                        activated: false,
                        message: Some(format!("skill '{name}' not found")),
                        content: String::new(),
                    },
                );
            }
            Some(skill) => {
                // Full content: SKILL.md body + referenced files, same
                // policy the CLI used in-process.
                let mut content = skill.body.clone();
                if let Ok(entries) = std::fs::read_dir(&skill.dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.is_file() {
                            let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                            if fname == "SKILL.md" {
                                continue;
                            }
                            if let Ok(text) = std::fs::read_to_string(&path) {
                                content.push_str(&format!(
                                    "\n\n---\n# Referenced file: {}\n\n{}",
                                    fname, text
                                ));
                            }
                        }
                    }
                }
                orchestrator.add_system_message(format!(
                    "---\n# Active Skill: {} — {}\n\n{}",
                    skill.name(),
                    skill.description(),
                    content
                ));
                self.emit(
                    Some(request_id),
                    Event::SkillActivatedResult {
                        skill: name.clone(),
                        activated: true,
                        message: None,
                        content,
                    },
                );
            }
        }
    }

    /// `Request::Plan`: full planner cycle on its own thread — LLM plan
    /// creation, approval gate (routed through the standard
    /// confirmation channel when `require_approval`), then execution.
    /// Planner events stream to the transport tagged with the request id.
    fn handle_plan(&mut self, request_id: String, goal: String, require_approval: bool) {
        let Some(orchestrator) = self.orchestrator.clone() else {
            self.emit(
                Some(request_id),
                Event::Error {
                    message: "not initialized".to_string(),
                },
            );
            return;
        };

        let provider = orchestrator.provider();
        let tools = orchestrator.tool_registry().clone();
        let sink = self.sink.clone();
        let pending_confirmation = self.pending_confirmation.clone();
        let current_request_id = self.current_request_id.clone();
        let running = self.running.clone();

        *self.current_request_id.lock().unwrap() = Some(request_id.clone());

        let handle = std::thread::spawn(move || {
            let write = |event: crate::events::Event| {
                sink.send(current_request_id.lock().unwrap().clone(), event);
            };

            let planner = crate::PlannerAgent::new(provider).with_config(crate::PlannerConfig {
                require_approval,
                ..Default::default()
            });

            // Forward planner lifecycle events to the transport.
            {
                let sink = sink.clone();
                let current_request_id = current_request_id.clone();
                planner.on(move |event| {
                    sink.send(current_request_id.lock().unwrap().clone(), event);
                });
            }

            // Approval gate: surface the rendered plan through the same
            // confirmation channel the CLI already answers.
            if require_approval {
                let pending = pending_confirmation.clone();
                let sink = sink.clone();
                let current_request_id = current_request_id.clone();
                planner.set_approval_callback(move |plan| {
                    let steps = plan
                        .steps
                        .iter()
                        .map(|step| crate::events::PlanStepInfo {
                            id: step.id.clone(),
                            description: step.description.clone(),
                        })
                        .collect();
                    sink.send(
                        current_request_id.lock().unwrap().clone(),
                        Event::PlanApprovalRequest {
                            plan_id: plan.id.clone(),
                            goal: plan.goal.clone(),
                            steps,
                        },
                    );
                    let (sender, receiver) = mpsc::channel();
                    *pending.lock().unwrap() = Some(sender);
                    receiver.recv().unwrap_or(false)
                });
            }

            let result = planner
                .create_plan(&goal, &tools)
                .and_then(|mut plan| planner.execute_plan(&mut plan, &tools));

            match result {
                Ok(plan_result) => write(Event::Done {
                    result: format!(
                        "plan '{}': {} step(s) completed, {} failed",
                        goal, plan_result.steps_completed, plan_result.steps_failed
                    ),
                }),
                Err(error) => write(Event::Error {
                    message: error.to_string(),
                }),
            }
            running.store(false, Ordering::SeqCst);
        });
        self.run_threads.push(handle);
    }

    fn start_run(&mut self, request_id: String, task: String, attachments: Vec<crate::Attachment>) {
        if self.running.swap(true, Ordering::SeqCst) {
            self.emit(
                Some(request_id),
                Event::Error {
                    message: "busy: another run is active".to_string(),
                },
            );
            return;
        }

        let Some(orchestrator) = self.orchestrator.clone() else {
            self.running.store(false, Ordering::SeqCst);
            self.emit(
                Some(request_id),
                Event::Error {
                    message: "not initialized".to_string(),
                },
            );
            return;
        };

        *self.current_request_id.lock().unwrap() = Some(request_id.clone());
        orchestrator.reset_cancel();
        orchestrator.clear_event_handlers();

        let sink = self.sink.clone();
        let running = self.running.clone();
        let current_request_id = self.current_request_id.clone();
        let event_request_id = request_id.clone();
        orchestrator.on_event(move |event| {
            let finished = match &event {
                Event::ToolOutput {
                    tool_call_id,
                    tool_name,
                    success,
                    ..
                } => Some(Event::ToolFinished {
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    success: *success,
                }),
                _ => None,
            };
            sink.send(Some(event_request_id.clone()), event);
            if let Some(finished) = finished {
                sink.send(Some(event_request_id.clone()), finished);
            }
        });

        let sink = self.sink.clone();
        self.run_threads.push(std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime engine tokio runtime");
            let delta_sink = sink.clone();
            let delta_request_id = request_id.clone();
            let result = runtime.block_on(orchestrator.run_stream_with_attachments(
                &task,
                attachments,
                move |content| {
                    delta_sink.send(
                        Some(delta_request_id.clone()),
                        Event::AssistantDelta { content },
                    );
                },
            ));

            let event = match result {
                Ok(result) => Event::Done { result },
                Err(error) => Event::Error {
                    message: error.to_string(),
                },
            };
            sink.send(Some(request_id), event);
            running.store(false, Ordering::SeqCst);
            *current_request_id.lock().unwrap() = None;
        }));
    }

    fn emit(&self, request_id: Option<String>, event: Event) {
        self.sink.send(request_id, event);
    }
}

struct RuntimeQuestionHandler<T: Transport> {
    sink: EventSink<T>,
    current_request_id: Arc<Mutex<Option<String>>>,
    pending: Arc<Mutex<Option<mpsc::Sender<Vec<QuestionAnswer>>>>>,
}

impl<T: Transport> QuestionHandler for RuntimeQuestionHandler<T> {
    fn handle(&self, questions: &[QuestionPrompt]) -> Vec<QuestionAnswer> {
        let request_id = self.current_request_id.lock().unwrap().clone();
        let (sender, receiver) = mpsc::channel();
        *self.pending.lock().unwrap() = Some(sender);
        self.sink.send(
            request_id,
            Event::QuestionRequest {
                questions: questions.to_vec(),
            },
        );
        receiver.recv().unwrap_or_else(|_| {
            questions
                .iter()
                .map(|question| QuestionAnswer {
                    question: question.question.clone(),
                    answer: vec![],
                    skipped: true,
                })
                .collect()
        })
    }
}

struct RuntimeTodoHandler<T: Transport> {
    sink: EventSink<T>,
    current_request_id: Arc<Mutex<Option<String>>>,
}

impl<T: Transport> TodoChangeHandler for RuntimeTodoHandler<T> {
    fn on_change(&self, todos: &[TodoItem]) {
        let request_id = self.current_request_id.lock().unwrap().clone();
        self.sink.send(
            request_id,
            Event::TodoChanged {
                todos: todos.to_vec(),
            },
        );
    }
}
