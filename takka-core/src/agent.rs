//! Agent configuration and state

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub id: String,
    pub name: String,
    pub provider_id: String,
    pub model: String,
    pub max_iterations: u32,
    pub tools: Vec<String>,
    #[serde(default)]
    pub system_prompt: Option<String>,
}

impl AgentConfig {
    pub fn new(
        id: impl Into<String>,
        provider_id: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        let id_str = id.into();
        Self {
            id: id_str.clone(),
            name: id_str,
            provider_id: provider_id.into(),
            model: model.into(),
            max_iterations: 10,
            tools: vec![],
            system_prompt: None,
        }
    }

    pub fn with_tools(mut self, tools: Vec<String>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_max_iterations(mut self, max: u32) -> Self {
        self.max_iterations = max;
        self
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub config: AgentConfig,
    pub current_iteration: u32,
    pub is_running: bool,
}

impl Agent {
    pub fn new(config: AgentConfig) -> Self {
        Self {
            config,
            current_iteration: 0,
            is_running: false,
        }
    }

    pub fn start(&mut self) {
        self.is_running = true;
        self.current_iteration = 0;
    }

    pub fn stop(&mut self) {
        self.is_running = false;
    }

    pub fn increment_iteration(&mut self) {
        self.current_iteration += 1;
    }

    pub fn can_continue(&self) -> bool {
        self.current_iteration < self.config.max_iterations
    }
}
