//! `fetch` tool — fetch a URL and return its content as cleaned text.
//!
//! Implementation notes:
//! - Uses `reqwest::blocking` (already a dependency) so the tool fits the
//!   sync `Tool::execute` signature without spawning a runtime.
//! - For HTML responses we strip `<script>` and `<style>` blocks, drop
//!   tags, decode the most common entities, and collapse whitespace.
//!   This is intentionally minimal: it produces readable plain text from
//!   most documentation pages without adding an HTML parser dependency.
//! - Per-session in-memory cache keyed by URL avoids re-fetching the same
//!   page within one process lifetime.
//! - Output is capped (default 25_000 chars) so the tool result fits in
//!   the orchestrator's truncation budget. The truncation marker tells
//!   the model what was lost.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use regex::Regex;

use crate::safety::UrlPolicy;
use crate::tool::{
    Concurrency, Mutability, SideEffects, Tool, ToolError, ToolMetadata, ToolParam, ToolResult,
    ToolSchema,
};

const DEFAULT_MAX_CHARS: usize = 25_000;
const REQUEST_TIMEOUT_SECS: u64 = 20;
const USER_AGENT: &str = concat!("takka-cli/", env!("CARGO_PKG_VERSION"));

/// Process-wide URL cache. Keyed on the canonicalized URL string.
fn cache() -> &'static Mutex<HashMap<String, String>> {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub struct FetchTool {
    max_chars: usize,
    url_policy: UrlPolicy,
}

impl FetchTool {
    pub fn new() -> Self {
        Self {
            max_chars: DEFAULT_MAX_CHARS,
            url_policy: UrlPolicy::default(),
        }
    }

    pub fn with_max_chars(mut self, max: usize) -> Self {
        self.max_chars = max.max(256);
        self
    }

    /// Attach a URL allowlist policy. By default the policy is
    /// unrestricted (matches pre-existing behavior).
    pub fn with_url_policy(mut self, policy: UrlPolicy) -> Self {
        self.url_policy = policy;
        self
    }
}

impl Default for FetchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for FetchTool {
    fn name(&self) -> &str {
        "fetch"
    }

    fn description(&self) -> &str {
        "Fetch a URL and return its content as cleaned text. HTML is \
         stripped of tags and scripts; plain text is returned as-is. \
         Useful for reading documentation pages or API references during \
         a coding task."
    }

    fn schema(&self) -> ToolSchema {
        let mut params = HashMap::new();
        params.insert(
            "url".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some("Absolute URL to fetch (http/https only).".to_string()),
                default: None,
            },
        );
        params.insert(
            "max_chars".to_string(),
            ToolParam {
                param_type: "number".to_string(),
                description: Some("Maximum characters returned. Defaults to 25000.".to_string()),
                default: Some(serde_json::json!(DEFAULT_MAX_CHARS)),
            },
        );

        ToolSchema {
            name: "fetch".to_string(),
            description: "Fetch a URL and return its content as cleaned text.".to_string(),
            parameters: params,
            required: vec!["url".to_string()],
        }
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult<serde_json::Value> {
        let args_obj = args
            .as_object()
            .ok_or_else(|| ToolError::new("Invalid arguments: expected object"))?;

        let url = args_obj
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new("Missing required parameter: url"))?
            .trim();

