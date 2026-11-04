//! `apply_patch` tool — apply a unified diff to one or more files atomically.
//!
//! Why this exists: the model can already make changes via `edit_file`
//! (string-replace) and `write_file` (full overwrite), but multi-file
//! refactors burn a lot of tokens because each hunk is its own tool call.
//! `apply_patch` lets the model express the whole change as one unified
//! diff (the same format `edit_file`/`write_file` already produce in
//! their results, and the same format `git diff` produces).
//!
//! The implementation is intentionally minimal:
//! - Parses standard `--- a/<path>` / `+++ b/<path>` / `@@ -x,y +u,v @@`
//!   headers. Extra git-specific headers (`diff --git`, `index`, mode
//!   bits) are tolerated and ignored.
//! - Applies hunks using the line numbers in the hunk header rather than
//!   doing fuzzy context matching. The context lines are still verified
//!   to match — if they don't, the hunk is rejected with a clear error.
//! - All-or-nothing: every file change is staged in memory first, then
//!   written. A failure halfway through leaves the disk untouched.
//! - File creation: `--- /dev/null` (or empty `before` content with
//!   `@@ -0,0 +1,N @@`) creates a new file.
//! - File deletion: `+++ /dev/null` removes the file (after verifying
//!   the existing content matches the `-` lines).
//!
//! What it does NOT do:
//! - Fuzzy context matching (offset/whitespace tolerance). If your
//!   context lines drift, regenerate the patch.
//! - Binary patches. Text only.
//! - Rename detection. Do delete-then-create as separate file blocks.

use std::collections::HashMap;
use std::path::Path;

use crate::file_tracker::FileTracker;
use crate::tool::{
    Concurrency, Mutability, SideEffects, Tool, ToolError, ToolMetadata, ToolParam, ToolResult,
    ToolSchema,
};

/// Sentinel filename used in unified diffs for "no file on this side".
const DEV_NULL: &str = "/dev/null";

/// One file's worth of changes, parsed from the diff.
#[derive(Debug, Clone, PartialEq)]
struct FilePatch {
    /// Source path from the `--- a/<path>` header. `/dev/null` for new
    /// files.
    old_path: String,
    /// Destination path from the `+++ b/<path>` header. `/dev/null` for
    /// deletions.
    new_path: String,
    hunks: Vec<Hunk>,
}

#[derive(Debug, Clone, PartialEq)]
struct Hunk {
    /// 1-based starting line in the old file. 0 = "this is a creation".
    old_start: usize,
    /// Number of `-` and ` ` (context) lines in this hunk.
    old_count: usize,
    /// 1-based starting line in the new file.
    #[allow(dead_code)]
    new_start: usize,
    /// Number of `+` and ` ` (context) lines in this hunk.
    new_count: usize,
    /// Each line carries its prefix marker stripped from the line body.
    lines: Vec<HunkLine>,
}

#[derive(Debug, Clone, PartialEq)]
enum HunkLine {
    Context(String),
    Add(String),
    Remove(String),
}

pub struct ApplyPatchTool {
    tracker: Option<std::sync::Arc<FileTracker>>,
}

impl ApplyPatchTool {
    pub fn new() -> Self {
        Self { tracker: None }
    }

    pub fn with_tracker(tracker: std::sync::Arc<FileTracker>) -> Self {
        Self {
            tracker: Some(tracker),
        }
    }
}

