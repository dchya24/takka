use anyhow::Result;
use ratatui::style::{Color as RColor, Modifier as RModifier, Style as RStyle};
use ratatui::text::{Line as RLine, Span as RSpan};
use takka_core::Config;

use crate::widgets::{components, inline};

use super::*;

impl Commands {
    // ── Model info (for REPL prompt) ────────────────────────

    pub fn model_info(&self) -> (String, String, String) {
        if let Some(p) = self.config.active_provider() {
            let model = p
                .models
                .first()
                .map(|m| m.display_name.as_deref().unwrap_or(&m.model))
                .unwrap_or("unknown")
                .to_string();
            (p.name.clone(), model, p.api_base.clone())
        } else {
            ("none".into(), "none".into(), "-".into())
        }
    }

    // ── Switch model ────────────────────────────────────────

    /// Switch to a model by name.
    ///
    /// Accepts either a provider-qualified id `"provider/model"` (matched
    /// exactly, the form the REPL model dropdown generates) or a plain /
    /// partial name (matched with the legacy contains() search).
    ///
    /// The qualified form is important: identical model ids or display
    /// names can exist in several providers (e.g. "GLM-4.7" in two
    /// providers, or an aliased id like "sm/deepseek-v4-flash" displayed
    /// as "kimi-k2.6"). A plain contains() search would silently pick the
    /// *first* provider in config order instead of the one the user chose.
    ///
    /// Returns (provider_name, model_name) if switched, or error string.
    pub fn switch_model(&mut self, name: &str) -> Result<(String, String), String> {
        let (pi, mi) = Self::find_model_index(&self.config, name)
            .ok_or_else(|| format!("Model '{}' not found", name))?;

        let (active_provider, active_model) = Self::reorder_config(&mut self.config, pi, mi);

        // Persist. Under test builds this refuses to touch the real config
        // path (see persist_config) so a stray test can never clobber the
        // developer's actual ~/.config/agentic/config.json.
        self.persist_config()?;

        // Reset orchestrator so it reinitializes with new model

        self.connection = None;

        Ok((active_provider, active_model))
    }

    /// Save the config to disk.
    ///
    /// In test builds this is a hard error: [`Config::save`] writes to the
    /// real user config path and a test must never reach it. This guard
    /// exists because a unit test once called `switch_model` and silently
    /// overwrote a developer's real `~/.config/agentic/config.json`.
    pub(crate) fn persist_config(&self) -> Result<(), String> {
        #[cfg(test)]
        {
            Err("refusing to save config under test (would overwrite the real ~/.config/agentic/config.json)"
                .to_string())
        }
        #[cfg(not(test))]
        {
            self.config
                .save()
                .map_err(|e| format!("Failed to save config: {}", e))
        }
    }

    /// Locate the (provider_idx, model_idx) for `name`.
    ///
    /// Tries an exact provider-qualified match (`"provider/model"`) first
    /// — the form the REPL model dropdown generates — then falls back to a
    /// loose contains() search for plain / partial names (manual typing).
    pub(crate) fn find_model_index(config: &Config, name: &str) -> Option<(usize, usize)> {
        if let Some((provider_name, model_name)) = name.split_once('/') {
            if let Some(found) = Self::find_model_exact(config, provider_name, model_name) {
                return Some(found);
            }
            // Fall through to the loose search (e.g. the user typed an id
            // that merely contains '/' but isn't a valid provider).
        }

        let name_lower = name.to_lowercase();
        for (pi, provider) in config.providers.iter().enumerate() {
            for (mi, model) in provider.models.iter().enumerate() {
                let display = model.display_name.as_deref().unwrap_or(&model.model);
                if model.model.to_lowercase().contains(&name_lower)
                    || display.to_lowercase().contains(&name_lower)
                {
                    return Some((pi, mi));
                }
            }
        }
        None
    }

    /// Exact provider + model-id lookup (case-insensitive).
    pub(crate) fn find_model_exact(
        config: &Config,
        provider_name: &str,
        model_name: &str,
    ) -> Option<(usize, usize)> {
        for (pi, provider) in config.providers.iter().enumerate() {
            if !provider.name.eq_ignore_ascii_case(provider_name) {
                continue;
            }
            for (mi, model) in provider.models.iter().enumerate() {
                if model.model.eq_ignore_ascii_case(model_name) {
                    return Some((pi, mi));
                }
            }
        }
        None
    }

    /// Reorder config so (pi, mi) becomes the active provider/model.
    /// Returns the resulting (provider, model) display names.
    pub(crate) fn reorder_config(config: &mut Config, pi: usize, mi: usize) -> (String, String) {
        // Reorder: move selected provider to front, selected model to front
        let mut providers = config.providers.clone();
        let mut provider = providers.remove(pi);
        let model = provider.models.remove(mi);
        provider.models.insert(0, model);
        providers.insert(0, provider);
        config.providers = providers;

        let active_provider = config.providers[0].name.clone();
        let active_model = config.providers[0].models[0]
            .display_name
            .clone()
            .unwrap_or_else(|| config.providers[0].models[0].model.clone());

        (active_provider, active_model)
    }

