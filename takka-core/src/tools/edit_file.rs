use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::file_tracker::{FileTracker, Freshness};
use crate::tool::{
    Concurrency, Mutability, SideEffects, Tool, ToolError, ToolMetadata, ToolParam, ToolResult,
    ToolSchema,
};

pub struct EditFileTool {
    tracker: Option<Arc<FileTracker>>,
}

impl Default for EditFileTool {
    fn default() -> Self {
        Self::new()
    }
}

impl EditFileTool {
    pub fn new() -> Self {
        Self { tracker: None }
    }

    /// Build an [`EditFileTool`] that consults a shared [`FileTracker`] to
    /// reject edits on files modified externally since the agent last read
    /// them.
    pub fn with_tracker(tracker: Arc<FileTracker>) -> Self {
        Self {
            tracker: Some(tracker),
        }
    }
}

impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Performs exact string replacements in files. The edit will fail if oldString is not found or found multiple times."
    }

    fn schema(&self) -> ToolSchema {
        let mut params = HashMap::new();
        params.insert(
            "file_path".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some("The absolute path to the file to modify".to_string()),
                default: None,
            },
        );
        params.insert(
            "old_string".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some("The text to replace".to_string()),
                default: None,
            },
        );
        params.insert(
            "new_string".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some("The text to replace it with".to_string()),
                default: None,
            },
        );
        params.insert(
            "replace_all".to_string(),
            ToolParam {
                param_type: "boolean".to_string(),
                description: Some(
                    "Replace all occurrences of old_string (default false)".to_string(),
                ),
                default: Some(serde_json::json!(false)),
            },
        );

        ToolSchema {
            name: "edit_file".to_string(),
            description: "Performs exact string replacements in files".to_string(),
            parameters: params,
            required: vec![
                "file_path".to_string(),
                "old_string".to_string(),
                "new_string".to_string(),
            ],
        }
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult<serde_json::Value> {
        let args_obj = args
            .as_object()
            .ok_or_else(|| ToolError::new("Invalid arguments: expected object"))?;

        let file_path = args_obj
            .get("file_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new("Missing required parameter: file_path"))?;

        let old_string = args_obj
            .get("old_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new("Missing required parameter: old_string"))?;

        let new_string = args_obj
            .get("new_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new("Missing required parameter: new_string"))?;

        let replace_all = args_obj
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if old_string == new_string {
            return Err(ToolError::new(
                "old_string and new_string must be different",
            ));
        }

        let path = Path::new(file_path);
        if !path.exists() {
            return Err(ToolError::new(format!(
                "File not found: {}",
                path.display()
            )));
        }

        // Staleness check: if the agent has read this file before but the
        // file was modified externally since then, refuse the edit and tell
        // the model to re-read.
        if let Some(t) = &self.tracker {
            if let Freshness::Stale { .. } = t.check(path) {
                return Err(ToolError::new(format!(
                    "Stale read: {} was modified after the agent last read it. Re-read the file before editing.",
                    path.display()
                )));
            }
            // NeverRead and Fresh both proceed. NeverRead is permissive
            // because the model may legitimately edit a file it just
            // wrote, or one it has strong context about from elsewhere.
        }

        let content = std::fs::read_to_string(path)
            .map_err(|e| ToolError::new(format!("Failed to read file: {}", e)))?;

        // First try an exact match. If that fails, retry with quote
        // normalization (curly → straight) on both sides. LLMs occasionally
        // emit smart quotes that don't match plain ASCII quotes in source.
        let (search, replacement, normalized) = {
            let count = content.matches(old_string).count();
            if count > 0 {
                (old_string.to_string(), new_string.to_string(), false)
            } else {
                let norm_content = normalize_quotes(&content);
                let norm_old = normalize_quotes(old_string);
                let norm_new = normalize_quotes(new_string);
                let norm_count = norm_content.matches(&norm_old).count();
                if norm_count > 0 {
                    // Operate on the normalized content so the replacement sticks.
                    let result = apply_replacement(
                        path,
                        &norm_content,
                        &norm_old,
                        &norm_new,
                        replace_all,
                        true,
                    );
                    if result.is_ok() {
                        if let Some(t) = &self.tracker {
                            t.mark_written(path);
                        }
                    }
                    return result;
                } else {
                    return Err(ToolError::new(format!(
                        "old_string not found in file: {}",
                        path.display()
                    )));
                }
            }
        };

        let _ = normalized; // unused on the fast path; documented for clarity
        let result = apply_replacement(path, &content, &search, &replacement, replace_all, false);
        if result.is_ok() {
            if let Some(t) = &self.tracker {
                t.mark_written(path);
            }
        }
        result
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            mutability: Mutability::Mutating,
            concurrency: Concurrency::Exclusive,
            idempotent: true,
            risk: 25,
            side_effects: SideEffects::FsWrite,
        }
    }
}

/// Normalize curly/typographic quotes to ASCII straight quotes so LLM-emitted
/// strings can match source files reliably.
fn normalize_quotes(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            other => other,
        })
        .collect()
}