        if url.is_empty() {
            return Err(ToolError::new("url must not be empty"));
        }
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(ToolError::new(format!(
                "Only http/https URLs are allowed (got: {})",
                url
            )));
        }

        // Domain allowlist gate. Skipped silently when the policy is
        // unrestricted (default) so existing behavior is preserved.
        if let Some(reason) = self.url_policy.explain_block(url) {
            return Err(ToolError::new(reason));
        }

        let max_chars = args_obj
            .get("max_chars")
            .and_then(|v| v.as_u64())
            .map(|v| (v as usize).max(256))
            .unwrap_or(self.max_chars);

        // Cache hit?
        let mut cached = false;
        let body = {
            let map = cache().lock().unwrap();
            map.get(url).cloned()
        };

        let cleaned = match body {
            Some(b) => {
                cached = true;
                b
            }
            None => {
                let client = reqwest::blocking::Client::builder()
                    .user_agent(USER_AGENT)
                    .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
                    .build()
                    .map_err(|e| ToolError::new(format!("HTTP client error: {}", e)))?;

                let resp = client
                    .get(url)
                    .send()
                    .map_err(|e| ToolError::new(format!("Fetch failed: {}", e)))?;

                let status = resp.status();
                let content_type = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_lowercase();

                if !status.is_success() {
                    return Err(ToolError::new(format!(
                        "Fetch failed: HTTP {} for {}",
                        status, url
                    )));
                }

                let raw = resp
                    .text()
                    .map_err(|e| ToolError::new(format!("Failed to read body: {}", e)))?;

                let cleaned = if is_htmlish(&content_type, &raw) {
                    html_to_text(&raw)
                } else {
                    raw
                };

                cache()
                    .lock()
                    .unwrap()
                    .insert(url.to_string(), cleaned.clone());
                cleaned
            }
        };

        let total = cleaned.len();
        let truncated = total > max_chars;

        // Scan the cleaned body (pre-truncation, but capped at the
        // first 25k chars to bound regex work) for prompt-injection
        // patterns. Done before `cleaned` is consumed by the truncation
        // branch below.
        let scan = {
            let scan_window: &str = if cleaned.len() > 25_000 {
                let mut end = 25_000;
                while end > 0 && !cleaned.is_char_boundary(end) {
                    end -= 1;
                }
                &cleaned[..end]
            } else {
                &cleaned
            };
            crate::safety::scan_injection(scan_window)
        };

        let body_out = if truncated {
            // Slice on a UTF-8 boundary.
            let mut end = max_chars;
            while end > 0 && !cleaned.is_char_boundary(end) {
                end -= 1;
            }
            format!(
                "{}\n\n[truncated: {} chars omitted of {} total]",
                &cleaned[..end],
                total - end,
                total
            )
        } else {
            cleaned
        };

        // When the scan flags something, prepend a short reminder so
        // the model gets a clear cue this content is data not
        // instructions. The structured `prompt_injection_warning`
        // field carries the same info for programmatic consumers.
        let body_out = if scan.is_clean() {
            body_out
        } else {
            format!("{}{}", scan.reminder_prefix(), body_out)
        };

        Ok(serde_json::json!({
            "url": url,
            "content": body_out,
            "total_chars": total,
            "truncated": truncated,
            "cached": cached,
            "prompt_injection_warning": if scan.is_clean() { serde_json::Value::Null } else { serde_json::json!({
                "matches": scan.matches,
                "max_severity": scan.max_severity,
            }) },
        }))
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            mutability: Mutability::ReadOnly,
            concurrency: Concurrency::ParallelSafe,
            idempotent: true,
            risk: 0,
            side_effects: SideEffects::Network,
        }
    }
}

fn is_htmlish(content_type: &str, body: &str) -> bool {
    if content_type.contains("html") || content_type.contains("xml") {
        return true;
    }
    // Heuristic: starts with a doctype or html tag.
    let head: String = body.chars().take(256).collect();
    let lower = head.to_lowercase();
    lower.contains("<html") || lower.contains("<!doctype html")
}

