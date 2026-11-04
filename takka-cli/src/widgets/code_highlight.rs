//! Lightweight syntax tinting for code blocks rendered by
//! [`super::markdown`].
//!
//! Why this exists: the markdown renderer already boxes code blocks
//! with their language label, but the body itself has been a single
//! gray colour. For long replies that include code that's a real
//! readability hit. We add per-language tokenization (keywords,
//! strings, comments, numbers) that produces a vec of styled spans
//! per line.
//!
//! Why not pull in syntect / tree-sitter:
//! - syntect adds ~1.5 MB of binary size and a runtime grammar load.
//! - tree-sitter needs language grammars vendored separately.
//! - We only target a handful of languages, and the goal is "good
//!   enough at a glance" not full IDE-grade highlighting.
//!
//! What is supported (canonical name + accepted aliases):
//!   - rust     (rs)
//!   - python   (py)
//!   - typescript (ts, tsx, javascript, js, jsx)
//!   - shell    (sh, bash, zsh)
//!   - json
//!   - toml
//!   - yaml     (yml)
//!
//! Languages outside this list, and unrecognized info strings, fall
//! back to plain rendering. The tokenizer is whitespace-friendly: it
//! consumes the input completely and never panics on malformed code,
//! since the model can stream incomplete fragments mid-turn.

use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
};

/// A single classified token produced by the language scanner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// Keywords (`fn`, `def`, `if`, `class`, …).
    Keyword,
    /// String literals (single, double, backtick, raw).
    String,
    /// Numeric literals.
    Number,
    /// Single-line and block comments.
    Comment,
    /// Function / method names following a keyword like `fn` or `def`.
    /// (Optional — scanners may skip and emit `Plain` instead.)
    Function,
    /// Type names — capitalized identifiers in Rust / TS.
    TypeName,
    /// Built-in punctuation, brackets, operators worth dimming.
    Punctuation,
    /// Anything not classified.
    Plain,
}

/// Token-style map — single source of truth for highlighting colours.
fn style_for(kind: TokenKind) -> Style {
    match kind {
        TokenKind::Keyword => Style::default()
            .fg(Color::Rgb(255, 121, 198))
            .add_modifier(Modifier::BOLD),
        TokenKind::String => Style::default().fg(Color::Rgb(241, 250, 140)),
        TokenKind::Number => Style::default().fg(Color::Rgb(189, 147, 249)),
        TokenKind::Comment => Style::default()
            .fg(Color::Rgb(98, 114, 164))
            .add_modifier(Modifier::ITALIC),
        TokenKind::Function => Style::default().fg(Color::Rgb(80, 250, 123)),
        TokenKind::TypeName => Style::default().fg(Color::Rgb(139, 233, 253)),
        TokenKind::Punctuation => Style::default().fg(Color::Rgb(150, 150, 160)),
        TokenKind::Plain => Style::default().fg(Color::Rgb(200, 200, 200)),
    }
}

/// Canonicalize an info-string from a fenced block. Returns `None` when
/// the language isn't supported.
pub fn canonical_lang(raw: &str) -> Option<&'static str> {
    let lang = raw.trim().to_lowercase();
    // Strip off any qualifiers like `rust,no_run` or `python {linenos=true}`.
    let lang = lang
        .split([',', ' ', '\t', '{', '|', '\n', '\r'])
        .next()
        .unwrap_or("");
    match lang {
        "rust" | "rs" => Some("rust"),
        "python" | "py" => Some("python"),
        "typescript" | "ts" | "tsx" | "javascript" | "js" | "jsx" => Some("typescript"),
        "shell" | "sh" | "bash" | "zsh" => Some("shell"),
        "json" => Some("json"),
        "toml" => Some("toml"),
        "yaml" | "yml" => Some("yaml"),
        _ => None,
    }
}

