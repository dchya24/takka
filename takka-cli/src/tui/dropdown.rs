//! Dropdown widget for command and file completion
//!
//! Handles:
//! - `/` command dropdown: shows available slash commands
//! - `@` file dropdown: shows all project files recursively (flat list)
//! - Respects `.gitignore` rules (skips node_modules, target, .git, etc.)
//! - Type `@` → full recursive file tree
//! - Type `@src/` → all files under src/
//! - Type `@chat` → fuzzy filter all files matching "chat"

use std::path::Path;

/// Type of dropdown
#[derive(Clone, Debug, PartialEq)]
pub enum DropdownType {
    /// Slash commands (/help, /config, etc.)
    Command,
    /// File paths (@src/main.rs, etc.)
    File,
    /// Model names (gpt-4o, claude-sonnet-4, etc.)
    Model,
    /// Skill names (loaded via `/skill`)
    Skill,
}

/// Dropdown state
#[derive(Clone, Debug)]
pub struct Dropdown {
    pub dropdown_type: DropdownType,
    pub items: Vec<String>,
    /// Parallel to `items`: per-item description shown dimmed next to the
    /// item (e.g. model capabilities / max tokens). Empty for types that
    /// embed their description in the item string (skills).
    pub descriptions: Vec<String>,
    /// Parallel to `items`: (provider, exact model id) for model dropdowns.
    /// Lets the REPL build an unambiguous `/models provider/id` command so
    /// identical display names / ids across providers resolve to the exact
    /// entry the user selected.
    pub model_meta: Vec<(String, String)>,
    pub selected: usize,
    pub visible_count: usize,
    pub query: String,
}

/// Available slash commands with aliases and descriptions
const SLASH_COMMANDS: &[(&str, &[&str], &str)] = &[
    ("help", &["h", "?"], "Show help message"),
    ("new", &["n"], "Start new session"),
    ("clear", &["cls", "c"], "Clear messages only"),
    ("sessions", &["ss"], "List & resume sessions"),
    ("models", &["m"], "Switch model"),
    ("provider", &[], "Switch provider"),
    ("search", &["s"], "Search conversation history"),
    ("image", &["img"], "Attach image"),
    ("skill", &["sk"], "Select and load a skill"),
    ("mcp", &[], "Show MCP server status"),
    ("plan", &["p"], "Generate a structured plan"),
    ("config", &["cfg"], "Show configuration"),
    ("tools", &["t"], "List available tools"),
    ("history", &["hist"], "Show command history"),
    ("stats", &[], "Show session statistics"),
    ("quit", &["q", "exit"], "Exit TUI"),
];

impl Dropdown {
    pub fn new(dropdown_type: DropdownType, query: String) -> Self {
        let mut dd = Self {
            dropdown_type,
            items: Vec::new(),
            descriptions: Vec::new(),
            model_meta: Vec::new(),
            selected: 0,
            visible_count: 8,
            query,
        };
        match dd.dropdown_type {
            DropdownType::Command => dd.items = Self::filter_commands(&dd.query),
            DropdownType::File => dd.items = Self::filter_files(&dd.query),
            DropdownType::Model => {
                let (items, descriptions, model_meta) = Self::filter_models(&dd.query);
                dd.items = items;
                dd.descriptions = descriptions;
                dd.model_meta = model_meta;
            }
            DropdownType::Skill => {} // use new_skill() instead
        }
        dd
    }

    /// Create a skill selection dropdown with discovered skills.
    /// Descriptions are truncated to 60 chars so each item fits on one terminal line.
    pub fn new_skill(query: String, skills: Vec<(String, String)>) -> Self {
        let trunc_desc = |desc: &str| -> String {
            // Truncate at char boundary to avoid panics on multi-byte UTF-8
            let max = 60;
            if desc.len() <= max {
                return desc.to_string();
            }
            let mut end = max;
            while end > 0 && !desc.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}…", &desc[..end])
        };
        let query_lower = query.to_lowercase();
        let items: Vec<String> = if query.is_empty() {
            skills
                .iter()
                .map(|(name, desc)| format!("{} — {}", name, trunc_desc(desc)))
                .collect()
        } else {
            skills
                .iter()
                .filter(|(name, desc)| {
                    name.to_lowercase().contains(&query_lower)
                        || desc.to_lowercase().contains(&query_lower)
                })
                .map(|(name, desc)| format!("{} — {}", name, trunc_desc(desc)))
                .collect()
        };

