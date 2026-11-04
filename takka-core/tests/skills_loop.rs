//! Integration tests for the skill system.
//!
//! Tests end-to-end: directory setup → discovery → tool execution.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use takka_core::skills::discovery::scan_skill_dir;
use takka_core::{
    discover_skills, DiscoveryConfig, Skill, SkillIndex, SkillMetadata, SkillTool, Tool,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn tmp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("skills_integration_{}", name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_skill(dir: &Path, name: &str, description: &str, body: &str) {
    let skill_dir = dir.join(name);
    fs::create_dir_all(&skill_dir).unwrap();
    let content = format!(
        "---\nname: {}\ndescription: {}\n---\n{}",
        name, description, body
    );
    fs::write(skill_dir.join("SKILL.md"), content).unwrap();
}

fn write_skill_with_refs(
    dir: &Path,
    name: &str,
    description: &str,
    body: &str,
    refs: &[(&str, &str)],
) {
    let skill_dir = dir.join(name);
    fs::create_dir_all(&skill_dir).unwrap();
    let content = format!(
        "---\nname: {}\ndescription: {}\n---\n{}",
        name, description, body
    );
    fs::write(skill_dir.join("SKILL.md"), content).unwrap();
    for (fname, fcontent) in refs {
        fs::write(skill_dir.join(fname), fcontent).unwrap();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn index_lookup_roundtrip() {
    let mut index = SkillIndex::new();

    let skill = Skill {
        metadata: SkillMetadata {
            name: "my-skill".to_string(),
            description: "A test skill".to_string(),
            ..Default::default()
        },
        dir: PathBuf::from("/tmp/my-skill"),
        content: "---\n...\n---\nbody".to_string(),
        frontmatter: "...".to_string(),
        body: "body".to_string(),
    };

    assert!(index.insert(skill));
    assert!(!index.is_empty());
    assert_eq!(index.len(), 1);

    let found = index.get("my-skill");
    assert!(found.is_some());
    assert_eq!(found.unwrap().description(), "A test skill");

    // Second insert with same name is rejected
    let dup = Skill {
        metadata: SkillMetadata {
            name: "my-skill".to_string(),
            description: "Duplicate".to_string(),
            ..Default::default()
        },
        dir: PathBuf::from("/tmp/my-skill-2"),
        content: "---\n...\n---\nother".to_string(),
        frontmatter: "...".to_string(),
        body: "other".to_string(),
    };
    assert!(!index.insert(dup));
    assert_eq!(index.len(), 1);
}

#[test]
fn index_remove_and_blocked() {
    let mut index = SkillIndex::new();

    let skill = Skill {
        metadata: SkillMetadata {
            name: "to-remove".to_string(),
            description: "Will be removed".to_string(),
            ..Default::default()
        },
        dir: PathBuf::from("/tmp/to-remove"),
        content: "".to_string(),
        frontmatter: "".to_string(),
        body: "".to_string(),
    };
    index.insert(skill);
    assert_eq!(index.len(), 1);

    index.add_blocked("bad-skill".to_string());
    assert_eq!(index.blocked(), &["bad-skill"]);

    let removed = index.remove("to-remove");
    assert!(removed.is_some());
    assert_eq!(index.len(), 0);
}

#[test]
fn skill_tool_e2e() {
    // Set up a real skill directory
    let dir = tmp_dir("tool_e2e");
    write_skill(
        &dir,
        "e2e-skill",
        "End-to-end test skill",
        "# E2E Skill\n\n## Usage\nRun the e2e tests with `cargo test`.",
    );

    // Build index from just this directory
    let mut index = SkillIndex::new();
    let config = DiscoveryConfig::default();
    scan_skill_dir(&dir, &config, &mut index);

    assert_eq!(index.len(), 1, "should have discovered e2e-skill");
    let skill = index.get("e2e-skill").unwrap();
    assert_eq!(skill.name(), "e2e-skill");
    assert_eq!(skill.description(), "End-to-end test skill");
    assert!(skill.body.contains("E2E Skill"));
}

#[test]
fn skill_tool_with_referenced_files() {
    let dir = tmp_dir("tool_refs");
    write_skill_with_refs(
        &dir,
        "config-skill",
        "Skill with config files",
        "# Config Skill\nLoad configs.",
        &[
            ("settings.json", r#"{"debug": true}"#),
            ("rules.yaml", "allow: all"),
        ],
    );

    let mut index = SkillIndex::new();
    let config = DiscoveryConfig::default();
    scan_skill_dir(&dir, &config, &mut index);

    assert_eq!(index.len(), 1);
    let skill = index.get("config-skill").unwrap();
    assert_eq!(skill.name(), "config-skill");

    // Now test the tool
    let index_arc = Arc::new(std::sync::RwLock::new(index));
    let tool = SkillTool::new(index_arc);

    let result = tool.execute(serde_json::json!({
        "name": "config-skill",
        "activate": false
    }));
    assert!(
        result.is_ok(),
        "skill tool should succeed: {:?}",
        result.err()
    );
    let val = result.unwrap();
    assert_eq!(val["skill"], "config-skill");
    let content = val["content"].as_str().unwrap();
    assert!(
        content.contains("settings.json"),
        "should include referenced file"
    );
    assert!(
        content.contains(r#""debug": true"#),
        "should include settings content"
    );
}

#[test]
fn skill_tool_missing_skill_returns_error() {
    let index = SkillIndex::new();
    let index_arc = Arc::new(std::sync::RwLock::new(index));
    let tool = SkillTool::new(index_arc);

    let result = tool.execute(serde_json::json!({
        "name": "nonexistent",
        "activate": false
    }));
    assert!(result.is_err());
    assert!(result.unwrap_err().0.contains("not found"));
}

#[test]
fn skill_tool_missing_name_param() {
    let index = SkillIndex::new();
    let index_arc = Arc::new(std::sync::RwLock::new(index));
    let tool = SkillTool::new(index_arc);

    let result = tool.execute(serde_json::json!({}));
    assert!(result.is_err());
    assert!(result.unwrap_err().0.contains("Missing required"));
}

#[test]
fn discover_skips_invalid_skill_dirs() {
    let dir = tmp_dir("invalid");

    // Valid skill
    write_skill(&dir, "valid-skill", "Valid", "# Valid");

    // Directory with no SKILL.md
    let no_skill = dir.join("no-skill");
    fs::create_dir_all(&no_skill).unwrap();

    // Directory with invalid SKILL.md
    let bad_skill = dir.join("bad-skill");
    fs::create_dir_all(&bad_skill).unwrap();
    fs::write(bad_skill.join("SKILL.md"), "Not valid frontmatter").unwrap();

    let mut index = SkillIndex::new();
    let config = DiscoveryConfig::default();
    scan_skill_dir(&dir, &config, &mut index);

    assert_eq!(index.len(), 1, "only valid-skill should be discovered");
    assert!(index.get("valid-skill").is_some());
}

#[test]
fn skills_system_prompt_section() {
    let mut index = SkillIndex::new();

    let s1 = Skill {
        metadata: SkillMetadata {
            name: "alpha".to_string(),
            description: "First skill".to_string(),
            ..Default::default()
        },
        dir: PathBuf::from("/tmp/alpha"),
        content: "".to_string(),
        frontmatter: "".to_string(),
        body: "".to_string(),
    };
    let s2 = Skill {
        metadata: SkillMetadata {
            name: "beta".to_string(),
            description: "Second skill".to_string(),
            ..Default::default()
        },
        dir: PathBuf::from("/tmp/beta"),
        content: "".to_string(),
        frontmatter: "".to_string(),
        body: "".to_string(),
    };
    index.insert(s1);
    index.insert(s2);

    let pairs: Vec<(&str, &str)> = index
        .all()
        .iter()
        .map(|s| (s.name(), s.description()))
        .collect();
    let section = takka_core::skills_system_section(&pairs).unwrap();

    assert!(section.contains("📦 alpha — First skill"));
    assert!(section.contains("📦 beta — Second skill"));
    assert!(section.contains("Skills"));
}

#[test]
fn integration_discovery_from_temp_dir() {
    // Simulate a project-level .agentic/skills/ directory
    let root = tmp_dir("integration_disc");
    let skills_dir = root.join(".agentic").join("skills");
    fs::create_dir_all(&skills_dir).unwrap();

    write_skill(
        &skills_dir,
        "project-skill",
        "A project skill",
        "# Project Skill\nDo project-specific things.",
    );

    // Set cwd to root and discover
    let prev = std::env::current_dir().ok();
    std::env::set_current_dir(&root).ok();

    let config = DiscoveryConfig::default();
    let index = discover_skills(&config);

    if let Some(prev) = prev {
        let _ = std::env::set_current_dir(prev);
    }

    // Should find our project skill (and possibly global skills)
    assert!(
        index.get("project-skill").is_some(),
        "discover_skills should find project-skill in .agentic/skills/"
    );
}

#[test]
fn discover_respects_blocklist() {
    let dir = tmp_dir("blocklist_integration");
    write_skill(&dir, "good", "Good skill", "# Good");
    write_skill(&dir, "bad", "Bad skill", "# Bad");

    let mut index = SkillIndex::new();
    let config = DiscoveryConfig {
        blocklist: vec!["bad".to_string()],
        compat_dirs: vec![],
    };
    scan_skill_dir(&dir, &config, &mut index);

    assert_eq!(index.len(), 1);
    assert!(index.get("good").is_some());
    assert!(index.get("bad").is_none());
}