/// Highlight one line of code. The returned spans cover the full input
/// (no characters dropped). Newline characters are NOT preserved here —
/// callers split on `\n` themselves and call `highlight_line` per line.
///
/// `lang` is the canonical name returned by [`canonical_lang`]; passing
/// a non-canonical value renders plain.
pub fn highlight_line(line: &str, lang: &str) -> Vec<Span<'static>> {
    let tokens = match lang {
        "rust" => tokenize_clike(line, RUST_KEYWORDS, /*hash_comments=*/ false),
        "typescript" => tokenize_clike(line, TS_KEYWORDS, /*hash_comments=*/ false),
        "python" => tokenize_clike(line, PYTHON_KEYWORDS, /*hash_comments=*/ true),
        "shell" => tokenize_shell(line),
        "json" => tokenize_json(line),
        "toml" => tokenize_toml(line),
        "yaml" => tokenize_yaml(line),
        _ => return vec![Span::styled(line.to_string(), style_for(TokenKind::Plain))],
    };

    tokens
        .into_iter()
        .map(|(kind, text)| Span::styled(text, style_for(kind)))
        .collect()
}

// ── Keyword tables ──────────────────────────────────────────────────────

const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while", "yield",
];

const TS_KEYWORDS: &[&str] = &[
    "abstract",
    "as",
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "from",
    "function",
    "get",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "new",
    "null",
    "of",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "set",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "type",
    "typeof",
    "undefined",
    "var",
    "void",
    "while",
    "with",
    "yield",
];

const PYTHON_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield", "match", "case",
];

// ── C-like tokenizer (rust, ts/js, python) ──────────────────────────────

fn tokenize_clike(line: &str, keywords: &[&str], hash_comments: bool) -> Vec<(TokenKind, String)> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];

        // Line comments
        if hash_comments && b == b'#' {
            push(&mut out, TokenKind::Comment, &line[i..]);
            return out;
        }
        if !hash_comments && b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            push(&mut out, TokenKind::Comment, &line[i..]);
            return out;
        }
        // Block comments are typically multi-line; we only flag the
        // start marker on this line. Anything after `/*` on this line
        // is treated as comment too — close handling is best-effort.
        if !hash_comments && b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            push(&mut out, TokenKind::Comment, &line[i..]);
            return out;
        }

        // Strings
        if b == b'"' || b == b'\'' || b == b'`' {
            let quote = b;
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
            push(&mut out, TokenKind::String, &line[start..i]);
            continue;
        }

        // Numbers
        if b.is_ascii_digit() {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'.' || bytes[i] == b'_')
            {
                i += 1;
            }
            push(&mut out, TokenKind::Number, &line[start..i]);
            continue;
        }

        // Identifier / keyword
        if b == b'_' || b.is_ascii_alphabetic() {
            let start = i;
            while i < bytes.len() && (bytes[i] == b'_' || bytes[i].is_ascii_alphanumeric()) {
                i += 1;
            }
            let word = &line[start..i];
            if keywords.contains(&word) {
                push(&mut out, TokenKind::Keyword, word);
            } else if word
                .chars()
                .next()
                .map(|c| c.is_ascii_uppercase())
                .unwrap_or(false)
            {
                push(&mut out, TokenKind::TypeName, word);
            } else {
                push(&mut out, TokenKind::Plain, word);
            }
            continue;
        }

        // Punctuation: single byte; group runs of punctuation together.
        if !b.is_ascii_alphanumeric() && !b.is_ascii_whitespace() {
            let start = i;
            while i < bytes.len()
                && !bytes[i].is_ascii_alphanumeric()
                && !bytes[i].is_ascii_whitespace()
                && bytes[i] != b'"'
                && bytes[i] != b'\''
                && bytes[i] != b'`'
            {
                i += 1;
            }
            push(&mut out, TokenKind::Punctuation, &line[start..i]);
            continue;
        }

        // Whitespace + everything else: pass through as Plain.
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i == start {
            // Multi-byte UTF-8 char we don't classify; advance by one
            // codepoint.
            let mut step = 1;
            while step < 4 && start + step <= line.len() && !line.is_char_boundary(start + step) {
                step += 1;
            }
            i = (start + step).min(line.len());
        }
        push(&mut out, TokenKind::Plain, &line[start..i]);
    }

    out
}

// ── Shell ───────────────────────────────────────────────────────────────

const SHELL_KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "case", "esac", "for", "while", "until", "do", "done",
    "in", "function", "select", "time", "return", "break", "continue", "exit", "export", "local",
    "readonly", "set", "unset", "shift", "trap", "true", "false",
];