/// Strip HTML to readable text. Order matters:
///   1. Drop entire <script>/<style> blocks (with content).
///   2. Convert `<br>` and block-level closing tags to newlines.
///   3. Strip remaining tags.
///   4. Decode common HTML entities.
///   5. Collapse whitespace.
fn html_to_text(html: &str) -> String {
    static SCRIPT: OnceLock<Regex> = OnceLock::new();
    static STYLE: OnceLock<Regex> = OnceLock::new();
    static NOSCRIPT: OnceLock<Regex> = OnceLock::new();
    static BLOCK_BREAKS: OnceLock<Regex> = OnceLock::new();
    static ANY_TAG: OnceLock<Regex> = OnceLock::new();
    static MULTI_BLANK: OnceLock<Regex> = OnceLock::new();

    // The `regex` crate doesn't support backreferences, so we strip each
    // tag block with its own pattern.
    let script = SCRIPT.get_or_init(|| Regex::new(r"(?is)<script[^>]*>.*?</script>").unwrap());
    let style = STYLE.get_or_init(|| Regex::new(r"(?is)<style[^>]*>.*?</style>").unwrap());
    let noscript =
        NOSCRIPT.get_or_init(|| Regex::new(r"(?is)<noscript[^>]*>.*?</noscript>").unwrap());
    let block_breaks = BLOCK_BREAKS.get_or_init(|| {
        Regex::new(r"(?i)<\s*(br\s*/?|/p|/div|/li|/h[1-6]|/tr|/article|/section)\s*>").unwrap()
    });
    let any_tag = ANY_TAG.get_or_init(|| Regex::new(r"<[^>]+>").unwrap());
    let multi_blank = MULTI_BLANK.get_or_init(|| Regex::new(r"\n{3,}").unwrap());

    let s = script.replace_all(html, "");
    let s = style.replace_all(&s, "");
    let s = noscript.replace_all(&s, "");
    let s = block_breaks.replace_all(&s, "\n");
    let s = any_tag.replace_all(&s, "");

    // Decode the most common entities. (Not exhaustive; intentional.)
    let s = s
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'");

    // Collapse runs of whitespace per line, then collapse 3+ blank lines.
    let lines: Vec<String> = s
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();
    let joined = lines.join("\n");
    let joined = multi_blank.replace_all(&joined, "\n\n");
    joined.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_text_strips_tags_and_scripts() {
        let html = r#"<html><head>
            <style>body { color: red; }</style>
            <script>alert(1);</script>
        </head><body>
            <h1>Title</h1>
            <p>Hello <b>world</b>.</p>
            <p>Another paragraph &amp; more.</p>
        </body></html>"#;
        let out = html_to_text(html);
        assert!(out.contains("Title"));
        assert!(out.contains("Hello world."));
        assert!(out.contains("Another paragraph & more."));
        assert!(!out.contains("<"));
        assert!(!out.contains("alert"));
        assert!(!out.contains("color: red"));
    }

    #[test]
    fn html_to_text_preserves_paragraph_breaks() {
        let html = "<p>first</p><p>second</p>";
        let out = html_to_text(html);
        assert!(out.contains("first"));
        assert!(out.contains("second"));
        // Should be split onto separate lines, not concatenated.
        assert!(out.lines().count() >= 2);
    }

    #[test]
    fn fetch_rejects_non_http_scheme() {
        let tool = FetchTool::new();
        let err = tool
            .execute(serde_json::json!({"url": "file:///etc/passwd"}))
            .expect_err("file:// should be rejected");
        assert!(err.to_string().contains("http/https"));
    }

    #[test]
    fn fetch_rejects_empty_url() {
        let tool = FetchTool::new();
        let err = tool
            .execute(serde_json::json!({"url": "   "}))
            .expect_err("empty url should be rejected");
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn fetch_rejects_url_outside_allowlist() {
        let policy = UrlPolicy::new(vec!["docs.rs".into()], false);
        let tool = FetchTool::new().with_url_policy(policy);
        let err = tool
            .execute(serde_json::json!({"url": "https://example.com/foo"}))
            .expect_err("host outside allowlist should be rejected");
        assert!(err.to_string().contains("allowed_domains"));
    }

    #[test]
    fn fetch_rejects_ip_url_when_block_ip_urls_set() {
        let policy = UrlPolicy::new(vec![], true);
        let tool = FetchTool::new().with_url_policy(policy);
        let err = tool
            .execute(serde_json::json!({"url": "http://192.168.1.1/"}))
            .expect_err("IP URL should be rejected");
        assert!(err.to_string().contains("IP-literal"));
    }

    // Note: the prompt-injection wiring is exercised end-to-end via the
    // injection module's own tests plus the web_search filter test below.
    // Hitting fetch() requires a network call; the injection scan path is
    // stand-alone in `safety::injection`.

    #[test]
    fn is_htmlish_detects_content_type() {
        assert!(is_htmlish("text/html; charset=utf-8", ""));
        assert!(is_htmlish("application/xhtml+xml", ""));
        assert!(!is_htmlish("text/plain", "hello"));
        assert!(is_htmlish("", "<!DOCTYPE html><html>..."));
    }
}