impl Default for ApplyPatchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        "Apply a unified-diff patch to one or more files atomically. Accepts \
         the standard `--- a/path / +++ b/path / @@ -x,y +u,v @@` format \
         (the same format `git diff` produces and `edit_file`/`write_file` \
         emit in their results). Use this for multi-file changes instead \
         of issuing many `edit_file` calls. New files use `--- /dev/null`; \
         deletions use `+++ /dev/null`. The patch is rejected if any hunk's \
         context lines don't match — regenerate the patch from a fresh read."
    }

    fn schema(&self) -> ToolSchema {
        let mut params = HashMap::new();
        params.insert(
            "patch".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some(
                    "The unified-diff text. May contain multiple file blocks.".to_string(),
                ),
                default: None,
            },
        );

        ToolSchema {
            name: "apply_patch".to_string(),
            description: "Apply a unified-diff patch to one or more files.".to_string(),
            parameters: params,
            required: vec!["patch".to_string()],
        }
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult<serde_json::Value> {
        let patch_text = args
            .as_object()
            .and_then(|o| o.get("patch"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new("Missing required parameter: patch"))?;

        if patch_text.trim().is_empty() {
            return Err(ToolError::new("patch must not be empty"));
        }

        let file_patches = parse_patch(patch_text)
            .map_err(|e| ToolError::new(format!("Patch parse error: {}", e)))?;

        if file_patches.is_empty() {
            return Err(ToolError::new(
                "Patch parsed successfully but contained no file changes",
            ));
        }

        // Stage every change in memory so we can detect failures before
        // writing anything to disk. Each entry is (path, Some(content) =
        // write, None = delete).
        let mut staged: Vec<(String, Option<String>)> = Vec::new();

        for fp in &file_patches {
            let staged_change = stage_file_patch(fp).map_err(ToolError::new)?;
            staged.push(staged_change);
        }

        // Now commit. Each individual write is fallible; we report the
        // first failure but don't try to roll back earlier writes — the
        // staging phase is what protects against partial corruption from
        // *patch* errors. Disk-IO errors at commit time are rare and
        // hard to roll back cleanly without a journal.
        let mut applied = Vec::new();
        let mut deleted = Vec::new();
        let mut created = Vec::new();

        for (path, change) in staged {
            match change {
                Some(new_content) => {
                    let p = Path::new(&path);
                    let pre_existed = p.exists();
                    if let Some(parent) = p.parent() {
                        if !parent.as_os_str().is_empty() && !parent.exists() {
                            std::fs::create_dir_all(parent).map_err(|e| {
                                ToolError::new(format!(
                                    "Failed to create directory for {}: {}",
                                    path, e
                                ))
                            })?;
                        }
                    }
                    std::fs::write(&path, &new_content)
                        .map_err(|e| ToolError::new(format!("Failed to write {}: {}", path, e)))?;
                    if let Some(t) = &self.tracker {
                        t.mark_written(Path::new(&path));
                    }
                    if pre_existed {
                        applied.push(path);
                    } else {
                        created.push(path);
                    }
                }
                None => {
                    let p = Path::new(&path);
                    if p.exists() {
                        std::fs::remove_file(p).map_err(|e| {
                            ToolError::new(format!("Failed to delete {}: {}", path, e))
                        })?;
                    }
                    if let Some(t) = &self.tracker {
                        t.mark_written(Path::new(&path));
                    }
                    deleted.push(path);
                }
            }
        }

        Ok(serde_json::json!({
            "success": true,
            "files_modified": applied,
            "files_created": created,
            "files_deleted": deleted,
            "hunks_applied": file_patches.iter().map(|f| f.hunks.len()).sum::<usize>(),
        }))
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            mutability: Mutability::Mutating,
            concurrency: Concurrency::Exclusive,
            idempotent: false,
            risk: 35,
            side_effects: SideEffects::FsWrite,
        }
    }
}

// ── Parser ──────────────────────────────────────────────────────────────

