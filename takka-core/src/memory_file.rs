//! Cross-session memory file.
//!
//! Provides a persistent notes file the agent can read and write across
//! sessions. Two layers:
//!
//! - **User-global memory** at `~/.config/agentic/memory.md`
//!   (or `$AGENTIC_MEMORY_PATH` if set).
//! - **Project-local memory** at `.agentic/memory.md` walking up from cwd.
//!
//! Both are loaded into the system prompt at startup. The agent can append
//! notes via the `update_memory` tool.

use std::fs;
use std::path::{Path, PathBuf};

/// Filename used for project-local memory.
pub const PROJECT_MEMORY_FILE: &str = ".agentic/memory.md";

/// Resolve the path to the user-global memory file.
///
/// Order of preference:
/// 1. `$AGENTIC_MEMORY_PATH` (overrides everything; useful for tests).
/// 2. `$XDG_CONFIG_HOME/agentic/memory.md`.
/// 3. `$HOME/.config/agentic/memory.md`.
/// 4. `./agentic/memory.md` (last-resort fallback).
pub fn user_memory_path() -> PathBuf {
    if let Ok(p) = std::env::var("AGENTIC_MEMORY_PATH") {
        return PathBuf::from(p);
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("agentic").join("memory.md");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join(".config")
            .join("agentic")
            .join("memory.md");
    }
    if let Ok(profile) = std::env::var("USERPROFILE") {
        return PathBuf::from(profile)
            .join(".config")
            .join("agentic")
            .join("memory.md");
    }
    PathBuf::from("agentic").join("memory.md")
}

/// Find the project-local memory file by walking up from `cwd`.
pub fn find_project_memory(cwd: &Path) -> Option<PathBuf> {
    let mut current: Option<&Path> = Some(cwd);
    while let Some(dir) = current {
        let candidate = dir.join(PROJECT_MEMORY_FILE);
        if candidate.is_file() {
            return Some(candidate);
        }
        current = dir.parent();
    }
    None
}

/// Read the user-global memory file. Returns `None` if the file doesn't
/// exist or can't be read. Errors are swallowed so a corrupt memory file
/// never blocks startup.
pub fn load_user_memory() -> Option<String> {
    let path = user_memory_path();
    fs::read_to_string(&path).ok()
}

/// Read the project-local memory file from `cwd` (walking up).
pub fn load_project_memory(cwd: &Path) -> Option<(PathBuf, String)> {
    let path = find_project_memory(cwd)?;
    let content = fs::read_to_string(&path).ok()?;
    Some((path, content))
}

/// Append a note to the user-global memory file. Creates parent
/// directories if needed.
///
/// The note is wrapped with a timestamped header so accumulated entries
/// remain readable. Returns the path written to on success.
pub fn append_user_memory(note: &str) -> std::io::Result<PathBuf> {
    let path = user_memory_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    append_to(&path, note)?;
    Ok(path)
}

/// Append a note to the project-local memory file (`./.agentic/memory.md`).
/// Creates the directory if needed.
pub fn append_project_memory(cwd: &Path, note: &str) -> std::io::Result<PathBuf> {
    let path = cwd.join(PROJECT_MEMORY_FILE);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    append_to(&path, note)?;
    Ok(path)
}

fn append_to(path: &Path, note: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let header = chrono::Utc::now()
        .format("## %Y-%m-%d %H:%M UTC")
        .to_string();
    writeln!(f, "\n{}\n\n{}", header, note.trim_end())?;
    Ok(())
}

/// Compose the memory section for inclusion in the system prompt.
///
/// Returns `None` if both files are absent or empty. The combined text is
/// labeled so the model can distinguish global notes from project notes.
pub fn assemble_memory_section(cwd: &Path) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();

    if let Some(user) = load_user_memory() {
        let user = user.trim();
        if !user.is_empty() {
            parts.push(format!("## Persistent memory (user-global)\n\n{}", user));
        }
    }

    if let Some((path, project)) = load_project_memory(cwd) {
        let project = project.trim();
        if !project.is_empty() {
            parts.push(format!(
                "## Persistent memory (project: {})\n\n{}",
                path.display(),
                project
            ));
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Tests in this module manipulate process-global env vars
    /// (`AGENTIC_MEMORY_PATH`, `HOME`, etc), so they must run serially.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("memory_file_test_{}", name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn user_memory_path_respects_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tmp_dir("override");
        let target = dir.join("custom.md");
        std::env::set_var("AGENTIC_MEMORY_PATH", &target);
        assert_eq!(user_memory_path(), target);
        std::env::remove_var("AGENTIC_MEMORY_PATH");
    }

    #[test]
    fn append_user_memory_creates_file_and_adds_header() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tmp_dir("append_user");
        let target = dir.join("notes.md");
        std::env::set_var("AGENTIC_MEMORY_PATH", &target);

        append_user_memory("first note").unwrap();
        append_user_memory("second note").unwrap();

        let content = fs::read_to_string(&target).unwrap();
        assert!(content.contains("first note"));
        assert!(content.contains("second note"));
        // Header lines start with "## " and contain "UTC".
        assert!(content.lines().filter(|l| l.starts_with("## ")).count() >= 2);

        std::env::remove_var("AGENTIC_MEMORY_PATH");
    }

    #[test]
    fn project_memory_walks_up() {
        let root = tmp_dir("project_walk");
        let nested = root.join("a").join("b");
        fs::create_dir_all(&nested).unwrap();
        let agentic_dir = root.join(".agentic");
        fs::create_dir_all(&agentic_dir).unwrap();
        fs::write(agentic_dir.join("memory.md"), "project notes").unwrap();

        let found = find_project_memory(&nested).expect("should find walking up");
        assert!(found.ends_with(".agentic/memory.md"));
    }

    #[test]
    fn append_project_memory_creates_directory() {
        let dir = tmp_dir("append_proj");
        // No .agentic dir initially.
        append_project_memory(&dir, "hello").unwrap();
        assert!(dir.join(".agentic/memory.md").is_file());
        let content = fs::read_to_string(dir.join(".agentic/memory.md")).unwrap();
        assert!(content.contains("hello"));
    }

    #[test]
    fn assemble_section_none_when_both_empty() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tmp_dir("empty");
        std::env::set_var("AGENTIC_MEMORY_PATH", dir.join("nonexistent.md"));
        assert!(assemble_memory_section(&dir).is_none());
        std::env::remove_var("AGENTIC_MEMORY_PATH");
    }

    #[test]
    fn assemble_section_includes_both_when_present() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tmp_dir("both");

        let user_path = dir.join("user.md");
        fs::write(&user_path, "USER NOTES").unwrap();
        std::env::set_var("AGENTIC_MEMORY_PATH", &user_path);

        let project_dir = dir.join("project");
        fs::create_dir_all(project_dir.join(".agentic")).unwrap();
        fs::write(project_dir.join(".agentic/memory.md"), "PROJECT NOTES").unwrap();

        let section = assemble_memory_section(&project_dir).expect("should assemble");
        assert!(section.contains("USER NOTES"));
        assert!(section.contains("PROJECT NOTES"));

        std::env::remove_var("AGENTIC_MEMORY_PATH");
    }
}