        Self {
            dropdown_type: DropdownType::Skill,
            items,
            descriptions: Vec::new(),
            model_meta: Vec::new(),
            selected: 0,
            visible_count: 8,
            query,
        }
    }

    /// Extract skill name from display string ("my-skill — Does X" → "my-skill")
    pub fn get_skill_name(&self, display: &str) -> Option<String> {
        if self.dropdown_type != DropdownType::Skill {
            return None;
        }
        display.split(" — ").next().map(|s| s.to_string())
    }

    /// Create a model dropdown with pre-fetched model list
    pub fn new_model(query: String, models: Vec<String>) -> Self {
        let query_lower = query.to_lowercase();
        let items: Vec<String> = if query.is_empty() {
            models
        } else {
            models
                .into_iter()
                .filter(|m| m.to_lowercase().contains(&query_lower))
                .collect()
        };

        Self {
            dropdown_type: DropdownType::Model,
            items,
            descriptions: Vec::new(),
            model_meta: Vec::new(),
            selected: 0,
            visible_count: 8,
            query,
        }
    }

    /// Get the current query string
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Filter slash commands by query (supports aliases)
    fn filter_commands(query: &str) -> Vec<String> {
        let query_lower = query.to_lowercase();
        SLASH_COMMANDS
            .iter()
            .filter(|(cmd, aliases, _)| {
                cmd.starts_with(&query_lower) || aliases.iter().any(|a| a.starts_with(&query_lower))
            })
            .map(|(cmd, _, _)| cmd.to_string())
            .collect()
    }

    /// Filter model names by query.
    ///
    /// Returns (items, descriptions, model_meta) kept in sync: each item
    /// is the display string (`name 👁 [provider] ●`), each description is
    /// a compact capability / limit summary, and each model_meta entry is
    /// the exact (provider, model id) for building an unambiguous switch
    /// command.
    fn filter_models(query: &str) -> (Vec<String>, Vec<String>, Vec<(String, String)>) {
        let query_lower = query.to_lowercase();

        // Load config to get all available models
        let config = match takka_core::Config::load() {
            Some(c) => c,
            None => return (Vec::new(), Vec::new(), Vec::new()),
        };

        let active_provider = config.active_provider().map(|p| p.name.clone());
        let active_model = config.active_model().map(|m| m.model.clone());

        let mut entries: Vec<(String, String, (String, String))> = Vec::new();
        for provider in &config.providers {
            for model in &provider.models {
                let display_name = model.display_name.as_deref().unwrap_or(&model.model);
                let is_active = active_provider.as_deref() == Some(&provider.name)
                    && active_model.as_deref() == Some(&model.model);

                let caps = model.effective_capabilities();
                let vision_icon = if caps.vision { " 👁" } else { "" };
                let active_marker = if is_active { " ●" } else { "" };

                let display = format!(
                    "{}{} [{}]{}",
                    display_name, vision_icon, provider.name, active_marker
                );

                // Filter by query
                if !query.is_empty()
                    && !display_name.to_lowercase().contains(&query_lower)
                    && !model.model.to_lowercase().contains(&query_lower)
                    && !provider.name.to_lowercase().contains(&query_lower)
                {
                    continue;
                }

                let description = build_model_description(model);
                entries.push((
                    display,
                    description,
                    (provider.name.clone(), model.model.clone()),
                ));
            }
        }

        // Sort: active first, then alphabetically (keep all vecs in sync)
        entries.sort_by(|(a, _, _), (b, _, _)| {
            let a_active = a.contains('●');
            let b_active = b.contains('●');
            match (a_active, b_active) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.cmp(b),
            }
        });

        let mut items = Vec::with_capacity(entries.len());
        let mut descriptions = Vec::with_capacity(entries.len());
        let mut model_meta = Vec::with_capacity(entries.len());
        for (display, description, meta) in entries {
            items.push(display);
            descriptions.push(description);
            model_meta.push(meta);
        }
        (items, descriptions, model_meta)
    }

    /// Filter files by query — always recursive, respects .gitignore.
    ///
    /// Behavior based on query:
    /// - `""`     → all files in project (recursive flat list)
    /// - `"src/"` → all files under `src/` (recursive)
    /// - `"src/ma"` → all files under `src/` matching `ma`
    /// - `"chat"` → all files in project matching `chat` (by name or path)
    fn filter_files(query: &str) -> Vec<String> {
        let mut results = Vec::new();

        // Parse query into (path_prefix, name_filter)
        // e.g. "src/components/cha" → prefix="src/components/", filter="cha"
        // e.g. "src/"              → prefix="src/",         filter=""
        // e.g. "chat"             → prefix="",              filter="chat"
        // e.g. ""                 → prefix="",              filter=""
        let (path_prefix, name_filter) = if query.is_empty() {
            (String::new(), String::new())
        } else if query.ends_with('/') {
            // Pure path browse: "src/" → show all under src/
            (query.to_string(), String::new())
        } else if query.contains('/') {
            // Path + filter: "src/components/cha"
            let last_slash = query.rfind('/').unwrap();
            (
                query[..=last_slash].to_string(),
                query[last_slash + 1..].to_string(),
            )
        } else {
            // Just a filter, no path
            (String::new(), query.to_string())
        };

        let filter_lower = name_filter.to_lowercase();

        // Walk the entire project recursively, respecting .gitignore
        let mut builder = ignore::WalkBuilder::new(".");
        builder
            .hidden(false)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .require_git(false);

        for entry in builder.build().filter_map(|e| e.ok()) {
            let path = entry.path();
            let path_str = path.to_string_lossy();

            // Skip `.` itself and `.git/` directory
            if path_str == "." || path_str == "./" || path_str == "./.git" {
                continue;
            }

            // Skip anything inside `.git/`
            let normalized = path_str.replace('\\', "/");
            if normalized.starts_with(".git/") || normalized.starts_with("./.git/") {
                continue;
            }

            // Normalize: backslashes to forward slashes, strip leading "./"
            let normalized = path_str.replace('\\', "/");
            let clean = normalized.strip_prefix("./").unwrap_or(&normalized);

            // If a path prefix was given, only include files under that prefix
            if !path_prefix.is_empty() {
                // Normalize: ensure prefix comparison works
                if !clean.starts_with(&path_prefix)
                    && !clean.starts_with(path_prefix.trim_end_matches('/'))
                {
                    continue;
                }
            }

            // If a name filter was given, match against filename and full path
            if !filter_lower.is_empty() {
                let fname = Path::new(&clean)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                let fname_lower = fname.to_lowercase();
                let clean_lower = clean.to_lowercase();

                // Match: filename starts with, filename contains, or path contains
                if !fname_lower.starts_with(&filter_lower)
                    && !fname_lower.contains(&filter_lower)
                    && !clean_lower.contains(&filter_lower)
                {
                    continue;
                }
            }

            // Format display: directories get trailing `/`
            if path.is_dir() {
                results.push(format!("{}/", clean));
            } else {
                results.push(clean.to_string());
            }
        }

        // Sort: directories first, then files — both alphabetically
        results.sort_by(|a, b| {
            let a_is_dir = a.ends_with('/');
            let b_is_dir = b.ends_with('/');
            match (a_is_dir, b_is_dir) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.to_lowercase().cmp(&b.to_lowercase()),
            }
        });

        // Cap results for performance
        results.truncate(50);
        results
    }

    /// Get selected item
    pub fn selected_item(&self) -> Option<&str> {
        self.items.get(self.selected).map(|s| s.as_str())
    }

    /// Select previous item (wraps around)
    pub fn select_prev(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        } else if !self.items.is_empty() {
            self.selected = self.items.len() - 1;
        }
    }

    /// Select next item (wraps around)
    pub fn select_next(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + 1) % self.items.len();
        }
    }

    /// Get visible items with selection info
    pub fn visible_items(&self) -> Vec<(usize, &str, bool)> {
        let start = if self.selected >= self.visible_count {
            self.selected - self.visible_count + 1
        } else {
            0
        };

        self.items
            .iter()
            .enumerate()
            .skip(start)
            .take(self.visible_count)
            .map(|(i, item)| (i, item.as_str(), i == self.selected))
            .collect()
    }

    /// Get description for the dropdown item at `index`, if any.
    ///
    /// Commands return their one-line help; models return the compact
    /// capability / limit summary computed by [`build_model_description`].
    /// Index-based (not item-string-based) so duplicate display strings
    /// — e.g. two models aliased to the same display name — still resolve
    /// to the correct per-item description.
    pub fn get_description(&self, index: usize) -> Option<String> {
        match self.dropdown_type {
            DropdownType::Command => self
                .items
                .get(index)
                .and_then(|cmd| SLASH_COMMANDS.iter().find(|(c, _, _)| *c == cmd.as_str()))
                .map(|(_, _, desc)| desc.to_string()),
            DropdownType::Model => self.descriptions.get(index).cloned(),
            _ => None,
        }
    }

    /// Check if dropdown has items
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Get icon for dropdown type
    pub fn icon(&self) -> &'static str {
        match self.dropdown_type {
            DropdownType::Command => "⌘",
            DropdownType::File => "📁",
            DropdownType::Model => "🤖",
            DropdownType::Skill => "⚡",
        }
    }

    /// Get title for dropdown
    pub fn title(&self) -> &'static str {
        match self.dropdown_type {
            DropdownType::Command => "Commands",
            DropdownType::File => "Files",
            DropdownType::Model => "Models",
            DropdownType::Skill => "Skills",
        }
    }

    /// Get model ID from display string (extracts model name from "gpt-4o [openai]" format)
    pub fn get_model_id(&self, display: &str) -> Option<String> {
        if self.dropdown_type != DropdownType::Model {
            return None;
        }
        // Extract everything before the provider bracket:
        // "GPT-4o mini 👁 [openai] ●" → "GPT-4o mini"
        // (splitting on the first space would truncate names containing
        // spaces, e.g. "GPT-4o mini" → "GPT-4o")
        let id = display.split(" [").next()?.to_string();
        // Strip the trailing vision icon if present: "gpt-4o 👁" → "gpt-4o"
        let id = id.strip_suffix('👁').map(str::trim_end).unwrap_or(&id);
        Some(id.to_string())
    }

    /// Exact (provider, model id) for the currently selected item.
    ///
    /// The display string only shows `display_name`, which can hide the
    /// real id (aliases) or collide across providers; the metadata carries
    /// the exact identity so the REPL can switch to precisely this entry.
    pub fn selected_model_meta(&self) -> Option<(String, String)> {
        if self.dropdown_type != DropdownType::Model {
            return None;
        }
        self.model_meta.get(self.selected).cloned()
    }
}

