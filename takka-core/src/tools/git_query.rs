//! Git query tools — `git_status` and `git_diff` as first-class tools.
//!
//! Why these exist as their own tools instead of forcing the model to
//! call `run_command "git status"`:
//!
//! - **Structured output**: `git status --porcelain` is parsed into
//!   typed entries (`{ path, index_status, worktree_status }`) so the
//!   model can reason about the working-tree state without re-parsing
//!   shell text.
//! - **Risk classification is exact**: at the safety layer, generic
//!   `run_command` invocations have to apply heuristic risk scoring.
//!   These tools are explicitly read-only, which lets the orchestrator
//!   batch them concurrently with other reads.
//! - **No `cd` games**: `workdir` argument keeps the working directory
//!   on the tool's frame; we don't depend on agent shell state that
//!   doesn't exist.
//!
//! Both tools require git to be on PATH. If the cwd isn't a git
//! checkout, they return a clear error rather than a cryptic exit-128.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use crate::tool::{SideEffects, Tool, ToolError, ToolMetadata, ToolParam, ToolResult, ToolSchema};

/// Cap on diff output forwarded to the model. Larger diffs are
/// summarized with the head + tail preserved.
const MAX_DIFF_CHARS: usize = 25_000;

// ── git_status ──────────────────────────────────────────────────────────

pub struct GitStatusTool;

impl GitStatusTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for GitStatusTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for GitStatusTool {
    fn name(&self) -> &str {
        "git_status"
    }

    fn description(&self) -> &str {
        "Show the git working-tree status as structured entries: each \
         path with its index and worktree status code (M=modified, \
         A=added, D=deleted, ??=untracked, etc.). Use this in preference \
         to run_command \"git status\" so the model gets a typed list."
    }

    fn schema(&self) -> ToolSchema {
        let mut params = HashMap::new();
        params.insert(
            "workdir".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some(
                    "Optional working directory inside the repo. Defaults \
                     to the agent's cwd."
                        .into(),
                ),
                default: None,
            },
        );
        params.insert(
            "include_branch".to_string(),
            ToolParam {
                param_type: "boolean".to_string(),
                description: Some(
                    "Include current branch name + ahead/behind counts. \
                     Defaults to true."
                        .into(),
                ),
                default: Some(serde_json::json!(true)),
            },
        );

        ToolSchema {
            name: "git_status".to_string(),
            description: "Structured git working-tree status.".to_string(),
            parameters: params,
            required: Vec::new(),
        }
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult<serde_json::Value> {
        let obj = args.as_object();
        let workdir = obj
            .and_then(|o| o.get("workdir"))
            .and_then(|v| v.as_str())
            .map(PathBuf::from);
        let include_branch = obj
            .and_then(|o| o.get("include_branch"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let cwd = resolve_cwd(workdir.as_deref())?;

        let mut cmd = Command::new("git");
        cmd.current_dir(&cwd).arg("status").arg("--porcelain=v1");
        if include_branch {
            cmd.arg("--branch");
        }

        let out = cmd
            .output()
            .map_err(|e| ToolError::new(format!("Failed to run git status: {}", e)))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(ToolError::new(format!(
                "git status failed (exit {:?}): {}",
                out.status.code(),
                stderr.trim()
            )));
        }

        let raw = String::from_utf8_lossy(&out.stdout).into_owned();
        let parsed = parse_status(&raw, include_branch);

        let entries_empty = parsed.entries.is_empty();
        Ok(serde_json::json!({
            "branch": parsed.branch,
            "upstream": parsed.upstream,
            "ahead": parsed.ahead,
            "behind": parsed.behind,
            "entries": parsed.entries,
            "clean": entries_empty,
        }))
    }

    fn metadata(&self) -> ToolMetadata {
        let mut m = ToolMetadata::read_only();
        m.side_effects = SideEffects::FsRead;
        m
    }
}

#[derive(Debug, Default)]
struct ParsedStatus {
    branch: Option<String>,
    upstream: Option<String>,
    ahead: u32,
    behind: u32,
    entries: Vec<serde_json::Value>,
}

fn parse_status(raw: &str, include_branch: bool) -> ParsedStatus {
    let mut p = ParsedStatus::default();

    for line in raw.lines() {
        if include_branch && line.starts_with("## ") {
            // `## main...origin/main [ahead 1, behind 2]`
            let body = &line[3..];
            let (refspec, tracking) = match body.find(' ') {
                Some(i) => (&body[..i], &body[i + 1..]),
                None => (body, ""),
            };
            let (branch, upstream) = match refspec.find("...") {
                Some(i) => (&refspec[..i], Some(refspec[i + 3..].to_string())),
                None => (refspec, None),
            };
            p.branch = Some(branch.to_string());
            p.upstream = upstream;
            for part in tracking
                .trim_start_matches('[')
                .trim_end_matches(']')
                .split(',')
            {
                let part = part.trim();
                if let Some(rest) = part.strip_prefix("ahead ") {
                    p.ahead = rest.parse().unwrap_or(0);
                } else if let Some(rest) = part.strip_prefix("behind ") {
                    p.behind = rest.parse().unwrap_or(0);
                }
            }
            continue;
        }

        // Status entries: 2 status chars + space + path. Renames have
        // the form "R  oldpath -> newpath". For brevity, we keep the
        // destination path only.
        if line.len() < 4 {
            continue;
        }
        let bytes = line.as_bytes();
        let index = bytes[0] as char;
        let worktree = bytes[1] as char;
        let path_part = line[3..].to_string();
        let path = match path_part.find(" -> ") {
            Some(i) => path_part[i + 4..].to_string(),
            None => path_part,
        };

        p.entries.push(serde_json::json!({
            "path": path,
            "index_status": index.to_string(),
            "worktree_status": worktree.to_string(),
        }));
    }

    p
}

// ── git_diff ────────────────────────────────────────────────────────────

pub struct GitDiffTool;

impl GitDiffTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for GitDiffTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for GitDiffTool {
    fn name(&self) -> &str {
        "git_diff"
    }

