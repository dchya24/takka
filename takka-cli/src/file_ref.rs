//! `@` file reference expansion
//!
//! Expands `@path/to/file` references in user input by reading the file contents
//! and inlining them into the prompt sent to the AI.
//!
//! Example:
//!   "explain @src/main.rs" → "explain <file path=\"src/main.rs\">\n<file contents>\n</file>"
//!
//! Supports:
//! - Multiple `@` references in one message
//! - Directories: `@src/` reads all files in the directory (non-recursive)
//! - Respects .gitignore via the `ignore` crate

use std::path::Path;

/// Expand all `@path` references in the input string.
///
/// - `@path/to/file.rs` → `<file path="path/to/file.rs" />`
/// - `@src/` (directory) → `<directory path="src"> listing </directory>`
///
/// Returns the expanded string. If a file doesn't exist,
/// includes an error message inline instead.
///
/// Image references (png/jpeg/gif/webp by extension OR magic bytes) are
/// kept as a short marker in the text — callers that want the actual
/// image bytes should use [`expand_with_attachments`] instead so the
/// image flows to the provider as a real attachment.
pub fn expand_file_refs(input: &str) -> String {
    expand_with_attachments(input).text
}

/// Result of [`expand_with_attachments`].
pub struct ExpandedRefs {
    /// The user input with `@path` references replaced. Image refs are
    /// replaced with a `<image path="..." />` marker; non-image files
    /// retain their `<file path="..." />` marker.
    pub text: String,
    /// Image attachments extracted from `@image.png`-style refs. The
    /// orchestrator will pass these to the provider's vision channel.
    pub attachments: Vec<takka_core::Attachment>,
}

/// Like [`expand_file_refs`] but additionally loads any `@image.png`
/// references as real attachments. Magic-byte detection (not extension
/// trust) decides whether a file is an image. The text marker remains
/// in place so the model sees the reference position; the actual
/// bytes ride alongside in the returned `attachments` vec.
pub fn expand_with_attachments(input: &str) -> ExpandedRefs {
    let mut result = String::new();
    let mut attachments: Vec<takka_core::Attachment> = Vec::new();
    let mut last_end = 0;

    for (at_pos, path) in find_file_refs(input) {
        result.push_str(&input[last_end..at_pos]);

        match try_load_as_image(&path) {
            Some(att) => {
                result.push_str(&format!("<image path=\"{}\" />", path));
                attachments.push(att);
            }
            None => {
                let expanded = read_file_ref(&path);
                result.push_str(&expanded);
            }
        }

        last_end = at_pos + 1 + path.len();
    }

    if last_end < input.len() {
        result.push_str(&input[last_end..]);
    }

    ExpandedRefs {
        text: result,
        attachments,
    }
}

/// Try to load `path_str` as an image attachment. Returns `None` for
/// non-image files, missing files, and directories — the caller falls
/// back to the regular `<file path=...>` rendering for those.
fn try_load_as_image(path_str: &str) -> Option<takka_core::Attachment> {
    let path = Path::new(path_str.trim_end_matches('/').trim_end_matches('\\'));
    if !path.is_file() {
        return None;
    }
    // Read first 16 bytes to sniff. Avoid reading the whole file when
    // it isn't an image.
    let mut head = [0u8; 16];
    let n = match std::fs::File::open(path).and_then(|mut f| {
        use std::io::Read;
        f.read(&mut head)
    }) {
        Ok(n) => n,
        Err(_) => return None,
    };
    takka_core::attachments::detect_image_mime(&head[..n])?;
    // Fully load + base64-encode through the canonical loader so size
    // caps + format validation apply uniformly.
    takka_core::attachments::load_image_from_path(path, takka_core::AttachmentLimits::default())
        .ok()
}

/// Find all `@path` references in the input.
/// Returns a vec of (position_of_@, path_string).
fn find_file_refs(input: &str) -> Vec<(usize, String)> {
    let mut refs = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '@' && (i == 0 || chars[i - 1].is_whitespace()) {
            // Start collecting the path
            let at_pos = i;
            let mut path_end = i + 1;

            // Collect characters that are valid in a file path
            for (j, c) in chars.iter().enumerate().skip(i + 1) {
                if c.is_whitespace() {
                    break;
                }
                path_end = j + 1;
            }

            if path_end > i + 1 {
                let path: String = chars[i + 1..path_end].iter().collect();
                // Only consider it a file ref if it looks like a path
                // (contains /, \, ., or common extensions)
                if looks_like_path(&path) {
                    refs.push((at_pos, path));
                    i = path_end;
                    continue;
                }
            }
        }
        i += 1;
    }

    refs
}

/// Check if a string looks like a file path (not just a social media @mention).
fn looks_like_path(s: &str) -> bool {
    // Contains path separators
    if s.contains('/') || s.contains('\\') {
        return true;
    }
    // Has a file extension
    if let Some(dot_pos) = s.rfind('.') {
        if dot_pos < s.len() - 1 {
            let ext = &s[dot_pos + 1..];
            // Common extensions
            return matches!(
                ext,
                "rs" | "ts"
                    | "tsx"
                    | "js"
                    | "jsx"
                    | "py"
                    | "go"
                    | "java"
                    | "c"
                    | "cpp"
                    | "h"
                    | "hpp"
                    | "rb"
                    | "php"
                    | "sh"
                    | "bash"
                    | "zsh"
                    | "fish"
                    | "css"
                    | "scss"
                    | "html"
                    | "htm"
                    | "xml"
                    | "json"
                    | "yaml"
                    | "yml"
                    | "toml"
                    | "ini"
                    | "cfg"
                    | "conf"
                    | "md"
                    | "txt"
                    | "csv"
                    | "sql"
                    | "lock"
                    | "log"
                    | "env"
                    | "gitignore"
                    | "dockerignore"
                    | "editorconfig"
                    | "Makefile"
                    | "Dockerfile"
            );
        }
    }
    // Matches known config/build files without extensions
    matches!(
        s,
        "Makefile"
            | "Dockerfile"
            | "Cargo.toml"
            | "Cargo.lock"
            | "package.json"
            | "tsconfig.json"
            | ".gitignore"
            | ".env"
            | ".env.local"
            | ".env.production"
            | "README"
            | "README.md"
            | "LICENSE"
            | "LICENSE.md"
    )
}

