//! `web_search` tool — search the web and return ranked results.
//!
//! Backends are picked at call-time based on which API key the user has
//! exported in their environment:
//!
//!   1. `TAVILY_API_KEY`       → Tavily Search API
//!   2. `BRAVE_SEARCH_API_KEY` → Brave Search API
//!   3. (no key)               → DuckDuckGo HTML scrape (best-effort)
//!
//! The DuckDuckGo path is a graceful fallback so the tool is always
//! present in the registry. It scrapes `html.duckduckgo.com/html/`,
//! which is intended for non-JS clients but has no SLA. If you depend
//! on web_search in production, set one of the API keys above.
//!
//! Output shape (stable across backends):
//!
//! ```json
//! {
//!   "query": "rust async traits",
//!   "backend": "tavily" | "brave" | "duckduckgo",
//!   "results": [
//!     {"title": "...", "url": "https://...", "snippet": "..."},
//!     ...
//!   ],
//!   "result_count": 5
//! }
//! ```

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use serde::Deserialize;

use crate::safety::UrlPolicy;
use crate::tool::{
    Concurrency, Mutability, SideEffects, Tool, ToolError, ToolMetadata, ToolParam, ToolResult,
    ToolSchema,
};

const DEFAULT_MAX_RESULTS: usize = 5;
const HARD_RESULT_CAP: usize = 20;
const REQUEST_TIMEOUT_SECS: u64 = 20;
const USER_AGENT: &str = concat!("takka-cli/", env!("CARGO_PKG_VERSION"));

/// Backend selected at call-time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Tavily,
    Brave,
    DuckDuckGo,
}

impl Backend {
    fn detect() -> Self {
        if std::env::var("TAVILY_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            Backend::Tavily
        } else if std::env::var("BRAVE_SEARCH_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            Backend::Brave
        } else {
            Backend::DuckDuckGo
        }
    }

    fn id(self) -> &'static str {
        match self {
            Backend::Tavily => "tavily",
            Backend::Brave => "brave",
            Backend::DuckDuckGo => "duckduckgo",
        }
    }
}

/// One search result.
#[derive(Debug, Clone)]
struct SearchHit {
    title: String,
    url: String,
    snippet: String,
}

pub struct WebSearchTool {
    max_results: usize,
    url_policy: UrlPolicy,
}

impl WebSearchTool {
    pub fn new() -> Self {
        Self {
            max_results: DEFAULT_MAX_RESULTS,
            url_policy: UrlPolicy::default(),
        }
    }

    pub fn with_max_results(mut self, n: usize) -> Self {
        self.max_results = n.clamp(1, HARD_RESULT_CAP);
        self
    }

