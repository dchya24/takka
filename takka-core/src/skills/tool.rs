//! `skill` tool — lets the agent load skill instructions on demand.
//!
//! The tool is registered in the tool registry like any other tool. When
//! called, it looks up the named skill in the global [`SkillIndex`] and
//! returns its full content (SKILL.md body + any referenced files).

use std::sync::Arc;

use crate::events::{Event, EventEmitter};
use crate::skills::{SkillIndex, SkillLoader};
use crate::tool::{Tool, ToolError, ToolMetadata, ToolParam, ToolResult, ToolSchema};
use std::collections::HashMap;

/// Tool name exposed to the model.
const SKILL_TOOL_NAME: &str = "skill";

/// The `skill` tool implementation.
///
/// Allows the agent to load a skill by name and receive its instructions.
/// When `activate: true` (default), the skill is also activated for the
/// session duration, meaning its instructions remain available for
/// subsequent turns.
pub struct SkillTool {
    /// The skill index to look up skills from.
    index: Option<Arc<std::sync::RwLock<SkillIndex>>>,
    /// The global skill loader for activation.
    loader: Option<Box<dyn SkillLoader>>,
    /// Optional shared event emitter. When present, a successful
    /// activation emits `Event::SkillActivated` so frontends can
    /// observe which skill is live for the session (P0-3).
    events: Option<Arc<EventEmitter>>,
}

impl SkillTool {
    /// Create a new `SkillTool` backed by a shared skill index.
    ///
    /// The index is wrapped in `Arc<RwLock<>>` so it can be updated
    /// (e.g., when reloading skills at runtime).
    pub fn new(index: Arc<std::sync::RwLock<SkillIndex>>) -> Self {
        Self {
            index: Some(index),
            loader: None,
            events: None,
        }
    }

    /// Attach a custom [`SkillLoader`] to this tool.
    pub fn with_skill_loader(mut self, loader: Box<dyn SkillLoader>) -> Self {
        self.loader = Some(loader);
        self
    }

    /// Share the host's event emitter so successful activations are
    /// visible on the session event stream.
    pub fn with_events(mut self, events: Arc<EventEmitter>) -> Self {
        self.events = Some(events);
        self
    }
}

impl Tool for SkillTool {
    fn name(&self) -> &str {
        SKILL_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Load a skill's instructions by name. Skills package domain-specific \
         knowledge that the agent can load on demand. Use this tool when \
         the task requires expertise in a specific area (e.g., Rust, React, \
         security, deployment). Call `skill` with the skill name to load \
         its full instructions. Set `activate: true` (default) to keep the \
         instructions active for the session."
    }

    fn schema(&self) -> ToolSchema {
        let mut params = HashMap::new();
        params.insert(
            "name".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some("The exact name of the skill to load.".to_string()),
                default: None,
            },
        );
        params.insert(
            "activate".to_string(),
            ToolParam {
                param_type: "boolean".to_string(),
                description: Some(
                    "Whether to keep the skill instructions active for the \
                     session (default: true)"
                        .to_string(),
                ),
                default: Some(serde_json::Value::Bool(true)),
            },
        );
        ToolSchema {
            name: SKILL_TOOL_NAME.to_string(),
            description: self.description().to_string(),
            parameters: params,
            required: vec!["name".to_string()],
        }
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult<serde_json::Value> {
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new("Missing required argument: 'name' (string)"))?;

