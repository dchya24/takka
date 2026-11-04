//! Unified-diff producer for tool results.
//!
//! Given before/after file content (or raw strings), produce a unified-diff
//! string in the standard `--- a/file +++ b/file @@` shape that the
//! takka-cli `widgets::diff` renderer expects.
//!
//! Used by `edit_file` and `write_file` so tool results carry a structured
//! diff that the CLI can render inline instead of dumping the full file.

use similar::{ChangeTag, TextDiff};

/// Build a unified-diff string from before/after content.
///
/// `path` is the file label used for the `--- a/<path>` / `+++ b/<path>`
/// headers; pass an empty string to suppress the headers.
///
/// `context_lines` controls how many unchanged lines surround each change
/// hunk. 3 is the conventional default (`git diff` uses 3).
///
/// The returned string ends with a trailing newline for easy `print!`.
pub fn unified_diff(path: &str, before: &str, after: &str, context_lines: usize) -> String {
    if before == after {
        return String::new();
    }

    let diff = TextDiff::from_lines(before, after);
    let mut out = String::new();

    if !path.is_empty() {
        out.push_str(&format!("--- a/{}\n", path));
        out.push_str(&format!("+++ b/{}\n", path));
    }

    for hunk in diff
        .unified_diff()
        .context_radius(context_lines)
        .iter_hunks()
    {
        out.push_str(&format!("{}\n", hunk.header()));
        for change in hunk.iter_changes() {
            let prefix = match change.tag() {
                ChangeTag::Insert => "+",
                ChangeTag::Delete => "-",
                ChangeTag::Equal => " ",
            };
            // similar always returns lines with their trailing newline,
            // so we don't add another. Strip the trailing newline if
            // present so the prefix lands cleanly.
            let value = change.value();
            let value = value.strip_suffix('\n').unwrap_or(value);
            out.push_str(prefix);
            out.push_str(value);
            out.push('\n');
        }
    }

    out
}

/// Compact summary: `+N -M lines`. Useful when the full diff would be too
/// noisy and the caller only wants a headline number.
pub fn change_summary(before: &str, after: &str) -> ChangeStats {
    if before == after {
        return ChangeStats {
            added: 0,
            removed: 0,
        };
    }
    let diff = TextDiff::from_lines(before, after);
    let mut added = 0usize;
    let mut removed = 0usize;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    ChangeStats { added, removed }
}

/// Line counts for added/removed lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangeStats {
    pub added: usize,
    pub removed: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_when_unchanged() {
        let d = unified_diff("foo.txt", "hello\n", "hello\n", 3);
        assert!(d.is_empty());
    }

    #[test]
    fn renders_unified_format() {
        let before = "fn main() {\n    println!(\"old\");\n}\n";
        let after = "fn main() {\n    println!(\"new\");\n}\n";
        let d = unified_diff("src/main.rs", before, after, 3);
        assert!(d.starts_with("--- a/src/main.rs\n"));
        assert!(d.contains("+++ b/src/main.rs\n"));
        assert!(d.contains("@@"));
        assert!(d.contains("-    println!(\"old\");"));
        assert!(d.contains("+    println!(\"new\");"));
    }

    #[test]
    fn no_headers_when_path_empty() {
        let d = unified_diff("", "a\n", "b\n", 3);
        assert!(!d.contains("---"));
        assert!(!d.contains("+++"));
        assert!(d.contains("-a"));
        assert!(d.contains("+b"));
    }

    #[test]
    fn change_summary_counts_lines() {
        let before = "a\nb\nc\n";
        let after = "a\nB\nc\nd\n";
        let stats = change_summary(before, after);
        assert_eq!(stats.added, 2); // B and d
        assert_eq!(stats.removed, 1); // b
    }

    #[test]
    fn change_summary_zero_when_unchanged() {
        let stats = change_summary("same\n", "same\n");
        assert_eq!(
            stats,
            ChangeStats {
                added: 0,
                removed: 0
            }
        );
    }
}