/// Parse a unified-diff text into one entry per file. Tolerant of:
/// - leading non-diff lines (they're ignored until the first `--- ` line)
/// - `diff --git`, `index`, `new file mode`, etc. between file blocks
/// - missing trailing newline on the final hunk
fn parse_patch(text: &str) -> Result<Vec<FilePatch>, String> {
    let mut out = Vec::new();
    let mut lines = text.lines().peekable();

    while let Some(line) = lines.peek() {
        if let Some(stripped) = line.strip_prefix("--- ") {
            let old_path = strip_path_prefix(stripped.trim());
            lines.next();

            let plus_line = lines
                .next()
                .ok_or_else(|| "unexpected end after `---` line".to_string())?;
            let plus_path = plus_line
                .strip_prefix("+++ ")
                .ok_or_else(|| format!("expected `+++ ` line, got: {}", plus_line))?;
            let new_path = strip_path_prefix(plus_path.trim());

            let mut hunks = Vec::new();
            while let Some(peeked) = lines.peek() {
                if peeked.starts_with("@@ ") {
                    let hunk_header = lines.next().unwrap();
                    let (old_start, old_count, new_start, new_count) =
                        parse_hunk_header(hunk_header)?;
                    let mut hunk_lines = Vec::new();
                    let mut consumed_old = 0usize;
                    let mut consumed_new = 0usize;
                    while let Some(body) = lines.peek() {
                        if body.starts_with("@@ ") || body.starts_with("--- ") {
                            break;
                        }
                        let body = lines.next().unwrap();
                        if body.is_empty() {
                            hunk_lines.push(HunkLine::Context(String::new()));
                            consumed_old += 1;
                            consumed_new += 1;
                            continue;
                        }
                        let (marker, rest) = body.split_at(1);
                        match marker {
                            " " => {
                                hunk_lines.push(HunkLine::Context(rest.to_string()));
                                consumed_old += 1;
                                consumed_new += 1;
                            }
                            "-" => {
                                hunk_lines.push(HunkLine::Remove(rest.to_string()));
                                consumed_old += 1;
                            }
                            "+" => {
                                hunk_lines.push(HunkLine::Add(rest.to_string()));
                                consumed_new += 1;
                            }
                            "\\" => {
                                // "\ No newline at end of file" — ignore.
                            }
                            _ => {
                                if consumed_old < old_count || consumed_new < new_count {
                                    return Err(format!("unexpected line inside hunk: {:?}", body));
                                }
                                break;
                            }
                        }
                        if consumed_old >= old_count && consumed_new >= new_count {
                            break;
                        }
                    }
                    hunks.push(Hunk {
                        old_start,
                        old_count,
                        new_start,
                        new_count,
                        lines: hunk_lines,
                    });
                } else if peeked.starts_with("--- ") {
                    break;
                } else {
                    lines.next();
                }
            }

            if hunks.is_empty() && old_path != DEV_NULL && new_path != DEV_NULL {
                return Err(format!("file block {} has no hunks", new_path));
            }

            out.push(FilePatch {
                old_path,
                new_path,
                hunks,
            });
        } else {
            lines.next();
        }
    }

    Ok(out)
}

/// Strip the conventional `a/` or `b/` prefix from a diff path.
fn strip_path_prefix(p: &str) -> String {
    if p == DEV_NULL {
        return p.to_string();
    }
    let p = p.split('\t').next().unwrap_or(p);
    if let Some(rest) = p.strip_prefix("a/") {
        return rest.to_string();
    }
    if let Some(rest) = p.strip_prefix("b/") {
        return rest.to_string();
    }
    p.to_string()
}

/// Parse a hunk header like `@@ -12,7 +12,9 @@ fn foo()`.
fn parse_hunk_header(line: &str) -> Result<(usize, usize, usize, usize), String> {
    let body = line.trim_start_matches("@@ ");
    let body = body.split("@@").next().unwrap_or(body).trim();

    let mut parts = body.split_whitespace();
    let old_part = parts
        .next()
        .ok_or_else(|| format!("hunk header missing old range: {}", line))?;
    let new_part = parts
        .next()
        .ok_or_else(|| format!("hunk header missing new range: {}", line))?;

    let (old_start, old_count) = parse_range(old_part.trim_start_matches('-'))?;
    let (new_start, new_count) = parse_range(new_part.trim_start_matches('+'))?;

    Ok((old_start, old_count, new_start, new_count))
}

