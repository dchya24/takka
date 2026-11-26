use anyhow::Result;
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Select};
use ratatui::style::Color as RColor;
use std::process::Command as ProcessCommand;
use takka_core::Config;

use crate::error::CommandError;
use crate::widgets::{components, inline};

use super::*;

/// Core helpers for `takka config migrate`: plan, guard, and execute
/// the legacy `~/.config/agentic` → `~/.config/takka` user-state move.
/// Split from the interactive [`Commands::config_migrate`] wrapper so
/// the unit tests (`config_migrate_tests`) stay hermetic: explicit
/// paths in, no HOME override, no interactivity.
pub(crate) mod config_migrate {
    use std::path::{Path, PathBuf};

    /// One movable piece of legacy user state.
    pub(crate) struct MigrateItem {
        pub source: PathBuf,
        pub dest: PathBuf,
        pub label: &'static str,
    }

    /// Plan what would move from the legacy dir to the new one:
    /// config.json, sessions/, memory.md — only pieces that exist.
    /// An empty result means "nothing to migrate".
    pub(crate) fn migration_items(legacy: &Path, new: &Path) -> Vec<MigrateItem> {
        let mut items = Vec::new();
        let config = legacy.join("config.json");
        if config.is_file() {
            items.push(MigrateItem {
                source: config,
                dest: new.join("config.json"),
                label: "config",
            });
        }
        let sessions = legacy.join("sessions");
        if sessions.is_dir() {
            items.push(MigrateItem {
                source: sessions,
                dest: new.join("sessions"),
                label: "sessions",
            });
        }
        let memory = legacy.join("memory.md");
        if memory.is_file() {
            items.push(MigrateItem {
                source: memory,
                dest: new.join("memory.md"),
                label: "memory",
            });
        }
        items
    }

    /// Refuse to run when the target config already exists (unless
    /// `--force`); an existing-but-configless target directory is fine.
    pub(crate) fn target_conflict(new: &Path, force: bool) -> Result<(), String> {
        let target = new.join("config.json");
        if target.exists() && !force {
            return Err(format!(
                "{} already exists; refusing to migrate over it (use --force to overwrite)",
                target.display()
            ));
        }
        Ok(())
    }

    /// Execute the plan: move each item, creating target directories
    /// as needed.
    pub(crate) fn apply_migration(items: &[MigrateItem]) -> Result<(), String> {
        for item in items {
            move_path(&item.source, &item.dest)?;
        }
        Ok(())
    }

    /// Move a file or directory tree. Falls back to copy + remove when
    /// a plain rename fails (e.g. across filesystems).
    pub(crate) fn move_path(source: &Path, dest: &Path) -> Result<(), String> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {}", parent.display(), e))?;
        }
        if std::fs::rename(source, dest).is_ok() {
            return Ok(());
        }
        if source.is_dir() {
            copy_dir_recursive(source, dest)
                .map_err(|e| format!("failed to copy {}: {}", source.display(), e))?;
            std::fs::remove_dir_all(source)
                .map_err(|e| format!("failed to remove {}: {}", source.display(), e))
        } else {
            std::fs::copy(source, dest)
                .map_err(|e| format!("failed to copy {}: {}", source.display(), e))?;
            std::fs::remove_file(source)
                .map_err(|e| format!("failed to remove {}: {}", source.display(), e))?;
            Ok(())
        }
    }

    /// Recursive directory copy used by the rename fallback.
    pub(crate) fn copy_dir_recursive(source: &Path, dest: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dest)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            let entry_type = entry.file_type()?;
            let target = dest.join(entry.file_name());
            if entry_type.is_dir() {
                copy_dir_recursive(&entry.path(), &target)?;
            } else {
                std::fs::copy(entry.path(), &target)?;
            }
        }
        Ok(())
    }
}

impl Commands {
    // ── Config init (default / --interactive / --provider) ──