/// Build a compact description line for a model dropdown item, e.g.
/// `"👁 vision · max 198K"` or `"id: sm/deepseek-v4-flash · max 198K"`.
/// Only non-default capabilities are shown (vision, missing tools,
/// missing streaming) so text-only models stay terse. Truncated so the
/// item still fits on one terminal line.
fn build_model_description(model: &takka_core::config::ModelConfig) -> String {
    let mut parts: Vec<String> = Vec::new();

    let caps = model.effective_capabilities();
    if caps.vision {
        parts.push("👁 vision".to_string());
    }
    if !caps.tools {
        parts.push("no tools".to_string());
    }
    if !caps.streaming {
        parts.push("no streaming".to_string());
    }

    // Surface the underlying model id when the display name hides it
    // (e.g. display "kimi-k2.6" → id "sm/deepseek-v4-flash").
    let id_hidden = model
        .display_name
        .as_deref()
        .is_some_and(|d| d != model.model.as_str());
    if id_hidden {
        parts.push(format!("id: {}", model.model));
    }

    parts.push(format!("max {}", format_max_tokens(model.max_tokens)));

    truncate_desc(&parts.join(" · "), 50)
}

/// Format a token count as a compact string: `1_000_000 → "1.0M"`,
/// `198_192 → "198K"`, `8192 → "8K"`, `512 → "512"`.
fn format_max_tokens(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}K", n / 1_000)
    } else {
        n.to_string()
    }
}