        let activate = args
            .get("activate")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        // Resolve via the per-instance loader first, then the index.
        // Neither attached → the skill system is not configured.
        let result = if let Some(loader) = &self.loader {
            loader.resolve(name)
        } else if let Some(index) = &self.index {
            index.read().unwrap().get(name).map(|skill| {
                let mut content = skill.body.clone();
                // Collect referenced files relative to skill directory
                if let Ok(entries) = std::fs::read_dir(&skill.dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.is_file() {
                            let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                            // Skip SKILL.md itself (already included as body)
                            if fname == "SKILL.md" {
                                continue;
                            }
                            if let Ok(file_content) = std::fs::read_to_string(&path) {
                                content.push_str(&format!(
                                    "\n\n---\n# Referenced file: {}\n\n{}",
                                    fname, file_content
                                ));
                            }
                        }
                    }
                }
                content
            })
        } else {
            None
        };

        match result {
            Some(content) => {
                // Optionally activate for session duration.
                // Activation is BEST-EFFORT: if no `SkillLoader` is
                // registered (e.g. the CLI didn't call
                // `set_skill_loader`), the skill content is still
                // returned as the tool result so the model receives
                // the instructions immediately.  Activation failure
                // should never discard or prevent the content from
                // reaching the model.
                let mut activated = false;
                if activate {
                    if let Some(loader) = &self.loader {
                        match loader.activate(name) {
                            Ok(_) => activated = true,
                            Err(ref e) => {
                                tracing::warn!(
                                    skill = name,
                                    error = %e,
                                    "Skill found but activation failed (best-effort)"
                                );
                            }
                        }
                    } else {
                        tracing::debug!(
                            skill = name,
                            "no SkillLoader attached; content-only load (activation skipped)"
                        );
                    }
                    if activated {
                        if let Some(events) = &self.events {
                            events.emit(Event::SkillActivated {
                                name: name.to_string(),
                            });
                        }
                    }
                }

                Ok(serde_json::json!({
                    "skill": name,
                    "content": content,
                    "activated": activated,
                }))
            }
            None => Err(ToolError::new(format!(
                "Skill '{}' not found. Use the /skills command to list \
                 available skills, or check that the skill directory exists \
                 and contains a valid SKILL.md file.",
                name
            ))),
        }
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::read_only()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::{Skill, SkillMetadata};

    fn make_index_with_skills(skills: Vec<Skill>) -> Arc<std::sync::RwLock<SkillIndex>> {
        let mut index = SkillIndex::new();
        for skill in skills {
            index.insert(skill);
        }
        Arc::new(std::sync::RwLock::new(index))
    }

    fn make_skill(name: &str, dir: &std::path::Path) -> Skill {
        Skill {
            metadata: SkillMetadata {
                name: name.to_string(),
                description: "A test skill".to_string(),
                ..Default::default()
            },
            dir: dir.to_path_buf(),
            content: "---\n---\n# Body".to_string(),
            frontmatter: String::new(),
            body: "# Body".to_string(),
        }
    }

    #[test]
    fn skill_tool_emits_skill_activated_on_successful_activation() {
        // P0-3: activation through a shared emitter surfaces
        // Event::SkillActivated on the session event stream.
        use std::sync::Mutex;

        let dir = std::env::temp_dir().join("skill_tool_emit_test");
        let _ = std::fs::create_dir_all(&dir);
        let index = make_index_with_skills(vec![make_skill("postgres", &dir)]);

        // A loader whose activate() always succeeds.
        struct OkLoader;
        impl SkillLoader for OkLoader {
            fn resolve(&self, name: &str) -> Option<String> {
                Some(format!("content of {}", name))
            }
            fn list(&self) -> Vec<(String, String)> {
                vec![]
            }
            fn activate(&self, _name: &str) -> Result<String, String> {
                Ok("activated".to_string())
            }
            fn deactivate(&self) {}
            fn active_skill(&self) -> Option<String> {
                None
            }
        }

        let events = Arc::new(EventEmitter::new());
        let seen: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let seen = seen.clone();
            events.on(move |e| seen.lock().unwrap().push(e));
        }

        // Loader-backed tool: resolve + activate both go through OkLoader.
        let tool = SkillTool::new(index)
            .with_skill_loader(Box::new(OkLoader))
            .with_events(events);

        let result = tool.execute(serde_json::json!({"name": "postgres"}));
        assert!(result.is_ok());
        assert_eq!(result.unwrap()["activated"], true);

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        match &seen[0] {
            Event::SkillActivated { name } => assert_eq!(name, "postgres"),
            other => panic!("expected SkillActivated, got {:?}", other),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skill_tool_no_event_when_not_activated() {
        // `activate: false` must not emit SkillActivated.
        use std::sync::Mutex;

        let dir = std::env::temp_dir().join("skill_tool_noemit_test");
        let _ = std::fs::create_dir_all(&dir);

        let events = Arc::new(EventEmitter::new());
        let seen: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let seen = seen.clone();
            events.on(move |e| seen.lock().unwrap().push(e));
        }

        let index = make_index_with_skills(vec![make_skill("quiet", &dir)]);
        let tool = SkillTool::new(index).with_events(events);

        let result = tool.execute(serde_json::json!({"name": "quiet", "activate": false}));
        assert!(result.is_ok());
        assert_eq!(result.unwrap()["activated"], false);
        assert!(seen.lock().unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skill_tool_returns_content() {
        // Since we can't create the temp file, we use the index but the dir
        // doesn't exist so execute should find the skill but fail on loading
        // files. Actually execute returns the body which is already loaded.
        // Let's test with a skill where the dir exists.
        let dir = std::env::temp_dir().join("skill_tool_test");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: test-skill\ndescription: A test\n---\n# Test\nDo stuff.",
        )
        .unwrap();

        let skill = Skill {
            metadata: SkillMetadata {
                name: "test-skill".to_string(),
                description: "A test".to_string(),
                ..Default::default()
            },
            dir: dir.clone(),
            content: "---\nname: test-skill\ndescription: A test\n---\n# Test\nDo stuff."
                .to_string(),
            frontmatter: "name: test-skill\ndescription: A test".to_string(),
            body: "# Test\nDo stuff.".to_string(),
        };

        let index = make_index_with_skills(vec![skill]);
        let tool = SkillTool::new(index);

        let result = tool.execute(serde_json::json!({"name": "test-skill", "activate": false}));
        assert!(result.is_ok());
        let val = result.unwrap();
        assert_eq!(val["skill"], "test-skill");
        assert!(val["content"].as_str().unwrap().contains("Do stuff."));
        assert_eq!(val["activated"], false);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skill_tool_missing_name() {
        let index = make_index_with_skills(Vec::new());
        let tool = SkillTool::new(index);
        let result = tool.execute(serde_json::json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn skill_tool_unknown_skill() {
        let index = make_index_with_skills(Vec::new());
        let tool = SkillTool::new(index);
        let result = tool.execute(serde_json::json!({"name": "nonexistent"}));
        assert!(result.is_err());
        assert!(result.unwrap_err().0.contains("not found"));
    }

    #[test]
    fn skill_tool_includes_referenced_files() {
        let dir = std::env::temp_dir().join("skill_tool_refs");
        let _ = std::fs::create_dir_all(&dir);

        // Write SKILL.md
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: refs-skill\ndescription: Skill with refs\n---\n# Main\nInstructions.",
        )
        .unwrap();

        // Write a referenced file
        std::fs::write(dir.join("config.json"), r#"{"setting": "value"}"#).unwrap();

        let skill = Skill {
            metadata: SkillMetadata {
                name: "refs-skill".to_string(),
                description: "Skill with refs".to_string(),
                ..Default::default()
            },
            dir: dir.clone(),
            content: "---\n...\n---\n# Main\nInstructions.".to_string(),
            frontmatter: "...".to_string(),
            body: "# Main\nInstructions.".to_string(),
        };

        let index = make_index_with_skills(vec![skill]);
        let tool = SkillTool::new(index);

        let result = tool.execute(serde_json::json!({"name": "refs-skill", "activate": false}));
        assert!(result.is_ok());
        let val = result.unwrap();
        let content = val["content"].as_str().unwrap();
        assert!(content.contains("config.json"));
        assert!(content.contains(r#""setting": "value""#));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skill_tool_is_read_only() {
        let index = make_index_with_skills(Vec::new());
        let tool = SkillTool::new(index);
        assert_eq!(
            tool.metadata().mutability,
            crate::tool::Mutability::ReadOnly
        );
    }
}
