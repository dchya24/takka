//! The agent side of one eval run. Runs in its own process (spawned by the
//! runner) with the task workdir as cwd, because the builtin tools resolve
//! relative paths against the process cwd and that is not safe to change
//! in a shared process.

use crate::task::TaskSpec;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use takka_core::providers::CassetteProvider;
use takka_core::safety::PermissionMode;
use takka_core::tools::{builtin_tools_with_deps, ToolDeps};
use takka_core::{Event, LLMProvider, Orchestrator, ToolRegistry};

/// Model name used for replay when neither the task nor `--model` sets one.
pub const DEFAULT_REPLAY_MODEL: &str = "eval-model";

#[derive(Debug, Clone)]
pub enum ProviderSource {
    /// Replay a cassette. Hermetic: no config, no network.
    Cassette(PathBuf),
    /// Real provider built from an explicit config file.
    Live { config_path: PathBuf },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChildOutcome {
    pub answer: Option<String>,
    pub error: Option<String>,
    pub events: Vec<Event>,
    pub wall_ms: u64,
    pub model: String,
}

pub fn build_provider(
    source: &ProviderSource,
    model_override: Option<&str>,
) -> Result<(Arc<dyn LLMProvider>, String)> {
    match source {
        ProviderSource::Cassette(path) => {
            let p = CassetteProvider::from_path(path)
                .map_err(|e| anyhow::anyhow!("{e}"))
                .with_context(|| format!("loading cassette {}", path.display()))?;
            Ok((
                Arc::new(p),
                model_override.unwrap_or(DEFAULT_REPLAY_MODEL).to_string(),
            ))
        }
        ProviderSource::Live { config_path } => {
            let config = takka_core::Config::load_from_path(
                config_path.to_str().context("config path is not UTF-8")?,
            )
            .with_context(|| format!("loading config {}", config_path.display()))?;
            let pc = config
                .to_provider_config()
                .context("no provider configured in that config")?;
            let model = model_override
                .map(str::to_string)
                .unwrap_or_else(|| pc.default_model.clone());
            Ok((Arc::new(takka_core::OpenAIProvider::new(pc)), model))
        }
    }
}

/// Run the task prompt against `provider` in the current directory.
pub fn run_in_cwd(spec: &TaskSpec, provider: Arc<dyn LLMProvider>, model: &str) -> ChildOutcome {
    let tools = ToolRegistry::new();
    for t in builtin_tools_with_deps(ToolDeps::new()) {
        tools.register(t);
    }

    // Hermetic prompt: base prompt only (no project instructions, skills,
    // or persistent memory from the developer's machine).
    let system_prompt = takka_core::assemble_system_prompt(None, None, None, None);

    let mut orch = Orchestrator::new(provider, tools);
    orch.set_permission_mode(PermissionMode::Yolo);
    orch.set_model(model);
    orch.set_system_prompt(system_prompt);
    orch.set_max_iterations(spec.max_iterations);
    orch.set_max_cost_usd(spec.max_cost_usd);

    let events: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    orch.on_event(move |e| {
        if let Ok(mut v) = sink.lock() {
            v.push(e);
        }
    });

    let started = std::time::Instant::now();
    let result = orch.run(&spec.prompt);
    let wall_ms = started.elapsed().as_millis() as u64;
    let events = events.lock().map(|v| v.clone()).unwrap_or_default();
    let (answer, error) = match result {
        Ok(a) => (Some(a), None),
        Err(e) => (None, Some(e.to_string())),
    };
    ChildOutcome {
        answer,
        error,
        events,
        wall_ms,
        model: model.to_string(),
    }
}

/// Entry point of the `child` subcommand.
pub fn run_child(
    spec_path: &Path,
    source: &ProviderSource,
    model_override: Option<&str>,
    default_max_cost_usd: Option<f64>,
    out: &Path,
) -> Result<()> {
    let mut spec: TaskSpec = serde_json::from_str(&std::fs::read_to_string(spec_path)?)?;
    if spec.max_cost_usd.is_none() {
        spec.max_cost_usd = default_max_cost_usd;
    }
    let model_override = model_override.or(spec.model.as_deref());
    let (provider, model) = build_provider(source, model_override)?;
    let outcome = run_in_cwd(&spec, provider, &model);
    std::fs::write(out, serde_json::to_vec(&outcome)?)?;
    Ok(())
}