/// Truncate a description at `max` bytes on a UTF-8 char boundary and
/// append an ellipsis when cut.
fn truncate_desc(desc: &str, max: usize) -> String {
    if desc.len() <= max {
        return desc.to_string();
    }
    let mut end = max;
    while end > 0 && !desc.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &desc[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_filter() {
        let dropdown = Dropdown::new(DropdownType::Command, "he".to_string());
        assert!(dropdown.items.contains(&"help".to_string()));
    }

    #[test]
    fn test_command_filter_by_alias() {
        let dropdown = Dropdown::new(DropdownType::Command, "h".to_string());
        assert!(dropdown.items.contains(&"help".to_string()));
    }

    #[test]
    fn test_command_filter_empty() {
        let dropdown = Dropdown::new(DropdownType::Command, "".to_string());
        assert_eq!(dropdown.items.len(), SLASH_COMMANDS.len());
    }

    #[test]
    fn test_select_navigation() {
        let mut dropdown = Dropdown::new(DropdownType::Command, "".to_string());
        assert_eq!(dropdown.selected, 0);

        dropdown.select_next();
        assert_eq!(dropdown.selected, 1);

        dropdown.select_prev();
        assert_eq!(dropdown.selected, 0);

        // Wrap around
        dropdown.select_prev();
        assert_eq!(dropdown.selected, dropdown.items.len() - 1);
    }

    #[test]
    fn test_file_filter_shows_nested() {
        // Empty query should show ALL files recursively
        let dropdown = Dropdown::new(DropdownType::File, String::new());
        if std::path::Path::new("src").exists() {
            // Should contain nested paths, not just top-level
            let has_nested = dropdown
                .items
                .iter()
                .any(|i| i.matches('/').count() >= 2 && !i.ends_with('/'));
            assert!(
                has_nested,
                "expected nested file paths in results, got: {:?}",
                dropdown.items
            );
        }
    }

    #[test]
    fn test_file_filter_path_prefix() {
        // "src/" should only show files under src/
        let dropdown = Dropdown::new(DropdownType::File, "src/".to_string());
        for item in &dropdown.items {
            assert!(
                item.starts_with("src/"),
                "expected item to start with 'src/', got: {}",
                item
            );
        }
    }

    #[test]
    fn test_file_filter_name_search() {
        // "main" should find files with "main" in name or path
        let dropdown = Dropdown::new(DropdownType::File, "main".to_string());
        // Just verify it doesn't crash and filters correctly
        for item in &dropdown.items {
            let lower = item.to_lowercase();
            assert!(
                lower.contains("main"),
                "expected item to contain 'main', got: {}",
                item
            );
        }
    }

    #[test]
    fn test_file_filter_path_and_name() {
        // "src/ma" should find files under src/ matching "ma"
        let dropdown = Dropdown::new(DropdownType::File, "src/ma".to_string());
        for item in &dropdown.items {
            assert!(
                item.starts_with("src/"),
                "expected item to start with 'src/', got: {}",
                item
            );
        }
    }

    #[test]
    fn test_query_stored() {
        let dropdown = Dropdown::new(DropdownType::Command, "he".to_string());
        assert_eq!(dropdown.query(), "he");
    }

    #[test]
    fn test_query_empty() {
        let dropdown = Dropdown::new(DropdownType::Command, "".to_string());
        assert_eq!(dropdown.query(), "");
    }

    #[test]
    fn test_get_model_id_plain() {
        let dropdown = Dropdown::new(DropdownType::Model, String::new());
        assert_eq!(
            dropdown.get_model_id("gpt-4o [openai]"),
            Some("gpt-4o".to_string())
        );
    }

    #[test]
    fn test_get_model_id_with_vision_icon() {
        let dropdown = Dropdown::new(DropdownType::Model, String::new());
        assert_eq!(
            dropdown.get_model_id("gpt-4o 👁 [openai]"),
            Some("gpt-4o".to_string())
        );
    }

    #[test]
    fn test_get_model_id_with_active_marker() {
        let dropdown = Dropdown::new(DropdownType::Model, String::new());
        assert_eq!(
            dropdown.get_model_id("gpt-4o [openai] ●"),
            Some("gpt-4o".to_string())
        );
    }

    #[test]
    fn test_get_model_id_with_space_in_name() {
        let dropdown = Dropdown::new(DropdownType::Model, String::new());
        // Display names containing spaces must not be truncated.
        assert_eq!(
            dropdown.get_model_id("GPT-4o mini 👁 [openai]"),
            Some("GPT-4o mini".to_string())
        );
    }

    #[test]
    fn test_get_model_id_ignores_other_types() {
        let dropdown = Dropdown::new(DropdownType::Command, String::new());
        assert_eq!(dropdown.get_model_id("models"), None);
    }

    #[test]
    fn test_build_model_description_text_only() {
        let model = takka_core::config::ModelConfig {
            model: "glm-4.7".into(),
            display_name: None,
            temperature: 0.7,
            max_tokens: 198_192,
            capabilities: None,
        };
        assert_eq!(build_model_description(&model), "max 198K");
    }

    #[test]
    fn test_build_model_description_vision() {
        let model = takka_core::config::ModelConfig {
            model: "gpt-4o".into(),
            display_name: None,
            temperature: 0.7,
            max_tokens: 8192,
            capabilities: Some(takka_core::capabilities::ModelCapabilities::new(
                true, true, true,
            )),
        };
        assert_eq!(build_model_description(&model), "👁 vision · max 8K");
    }

    #[test]
    fn test_build_model_description_hidden_id() {
        let model = takka_core::config::ModelConfig {
            model: "sm/deepseek-v4-flash".into(),
            display_name: Some("kimi-k2.6".into()),
            temperature: 0.7,
            max_tokens: 8192,
            capabilities: None,
        };
        assert_eq!(
            build_model_description(&model),
            "id: sm/deepseek-v4-flash · max 8K"
        );
    }

    #[test]
    fn test_build_model_description_missing_caps() {
        let model = takka_core::config::ModelConfig {
            model: "o1".into(),
            display_name: None,
            temperature: 0.7,
            max_tokens: 200_000,
            capabilities: Some(takka_core::capabilities::ModelCapabilities::new(
                false, true, false,
            )),
        };
        assert_eq!(build_model_description(&model), "no streaming · max 200K");
    }

    #[test]
    fn test_format_max_tokens() {
        assert_eq!(format_max_tokens(512), "512");
        assert_eq!(format_max_tokens(8192), "8K");
        assert_eq!(format_max_tokens(198_192), "198K");
        assert_eq!(format_max_tokens(1_000_000), "1.0M");
    }

    #[test]
    fn test_truncate_desc_keeps_char_boundary() {
        let long = "👁 vision · id: sm/deepseek-v4-flash · max 198K";
        let truncated = truncate_desc(long, 20);
        // 20 bytes + multi-byte ellipsis
        assert!(truncated.len() <= 20 + "…".len());
        assert!(truncated.ends_with('…'));
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn test_model_descriptions_parallel_to_items() {
        // The model dropdown must keep descriptions in sync with items
        // (same length, every item resolvable via get_description).
        let dropdown = Dropdown::new(DropdownType::Model, String::new());
        assert_eq!(dropdown.items.len(), dropdown.descriptions.len());
        for (i, _item) in dropdown.items.iter().enumerate() {
            assert!(
                dropdown.get_description(i).is_some(),
                "missing description for item {}",
                i
            );
        }
    }

    #[test]
    fn test_selected_model_meta_matches_items() {
        // Every model item must carry an exact (provider, model id) so the
        // REPL can build an unambiguous /models provider/id command.
        let dropdown = Dropdown::new(DropdownType::Model, String::new());
        assert_eq!(dropdown.items.len(), dropdown.model_meta.len());
        for (i, item) in dropdown.items.iter().enumerate() {
            let (provider, id) = dropdown.model_meta.get(i).expect("meta missing");
            assert!(!provider.is_empty(), "empty provider for {item}");
            assert!(!id.is_empty(), "empty model id for {item}");
        }
    }

    #[test]
    fn test_selected_model_meta_follows_navigation() {
        let mut dropdown = Dropdown::new(DropdownType::Model, String::new());
        if dropdown.items.is_empty() {
            return; // no models configured in this environment
        }
        let first = dropdown.selected_model_meta().unwrap();
        dropdown.select_next();
        let second = dropdown.selected_model_meta().unwrap();
        // Navigation must move the metadata in lockstep with the item.
        assert_ne!(first, second);
    }
}