    /// Interactive model picker using ratatui full-screen TUI.
    /// Returns (provider_name, model_name) if switched, None if cancelled.
    /// Display available models inline (non-modal).
    pub fn list_models_inline(&self) {
        inline::print_blank();
        inline::print_line(&components::section_header(
            "🤖",
            "Available Models",
            RColor::Cyan,
        ));
        inline::print_blank();

        let active_provider = self.config.active_provider().map(|p| p.name.clone());
        let active_model = self.config.active_model().map(|m| m.model.clone());

        for provider in &self.config.providers {
            let provider_style = RStyle::default()
                .fg(RColor::Rgb(255, 215, 0))
                .add_modifier(RModifier::BOLD);

            inline::print_line(&RLine::from(vec![
                RSpan::raw("  "),
                RSpan::styled(format!("📡 {}", provider.name), provider_style),
            ]));

            for model in &provider.models {
                let is_active = active_provider.as_deref() == Some(&provider.name)
                    && active_model.as_deref() == Some(&model.model);

                let display = model.display_name.as_deref().unwrap_or(&model.model);
                let caps = model.effective_capabilities();

                let mut spans = vec![RSpan::raw("    ")];

                if is_active {
                    spans.push(RSpan::styled("● ", RStyle::default().fg(RColor::Green)));
                } else {
                    spans.push(RSpan::raw("  "));
                }

                spans.push(RSpan::styled(
                    display.to_string(),
                    RStyle::default().fg(RColor::Rgb(180, 180, 200)),
                ));

                if caps.vision {
                    spans.push(RSpan::styled(
                        "  👁",
                        RStyle::default().fg(RColor::Rgb(135, 206, 250)),
                    ));
                }

                spans.push(RSpan::styled(
                    format!("  ({})", model.model),
                    RStyle::default()
                        .fg(RColor::Rgb(100, 100, 120))
                        .add_modifier(RModifier::DIM),
                ));

                inline::print_line(&RLine::from(spans));
            }
            inline::print_blank();
        }

        inline::print_line(&RLine::from(vec![
            RSpan::styled("💡 ", RStyle::default()),
            RSpan::styled("Tip: ", RStyle::default().add_modifier(RModifier::DIM)),
            RSpan::raw("Use "),
            RSpan::styled(
                "/models <name>",
                RStyle::default()
                    .fg(RColor::Rgb(255, 215, 0))
                    .add_modifier(RModifier::BOLD),
            ),
            RSpan::raw(" to switch. Type "),
            RSpan::styled("/models ", RStyle::default().fg(RColor::Rgb(255, 215, 0))),
            RSpan::raw("then press "),
            RSpan::styled(
                "Tab",
                RStyle::default()
                    .fg(RColor::Rgb(135, 206, 250))
                    .add_modifier(RModifier::BOLD),
            ),
            RSpan::raw(" for completion"),
        ]));
        inline::print_blank();
    }

    /// Interactive model picker using dialoguer (fuzzy searchable).
    pub fn pick_model_interactive_inline(&mut self) -> Option<(String, String)> {
        use dialoguer::{theme::ColorfulTheme, FuzzySelect};

        let active_provider = self.config.active_provider().map(|p| p.name.clone());
        let active_model = self.config.active_model().map(|m| m.model.clone());

        // Build list of (display_string, provider_name, model_name)
        let mut items: Vec<(String, String, String)> = Vec::new();
        let mut default_idx = 0;

        for provider in &self.config.providers {
            for model in &provider.models {
                let display_name = model.display_name.as_deref().unwrap_or(&model.model);
                let caps = model.effective_capabilities();
                let vision_icon = if caps.vision { " 👁" } else { "" };

                let is_active = active_provider.as_deref() == Some(&provider.name)
                    && active_model.as_deref() == Some(&model.model);

                if is_active {
                    default_idx = items.len();
                }

                let display = format!(
                    "{}{} [{}]{}",
                    display_name,
                    vision_icon,
                    provider.name,
                    if is_active { " ●" } else { "" }
                );

                items.push((display, provider.name.clone(), model.model.clone()));
            }
        }

        if items.is_empty() {
            inline::print_blank();
            inline::print_line(&components::warning_badge("No models configured."));
            inline::print_blank();
            return None;
        }

        inline::print_blank();

        let selection = FuzzySelect::with_theme(&ColorfulTheme::default())
            .with_prompt(
                "🤖 Select Model (type to filter, ↑↓ to navigate, Enter to select, Esc to cancel)",
            )
            .items(&items.iter().map(|(d, _, _)| d.as_str()).collect::<Vec<_>>())
            .default(default_idx)
            .interact_opt();

        match selection {
            Ok(Some(idx)) => {
                let (_, provider, model) = &items[idx];
                // Provider-qualified so identical ids / display names in
                // different providers switch to the exact entry chosen.
                self.switch_model(&format!("{}/{}", provider, model)).ok()
            }
            Ok(None) | Err(_) => None,
        }
    }
}

// ── switch_model disambiguation ────────────────────────────

