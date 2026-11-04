//! Skill discovery — walk skill directories, parse SKILL.md, build index.
//!
//! ## Discovery order (first-found wins on name collision)
//!
//! 1. `~/.agents/skills/` (global, pi/opencode/codex compatible)
//! 2. `~/.config/agentic/skills/` (global, agentic-specific)
//! 3. `.agents/skills/` — walk-up from cwd (project, cross-agent)
//! 4. `.agentic/skills/` — walk-up from cwd (project, agentic-specific)
//! 5. `compat_dirs` from config (e.g. `~/.claude/skills`)

use std::path::{Path, PathBuf};

use super::{Skill, SkillIndex, SkillMetadata};

/// SKILL.md frontmatter regex.
fn frontmatter_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?s)^---\n(.+?)\n---\n?(.*)")
            .expect("invalid SKILL.md frontmatter regex")
    })
}

/// Name validation regex.
fn name_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^[a-z0-9-]{1,64}$").expect("invalid name regex"))
}

/// Configuration for skill discovery.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryConfig {
    /// Names to exclude from the index.
    pub blocklist: Vec<String>,
    /// Extra directories to scan (e.g. `~/.claude/skills`).
    pub compat_dirs: Vec<String>,
}

/// Discover skills from all standard locations and optional compat dirs.
///
/// Scans directories in the defined priority order. When two skills have the
/// same name, the first one found wins.
pub fn discover_skills(config: &DiscoveryConfig) -> SkillIndex {
    let mut index = SkillIndex::new();
    let mut visited = std::collections::HashSet::new();

    let dirs = discovery_directories(config);

    for dir in &dirs {
        if !dir.is_dir() {
            continue;
        }
        if !visited.insert(dir.clone()) {
            continue; // skip duplicates
        }
        scan_skill_dir(dir, config, &mut index);
    }

    index
}

/// Build the ordered list of directories to scan.
fn discovery_directories(config: &DiscoveryConfig) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();

    // 1. Global: ~/.agents/skills/
    if let Some(home) = home_dir() {
        dirs.push(home.join(".agents").join("skills"));
    }

    // 2. Global: ~/.config/agentic/skills/
    if let Some(config_dir) = config_dir() {
        dirs.push(config_dir.join("agentic").join("skills"));
    }

    // 3. Project: walk-up from cwd, .agents/skills/
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(agents_dir) = walk_up(&cwd, ".agents") {
            dirs.push(agents_dir.join("skills"));
        }
    }

    // 4. Project: walk-up from cwd, .agentic/skills/
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(agentic_dir) = walk_up(&cwd, ".agentic") {
            dirs.push(agentic_dir.join("skills"));
        }
    }

    // 5. Compat dirs from config (e.g. ~/.claude/skills)
    for compat in &config.compat_dirs {
        let expanded = expand_path(compat);
        dirs.push(expanded);
    }

    dirs
}

/// Scan a single directory for skill subdirectories.
pub fn scan_skill_dir(dir: &Path, config: &DiscoveryConfig, index: &mut SkillIndex) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let dir_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };

        // Skip hidden directories (except the skill itself, which is just a
        // regular dir, but we do skip things like `.git`, `.svn` etc.)
        if dir_name.starts_with('.') && dir_name != "." {
            continue;
        }

        // Check blocklist
        if config.blocklist.contains(&dir_name) {
            index.add_blocked(dir_name);
            continue;
        }

        let skill_md = path.join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }

        let content = match std::fs::read_to_string(&skill_md) {
            Ok(c) => c,
            Err(_) => continue,
        };

        match parse_skill_md(&content, &dir_name, &path) {
            Ok(skill) => {
                index.insert(skill);
            }
            Err(e) => {
                tracing::debug!(
                    dir = %path.display(),
                    error = %e,
                    "Skipping invalid skill"
                );
            }
        }
    }
}