    /// Attach a URL allowlist policy. Result URLs whose host is not
    /// permitted are dropped from the response so the model never
    /// reasons about a URL it can't open. Default is unrestricted.
    pub fn with_url_policy(mut self, policy: UrlPolicy) -> Self {
        self.url_policy = policy;
        self
    }
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web and return ranked results (title, URL, snippet). \
         Uses Tavily or Brave when the corresponding API key is set, \
         else falls back to a DuckDuckGo scrape. Pair with `fetch` to \
         pull full page content."
    }

    fn schema(&self) -> ToolSchema {
        let mut params = HashMap::new();
        params.insert(
            "query".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some("Search query.".to_string()),
                default: None,
            },
        );
        params.insert(
            "max_results".to_string(),
            ToolParam {
                param_type: "number".to_string(),
                description: Some(format!(
                    "Maximum number of results to return (1..={}). Defaults to {}.",
                    HARD_RESULT_CAP, DEFAULT_MAX_RESULTS
                )),
                default: Some(serde_json::json!(DEFAULT_MAX_RESULTS)),
            },
        );

        ToolSchema {
            name: "web_search".to_string(),
            description: "Search the web and return ranked results.".to_string(),
            parameters: params,
            required: vec!["query".to_string()],
        }
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult<serde_json::Value> {
        let args_obj = args
            .as_object()
            .ok_or_else(|| ToolError::new("Invalid arguments: expected object"))?;

        let query = args_obj
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new("Missing required parameter: query"))?
            .trim();

        if query.is_empty() {
            return Err(ToolError::new("query must not be empty"));
        }

        let max_results = args_obj
            .get("max_results")
            .and_then(|v| v.as_u64())
            .map(|v| (v as usize).clamp(1, HARD_RESULT_CAP))
            .unwrap_or(self.max_results);

        let backend = Backend::detect();

        let client = reqwest::blocking::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|e| ToolError::new(format!("HTTP client error: {}", e)))?;

        let hits = match backend {
            Backend::Tavily => search_tavily(&client, query, max_results)?,
            Backend::Brave => search_brave(&client, query, max_results)?,
            Backend::DuckDuckGo => search_duckduckgo(&client, query, max_results)?,
        };

        // Filter result URLs through the allowlist. When the policy is
        // unrestricted this is a no-op. We track how many were dropped
        // so the model knows the response was filtered.
        let total_before_filter = hits.len();
        let hits = filter_hits_by_policy(hits, &self.url_policy);
        let filtered_out = total_before_filter - hits.len();

        // Run a prompt-injection scan over each result's title +
        // snippet. We don't pull the full page (that's `fetch`'s job),
        // but the snippets themselves are a known injection vector
        // (e.g. SEO-poisoned pages whose excerpt contains "ignore
        // previous instructions…"). Per-hit warnings are attached so
        // the model gets a localized cue, plus an aggregate summary.
        let mut hit_warnings: Vec<serde_json::Value> = Vec::new();
        let mut max_severity: f32 = 0.0;
        let scanned: Vec<(SearchHit, crate::safety::InjectionScan)> = hits
            .into_iter()
            .map(|h| {
                let combined = format!("{}\n{}", h.title, h.snippet);
                let scan = crate::safety::scan_injection(&combined);
                if scan.max_severity > max_severity {
                    max_severity = scan.max_severity;
                }
                (h, scan)
            })
            .collect();

        let results: Vec<serde_json::Value> = scanned
            .iter()
            .map(|(h, scan)| {
                let mut obj = serde_json::json!({
                    "title": h.title,
                    "url": h.url,
                    "snippet": h.snippet,
                });
                if !scan.is_clean() {
                    obj["prompt_injection_warning"] = serde_json::json!({
                        "matches": scan.matches.clone(),
                        "max_severity": scan.max_severity,
                    });
                    hit_warnings.push(serde_json::json!({
                        "url": h.url,
                        "matches": scan.matches.clone(),
                    }));
                }
                obj
            })
            .collect();

        let result_count = results.len();

        Ok(serde_json::json!({
            "query": query,
            "backend": backend.id(),
            "results": results,
            "result_count": result_count,
            "filtered_by_allowlist": filtered_out,
            "prompt_injection_warning": if hit_warnings.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::json!({
                    "affected_hits": hit_warnings.len(),
                    "max_severity": max_severity,
                    "hits": hit_warnings,
                })
            },
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

// ── Backends ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TavilyResponse {
    #[serde(default)]
    results: Vec<TavilyResult>,
}

#[derive(Debug, Deserialize)]
struct TavilyResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    content: String,
}