    pub(crate) fn config_init(&self, interactive: bool, provider_name: Option<&str>) -> Result<()> {
        let config_path = Config::config_path();

        if config_path.exists() {
            let overwrite = Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt(format!(
                    "Config file already exists at {}. Overwrite?",
                    config_path.display()
                ))
                .default(false)
                .interact()
                .unwrap_or(false);
            if !overwrite {
                println!("Aborted.");
                return Ok(());
            }
        }

        let new_config = if interactive {
            self.config_init_wizard()?
        } else if let Some(name) = provider_name {
            self.config_init_provider(name)?
        } else {
            // Default: create fallback config
            print_info("Creating default config (OpenAI-compatible)...");
            Config::fallback()
        };

        // Ensure directory exists
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| CommandError::Config(format!("Failed to create directory: {}", e)))?;
        }

        let content = serde_json::to_string_pretty(&new_config)
            .map_err(|e| CommandError::Config(e.to_string()))?;

        std::fs::write(&config_path, content)
            .map_err(|e| CommandError::Config(format!("Failed to write config: {}", e)))?;

        print_success(&format!(
            "Config file created at: {}",
            config_path.display()
        ));

        // Next steps guidance
        if new_config.providers.iter().any(|p| p.api_key.is_empty()) {
            println!();
            print_info("Next steps:");
            println!("  1. Set your API key:");
            println!("     takka config edit");
            println!("     # or set env var: OPENAI_API_KEY=sk-...");
            println!("  2. Verify: takka config validate");
            println!("  3. Try it: takka run \"list files\"");
        } else {
            println!();
            print_info("You're all set! Try: takka run \"hello world\"");
        }

        Ok(())
    }

    /// Full interactive wizard using dialoguer
    pub(crate) fn config_init_wizard(&self) -> Result<Config> {
        inline::print_blank();
        inline::print_line(&components::banner_title(
            "🤖 Takka Config Wizard",
            RColor::Cyan,
            RColor::Magenta,
        ));
        inline::print_blank();

        // Step 1: Choose provider
        let provider_names: Vec<&str> = PROVIDER_PRESETS.iter().map(|p| p.name).collect();
        let provider_idx = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Step 1: Choose your LLM provider")
            .items(&provider_names)
            .default(0)
            .interact()
            .map_err(|e| anyhow::anyhow!("Failed to read input: {}", e))?;

        let preset = &PROVIDER_PRESETS[provider_idx];

        // Step 2: API base URL
        let default_base = if preset.api_base.is_empty() {
            String::new()
        } else {
            preset.api_base.to_string()
        };
        let api_base: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("Step 2: API base URL")
            .default(default_base)
            .interact()
            .map_err(|e| anyhow::anyhow!("Failed to read input: {}", e))?;

        // Step 3: API key
        let env_key = if provider_idx == 0 {
            "OPENAI_API_KEY"
        } else if provider_idx == 1 {
            "ANTHROPIC_API_KEY"
        } else {
            "API_KEY"
        };

        let env_value = std::env::var(env_key).unwrap_or_default();
        let api_key_default = if env_value.is_empty() {
            String::new()
        } else {
            env_value
        };

        let api_key: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt(format!("Step 3: API key (or env:${})", env_key))
            .default(api_key_default)
            .interact()
            .map_err(|e| anyhow::anyhow!("Failed to read input: {}", e))?;

        // Resolve if user typed env reference
        let resolved_key = api_key
            .strip_prefix('$')
            .and_then(|rest| std::env::var(rest).ok())
            .unwrap_or(api_key);

        // Step 4: Choose model
        let model = if preset.models.is_empty() {
            // Custom provider — ask for model name
            let model_name: String = Input::with_theme(&ColorfulTheme::default())
                .with_prompt("Step 4: Model name (e.g. gpt-4o)")
                .interact()
                .map_err(|e| anyhow::anyhow!("Failed to read input: {}", e))?;
            takka_core::ModelConfig {
                model: model_name,
                display_name: None,
                temperature: 0.7,
                max_tokens: 8192,
                capabilities: None,
            }
        } else {
            let model_labels: Vec<String> = preset
                .models
                .iter()
                .map(|(id, name)| format!("{} ({})", name, id))
                .collect();

            let model_idx = Select::with_theme(&ColorfulTheme::default())
                .with_prompt("Step 4: Choose default model")
                .items(&model_labels)
                .default(0)
                .interact()
                .map_err(|e| anyhow::anyhow!("Failed to read input: {}", e))?;

            takka_core::ModelConfig {
                model: preset.models[model_idx].0.to_string(),
                display_name: Some(preset.models[model_idx].1.to_string()),
                temperature: 0.7,
                max_tokens: 8192,
                capabilities: None,
            }
        };

        // Step 5: Safety options
        let auto_approve = Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt("Step 5: Auto-approve low-risk tool calls?")
            .default(true)
            .interact()
            .unwrap_or(true);

        // Build the config
        let config = Config {
            providers: vec![takka_core::ProviderConfig {
                name: preset.name.to_string(),
                provider_type: preset.provider_type.to_string(),
                api_base,
                api_key: resolved_key.clone(),
                models: vec![model],
                cache: takka_core::CacheConfig::default(),
            }],
            safety: takka_core::SafetyConfig {
                auto_approve_low_risk: auto_approve,
                blocked_commands: vec![
                    "rm -rf /".to_string(),
                    "mkfs".to_string(),
                    "dd if=".to_string(),
                ],
                allowed_domains: vec![],
                block_ip_urls: false,
            },
            output: takka_core::OutputConfig {
                color: true,
                stream: true,
                show_thoughts: true,
                show_tool_calls: true,
            },
            mcp_servers: std::collections::HashMap::new(),
            system_prompt: None,
            agent: takka_core::AgentLoopConfig::default(),
            skills: takka_core::SkillsConfig::default(),
            updates: takka_core::UpdatesConfig::default(),
        };

        // Summary
        println!();
        print_success("Configuration summary:");
        println!("  Provider: {}", config.providers[0].name);
        println!("  API Base: {}", config.providers[0].api_base);
        println!("  Model:    {}", config.providers[0].models[0].model);
        println!(
            "  API Key:  {}",
            if resolved_key.is_empty() {
                "(empty)".to_string()
            } else {
                format!(
                    "{}...{}",
                    &resolved_key[..4.min(resolved_key.len())],
                    &resolved_key[resolved_key.len().saturating_sub(4)..]
                )
            }
        );

        Ok(config)
    }

    /// Quick provider preset setup
    pub(crate) fn config_init_provider(&self, name: &str) -> Result<Config> {
        let preset = PROVIDER_PRESETS
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| {
                let available: Vec<&str> = PROVIDER_PRESETS.iter().map(|p| p.name).collect();
                CommandError::not_found_with_suggestion(
                    format!("provider '{}'", name),
                    format!("available: {}", available.join(", ")),
                )
            })?;

        if preset.name == "custom" {
            // Redirect to interactive wizard for custom
            return self.config_init_wizard();
        }

        // Try to get API key from env
        let env_var = format!("{}_API_KEY", preset.name.to_uppercase());
        let api_key = std::env::var(&env_var)
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .unwrap_or_default();

        let default_model = preset
            .models
            .first()
            .map(|(id, display)| takka_core::ModelConfig {
                model: id.to_string(),
                display_name: Some(display.to_string()),
                temperature: 0.7,
                max_tokens: 8192,
                capabilities: None,
            })
            .unwrap_or(takka_core::ModelConfig {
                model: "gpt-4o".to_string(),
                display_name: Some("GPT-4o".to_string()),
                temperature: 0.7,
                max_tokens: 8192,
                capabilities: None,
            });

        print_info(&format!(
            "Creating config for {} ({})",
            preset.name, preset.api_base
        ));

        let config = Config {
            providers: vec![takka_core::ProviderConfig {
                name: preset.name.to_string(),
                provider_type: preset.provider_type.to_string(),
                api_base: preset.api_base.to_string(),
                api_key,
                models: vec![default_model],
                cache: takka_core::CacheConfig::default(),
            }],
            safety: takka_core::SafetyConfig::default(),
            output: takka_core::OutputConfig::default(),
            mcp_servers: std::collections::HashMap::new(),
            system_prompt: None,
            agent: takka_core::AgentLoopConfig::default(),
            skills: takka_core::SkillsConfig::default(),
            updates: takka_core::UpdatesConfig::default(),
        };

        Ok(config)
    }

    // ── Config edit ─────────────────────────────────────────

    pub(crate) fn config_edit(&self) -> Result<()> {
        let config_path = Config::config_path();

        if !config_path.exists() {
            return Err(anyhow::anyhow!(
                "Config file not found. Run 'takka config init' to create one."
            ));
        }

        let editor = std::env::var("EDITOR")
            .or_else(|_| std::env::var("VISUAL"))
            .unwrap_or_else(|_| {
                if cfg!(windows) {
                    "notepad".to_string()
                } else if cfg!(target_os = "macos") {
                    "open".to_string()
                } else {
                    "nano".to_string()
                }
            });

        print_info(&format!("Opening {}...", config_path.display()));

        let status = ProcessCommand::new(&editor)
            .arg(&config_path)
            .status()
            .map_err(|e| anyhow::anyhow!("Failed to open editor: {}", e))?;

        if status.success() {
            print_success("Config file updated.");
        } else {
            print_warning("Editor exited with non-zero status.");
        }

        Ok(())
    }

    // ── Config validate ─────────────────────────────────────

    pub(crate) fn config_validate(&self, verbose: bool) -> Result<()> {
        print_info("Validating configuration...");
        println!();

        let mut errors = Vec::new();
        let mut warnings = Vec::new();

        // Check providers
        if self.config.providers.is_empty() {
            errors.push("No providers configured".to_string());
        }

        for (i, provider) in self.config.providers.iter().enumerate() {
            let label = if provider.name.is_empty() {
                format!("Provider #{}", i + 1)
            } else {
                format!("Provider '{}'", provider.name)
            };

            if verbose {
                println!("  Checking {}...", label);
            }

            if provider.name.is_empty() {
                errors.push(format!("{}: name is empty", label));
            }
            if provider.provider_type.is_empty() {
                errors.push(format!("{}: type is empty", label));
            }
            if provider.api_base.is_empty() {
                errors.push(format!("{}: API base URL is empty", label));
            }
            if provider.api_key.is_empty() {
                warnings.push(format!("{}: API key is empty", label));
            }
            if provider.models.is_empty() {
                warnings.push(format!("{}: No models configured", label));
            }

            if verbose {
                println!("    API Base: {}", provider.api_base);
                println!(
                    "    API Key:  {}",
                    if provider.api_key.is_empty() {
                        "(empty)".to_string()
                    } else {
                        format!(
                            "{}...{}",
                            &provider.api_key[..4.min(provider.api_key.len())],
                            &provider.api_key[provider.api_key.len().saturating_sub(4)..]
                        )
                    }
                );
                println!("    Models:   {}", provider.models.len());
                for m in &provider.models {
                    println!(
                        "      • {} (temp: {}, max_tokens: {})",
                        m.model, m.temperature, m.max_tokens
                    );
                }
            }
        }

        // Check safety
        if self.config.safety.blocked_commands.is_empty() {
            warnings.push(
                "No blocked commands configured. Consider adding dangerous commands.".to_string(),
            );
        }

        if verbose {
            println!("  Checking safety config...");
            println!(
                "    Auto-approve low risk: {}",
                self.config.safety.auto_approve_low_risk
            );
            println!(
                "    Blocked commands: {}",
                self.config.safety.blocked_commands.join(", ")
            );
        }

        // Print results
        println!();
        if errors.is_empty() && warnings.is_empty() {
            print_success("Configuration is valid!");
            print_info(&format!("Config file: {}", Config::config_path().display()));
        } else {
            for error in &errors {
                print_error(error, self.color_enabled);
            }
            for warning in &warnings {
                print_warning(warning);
            }

            if !errors.is_empty() {
                return Err(anyhow::anyhow!(
                    "Configuration validation failed with {} error(s)",
                    errors.len()
                ));
            }
            print_warning(&format!(
                "Configuration valid with {} warning(s)",
                warnings.len()
            ));
        }

        Ok(())
    }

    // ── Config reset ────────────────────────────────────────

    pub(crate) fn config_reset(&self, force: bool) -> Result<()> {
        let config_path = Config::config_path();

        if config_path.exists() {
            if !force {
                let confirm = Confirm::with_theme(&ColorfulTheme::default())
                    .with_prompt(format!(
                        "This will reset your config at {}. Continue?",
                        config_path.display()
                    ))
                    .default(false)
                    .interact()
                    .unwrap_or(false);
                if !confirm {
                    println!("Aborted.");
                    return Ok(());
                }
            }

            std::fs::remove_file(&config_path)
                .map_err(|e| CommandError::Config(format!("Failed to remove config: {}", e)))?;
        }

        let default_config = Config::fallback();

        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| CommandError::Config(format!("Failed to create directory: {}", e)))?;
        }

        let content = serde_json::to_string_pretty(&default_config)
            .map_err(|e| CommandError::Config(e.to_string()))?;

        std::fs::write(&config_path, content)
            .map_err(|e| CommandError::Config(format!("Failed to write config: {}", e)))?;

        print_success(&format!(
            "Default config created at: {}",
            config_path.display()
        ));
        print_info("Remember to set your API key in the config file or via environment variables.");
        Ok(())
    }

    // ── Config path ─────────────────────────────────────────

    pub(crate) fn config_path(&self) -> Result<()> {
        println!("{}", Config::config_path().display());
        Ok(())
    }

    // ── Config migrate ──────────────────────────────────────

    /// Move legacy `~/.config/agentic` user state (config, sessions,
    /// memory) to `~/.config/takka`. Interactive `y/N` confirmation
    /// unless `--yes`; refuses an existing target config unless
    /// `--force`. Idempotent: with nothing left at the legacy path it
    /// reports there is nothing to migrate.
    pub(crate) fn config_migrate(&self, yes: bool, force: bool) -> Result<()> {
        let legacy = takka_core::config::legacy_config_dir();
        let new = takka_core::config::config_dir();

        let items = config_migrate::migration_items(&legacy, &new);
        if items.is_empty() {
            print_info(&format!(
                "Nothing to migrate: no legacy data found at {}",
                legacy.display()
            ));
            return Ok(());
        }

        config_migrate::target_conflict(&new, force).map_err(CommandError::Config)?;

        println!(
            "Moving legacy user data from {} to {}:",
            legacy.display(),
            new.display()
        );
        for item in &items {
            println!(
                "  {:<8} {} → {}",
                item.label,
                item.source.display(),
                item.dest.display()
            );
        }

        if !yes {
            let confirm = Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt("Continue?")
                .default(false)
                .interact()
                .unwrap_or(false);
            if !confirm {
                println!("Aborted.");
                return Ok(());
            }
        }

        config_migrate::apply_migration(&items).map_err(CommandError::Config)?;
        print_success(&format!(
            "Migration complete — your data now lives at {}",
            new.display()
        ));
        Ok(())
    }

    // ── Config backup ───────────────────────────────────────

    pub(crate) fn config_backup(&self) -> Result<()> {
        // Back up the config that would actually load: new location,
        // or the legacy file for a pre-migration user.
        let (config_path, _) = Config::effective_config_path();

        if !config_path.exists() {
            return Err(anyhow::anyhow!(
                "No config file found at: {}",
                config_path.display()
            ));
        }

        let backup_dir = config_path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("backups");

        std::fs::create_dir_all(&backup_dir)
            .map_err(|e| CommandError::Config(format!("Failed to create backup dir: {}", e)))?;

        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let backup_file = backup_dir.join(format!("config_{}.json", timestamp));

        std::fs::copy(&config_path, &backup_file)
            .map_err(|e| CommandError::Config(format!("Failed to create backup: {}", e)))?;

        print_success(&format!("Backup created at: {}", backup_file.display()));
        Ok(())
    }

    // ── Config restore ──────────────────────────────────────

    pub(crate) fn config_restore(&self, file: &str) -> Result<()> {
        let source = std::path::PathBuf::from(file);

        if !source.exists() {
            return Err(anyhow::anyhow!("Backup file not found: {}", file));
        }

        let config_path = Config::config_path();

        if config_path.exists() {
            let backup_dir = config_path
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .join("backups");
            std::fs::create_dir_all(&backup_dir).ok();
            let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
            let pre_restore = backup_dir.join(format!("pre_restore_{}.json", ts));
            std::fs::copy(&config_path, &pre_restore).ok();
            print_info(&format!(
                "Current config backed up to: {}",
                pre_restore.display()
            ));
        }

        std::fs::copy(&source, &config_path)
            .map_err(|e| CommandError::Config(format!("Failed to restore config: {}", e)))?;

        print_success(&format!("Config restored from: {}", file));
        Ok(())
    }

    // ── Config export ───────────────────────────────────────

    pub(crate) fn config_export(&self) -> Result<()> {
        let config_path = Config::config_path();

        if !config_path.exists() {
            return Err(anyhow::anyhow!(
                "No config file found. Run 'takka config init' first."
            ));
        }

        let content = std::fs::read_to_string(&config_path)
            .map_err(|e| CommandError::Config(format!("Failed to read config: {}", e)))?;

        let mut json: serde_json::Value = serde_json::from_str(&content)
            .map_err(|e| CommandError::Config(format!("Invalid JSON: {}", e)))?;

        if let Some(providers) = json.get_mut("providers").and_then(|p| p.as_array_mut()) {
            for provider in providers.iter_mut() {
                if let Some(api_key) = provider.get_mut("api_key") {
                    if let Some(key) = api_key.as_str() {
                        if key.len() > 8 {
                            *api_key = serde_json::Value::String(format!(
                                "{}...{}",
                                &key[..4],
                                &key[key.len() - 4..]
                            ));
                        } else {
                            *api_key = serde_json::Value::String("****".to_string());
                        }
                    }
                }
            }
        }

        let exported =
            serde_json::to_string_pretty(&json).map_err(|e| CommandError::Config(e.to_string()))?;

        println!("{}", exported);
        print_info("API keys have been masked for safe sharing.");
        Ok(())
    }

    // ── Config import ───────────────────────────────────────

    pub(crate) fn config_import(&self, file: &str) -> Result<()> {
        let source = std::path::PathBuf::from(file);

        if !source.exists() {
            return Err(anyhow::anyhow!("Import file not found: {}", file));
        }

        let content = std::fs::read_to_string(&source)
            .map_err(|e| CommandError::Config(format!("Failed to read import file: {}", e)))?;

        let _: serde_json::Value = serde_json::from_str(&content)
            .map_err(|e| CommandError::Config(format!("Invalid JSON in import file: {}", e)))?;

        let config_path = Config::config_path();

        if config_path.exists() {
            let backup_dir = config_path
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .join("backups");
            std::fs::create_dir_all(&backup_dir).ok();
            let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
            let pre_import = backup_dir.join(format!("pre_import_{}.json", ts));
            std::fs::copy(&config_path, &pre_import).ok();
            print_info(&format!(
                "Current config backed up to: {}",
                pre_import.display()
            ));
        }

        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| CommandError::Config(format!("Failed to create directory: {}", e)))?;
        }

        std::fs::write(&config_path, &content)
            .map_err(|e| CommandError::Config(format!("Failed to write config: {}", e)))?;

        print_success(&format!("Config imported from: {}", file));
        print_info("Run 'takka config validate' to verify the imported config.");
        Ok(())
    }
}