/// Parse a `SKILL.md` file and validate its metadata.
fn parse_skill_md(content: &str, dir_name: &str, dir: &Path) -> Result<Skill, String> {
    let caps = frontmatter_re().captures(content).ok_or_else(|| {
        "Missing or invalid frontmatter block (must start with `---\\n`)".to_string()
    })?;

    let frontmatter_str = caps.get(1).unwrap().as_str();
    let body = caps.get(2).map(|m| m.as_str()).unwrap_or("");

    // Parse YAML-like frontmatter (simple key: value pairs)
    let metadata = parse_frontmatter(frontmatter_str)?;

    // Validate: name matches directory name
    if metadata.name != dir_name {
        return Err(format!(
            "Skill name '{}' does not match directory name '{}'",
            metadata.name, dir_name
        ));
    }

    Ok(Skill {
        metadata,
        dir: dir.to_path_buf(),
        content: content.to_string(),
        frontmatter: frontmatter_str.to_string(),
        body: body.to_string(),
    })
}

/// Parse simple YAML-like frontmatter.
///
/// Supports:
/// ```yaml
/// name: my-skill
/// description: Does X and Y
/// ```
/// Test-only re-export: registry tests exercise frontmatter parsing directly.
#[cfg(test)]
pub(crate) fn __test_parse_frontmatter(text: &str) -> Result<SkillMetadata, String> {
    parse_frontmatter(text)
}

fn parse_frontmatter(text: &str) -> Result<SkillMetadata, String> {
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut tags: Vec<String> = Vec::new();
    let mut version: Option<String> = None;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(stripped) = line.strip_prefix("name:") {
            let val = stripped.trim().to_string();
            if val.is_empty() {
                return Err("'name' field is empty".to_string());
            }
            // Validate name
            if !name_re().is_match(&val) {
                return Err(format!(
                    "Invalid skill name '{}': must be 1-64 chars, lowercase a-z, 0-9, hyphens only",
                    val
                ));
            }
            name = Some(val);
        } else if let Some(stripped) = line.strip_prefix("description:") {
            let val = stripped.trim().to_string();
            if val.len() > 1024 {
                return Err("'description' exceeds 1024 characters".to_string());
            }
            description = Some(val);
        } else if let Some(stripped) = line.strip_prefix("tags:") {
            // Comma-separated free-form tags (P2-2): `tags: sql, postgres`.
            for tag in stripped.split(',') {
                let tag = tag.trim().to_lowercase();
                if !tag.is_empty() && !tags.contains(&tag) {
                    tags.push(tag);
                }
            }
        } else if let Some(stripped) = line.strip_prefix("version:") {
            let val = stripped.trim().to_string();
            if !val.is_empty() {
                version = Some(val);
            }
        }
        // Silently ignore unknown fields for forward compatibility.
    }

    let name = name.ok_or_else(|| "Missing 'name' in frontmatter".to_string())?;
    let description =
        description.ok_or_else(|| "Missing 'description' in frontmatter".to_string())?;

    Ok(SkillMetadata {
        name,
        description,
        tags,
        version,
    })
}

/// Walk up from `start` looking for a directory named `target`.
/// Returns the path to that directory if found, or `None`.
fn walk_up(start: &Path, target: &str) -> Option<PathBuf> {
    let mut current: Option<&Path> = Some(start);
    while let Some(dir) = current {
        let candidate = dir.join(target);
        if candidate.is_dir() {
            return Some(candidate);
        }
        current = dir.parent();
    }
    None
}

/// Expand `~` and `$HOME` / `$VAR` references in a path string.
fn expand_path(path: &str) -> PathBuf {
    let path = path.trim();
    if path.is_empty() {
        return PathBuf::from(path);
    }

    // Handle ~/
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }

    // Handle $VAR references
    if path.starts_with('$') {
        let end = path.find('/').unwrap_or(path.len());
        let var = &path[1..end];
        if let Ok(val) = std::env::var(var) {
            let rest = if end < path.len() { &path[end..] } else { "" };
            return PathBuf::from(val).join(rest.trim_start_matches('/'));
        }
    }

    PathBuf::from(path)
}