fn apply_replacement(
    path: &Path,
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    quotes_normalized: bool,
) -> ToolResult<serde_json::Value> {
    let count = content.matches(old_string).count();
    if count == 0 {
        return Err(ToolError::new(format!(
            "old_string not found in file: {}",
            path.display()
        )));
    }

    if !replace_all && count > 1 {
        return Err(ToolError::new(format!(
            "Found {} matches for old_string. Use replace_all=true or provide more context to make it unique.",
            count
        )));
    }

    let new_content = if replace_all {
        content.replace(old_string, new_string)
    } else {
        content.replacen(old_string, new_string, 1)
    };

    std::fs::write(path, &new_content)
        .map_err(|e| ToolError::new(format!("Failed to write file: {}", e)))?;

    // Build a unified diff and stats from before → after so the CLI can
    // render a real diff widget instead of just a success line. The diff
    // is included in the JSON result; the orchestrator forwards the same
    // payload to event subscribers.
    let path_label = path.to_string_lossy().to_string();
    let diff = crate::diff_util::unified_diff(&path_label, content, &new_content, 3);
    let stats = crate::diff_util::change_summary(content, &new_content);

    Ok(serde_json::json!({
        "path": path_label,
        "success": true,
        "replacements": if replace_all { count } else { 1 },
        "quotes_normalized": quotes_normalized,
        "diff": diff,
        "lines_added": stats.added,
        "lines_removed": stats.removed,
    }))
}

#[cfg(test)]
mod edit_file_tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(name: &str, content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("edit_file_tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn normalize_quotes_converts_curly_to_straight() {
        let input = "hello \u{201C}world\u{201D} and \u{2018}rust\u{2019}";
        let out = normalize_quotes(input);
        assert_eq!(out, "hello \"world\" and 'rust'");
    }

    #[test]
    fn edit_with_curly_quotes_falls_back_to_normalized_match() {
        let path = write_tmp("curly.txt", "let s = \"hello\";\n");
        let tool = EditFileTool::new();
        // Old string uses curly quotes — must still match.
        let args = serde_json::json!({
            "file_path": path.to_string_lossy(),
            "old_string": "let s = \u{201C}hello\u{201D};",
            "new_string": "let s = \"world\";"
        });
        let res = tool.execute(args).expect("edit should succeed");
        assert_eq!(res["quotes_normalized"], true);
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("world"));
    }

    #[test]
    fn edit_normal_match_does_not_normalize() {
        let path = write_tmp("plain.txt", "foo bar\n");
        let tool = EditFileTool::new();
        let args = serde_json::json!({
            "file_path": path.to_string_lossy(),
            "old_string": "foo",
            "new_string": "baz"
        });
        let res = tool.execute(args).expect("edit should succeed");
        assert_eq!(res["quotes_normalized"], false);
    }

    #[test]
    fn edit_rejects_stale_file() {
        use crate::file_tracker::FileTracker;
        use std::sync::Arc;
        use std::thread::sleep;
        use std::time::Duration;

        let path = write_tmp("stale_edit.txt", "alpha\n");
        let tracker = Arc::new(FileTracker::new());
        // Simulate the agent having read the file.
        tracker.mark_read(&path);

        // External writer modifies the file.
        sleep(Duration::from_millis(20));
        std::fs::write(&path, b"changed by someone else\n").unwrap();

        let tool = EditFileTool::with_tracker(tracker);
        let args = serde_json::json!({
            "file_path": path.to_string_lossy(),
            "old_string": "changed",
            "new_string": "updated"
        });
        let err = tool.execute(args).expect_err("should reject stale edit");
        assert!(err.to_string().contains("Stale read"));
    }

    #[test]
    fn edit_after_fresh_read_succeeds_and_marks_written() {
        use crate::file_tracker::{FileTracker, Freshness};
        use std::sync::Arc;

        let path = write_tmp("fresh_edit.txt", "alpha\n");
        let tracker = Arc::new(FileTracker::new());
        tracker.mark_read(&path);

        let tool = EditFileTool::with_tracker(tracker.clone());
        let args = serde_json::json!({
            "file_path": path.to_string_lossy(),
            "old_string": "alpha",
            "new_string": "beta"
        });
        tool.execute(args).expect("fresh edit should succeed");

        // After our own write, the file should still be considered fresh.
        assert_eq!(tracker.check(&path), Freshness::Fresh);
    }
}