/// Unit tests for the legacy→takka config migration. The migration
/// core is split into pure path-in/path-out helpers so these tests are
/// hermetic (explicit tempdirs, no HOME override, no interactivity).
#[cfg(test)]
mod config_migrate_tests {
    use super::config_migrate::{
        apply_migration, copy_dir_recursive, migration_items, move_path, target_conflict,
        MigrateItem,
    };
    use std::path::{Path, PathBuf};

    fn tempdir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("takka-migrate-test-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn seed_legacy_state(legacy: &Path) {
        std::fs::create_dir_all(legacy.join("sessions")).unwrap();
        std::fs::write(legacy.join("config.json"), r#"{"providers":[]}"#).unwrap();
        std::fs::write(legacy.join("sessions").join("ses_1.json"), "{}").unwrap();
        std::fs::write(legacy.join("memory.md"), "notes").unwrap();
    }

    #[test]
    fn plan_lists_config_sessions_and_memory() {
        let root = tempdir("plan_full");
        let legacy = root.join("agentic");
        let new = root.join("takka");
        seed_legacy_state(&legacy);

        let items = migration_items(&legacy, &new);
        assert_eq!(items.len(), 3, "config + sessions + memory expected");
        assert_eq!(items[0].source, legacy.join("config.json"));
        assert_eq!(items[0].dest, new.join("config.json"));
        assert_eq!(items[1].source, legacy.join("sessions"));
        assert_eq!(items[1].dest, new.join("sessions"));
        assert_eq!(items[2].source, legacy.join("memory.md"));
        assert_eq!(items[2].dest, new.join("memory.md"));
    }

    #[test]
    fn plan_is_empty_when_legacy_missing() {
        let root = tempdir("plan_empty");
        let items = migration_items(&root.join("agentic"), &root.join("takka"));
        assert!(items.is_empty(), "nothing to migrate → empty plan");
    }

    #[test]
    fn plan_skips_absent_pieces() {
        let root = tempdir("plan_partial");
        let legacy = root.join("agentic");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("memory.md"), "only memory").unwrap();

        let items = migration_items(&legacy, &root.join("takka"));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].source, legacy.join("memory.md"));
    }

    #[test]
    fn target_conflict_refuses_existing_new_config_without_force() {
        let root = tempdir("conflict");
        let new = root.join("takka");
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(new.join("config.json"), r#"{"providers":[]}"#).unwrap();

        assert!(
            target_conflict(&new, false).is_err(),
            "must refuse pre-migration target"
        );
        assert!(
            target_conflict(&new, true).is_ok(),
            "--force must override the refusal"
        );
    }

    #[test]
    fn target_conflict_ok_for_fresh_or_empty_target() {
        let root = tempdir("conflict_fresh");
        let new = root.join("takka");
        assert!(
            target_conflict(&new, false).is_ok(),
            "no new config yet → ok"
        );
        std::fs::create_dir_all(&new).unwrap();
        assert!(
            target_conflict(&new, false).is_ok(),
            "existing-but-configless target dir → ok"
        );
    }

    #[test]
    fn apply_moves_files_and_directories() {
        let root = tempdir("apply_full");
        let legacy = root.join("agentic");
        let new = root.join("takka");
        seed_legacy_state(&legacy);

        let items = migration_items(&legacy, &new);
        apply_migration(&items).unwrap();

        assert!(new.join("config.json").is_file());
        assert!(new.join("sessions").join("ses_1.json").is_file());
        assert!(new.join("memory.md").is_file());
        assert!(
            !legacy.join("config.json").exists(),
            "source must be moved, not copied"
        );
        assert!(!legacy.join("sessions").exists());
        assert!(!legacy.join("memory.md").exists());
    }

    #[test]
    fn migration_is_idempotent_after_apply() {
        let root = tempdir("idempotent");
        let legacy = root.join("agentic");
        let new = root.join("takka");
        seed_legacy_state(&legacy);

        apply_migration(&migration_items(&legacy, &new)).unwrap();
        // Second run: nothing left on the legacy side → empty plan,
        // i.e. `takka config migrate` twice is safe.
        assert!(migration_items(&legacy, &new).is_empty());
    }

    #[test]
    fn apply_creates_missing_target_directories() {
        let root = tempdir("apply_deep");
        let legacy = root.join("agentic");
        let new = root.join("takka").join("nested");
        std::fs::create_dir_all(legacy.join("sessions").join("nested-session")).unwrap();
        std::fs::write(
            legacy
                .join("sessions")
                .join("nested-session")
                .join("a.json"),
            "{}",
        )
        .unwrap();

        let items = vec![MigrateItem {
            source: legacy.join("sessions"),
            dest: new.join("sessions"),
            label: "sessions",
        }];
        apply_migration(&items).unwrap();
        assert!(new
            .join("sessions")
            .join("nested-session")
            .join("a.json")
            .is_file());
    }

    #[test]
    fn copy_dir_recursive_copies_tree() {
        let root = tempdir("copy_tree");
        let src = root.join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), "A").unwrap();
        std::fs::write(src.join("sub").join("b.txt"), "B").unwrap();

        let dest = root.join("dest");
        copy_dir_recursive(&src, &dest).unwrap();
        assert_eq!(std::fs::read_to_string(dest.join("a.txt")).unwrap(), "A");
        assert_eq!(
            std::fs::read_to_string(dest.join("sub").join("b.txt")).unwrap(),
            "B"
        );
    }

    #[test]
    fn move_path_survives_existing_parent() {
        let root = tempdir("move_path");
        std::fs::write(root.join("x.json"), "{}").unwrap();
        move_path(&root.join("x.json"), &root.join("deep").join("y.json")).unwrap();
        assert!(root.join("deep").join("y.json").is_file());
        assert!(!root.join("x.json").exists());
    }
}