fn search_tavily(
    client: &reqwest::blocking::Client,
    query: &str,
    max_results: usize,
) -> ToolResult<Vec<SearchHit>> {
    let api_key =
        std::env::var("TAVILY_API_KEY").map_err(|_| ToolError::new("TAVILY_API_KEY missing"))?;

    let body = serde_json::json!({
        "api_key": api_key,
        "query": query,
        "max_results": max_results,
    });

    let resp = client
        .post("https://api.tavily.com/search")
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .map_err(|e| ToolError::new(format!("Tavily request failed: {}", e)))?;

    if !resp.status().is_success() {
        return Err(ToolError::new(format!(
            "Tavily returned HTTP {}",
            resp.status()
        )));
    }

    let parsed: TavilyResponse = resp
        .json()
        .map_err(|e| ToolError::new(format!("Tavily response parse error: {}", e)))?;

    Ok(parsed
        .results
        .into_iter()
        .take(max_results)
        .map(|r| SearchHit {
            title: r.title,
            url: r.url,
            snippet: r.content,
        })
        .collect())
}

#[derive(Debug, Deserialize)]
struct BraveResponse {
    #[serde(default)]
    web: BraveWeb,
}

#[derive(Debug, Default, Deserialize)]
struct BraveWeb {
    #[serde(default)]
    results: Vec<BraveResult>,
}

#[derive(Debug, Deserialize)]
struct BraveResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    description: String,
}

fn search_brave(
    client: &reqwest::blocking::Client,
    query: &str,
    max_results: usize,
) -> ToolResult<Vec<SearchHit>> {
    let api_key = std::env::var("BRAVE_SEARCH_API_KEY")
        .map_err(|_| ToolError::new("BRAVE_SEARCH_API_KEY missing"))?;

    let resp = client
        .get("https://api.search.brave.com/res/v1/web/search")
        .header("X-Subscription-Token", api_key)
        .header("Accept", "application/json")
        .query(&[("q", query), ("count", &max_results.to_string())])
        .send()
        .map_err(|e| ToolError::new(format!("Brave request failed: {}", e)))?;

    if !resp.status().is_success() {
        return Err(ToolError::new(format!(
            "Brave returned HTTP {}",
            resp.status()
        )));
    }

    let parsed: BraveResponse = resp
        .json()
        .map_err(|e| ToolError::new(format!("Brave response parse error: {}", e)))?;

    Ok(parsed
        .web
        .results
        .into_iter()
        .take(max_results)
        .map(|r| SearchHit {
            title: r.title,
            url: r.url,
            snippet: r.description,
        })
        .collect())
}

/// Apply a URL allowlist policy to a list of search hits. Hits whose
/// `url` is not permitted are dropped. When the policy is unrestricted
/// the input is returned unchanged.
fn filter_hits_by_policy(hits: Vec<SearchHit>, policy: &UrlPolicy) -> Vec<SearchHit> {
    if policy.is_unrestricted() {
        return hits;
    }
    hits.into_iter()
        .filter(|h| policy.is_allowed(&h.url))
        .collect()
}

fn search_duckduckgo(
    client: &reqwest::blocking::Client,
    query: &str,
    max_results: usize,
) -> ToolResult<Vec<SearchHit>> {
    // The HTML endpoint is the no-JS version; result markup is more
    // stable than the main domain.
    let resp = client
        .get("https://html.duckduckgo.com/html/")
        .query(&[("q", query)])
        .send()
        .map_err(|e| ToolError::new(format!("DuckDuckGo request failed: {}", e)))?;

    if !resp.status().is_success() {
        return Err(ToolError::new(format!(
            "DuckDuckGo returned HTTP {}",
            resp.status()
        )));
    }

    let html = resp
        .text()
        .map_err(|e| ToolError::new(format!("Failed to read DuckDuckGo body: {}", e)))?;

    Ok(parse_duckduckgo_html(&html, max_results))
}