/// Read a file or directory and return its contents formatted for the AI prompt.
fn read_file_ref(path_str: &str) -> String {
    let path_str = path_str.trim_end_matches('/').trim_end_matches('\\');
    let path = Path::new(path_str);

    if !path.exists() {
        return format!("<path=\"{}\" /> [Not found]", path_str);
    }

    if path.is_dir() {
        format!("<directory path=\"{}\" />", path_str)
    } else {
        format!("<file path=\"{}\" />", path_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_refs() {
        assert_eq!(expand_file_refs("hello world"), "hello world");
    }

    #[test]
    fn test_email_not_expanded() {
        // @ in email should NOT be treated as file ref
        let result = expand_file_refs("send to user@example.com");
        assert_eq!(result, "send to user@example.com");
    }

    #[test]
    fn test_mention_not_expanded() {
        // @mention without path-like structure should NOT be expanded
        let result = expand_file_refs("hey @john can you help");
        assert_eq!(result, "hey @john can you help");
    }

    #[test]
    fn test_path_with_slash_detected() {
        let refs = find_file_refs("look at @src/main.rs please");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].1, "src/main.rs");
    }

    #[test]
    fn test_path_with_extension_detected() {
        let refs = find_file_refs("check @Cargo.toml for deps");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].1, "Cargo.toml");
    }

    #[test]
    fn test_multiple_refs() {
        let refs = find_file_refs("compare @src/a.rs and @src/b.rs");
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].1, "src/a.rs");
        assert_eq!(refs[1].1, "src/b.rs");
    }

    #[test]
    fn test_nonexistent_file() {
        let result = expand_file_refs("read @nonexistent/file.txt");
        assert!(result.contains("nonexistent/file.txt"));
        assert!(result.contains("Not found"));
    }

    #[test]
    fn test_existing_file() {
        // Use Cargo.toml which exists in the project
        let result = expand_file_refs("check @Cargo.toml");
        assert!(result.contains("<file path=\"Cargo.toml\" />"));
    }

    #[test]
    fn test_existing_directory() {
        let result = expand_file_refs("list @src/");
        assert!(result.contains("<directory path=\"src\" />"));
    }

    #[test]
    fn test_looks_like_path() {
        assert!(looks_like_path("src/main.rs"));
        assert!(looks_like_path("Cargo.toml"));
        assert!(looks_like_path(".gitignore"));
        assert!(looks_like_path("config/app.json"));
        assert!(!looks_like_path("john"));
        assert!(!looks_like_path("everyone"));
    }

    #[test]
    fn test_ref_at_start_of_input() {
        let refs = find_file_refs("@Cargo.toml is the config");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].1, "Cargo.toml");
    }

    #[test]
    fn test_ref_at_end_of_input() {
        let refs = find_file_refs("check @Cargo.toml");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].1, "Cargo.toml");
    }

    // ── attachment expansion ───────────────────────────

    fn write_temp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("agentic-fileref-{}-{}", std::process::id(), nanos));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn png_bytes() -> Vec<u8> {
        // Minimal PNG signature + IHDR (1×1 RGBA).
        let mut b = b"\x89PNG\r\n\x1a\n".to_vec();
        b.extend_from_slice(&[
            0x00, 0x00, 0x00, 0x0D, b'I', b'H', b'D', b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
            0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89,
        ]);
        b
    }

    #[test]
    fn expand_with_attachments_extracts_image() {
        let p = write_temp("shot.png", &png_bytes());
        // No trailing punctuation — the parser treats `?` as part of the path.
        let input = format!("see @{}", p.to_string_lossy());
        let out = expand_with_attachments(&input);
        assert_eq!(out.attachments.len(), 1);
        assert_eq!(out.attachments[0].mime_type, "image/png");
        assert!(
            out.text.contains("<image path="),
            "expected <image path=...>, got: {}",
            out.text
        );
    }

    #[test]
    fn expand_with_attachments_keeps_text_files_as_file_marker() {
        let p = write_temp("notes.txt", b"hello");
        let input = format!("summarize @{}", p.to_string_lossy());
        let out = expand_with_attachments(&input);
        assert!(out.attachments.is_empty());
        assert!(out.text.contains("<file path="));
    }

    #[test]
    fn expand_with_attachments_handles_renamed_extension() {
        // .txt extension but PNG bytes — magic-byte sniff still detects it.
        let p = write_temp("sneaky.txt", &png_bytes());
        let input = format!("see @{}", p.to_string_lossy());
        let out = expand_with_attachments(&input);
        assert_eq!(out.attachments.len(), 1);
        assert_eq!(out.attachments[0].mime_type, "image/png");
    }
}