    fn description(&self) -> &str {
        "Show a unified-diff of the current git working tree. Defaults \
         to unstaged changes vs HEAD; pass `staged=true` for staged \
         vs HEAD, or `target=\"<commit>\"` for working-tree vs commit. \
         Output is capped at 25k chars (head+tail preserved). Use this \
         in preference to run_command \"git diff\" so the orchestrator \
         can render the diff inline through the shared diff widget."
    }

    fn schema(&self) -> ToolSchema {
        let mut params = HashMap::new();
        params.insert(
            "workdir".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some("Optional working directory inside the repo.".into()),
                default: None,
            },
        );
        params.insert(
            "staged".to_string(),
            ToolParam {
                param_type: "boolean".to_string(),
                description: Some(
                    "When true, diff staged changes vs HEAD instead of \
                     working-tree vs HEAD. Defaults to false."
                        .into(),
                ),
                default: Some(serde_json::json!(false)),
            },
        );
        params.insert(
            "target".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some(
                    "Optional commit-ish to diff against (e.g. \"main\", \
                     \"HEAD~3\"). Defaults to HEAD."
                        .into(),
                ),
                default: None,
            },
        );
        params.insert(
            "path".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some(
                    "Optional path filter. Limits diff to files under \
                     this path (passed as a pathspec)."
                        .into(),
                ),
                default: None,
            },
        );

        ToolSchema {
            name: "git_diff".to_string(),
            description: "Unified-diff of the git working tree.".to_string(),
            parameters: params,
            required: Vec::new(),
        }
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult<serde_json::Value> {
        let obj = args.as_object();
        let workdir = obj
            .and_then(|o| o.get("workdir"))
            .and_then(|v| v.as_str())
            .map(PathBuf::from);
        let staged = obj
            .and_then(|o| o.get("staged"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let target = obj
            .and_then(|o| o.get("target"))
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let path_filter = obj
            .and_then(|o| o.get("path"))
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let cwd = resolve_cwd(workdir.as_deref())?;

        let mut cmd = Command::new("git");
        cmd.current_dir(&cwd).arg("diff");
        if staged {
            cmd.arg("--cached");
        }
        if let Some(t) = target.as_deref() {
            cmd.arg(t);
        }
        if let Some(ref p) = path_filter {
            cmd.arg("--").arg(p);
        }

        let out = cmd
            .output()
            .map_err(|e| ToolError::new(format!("Failed to run git diff: {}", e)))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(ToolError::new(format!(
                "git diff failed (exit {:?}): {}",
                out.status.code(),
                stderr.trim()
            )));
        }

        let raw = String::from_utf8_lossy(&out.stdout).into_owned();
        let original_len = raw.len();
        let truncated_flag = original_len > MAX_DIFF_CHARS;
        let diff = if truncated_flag {
            truncate_diff(&raw, MAX_DIFF_CHARS)
        } else {
            raw
        };

        // Stats: count `+` / `-` lines (excluding diff metadata).
        let mut additions = 0u32;
        let mut deletions = 0u32;
        let mut files_changed = 0u32;
        for line in diff.lines() {
            if line.starts_with("diff --git ") {
                files_changed += 1;
            } else if line.starts_with("+++ ") || line.starts_with("--- ") {
                continue;
            } else if let Some(b) = line.as_bytes().first() {
                match b {
                    b'+' => additions += 1,
                    b'-' => deletions += 1,
                    _ => {}
                }
            }
        }

        Ok(serde_json::json!({
            "diff": diff,
            "staged": staged,
            "target": target.unwrap_or_else(|| "HEAD".to_string()),
            "path_filter": path_filter,
            "files_changed": files_changed,
            "additions": additions,
            "deletions": deletions,
            "truncated": truncated_flag,
            "original_chars": original_len,
        }))
    }

    fn metadata(&self) -> ToolMetadata {
        let mut m = ToolMetadata::read_only();
        m.side_effects = SideEffects::FsRead;
        m
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn resolve_cwd(workdir: Option<&std::path::Path>) -> ToolResult<PathBuf> {
    match workdir {
        Some(p) => Ok(p.to_path_buf()),
        None => {
            std::env::current_dir().map_err(|e| ToolError::new(format!("cwd unavailable: {}", e)))
        }
    }
}

/// Keep the head and tail of a long diff so the model still sees what
/// changed at both ends. UTF-8 safe.
fn truncate_diff(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        return s.to_string();
    }
    let half = max_chars / 2;
    let head_end = floor_char_boundary(s, half);
    let tail_start = ceil_char_boundary(s, s.len().saturating_sub(half));
    format!(
        "{}\n\n[... {} chars omitted ...]\n\n{}",
        &s[..head_end],
        s.len() - head_end - (s.len() - tail_start),
        &s[tail_start..]
    )
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    let len = s.len();
    if i >= len {
        return len;
    }
    while i < len && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_status_branch_line() {
        let raw = "## main...origin/main [ahead 2, behind 1]\n";
        let p = parse_status(raw, true);
        assert_eq!(p.branch.as_deref(), Some("main"));
        assert_eq!(p.upstream.as_deref(), Some("origin/main"));
        assert_eq!(p.ahead, 2);
        assert_eq!(p.behind, 1);
    }

    #[test]
    fn parse_status_branch_no_tracking_info() {
        let raw = "## feature/x\n";
        let p = parse_status(raw, true);
        assert_eq!(p.branch.as_deref(), Some("feature/x"));
        assert!(p.upstream.is_none());
        assert_eq!(p.ahead, 0);
        assert_eq!(p.behind, 0);
    }

    #[test]
    fn parse_status_modified_and_untracked() {
        let raw = "## main...origin/main\n\
                   M  src/lib.rs\n\
                   ?? new_file.txt\n\
                   A  staged.txt\n";
        let p = parse_status(raw, true);
        assert_eq!(p.entries.len(), 3);
        assert_eq!(p.entries[0]["path"], "src/lib.rs");
        assert_eq!(p.entries[0]["index_status"], "M");
        assert_eq!(p.entries[1]["path"], "new_file.txt");
        assert_eq!(p.entries[1]["index_status"], "?");
        assert_eq!(p.entries[1]["worktree_status"], "?");
        assert_eq!(p.entries[2]["path"], "staged.txt");
        assert_eq!(p.entries[2]["index_status"], "A");
    }

    #[test]
    fn parse_status_rename_keeps_destination_path() {
        let raw = "## main\nR  old.rs -> new.rs\n";
        let p = parse_status(raw, true);
        assert_eq!(p.entries.len(), 1);
        assert_eq!(p.entries[0]["path"], "new.rs");
        assert_eq!(p.entries[0]["index_status"], "R");
    }

    #[test]
    fn parse_status_clean_repo() {
        let raw = "## main...origin/main\n";
        let p = parse_status(raw, true);
        assert!(p.entries.is_empty());
    }

    #[test]
    fn truncate_diff_preserves_short_input() {
        let body = "diff --git a/x b/x\n+hello\n";
        assert_eq!(truncate_diff(body, 1000), body);
    }

    #[test]
    fn truncate_diff_keeps_head_and_tail() {
        let body = "a".repeat(50_000);
        let out = truncate_diff(&body, 1000);
        assert!(out.len() < body.len());
        assert!(out.contains("chars omitted"));
        assert!(out.starts_with("a"));
        assert!(out.ends_with("a"));
    }
}