/// Best-effort parser for the duckduckgo HTML results page.
///
/// Each result block looks roughly like:
///
/// ```html
/// <div class="result results_links ...">
///   <h2 class="result__title">
///     <a class="result__a" href="...">Title</a>
///   </h2>
///   <a class="result__snippet" href="...">Snippet text</a>
/// </div>
/// ```
///
/// We scan for `result__a` (title + url) and `result__snippet` (snippet)
/// in document order, then zip them. Anything we can't parse cleanly is
/// dropped.
fn parse_duckduckgo_html(html: &str, max_results: usize) -> Vec<SearchHit> {
    static TITLE_RE: OnceLock<Regex> = OnceLock::new();
    static SNIPPET_RE: OnceLock<Regex> = OnceLock::new();

    // (?is) = case-insensitive, dotall.
    let title_re = TITLE_RE.get_or_init(|| {
        Regex::new(
            r#"(?is)<a[^>]*class="[^"]*\bresult__a\b[^"]*"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#,
        )
        .unwrap()
    });
    let snippet_re = SNIPPET_RE.get_or_init(|| {
        Regex::new(r#"(?is)<a[^>]*class="[^"]*\bresult__snippet\b[^"]*"[^>]*>(.*?)</a>"#).unwrap()
    });

    let mut titles: Vec<(String, String)> = title_re
        .captures_iter(html)
        .filter_map(|c| {
            let raw_url = c.get(1)?.as_str().to_string();
            let title = strip_tags_and_decode(c.get(2)?.as_str());
            let url = unwrap_ddg_redirect(&raw_url);
            if url.is_empty() || title.is_empty() {
                None
            } else {
                Some((title, url))
            }
        })
        .collect();

    let snippets: Vec<String> = snippet_re
        .captures_iter(html)
        .filter_map(|c| Some(strip_tags_and_decode(c.get(1)?.as_str())))
        .collect();

    titles.truncate(max_results);

    titles
        .into_iter()
        .enumerate()
        .map(|(i, (title, url))| SearchHit {
            title,
            url,
            snippet: snippets.get(i).cloned().unwrap_or_default(),
        })
        .collect()
}

/// DuckDuckGo wraps result links in `/l/?uddg=<urlencoded>`. Pull the
/// real URL out of that wrapper. Accepts protocol-relative `//host/...`
/// inputs by promoting them to https.
fn unwrap_ddg_redirect(href: &str) -> String {
    let normalized = if let Some(stripped) = href.strip_prefix("//") {
        format!("https://{}", stripped)
    } else {
        href.to_string()
    };

    // Look for `uddg=` query param.
    if let Some(idx) = normalized.find("uddg=") {
        let tail = &normalized[idx + 5..];
        let end = tail.find('&').unwrap_or(tail.len());
        let encoded = &tail[..end];
        if let Some(decoded) = url_decode(encoded) {
            return decoded;
        }
    }
    normalized
}

/// Minimal `application/x-www-form-urlencoded` decoder. Returns None if
/// the input contains an invalid `%XX` sequence.
fn url_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16)?;
                let lo = (bytes[i + 2] as char).to_digit(16)?;
                out.push(((hi << 4) | lo) as u8);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Strip any leftover HTML tags, decode the most common entities, and