fn tokenize_shell(line: &str) -> Vec<(TokenKind, String)> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];

        // # line comment (but not inside a string — at the start of a
        // word boundary).
        if b == b'#' && (i == 0 || bytes[i - 1].is_ascii_whitespace()) {
            push(&mut out, TokenKind::Comment, &line[i..]);
            return out;
        }

        // Strings
        if b == b'"' || b == b'\'' {
            let quote = b;
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
            push(&mut out, TokenKind::String, &line[start..i]);
            continue;
        }

        // Variables: $FOO, ${BAR}, $1
        if b == b'$' {
            let start = i;
            i += 1;
            if i < bytes.len() && bytes[i] == b'{' {
                while i < bytes.len() && bytes[i] != b'}' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1; // skip closing }
                }
            } else {
                while i < bytes.len() && (bytes[i] == b'_' || bytes[i].is_ascii_alphanumeric()) {
                    i += 1;
                }
            }
            push(&mut out, TokenKind::Function, &line[start..i]);
            continue;
        }

        // Numbers
        if b.is_ascii_digit() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            push(&mut out, TokenKind::Number, &line[start..i]);
            continue;
        }

        // Identifiers / keywords / commands
        if b == b'_' || b.is_ascii_alphabetic() {
            let start = i;
            while i < bytes.len()
                && (bytes[i] == b'_' || bytes[i] == b'-' || bytes[i].is_ascii_alphanumeric())
            {
                i += 1;
            }
            let word = &line[start..i];
            if SHELL_KEYWORDS.contains(&word) {
                push(&mut out, TokenKind::Keyword, word);
            } else {
                push(&mut out, TokenKind::Plain, word);
            }
            continue;
        }

        // Default: single byte / whitespace / punctuation
        let start = i;
        i += 1;
        push(&mut out, TokenKind::Plain, &line[start..i]);
    }

    out
}

// ── JSON ────────────────────────────────────────────────────────────────

fn tokenize_json(line: &str) -> Vec<(TokenKind, String)> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];

        if b == b'"' {
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            // Heuristic: a key is a string that's followed by `:`
            // (whitespace permitted).
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            let kind = if j < bytes.len() && bytes[j] == b':' {
                TokenKind::Function
            } else {
                TokenKind::String
            };
            push(&mut out, kind, &line[start..i]);
            continue;
        }

        if b.is_ascii_digit() || (b == b'-' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit())
        {
            let start = i;
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_digit()
                    || bytes[i] == b'.'
                    || bytes[i] == b'e'
                    || bytes[i] == b'E'
                    || bytes[i] == b'+'
                    || bytes[i] == b'-')
            {
                i += 1;
            }
            push(&mut out, TokenKind::Number, &line[start..i]);
            continue;
        }

        if b.is_ascii_alphabetic() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                i += 1;
            }
            let word = &line[start..i];
            let kind = match word {
                "true" | "false" | "null" => TokenKind::Keyword,
                _ => TokenKind::Plain,
            };
            push(&mut out, kind, word);
            continue;
        }

        let start = i;
        i += 1;
        push(&mut out, TokenKind::Plain, &line[start..i]);
    }

    out
}

// ── TOML ────────────────────────────────────────────────────────────────

fn tokenize_toml(line: &str) -> Vec<(TokenKind, String)> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;

    // Comments
    if let Some(idx) = find_byte_outside_string(line, b'#') {
        let (head, tail) = line.split_at(idx);
        // Recursively tokenize the head, then append the comment.
        if !head.is_empty() {
            out.extend(tokenize_toml(head));
        }
        push(&mut out, TokenKind::Comment, tail);
        return out;
    }

    // [section] header
    if line.trim_start().starts_with('[') {
        push(&mut out, TokenKind::TypeName, line);
        return out;
    }

    while i < bytes.len() {
        let b = bytes[i];

        if b == b'"' || b == b'\'' {
            let quote = b;
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && quote == b'"' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
            push(&mut out, TokenKind::String, &line[start..i]);
            continue;
        }

        if b.is_ascii_digit() {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'.' || bytes[i] == b'-')
            {
                i += 1;
            }
            push(&mut out, TokenKind::Number, &line[start..i]);
            continue;
        }

        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'-')
            {
                i += 1;
            }
            let word = &line[start..i];
            // Heuristic: identifier followed by `=` is a key.
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            let kind = if j < bytes.len() && bytes[j] == b'=' {
                TokenKind::Function
            } else if word == "true" || word == "false" {
                TokenKind::Keyword
            } else {
                TokenKind::Plain
            };
            push(&mut out, kind, word);
            continue;
        }

        let start = i;
        i += 1;
        push(&mut out, TokenKind::Plain, &line[start..i]);
    }

    out
}