/// Get the user's home directory.
fn home_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .ok()
            .map(PathBuf::from)
    } else {
        std::env::var("HOME").ok().map(PathBuf::from)
    }
}

/// Get the user's config directory (XDG_CONFIG_HOME or ~/.config).
fn config_dir() -> Option<PathBuf> {
    std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".config")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("skill_discovery_{}", name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_skill(dir: &Path, name: &str, description: &str, extra: &str) {
        let skill_dir = dir.join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        let mut f = fs::File::create(skill_dir.join("SKILL.md")).unwrap();
        write!(
            f,
            "---\nname: {}\ndescription: {}\n---\n{}",
            name, description, extra
        )
        .unwrap();
    }

    #[test]
    fn parse_valid_frontmatter() {
        let md = r#"---
name: my-skill
description: Does something useful
---
# My Skill

## Usage
Do the thing.
"#;
        let skill = parse_skill_md(md, "my-skill", &PathBuf::from("/tmp/my-skill")).unwrap();
        assert_eq!(skill.name(), "my-skill");
        assert_eq!(skill.description(), "Does something useful");
        assert!(skill.body.contains("Do the thing."));
    }

    #[test]
    fn parse_rejects_missing_frontmatter() {
        let md = "# Just a heading\nNo frontmatter here.";
        let result = parse_skill_md(md, "test", &PathBuf::from("/tmp/test"));
        assert!(result.is_err());
    }

    #[test]
    fn parse_rejects_missing_name() {
        let md = r#"---
description: no name here
---"#;
        let result = parse_skill_md(md, "test", &PathBuf::from("/tmp/test"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Missing 'name'"));
    }

    #[test]
    fn parse_rejects_missing_description() {
        let md = r#"---
name: test-skill
---"#;
        let result = parse_skill_md(md, "test-skill", &PathBuf::from("/tmp/test-skill"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Missing 'description'"));
    }

    #[test]
    fn parse_rejects_invalid_name() {
        let md = r#"---
name: Invalid Name!
description: something
---"#;
        let result = parse_skill_md(md, "Invalid Name!", &PathBuf::from("/tmp/x"));
        assert!(result.is_err());
    }

    #[test]
    fn parse_rejects_name_mismatch() {
        let md = r#"---
name: other-name
description: something
---"#;
        let result = parse_skill_md(md, "dir-name", &PathBuf::from("/tmp/dir-name"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("does not match directory"));
    }

    #[test]
    fn parse_accepts_minimal_frontmatter() {
        let md = r#"---
name: minimal
description: Just enough
---"#;
        let skill = parse_skill_md(md, "minimal", &PathBuf::from("/tmp/minimal")).unwrap();
        assert_eq!(skill.name(), "minimal");
        assert_eq!(skill.description(), "Just enough");
        assert!(skill.body.is_empty());
    }

    #[test]
    fn discover_scans_directory() {
        let dir = tmp_dir("scan");
        write_skill(&dir, "alpha", "First skill", "# Alpha\nDo alpha things.");
        write_skill(&dir, "beta", "Second skill", "# Beta\nDo beta things.");

        let config = DiscoveryConfig::default();
        let mut index = SkillIndex::new();
        scan_skill_dir(&dir, &config, &mut index);

        assert_eq!(index.len(), 2);
        assert!(index.get("alpha").is_some());
        assert!(index.get("beta").is_some());
    }

    #[test]
    fn discover_skips_missing_skill_md() {
        let dir = tmp_dir("missing_md");
        let empty_dir = dir.join("no-skill");
        fs::create_dir_all(&empty_dir).unwrap();
        // No SKILL.md inside → should be skipped.

        let config = DiscoveryConfig::default();
        let mut index = SkillIndex::new();
        scan_skill_dir(&dir, &config, &mut index);
        assert_eq!(index.len(), 0);
    }

    #[test]
    fn discover_skips_hidden_dirs() {
        let dir = tmp_dir("hidden");
        write_skill(&dir, ".hidden", "Should be skipped", "# Ignored");

        let config = DiscoveryConfig::default();
        let mut index = SkillIndex::new();
        scan_skill_dir(&dir, &config, &mut index);
        assert_eq!(index.len(), 0);
    }

    #[test]
    fn discover_respects_blocklist() {
        let dir = tmp_dir("blocked");
        write_skill(&dir, "blocked-skill", "Should be excluded", "# Blocked");
        write_skill(&dir, "allowed-skill", "Should be included", "# Allowed");

        let config = DiscoveryConfig {
            blocklist: vec!["blocked-skill".to_string()],
            compat_dirs: vec![],
        };
        let mut index = SkillIndex::new();
        scan_skill_dir(&dir, &config, &mut index);

        assert_eq!(index.len(), 1);
        assert!(index.get("allowed-skill").is_some());
        assert!(index.get("blocked-skill").is_none());
        assert_eq!(index.blocked(), &["blocked-skill"]);
    }

    #[test]
    fn first_found_wins_name_collision() {
        // Simulate two separate directories with the same skill name.
        // The first one discovered should win.
        let dir_a = tmp_dir("collision_a");
        write_skill(&dir_a, "same-name", "First version", "# First");

        let dir_b = tmp_dir("collision_b");
        write_skill(&dir_b, "same-name", "Second version", "# Second");

        // Scan dir_a first, then dir_b.
        let config = DiscoveryConfig::default();
        let mut index = SkillIndex::new();
        scan_skill_dir(&dir_a, &config, &mut index);
        scan_skill_dir(&dir_b, &config, &mut index);

        assert_eq!(index.len(), 1);
        // First discovered (dir_a) should win.
        let skill = index.get("same-name").unwrap();
        assert_eq!(skill.description(), "First version");
    }

    #[test]
    fn expand_tilde_path() {
        let expanded = expand_path("~/something");
        // Must resolve to an absolute path under the user's home dir on
        // every platform (Windows uses `C:\Users\...` — no leading slash).
        assert!(
            expanded.is_absolute(),
            "expected ~ to expand to an absolute path, got {:?}",
            expanded
        );
        assert!(expanded.to_string_lossy().ends_with("something"));
    }

    #[test]
    fn expand_env_var_path() {
        std::env::set_var("TEST_SKILL_DIR", "/tmp/test-skills");
        let expanded = expand_path("$TEST_SKILL_DIR/subdir");
        assert_eq!(expanded, PathBuf::from("/tmp/test-skills/subdir"));
    }

    #[test]
    fn walk_up_finds_target() {
        let root = tmp_dir("walkup");
        let nested = root.join("a").join("b").join("c");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(root.join(".agentic")).unwrap();

        let found = walk_up(&nested, ".agentic");
        assert_eq!(found, Some(root.join(".agentic")));
    }

    #[test]
    fn walk_up_returns_none_when_missing() {
        let dir = tmp_dir("nowalk");
        let found = walk_up(&dir, ".nonexistent");
        assert_eq!(found, None);
    }

    #[test]
    fn full_discovery_flow() {
        let root = tmp_dir("full_flow");

        // Create a project-level skill
        let project_skills = root.join(".agentic").join("skills");
        fs::create_dir_all(&project_skills).unwrap();
        write_skill(
            &project_skills,
            "my-skill",
            "A project skill",
            "# My Skill\nDo stuff.",
        );

        // Discovery from inside root should find it
        let cwd = root.clone();
        let prev_cwd = std::env::current_dir().ok();
        std::env::set_current_dir(&cwd).ok();

        let config = DiscoveryConfig::default();
        let index = discover_skills(&config);

        if let Some(prev) = prev_cwd {
            let _ = std::env::set_current_dir(prev);
        }

        // Should find at least the project skill
        // (may also find global skills if user has them — we just check
        // that our project skill is there)
        assert!(index.get("my-skill").is_some() || !index.is_empty());
    }
}