/// collapse whitespace.
fn strip_tags_and_decode(s: &str) -> String {
    static TAG_RE: OnceLock<Regex> = OnceLock::new();
    let tag_re = TAG_RE.get_or_init(|| Regex::new(r"<[^>]+>").unwrap());
    let stripped = tag_re.replace_all(s, "");
    let decoded = stripped
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'");
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_query() {
        let tool = WebSearchTool::new();
        let err = tool
            .execute(serde_json::json!({"query": "   "}))
            .expect_err("empty query");
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn missing_query_param_errors() {
        let tool = WebSearchTool::new();
        let err = tool
            .execute(serde_json::json!({}))
            .expect_err("missing query");
        assert!(err.to_string().contains("query"));
    }

    #[test]
    fn unwrap_ddg_redirect_decodes_uddg_param() {
        let href = "/l/?kh=-1&uddg=https%3A%2F%2Fexample.com%2Fpath";
        assert_eq!(unwrap_ddg_redirect(href), "https://example.com/path");
    }

    #[test]
    fn unwrap_ddg_redirect_promotes_protocol_relative() {
        let href = "//example.com/foo";
        assert_eq!(unwrap_ddg_redirect(href), "https://example.com/foo");
    }

    #[test]
    fn url_decode_handles_basic_cases() {
        assert_eq!(url_decode("a+b").as_deref(), Some("a b"));
        assert_eq!(url_decode("a%20b").as_deref(), Some("a b"));
        assert_eq!(url_decode("100%25").as_deref(), Some("100%"));
        assert_eq!(url_decode("%ZZ"), None);
    }

    #[test]
    fn strip_tags_and_decode_collapses() {
        let s = "Hello&nbsp;<b>world</b>&#39;s &amp; more";
        assert_eq!(strip_tags_and_decode(s), "Hello world's & more");
    }

    #[test]
    fn parse_duckduckgo_html_extracts_results() {
        // Synthetic snippet matching the live DOM shape closely enough to
        // exercise the regexes.
        let html = r##"
            <div class="result">
              <h2 class="result__title">
                <a class="result__a" href="/l/?uddg=https%3A%2F%2Fa.example%2Fpage1">First result</a>
              </h2>
              <a class="result__snippet" href="https://a.example/page1">First snippet here</a>
            </div>
            <div class="result">
              <h2 class="result__title">
                <a class="result__a" href="/l/?uddg=https%3A%2F%2Fb.example%2Fpage2">Second &amp; result</a>
              </h2>
              <a class="result__snippet" href="https://b.example/page2">Second snippet</a>
            </div>
        "##;
        let hits = parse_duckduckgo_html(html, 5);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "First result");
        assert_eq!(hits[0].url, "https://a.example/page1");
        assert_eq!(hits[0].snippet, "First snippet here");
        assert_eq!(hits[1].title, "Second & result");
        assert_eq!(hits[1].url, "https://b.example/page2");
        assert_eq!(hits[1].snippet, "Second snippet");
    }

    #[test]
    fn parse_duckduckgo_html_truncates_to_max_results() {
        let mut html = String::new();
        for i in 0..10 {
            html.push_str(&format!(
                r##"<div class="result">
                  <a class="result__a" href="/l/?uddg=https%3A%2F%2Fe.example%2Fp{i}">Title {i}</a>
                  <a class="result__snippet" href="x">Snippet {i}</a>
                </div>"##
            ));
        }
        let hits = parse_duckduckgo_html(&html, 3);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[2].title, "Title 2");
    }

    #[test]
    fn backend_detect_falls_back_to_duckduckgo() {
        // We can't safely mutate process env in parallel tests, so just
        // assert the default mapping shape.
        let id = Backend::DuckDuckGo.id();
        assert_eq!(id, "duckduckgo");
        assert_eq!(Backend::Tavily.id(), "tavily");
        assert_eq!(Backend::Brave.id(), "brave");
    }

    #[test]
    fn filter_hits_by_policy_unrestricted_keeps_all() {
        let hits = vec![
            SearchHit {
                title: "a".into(),
                url: "https://a.example/".into(),
                snippet: "".into(),
            },
            SearchHit {
                title: "b".into(),
                url: "https://b.example/".into(),
                snippet: "".into(),
            },
        ];
        let kept = filter_hits_by_policy(hits, &UrlPolicy::default());
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn filter_hits_by_policy_drops_disallowed() {
        let hits = vec![
            SearchHit {
                title: "keep".into(),
                url: "https://docs.rs/page".into(),
                snippet: "".into(),
            },
            SearchHit {
                title: "drop".into(),
                url: "https://example.com/page".into(),
                snippet: "".into(),
            },
            SearchHit {
                title: "keep-sub".into(),
                url: "https://api.docs.rs/page".into(),
                snippet: "".into(),
            },
        ];
        let policy = UrlPolicy::new(vec!["docs.rs".into()], false);
        let kept = filter_hits_by_policy(hits, &policy);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].title, "keep");
        assert_eq!(kept[1].title, "keep-sub");
    }
}