// ── YAML ────────────────────────────────────────────────────────────────

fn tokenize_yaml(line: &str) -> Vec<(TokenKind, String)> {
    let mut out = Vec::new();

    // Comments (`# …` after whitespace)
    if let Some(idx) = find_byte_outside_string(line, b'#') {
        let (head, tail) = line.split_at(idx);
        if !head.is_empty() {
            out.extend(tokenize_yaml(head));
        }
        push(&mut out, TokenKind::Comment, tail);
        return out;
    }

    // Find the first ':' (key/value separator) at top level.
    let bytes = line.as_bytes();
    let mut colon_idx = None;
    let mut in_str: Option<u8> = None;
    for (i, &b) in bytes.iter().enumerate() {
        match (b, in_str) {
            (b'"', None) | (b'\'', None) => in_str = Some(b),
            (q, Some(c)) if q == c => in_str = None,
            (b':', None) if (i + 1 == bytes.len() || bytes[i + 1].is_ascii_whitespace()) => {
                colon_idx = Some(i);
                break;
            }
            _ => {}
        }
    }

    if let Some(idx) = colon_idx {
        let (key_part, rest) = line.split_at(idx);
        // Key: leading whitespace + identifier + optional quote
        let trimmed = key_part.trim_end();
        let pad_len = key_part.len() - trimmed.len();
        if !trimmed.is_empty() {
            push(&mut out, TokenKind::Function, trimmed);
        }
        if pad_len > 0 {
            push(&mut out, TokenKind::Plain, &key_part[trimmed.len()..]);
        }
        push(&mut out, TokenKind::Punctuation, &rest[..1]); // `:`
        let value = &rest[1..];
        if !value.is_empty() {
            // Tokenize the value as JSON-ish (string, number, bool).
            out.extend(tokenize_yaml_value(value));
        }
        return out;
    }

    // Bullet list: `- item`
    let trim = line.trim_start();
    if trim.starts_with("- ") {
        let pad = &line[..line.len() - trim.len()];
        if !pad.is_empty() {
            push(&mut out, TokenKind::Plain, pad);
        }
        push(&mut out, TokenKind::Punctuation, "-");
        push(&mut out, TokenKind::Plain, &trim[1..]);
        return out;
    }

    push(&mut out, TokenKind::Plain, line);
    out
}