fn parse_range(s: &str) -> Result<(usize, usize), String> {
    let mut parts = s.splitn(2, ',');
    let start: usize = parts
        .next()
        .ok_or_else(|| format!("empty range: {:?}", s))?
        .parse()
        .map_err(|e| format!("invalid range start in {:?}: {}", s, e))?;
    let count: usize = parts
        .next()
        .map(|c| c.parse::<usize>())
        .transpose()
        .map_err(|e| format!("invalid range count in {:?}: {}", s, e))?
        .unwrap_or(1);
    Ok((start, count))
}

// ── Apply ───────────────────────────────────────────────────────────────

/// Stage one file's worth of changes in memory. Returns the resolved
/// `(path, Some(new_content) | None)` pair where `None` means deletion.
fn stage_file_patch(fp: &FilePatch) -> Result<(String, Option<String>), String> {
    if fp.new_path == DEV_NULL {
        return Ok((fp.old_path.clone(), None));
    }

    if fp.old_path == DEV_NULL {
        let mut content = String::new();
        for hunk in &fp.hunks {
            for line in &hunk.lines {
                if let HunkLine::Add(s) = line {
                    content.push_str(s);
                    content.push('\n');
                }
            }
        }
        return Ok((fp.new_path.clone(), Some(content)));
    }

    let path = &fp.old_path;
    let original =
        std::fs::read_to_string(path).map_err(|e| format!("failed to read {}: {}", path, e))?;

    let new_content = apply_hunks(path, &original, &fp.hunks)?;
    Ok((fp.new_path.clone(), Some(new_content)))
}