#[cfg(test)]
mod switch_model_tests {
    use super::*;
    use takka_core::config::{ModelConfig, ProviderConfig};

    /// Config with the same display name "GLM-4.7" (and id "glm-4.7") in
    /// two providers, plus an aliased model (display "kimi-k2.6", id
    /// "sm/deepseek-v4-flash") duplicated across providers — the exact
    /// situations that made a plain contains() switch pick the wrong one.
    fn ambiguous_config() -> Config {
        let model = |id: &str, display: Option<&str>| ModelConfig {
            model: id.into(),
            display_name: display.map(str::to_string),
            temperature: 0.7,
            max_tokens: 8192,
            capabilities: None,
        };
        let provider = |name: &str, models: Vec<ModelConfig>| ProviderConfig {
            name: name.into(),
            provider_type: "openai".into(),
            api_base: format!("https://{}.test/v1", name.to_lowercase()),
            api_key: String::new(),
            models,
            cache: Default::default(),
        };

        Config {
            providers: vec![
                provider(
                    "Provider A",
                    vec![
                        model("glm-4.7", Some("GLM-4.7")),
                        model("sm/deepseek-v4-flash", Some("kimi-k2.6")),
                    ],
                ),
                provider(
                    "Provider B",
                    vec![
                        model("glm-4.7", Some("GLM-4.7")),
                        model("glm-5", Some("GLM-5")),
                        model("sm/deepseek-v4-flash", Some("sm/deepseek-v4-flash")),
                    ],
                ),
            ],
            safety: Default::default(),
            output: Default::default(),
            mcp_servers: Default::default(),
            system_prompt: None,
            agent: Default::default(),
            skills: Default::default(),
            updates: Default::default(),
        }
    }

    /// Switch via the pure lookup + reorder helpers. These mirror exactly
    /// what `switch_model` does, minus the config save (which writes to
    /// the real user config path and is deliberately not exercised here).
    fn switch(config: &mut Config, name: &str) -> Result<(String, String), String> {
        let (pi, mi) = Commands::find_model_index(config, name)
            .ok_or_else(|| format!("Model '{name}' not found"))?;
        Ok(Commands::reorder_config(config, pi, mi))
    }

    #[test]
    fn unqualified_switch_hits_first_provider() {
        let mut config = ambiguous_config();
        let (provider, model) = switch(&mut config, "GLM-4.7").unwrap();
        assert_eq!(provider, "Provider A");
        assert_eq!(model, "GLM-4.7");
    }

    #[test]
    fn qualified_switch_disambiguates_duplicate_name() {
        let mut config = ambiguous_config();
        // Same id/display in two providers: the qualified form must pick
        // Provider B even though Provider A comes first in config order.
        let (provider, model) = switch(&mut config, "Provider B/glm-4.7").unwrap();
        assert_eq!(provider, "Provider B");
        assert_eq!(model, "GLM-4.7");
    }

    #[test]
    fn qualified_switch_disambiguates_aliased_id() {
        let mut config = ambiguous_config();
        // "sm/deepseek-v4-flash" exists in both providers; the qualified
        // form must hit Provider B's copy, not Provider A's (aliased as
        // "kimi-k2.6").
        let (provider, model) = switch(&mut config, "Provider B/sm/deepseek-v4-flash").unwrap();
        assert_eq!(provider, "Provider B");
        assert_eq!(model, "sm/deepseek-v4-flash");
    }

    #[test]
    fn qualified_switch_case_insensitive_provider() {
        let mut config = ambiguous_config();
        let (provider, _) = switch(&mut config, "provider b/glm-5").unwrap();
        assert_eq!(provider, "Provider B");
    }

    #[test]
    fn qualified_switch_after_unqualified_reorders_correctly() {
        let mut config = ambiguous_config();
        // Unqualified reorders Provider A to the front...
        let (provider, _) = switch(&mut config, "GLM-4.7").unwrap();
        assert_eq!(provider, "Provider A");

        // ...then a qualified switch still lands on the exact provider.
        let (provider, model) = switch(&mut config, "Provider B/glm-5").unwrap();
        assert_eq!(provider, "Provider B");
        assert_eq!(model, "GLM-5");
    }

    #[test]
    fn qualified_switch_unknown_provider_errors() {
        let mut config = ambiguous_config();
        // "nope/glm-4.7" has no such provider → exact match fails, and the
        // loose fallback searches the whole string which matches nothing.
        let err = switch(&mut config, "nope/glm-4.7").unwrap_err();
        assert!(err.contains("not found"), "unexpected error: {err}");
    }

    #[test]
    fn switch_model_never_persists_in_tests() {
        // Regression guard: switch_model must refuse to save config under
        // test builds — save() targets the real user config path and a test
        // once overwrote a developer's actual ~/.config/agentic/config.json.
        let mut commands = Commands::new(ambiguous_config());
        let err = commands.switch_model("GLM-4.7").unwrap_err();
        assert!(
            err.contains("refusing to save config under test"),
            "expected test-save refusal, got: {err}"
        );
    }
}