fn tokenize_yaml_value(value: &str) -> Vec<(TokenKind, String)> {
    let trimmed = value.trim_start();
    let pad_len = value.len() - trimmed.len();
    let mut out = Vec::new();
    if pad_len > 0 {
        push(&mut out, TokenKind::Plain, &value[..pad_len]);
    }
    if trimmed.is_empty() {
        return out;
    }
    let lower = trimmed.to_lowercase();
    let kind = if matches!(
        lower.as_str(),
        "true" | "false" | "null" | "yes" | "no" | "~"
    ) {
        TokenKind::Keyword
    } else if trimmed.starts_with('"') || trimmed.starts_with('\'') {
        TokenKind::String
    } else if trimmed
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
    {
        TokenKind::Number
    } else {
        TokenKind::String
    };
    push(&mut out, kind, trimmed);
    out
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn push(out: &mut Vec<(TokenKind, String)>, kind: TokenKind, text: &str) {
    if text.is_empty() {
        return;
    }
    // Coalesce consecutive Plain/Punctuation tokens to keep the span
    // count down (each Span becomes a heap allocation).
    if let Some((last_kind, last_text)) = out.last_mut() {
        if *last_kind == kind && matches!(kind, TokenKind::Plain | TokenKind::Punctuation) {
            last_text.push_str(text);
            return;
        }
    }
    out.push((kind, text.to_string()));
}

/// Find the first occurrence of `target` in `line` that's not inside a
/// quoted string. Returns the byte index, or `None` if not present.
fn find_byte_outside_string(line: &str, target: u8) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut in_str: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match (b, in_str) {
            (b'\\', Some(_)) if i + 1 < bytes.len() => {
                i += 2;
                continue;
            }
            (b'"', None) | (b'\'', None) => in_str = Some(b),
            (q, Some(c)) if q == c => in_str = None,
            (t, None) if t == target => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn join(spans: &[Span<'_>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect::<String>()
    }

    fn classify_words(spans: &[Span<'_>], lookup: &str) -> Vec<TokenKind> {
        spans
            .iter()
            .filter(|s| s.content.contains(lookup))
            .map(|s| {
                // Recover kind from the styled colour. Cheap reverse
                // mapping for tests only.
                if s.style == style_for(TokenKind::Keyword) {
                    TokenKind::Keyword
                } else if s.style == style_for(TokenKind::String) {
                    TokenKind::String
                } else if s.style == style_for(TokenKind::Number) {
                    TokenKind::Number
                } else if s.style == style_for(TokenKind::Comment) {
                    TokenKind::Comment
                } else if s.style == style_for(TokenKind::Function) {
                    TokenKind::Function
                } else if s.style == style_for(TokenKind::TypeName) {
                    TokenKind::TypeName
                } else if s.style == style_for(TokenKind::Punctuation) {
                    TokenKind::Punctuation
                } else {
                    TokenKind::Plain
                }
            })
            .collect()
    }

    #[test]
    fn canonical_lang_handles_aliases_and_qualifiers() {
        assert_eq!(canonical_lang("rust"), Some("rust"));
        assert_eq!(canonical_lang("rs"), Some("rust"));
        assert_eq!(canonical_lang("ts"), Some("typescript"));
        assert_eq!(canonical_lang("javascript"), Some("typescript"));
        assert_eq!(canonical_lang("py"), Some("python"));
        assert_eq!(canonical_lang("bash"), Some("shell"));
        assert_eq!(canonical_lang("YAML"), Some("yaml"));
        assert_eq!(canonical_lang("rust,no_run"), Some("rust"));
        assert_eq!(canonical_lang("python {linenos=true}"), Some("python"));
        assert_eq!(canonical_lang("rust|fn main()"), Some("rust"));
        assert_eq!(canonical_lang("ros2"), None);
        assert_eq!(canonical_lang("rust  "), Some("rust"));
        assert_eq!(canonical_lang("  rust"), Some("rust"));
        assert_eq!(canonical_lang("klingon"), None);
        assert_eq!(canonical_lang(""), None);
    }

    #[test]
    fn unknown_lang_falls_back_to_plain_single_span() {
        let spans = highlight_line("anything here", "klingon");
        assert_eq!(spans.len(), 1);
        assert_eq!(join(&spans), "anything here");
    }

    #[test]
    fn rust_keywords_and_strings() {
        let spans = highlight_line(r#"fn main() { let x = "hi"; }"#, "rust");
        assert_eq!(join(&spans), r#"fn main() { let x = "hi"; }"#);
        assert!(classify_words(&spans, "fn").contains(&TokenKind::Keyword));
        assert!(classify_words(&spans, "let").contains(&TokenKind::Keyword));
        assert!(classify_words(&spans, r#""hi""#).contains(&TokenKind::String));
    }

    #[test]
    fn rust_capitalized_idents_get_typename_color() {
        let spans = highlight_line("let v: Vec<String> = Vec::new();", "rust");
        assert!(classify_words(&spans, "Vec").contains(&TokenKind::TypeName));
        assert!(classify_words(&spans, "String").contains(&TokenKind::TypeName));
    }

    #[test]
    fn rust_line_comment() {
        let spans = highlight_line("let x = 1; // trailing", "rust");
        // The whole `// trailing` segment should be a single Comment span.
        let comment_segments: Vec<&str> = spans
            .iter()
            .filter(|s| s.style == style_for(TokenKind::Comment))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(comment_segments, vec!["// trailing"]);
    }

    #[test]
    fn python_hash_comment_and_keywords() {
        let spans = highlight_line("def foo():  # docstring", "python");
        assert!(classify_words(&spans, "def").contains(&TokenKind::Keyword));
        assert!(classify_words(&spans, "# docstring").contains(&TokenKind::Comment));
    }

    #[test]
    fn typescript_strings_and_keywords() {
        let spans = highlight_line("const x: string = `template ${y}` + 'plain';", "typescript");
        assert!(classify_words(&spans, "const").contains(&TokenKind::Keyword));
        assert!(spans.iter().any(|s| s.content.contains("`template")));
        assert!(spans.iter().any(|s| s.content.contains("'plain'")));
    }

    #[test]
    fn shell_variables_and_keywords() {
        let spans = highlight_line(r#"if [ "$x" = "y" ]; then echo $foo; fi"#, "shell");
        assert!(classify_words(&spans, "if").contains(&TokenKind::Keyword));
        assert!(classify_words(&spans, "fi").contains(&TokenKind::Keyword));
        assert!(classify_words(&spans, "$foo").contains(&TokenKind::Function));
    }

    #[test]
    fn shell_hash_comment() {
        let spans = highlight_line("ls -la # list everything", "shell");
        assert!(classify_words(&spans, "# list everything").contains(&TokenKind::Comment));
    }

    #[test]
    fn json_keys_get_function_color_values_dont() {
        let spans = highlight_line(r#"{"name": "alice", "age": 30}"#, "json");
        // The "name" key should be Function; the "alice" value String.
        let key_spans: Vec<&str> = spans
            .iter()
            .filter(|s| s.style == style_for(TokenKind::Function))
            .map(|s| s.content.as_ref())
            .collect();
        assert!(key_spans.iter().any(|s| s.contains("name")));
        assert!(key_spans.iter().any(|s| s.contains("age")));

        let str_spans: Vec<&str> = spans
            .iter()
            .filter(|s| s.style == style_for(TokenKind::String))
            .map(|s| s.content.as_ref())
            .collect();
        assert!(str_spans.iter().any(|s| s.contains("alice")));
    }

    #[test]
    fn json_booleans_and_null() {
        let spans = highlight_line(r#"{"ok": true, "miss": null}"#, "json");
        assert!(classify_words(&spans, "true").contains(&TokenKind::Keyword));
        assert!(classify_words(&spans, "null").contains(&TokenKind::Keyword));
    }

    #[test]
    fn toml_section_header() {
        let spans = highlight_line("[dependencies]", "toml");
        // Whole line should be a TypeName (section).
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].style, style_for(TokenKind::TypeName));
    }

    #[test]
    fn toml_key_value_with_comment() {
        let spans = highlight_line(r#"name = "agentic"  # main name"#, "toml");
        assert!(classify_words(&spans, "name").contains(&TokenKind::Function));
        assert!(
            spans
                .iter()
                .any(|s| s.content.contains(r#""agentic""#)
                    && s.style == style_for(TokenKind::String))
        );
        assert!(classify_words(&spans, "# main name").contains(&TokenKind::Comment));
    }

    #[test]
    fn yaml_key_value() {
        let spans = highlight_line("name: agentic-cli", "yaml");
        assert!(classify_words(&spans, "name").contains(&TokenKind::Function));
        assert!(spans.iter().any(|s| s.content.contains(":")));
    }

    #[test]
    fn yaml_bullet_list_item() {
        let spans = highlight_line("  - item-one", "yaml");
        assert!(spans
            .iter()
            .any(|s| s.content == "-" && s.style == style_for(TokenKind::Punctuation)));
    }

    #[test]
    fn no_panics_on_unterminated_string() {
        // The stream may end mid-token. Must not panic.
        let _ = highlight_line(r#"let x = "unfinished"#, "rust");
        let _ = highlight_line(r#"name = "still-going"#, "toml");
        let _ = highlight_line(r#"{"key": "no-close"#, "json");
    }

    #[test]
    fn no_panics_on_utf8_content() {
        let _ = highlight_line("// 日本語コメント", "rust");
        let _ = highlight_line("# 日本語", "python");
        let _ = highlight_line(r#"name = "résumé""#, "toml");
    }

    #[test]
    fn full_input_is_preserved() {
        // Sanity: tokens cover the whole input, no characters dropped.
        let inputs = [
            ("rust", r#"fn x() { let y = 42; }"#),
            ("python", "def f(x):\n    return x + 1"),
            ("typescript", "const a: number = 0;"),
            ("shell", "for i in 1 2 3; do echo $i; done"),
            ("json", r#"{"k": [1,2,3]}"#),
            ("toml", "[a]\nk = 1"),
            ("yaml", "k: v\n- l"),
        ];
        for (lang, text) in inputs {
            for line in text.lines() {
                let spans = highlight_line(line, lang);
                assert_eq!(
                    join(&spans),
                    line,
                    "round-trip failure for lang={}, line={:?}",
                    lang,
                    line
                );
            }
        }
    }
}