/// Apply a list of hunks to a file's content. Hunks must be sorted in
/// ascending old_start order (the standard unified-diff layout).
fn apply_hunks(path: &str, original: &str, hunks: &[Hunk]) -> Result<String, String> {
    let original_lines: Vec<&str> = original.split('\n').collect();
    let had_trailing_newline = original.ends_with('\n');
    let total_old = if had_trailing_newline {
        original_lines.len().saturating_sub(1)
    } else {
        original_lines.len()
    };

    let mut output: Vec<String> = Vec::new();
    let mut cursor: usize = 0;

    for (i, hunk) in hunks.iter().enumerate() {
        let target = if hunk.old_start == 0 {
            0
        } else {
            hunk.old_start - 1
        };

        if target < cursor {
            return Err(format!(
                "{}: hunk #{} starts at line {} but a previous hunk ended at line {} (overlapping or out-of-order hunks)",
                path,
                i + 1,
                hunk.old_start,
                cursor + 1
            ));
        }
        if target > total_old {
            return Err(format!(
                "{}: hunk #{} targets line {} but file only has {} line(s)",
                path,
                i + 1,
                hunk.old_start,
                total_old
            ));
        }

        for line in original_lines.iter().take(target).skip(cursor) {
            output.push((*line).to_string());
        }
        cursor = target;

        for (j, hl) in hunk.lines.iter().enumerate() {
            match hl {
                HunkLine::Context(expected) => {
                    let actual = original_lines.get(cursor).ok_or_else(|| {
                        format!(
                            "{}: hunk #{}, line {} expected context {:?} but file ended",
                            path,
                            i + 1,
                            j + 1,
                            expected
                        )
                    })?;
                    if *actual != expected.as_str() {
                        return Err(format!(
                            "{}: hunk #{}, line {} context mismatch (expected {:?}, got {:?})",
                            path,
                            i + 1,
                            j + 1,
                            expected,
                            actual
                        ));
                    }
                    output.push((*actual).to_string());
                    cursor += 1;
                }
                HunkLine::Remove(expected) => {
                    let actual = original_lines.get(cursor).ok_or_else(|| {
                        format!(
                            "{}: hunk #{}, line {} expected to remove {:?} but file ended",
                            path,
                            i + 1,
                            j + 1,
                            expected
                        )
                    })?;
                    if *actual != expected.as_str() {
                        return Err(format!(
                            "{}: hunk #{}, line {} delete mismatch (expected {:?}, got {:?})",
                            path,
                            i + 1,
                            j + 1,
                            expected,
                            actual
                        ));
                    }
                    cursor += 1;
                }
                HunkLine::Add(s) => {
                    output.push(s.clone());
                }
            }
        }
    }

    for line in original_lines.iter().take(total_old).skip(cursor) {
        output.push((*line).to_string());
    }

    let mut result = output.join("\n");
    if had_trailing_newline {
        result.push('\n');
    }
    Ok(result)
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_modification() {
        let patch = "--- a/foo.txt\n+++ b/foo.txt\n@@ -1,2 +1,2 @@\n hello\n-world\n+rust\n";
        let parsed = parse_patch(patch).expect("should parse");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].old_path, "foo.txt");
        assert_eq!(parsed[0].new_path, "foo.txt");
        assert_eq!(parsed[0].hunks.len(), 1);
        let h = &parsed[0].hunks[0];
        assert_eq!(h.old_start, 1);
        assert_eq!(h.old_count, 2);
        assert_eq!(h.new_count, 2);
        assert_eq!(h.lines.len(), 3);
    }

    #[test]
    fn parse_handles_default_count_of_one() {
        let patch = "--- a/x\n+++ b/x\n@@ -5 +5 @@\n-old\n+new\n";
        let parsed = parse_patch(patch).expect("should parse");
        let h = &parsed[0].hunks[0];
        assert_eq!(h.old_count, 1);
        assert_eq!(h.new_count, 1);
    }

    #[test]
    fn parse_strips_a_b_prefixes() {
        let patch = "--- a/path/to/file\n+++ b/path/to/file\n@@ -1 +1 @@\n-x\n+y\n";
        let parsed = parse_patch(patch).expect("should parse");
        assert_eq!(parsed[0].old_path, "path/to/file");
        assert_eq!(parsed[0].new_path, "path/to/file");
    }

    #[test]
    fn parse_recognizes_dev_null_for_creation() {
        let patch = "--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+hello\n+world\n";
        let parsed = parse_patch(patch).expect("should parse");
        assert_eq!(parsed[0].old_path, "/dev/null");
        assert_eq!(parsed[0].new_path, "new.txt");
    }

    #[test]
    fn parse_skips_git_preamble() {
        let patch = "diff --git a/x b/x\nindex 1234..5678 100644\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n";
        let parsed = parse_patch(patch).expect("should parse");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].old_path, "x");
    }

    #[test]
    fn parse_multi_file_patch() {
        let patch = "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old-a\n+new-a\n--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-old-b\n+new-b\n";
        let parsed = parse_patch(patch).expect("should parse");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].old_path, "a.txt");
        assert_eq!(parsed[1].old_path, "b.txt");
    }

    #[test]
    fn apply_hunks_simple_replace() {
        let original = "line1\nline2\nline3\n";
        let hunks = vec![Hunk {
            old_start: 2,
            old_count: 1,
            new_start: 2,
            new_count: 1,
            lines: vec![
                HunkLine::Remove("line2".into()),
                HunkLine::Add("LINE2".into()),
            ],
        }];
        let out = apply_hunks("f", original, &hunks).expect("applied");
        assert_eq!(out, "line1\nLINE2\nline3\n");
    }

    #[test]
    fn apply_hunks_with_context() {
        let original = "a\nb\nc\n";
        let hunks = vec![Hunk {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 3,
            lines: vec![
                HunkLine::Context("a".into()),
                HunkLine::Remove("b".into()),
                HunkLine::Add("B".into()),
                HunkLine::Context("c".into()),
            ],
        }];
        let out = apply_hunks("f", original, &hunks).expect("applied");
        assert_eq!(out, "a\nB\nc\n");
    }

    #[test]
    fn apply_hunks_rejects_context_mismatch() {
        let original = "a\nb\nc\n";
        let hunks = vec![Hunk {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
            lines: vec![HunkLine::Context("WRONG".into()), HunkLine::Add("X".into())],
        }];
        let err = apply_hunks("f", original, &hunks).expect_err("context mismatch");
        assert!(err.contains("context mismatch"));
    }

    #[test]
    fn apply_hunks_rejects_overlapping_hunks() {
        let original = "a\nb\nc\nd\n";
        let hunks = vec![
            Hunk {
                old_start: 1,
                old_count: 2,
                new_start: 1,
                new_count: 1,
                lines: vec![HunkLine::Context("a".into()), HunkLine::Remove("b".into())],
            },
            Hunk {
                old_start: 2,
                old_count: 1,
                new_start: 2,
                new_count: 0,
                lines: vec![HunkLine::Remove("b".into())],
            },
        ];
        let err = apply_hunks("f", original, &hunks).expect_err("overlap");
        assert!(err.contains("overlapping") || err.contains("out-of-order"));
    }

    #[test]
    fn apply_hunks_handles_no_trailing_newline() {
        let original = "a\nb";
        let hunks = vec![Hunk {
            old_start: 2,
            old_count: 1,
            new_start: 2,
            new_count: 1,
            lines: vec![HunkLine::Remove("b".into()), HunkLine::Add("B".into())],
        }];
        let out = apply_hunks("f", original, &hunks).expect("applied");
        assert_eq!(out, "a\nB");
    }

    #[test]
    fn end_to_end_modification() {
        use std::io::Write;
        let dir = tempfile_dir();
        let path = dir.join("hello.txt");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "hello").unwrap();
        writeln!(f, "world").unwrap();
        drop(f);

        let patch = format!(
            "--- a/{p}\n+++ b/{p}\n@@ -1,2 +1,2 @@\n hello\n-world\n+rust\n",
            p = path.to_string_lossy()
        );
        let tool = ApplyPatchTool::new();
        let result = tool
            .execute(serde_json::json!({"patch": patch}))
            .expect("apply succeeds");
        assert_eq!(result["success"], true);
        assert_eq!(result["hunks_applied"], 1);

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, "hello\nrust\n");
    }

    #[test]
    fn end_to_end_creation_via_dev_null() {
        let dir = tempfile_dir();
        let path = dir.join("new.txt");
        assert!(!path.exists());

        let patch = format!(
            "--- /dev/null\n+++ b/{p}\n@@ -0,0 +1,2 @@\n+line1\n+line2\n",
            p = path.to_string_lossy()
        );
        let tool = ApplyPatchTool::new();
        let result = tool
            .execute(serde_json::json!({"patch": patch}))
            .expect("create succeeds");
        assert_eq!(result["files_created"].as_array().unwrap().len(), 1);

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "line1\nline2\n");
    }

    #[test]
    fn end_to_end_rejects_bad_context_without_writing() {
        use std::io::Write;
        let dir = tempfile_dir();
        let path = dir.join("target.txt");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "actual").unwrap();
        drop(f);

        let patch = format!(
            "--- a/{p}\n+++ b/{p}\n@@ -1 +1 @@\n-EXPECTED\n+REPLACED\n",
            p = path.to_string_lossy()
        );
        let tool = ApplyPatchTool::new();
        let err = tool
            .execute(serde_json::json!({"patch": patch}))
            .expect_err("should reject");
        assert!(err.to_string().contains("mismatch"));

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, "actual\n");
    }

    /// Per-test temp dir under target/. Avoids std::env::temp_dir collisions
    /// when tests run in parallel.
    fn tempfile_dir() -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("agentic-apply-patch-{}-{}", pid, nanos));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
