//! `run_tests` tool — auto-detect the project's test runner and run it.
//!
//! What this is:
//! - A convenience wrapper that picks the right test command for the
//!   current working directory by sniffing project files (`Cargo.toml`,
//!   `package.json`, `pyproject.toml`, `go.mod`, …).
//! - Returns structured output (passed / failed / duration / stdout)
//!   so the model can act on the result without re-parsing arbitrary
//!   shell text.
//!
//! What this is NOT:
//! - A test runner. We just dispatch to whatever's installed.
//! - A coverage reporter.
//! - A test selector — we run the project's default suite. The
//!   `filter` argument is forwarded as-is to the underlying runner
//!   when supported.
//!
//! Detection priority (first match wins):
//!   - `Cargo.toml`     → `cargo test [filter]`
//!   - `package.json`   → `npm test -- [filter]` (or `pnpm`/`yarn` if their
//!     lockfile is present)
//!   - `pyproject.toml` / `setup.py` / `pytest.ini` / `tests/`
//!     → `pytest [filter]`
//!   - `go.mod`         → `go test ./... [filter]`
//!   - none of the above → return an `Unknown` error so the agent can
//!     fall back to `run_command` with an explicit
//!     test command.
//!
//! Output is capped at 200 lines / 25k chars to keep the tool result
//! within the orchestrator's truncation budget. Long suites lose head
//! and tail context; the failure section (if any) is preserved.
//!
//! Safety: the underlying call is `run_command`-equivalent. The tool's
//! `is_read_only` is `false` because tests can run side-effecting
//! code. Path stays within `cwd` unless the user passes a `workdir`.
//!
//! Usage shape (model side):
//! ```json
//! { "filter": "search string", "workdir": "subdir/" }
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::tool::{
    Concurrency, Mutability, SideEffects, Tool, ToolError, ToolMetadata, ToolParam, ToolResult,
    ToolSchema,
};

const DEFAULT_TIMEOUT_SECS: u64 = 600;
const MAX_OUTPUT_CHARS: usize = 25_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Runner {
    CargoTest,
    Pytest,
    NpmTest { variant: NpmVariant },
    GoTest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NpmVariant {
    Npm,
    Pnpm,
    Yarn,
}

impl Runner {
    fn as_str(self) -> &'static str {
        match self {
            Runner::CargoTest => "cargo test",
            Runner::Pytest => "pytest",
            Runner::NpmTest {
                variant: NpmVariant::Npm,
            } => "npm test",
            Runner::NpmTest {
                variant: NpmVariant::Pnpm,
            } => "pnpm test",
            Runner::NpmTest {
                variant: NpmVariant::Yarn,
            } => "yarn test",
            Runner::GoTest => "go test",
        }
    }
}

pub struct RunTestsTool {
    timeout_secs: u64,
}

impl RunTestsTool {
    pub fn new() -> Self {
        Self {
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }

    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        self.timeout_secs = secs.max(5);
        self
    }
}

impl Default for RunTestsTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for RunTestsTool {
    fn name(&self) -> &str {
        "run_tests"
    }

    fn description(&self) -> &str {
        "Auto-detect the project's test runner and run the test suite. \
         Sniffs Cargo.toml / package.json / pyproject.toml / go.mod in \
         the working directory and dispatches to the right command \
         (cargo test, pytest, npm/pnpm/yarn test, go test). Returns \
         structured pass/fail counts plus stdout, capped at 25k chars. \
         Use this in preference to run_command for test invocations so \
         the project's conventions are respected."
    }

    fn schema(&self) -> ToolSchema {
        let mut params = HashMap::new();
        params.insert(
            "filter".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some(
                    "Optional filter string passed to the underlying \
                     runner (cargo test <filter>, pytest <filter>, \
                     npm test -- <filter>). Empty = run all."
                        .into(),
                ),
                default: None,
            },
        );
        params.insert(
            "workdir".to_string(),
            ToolParam {
                param_type: "string".to_string(),
                description: Some(
                    "Optional working directory. Defaults to the agent's \
                     cwd."
                        .into(),
                ),
                default: None,
            },
        );

        ToolSchema {
            name: "run_tests".to_string(),
            description: "Auto-detect and run the project test suite.".to_string(),
            parameters: params,
            required: Vec::new(),
        }
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult<serde_json::Value> {
        let obj = args.as_object();
        let filter = obj
            .and_then(|o| o.get("filter"))
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let workdir = obj
            .and_then(|o| o.get("workdir"))
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let cwd = match workdir.as_deref() {
            Some(p) => PathBuf::from(p),
            None => std::env::current_dir()
                .map_err(|e| ToolError::new(format!("cwd unavailable: {}", e)))?,
        };

        let runner = detect_runner(&cwd).ok_or_else(|| {
            ToolError::new(
                "Could not detect a test runner. Looked for Cargo.toml, \
                 package.json, pyproject.toml, go.mod. Use run_command \
                 with an explicit test command instead.",
            )
        })?;

        let mut cmd = build_command(runner, filter.as_deref());
        cmd.current_dir(&cwd);

        let started = std::time::Instant::now();
        let output = cmd
            .output()
            .map_err(|e| ToolError::new(format!("Failed to spawn {}: {}", runner.as_str(), e)))?;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        // We bail early when the spawn fails outright; cmd.output() returns
        // Ok even when the test process exits non-zero. Failure parsing
        // happens against the captured stdout/stderr below.
        if elapsed_ms > self.timeout_secs * 1000 {
            // The blocking output() call doesn't enforce timeouts; this
            // is informational only. Heavy timeouts would require a
            // child-process-with-kill helper which is out of scope.
            tracing::warn!(elapsed_ms, "run_tests exceeded soft timeout");
        }

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let combined = format!("{}{}", stdout, stderr);
        let summary = parse_summary(runner, &combined);
        let truncated_output = truncate_for_model(&combined, MAX_OUTPUT_CHARS);

        Ok(serde_json::json!({
            "runner": runner.as_str(),
            "command": format!("{:?}", cmd),
            "exit_code": output.status.code(),
            "success": output.status.success(),
            "elapsed_ms": elapsed_ms,
            "passed": summary.passed,
            "failed": summary.failed,
            "ignored": summary.ignored,
            "output": truncated_output,
            "output_truncated": combined.len() > MAX_OUTPUT_CHARS,
        }))
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            mutability: Mutability::Mutating,
            concurrency: Concurrency::Exclusive,
            idempotent: false,
            risk: 30,
            side_effects: SideEffects::Shell,
        }
    }
}

// ── Detection ───────────────────────────────────────────────────────────

fn detect_runner(cwd: &Path) -> Option<Runner> {
    if cwd.join("Cargo.toml").is_file() {
        return Some(Runner::CargoTest);
    }
    if cwd.join("package.json").is_file() {
        let variant = if cwd.join("pnpm-lock.yaml").is_file() {
            NpmVariant::Pnpm
        } else if cwd.join("yarn.lock").is_file() {
            NpmVariant::Yarn
        } else {
            NpmVariant::Npm
        };
        return Some(Runner::NpmTest { variant });
    }
    if cwd.join("pyproject.toml").is_file()
        || cwd.join("pytest.ini").is_file()
        || cwd.join("setup.py").is_file()
        || cwd.join("tests").is_dir()
    {
        return Some(Runner::Pytest);
    }
    if cwd.join("go.mod").is_file() {
        return Some(Runner::GoTest);
    }
    None
}

fn build_command(runner: Runner, filter: Option<&str>) -> Command {
    match runner {
        Runner::CargoTest => {
            let mut c = Command::new("cargo");
            c.arg("test");
            if let Some(f) = filter {
                c.arg(f);
            }
            c
        }
        Runner::Pytest => {
            let mut c = Command::new("pytest");
            if let Some(f) = filter {
                c.arg("-k").arg(f);
            }
            c
        }
        Runner::NpmTest { variant } => {
            let prog = match variant {
                NpmVariant::Npm => "npm",
                NpmVariant::Pnpm => "pnpm",
                NpmVariant::Yarn => "yarn",
            };
            let mut c = Command::new(prog);
            c.arg("test");
            if let Some(f) = filter {
                // npm forwards args after `--`; pnpm/yarn accept them
                // directly. We always emit `--` to be safe.
                c.arg("--").arg(f);
            }
            c
        }
        Runner::GoTest => {
            let mut c = Command::new("go");
            c.arg("test").arg("./...");
            if let Some(f) = filter {
                c.arg("-run").arg(f);
            }
            c
        }
    }
}

// ── Summary parsing ─────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, Copy)]
struct Summary {
    passed: u32,
    failed: u32,
    ignored: u32,
}

/// Best-effort summary parser. We don't try to be exhaustive — we just
/// extract enough to surface "X passed, Y failed" to the model so it
/// can decide what to do next. Exit code remains the source of truth.
fn parse_summary(runner: Runner, output: &str) -> Summary {
    match runner {
        Runner::CargoTest => parse_cargo_test(output),
        Runner::Pytest => parse_pytest(output),
        Runner::NpmTest { .. } => parse_npm_test(output),
        Runner::GoTest => parse_go_test(output),
    }
}

/// Cargo test format:
///   `test result: ok. 5 passed; 0 failed; 1 ignored; ...`
///   `test result: FAILED. 5 passed; 1 failed; ...`
/// Multiple test binaries → multiple lines; we sum them.
fn parse_cargo_test(output: &str) -> Summary {
    let re = match regex::Regex::new(
        r"test result: (?:ok|FAILED)\.\s+(\d+)\s+passed;\s+(\d+)\s+failed;\s+(\d+)\s+ignored",
    ) {
        Ok(r) => r,
        Err(_) => return Summary::default(),
    };
    let mut s = Summary::default();
    for caps in re.captures_iter(output) {
        s.passed += caps[1].parse::<u32>().unwrap_or(0);
        s.failed += caps[2].parse::<u32>().unwrap_or(0);
        s.ignored += caps[3].parse::<u32>().unwrap_or(0);
    }
    s
}

/// Pytest summary: `=== 12 passed, 2 failed, 1 skipped in 1.23s ===`
fn parse_pytest(output: &str) -> Summary {
    let mut s = Summary::default();
    let re = match regex::Regex::new(r"(\d+)\s+(passed|failed|skipped|error|errors)") {
        Ok(r) => r,
        Err(_) => return s,
    };
    for caps in re.captures_iter(output) {
        let n = caps[1].parse::<u32>().unwrap_or(0);
        match &caps[2] {
            "passed" => s.passed += n,
            "failed" | "error" | "errors" => s.failed += n,
            "skipped" => s.ignored += n,
            _ => {}
        }
    }
    s
}

/// npm/yarn/pnpm `test` typically delegates to jest/vitest/mocha. The
/// most common summary is jest's:
///   `Tests:       1 failed, 5 passed, 6 total`
fn parse_npm_test(output: &str) -> Summary {
    let mut s = Summary::default();
    let re =
        match regex::Regex::new(r"(\d+)\s+(passed|passing|failed|failing|skipped|pending|todo)") {
            Ok(r) => r,
            Err(_) => return s,
        };
    for caps in re.captures_iter(output) {
        let n = caps[1].parse::<u32>().unwrap_or(0);
        match &caps[2] {
            "passed" | "passing" => s.passed += n,
            "failed" | "failing" => s.failed += n,
            "skipped" | "pending" | "todo" => s.ignored += n,
            _ => {}
        }
    }
    s
}

/// `go test` per-package summary: `--- PASS: TestFoo (0.01s)` and
/// `--- FAIL: TestBar (...)`. We also accept the bottom-line `ok` /
/// `FAIL` markers but they don't carry counts.
fn parse_go_test(output: &str) -> Summary {
    let mut s = Summary::default();
    for line in output.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("--- PASS") {
            s.passed += 1;
        } else if trimmed.starts_with("--- FAIL") {
            s.failed += 1;
        } else if trimmed.starts_with("--- SKIP") {
            s.ignored += 1;
        }
    }
    s
}

// ── Output truncation ───────────────────────────────────────────────────

/// When test output exceeds the cap, keep the head + tail so the failure
/// summary at the bottom of typical runners isn't cut off. The middle is
/// replaced with a marker. We respect UTF-8 boundaries.
fn truncate_for_model(s: &str, max_chars: usize) -> String {
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
    use std::fs;

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("agentic-runtests-{}-{}-{}", pid, nanos, name));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn detect_cargo_when_cargo_toml_present() {
        let d = temp_dir("cargo");
        fs::write(d.join("Cargo.toml"), "").unwrap();
        assert_eq!(detect_runner(&d), Some(Runner::CargoTest));
    }

    #[test]
    fn detect_pytest_via_pyproject() {
        let d = temp_dir("py-pyproject");
        fs::write(d.join("pyproject.toml"), "").unwrap();
        assert_eq!(detect_runner(&d), Some(Runner::Pytest));
    }

    #[test]
    fn detect_pytest_via_tests_dir() {
        let d = temp_dir("py-tests");
        fs::create_dir_all(d.join("tests")).unwrap();
        assert_eq!(detect_runner(&d), Some(Runner::Pytest));
    }

    #[test]
    fn detect_go_test() {
        let d = temp_dir("go");
        fs::write(d.join("go.mod"), "").unwrap();
        assert_eq!(detect_runner(&d), Some(Runner::GoTest));
    }

    #[test]
    fn detect_npm_default() {
        let d = temp_dir("npm");
        fs::write(d.join("package.json"), "{}").unwrap();
        assert_eq!(
            detect_runner(&d),
            Some(Runner::NpmTest {
                variant: NpmVariant::Npm
            })
        );
    }

    #[test]
    fn detect_pnpm_via_lockfile() {
        let d = temp_dir("pnpm");
        fs::write(d.join("package.json"), "{}").unwrap();
        fs::write(d.join("pnpm-lock.yaml"), "").unwrap();
        assert_eq!(
            detect_runner(&d),
            Some(Runner::NpmTest {
                variant: NpmVariant::Pnpm
            })
        );
    }

    #[test]
    fn detect_yarn_via_lockfile() {
        let d = temp_dir("yarn");
        fs::write(d.join("package.json"), "{}").unwrap();
        fs::write(d.join("yarn.lock"), "").unwrap();
        assert_eq!(
            detect_runner(&d),
            Some(Runner::NpmTest {
                variant: NpmVariant::Yarn
            })
        );
    }

    #[test]
    fn detect_returns_none_for_empty_dir() {
        let d = temp_dir("empty");
        assert_eq!(detect_runner(&d), None);
    }

    #[test]
    fn detect_priority_cargo_beats_other_files() {
        let d = temp_dir("priority");
        fs::write(d.join("Cargo.toml"), "").unwrap();
        fs::write(d.join("package.json"), "{}").unwrap();
        fs::write(d.join("go.mod"), "").unwrap();
        // Cargo wins (top of detect_runner's match order).
        assert_eq!(detect_runner(&d), Some(Runner::CargoTest));
    }

    #[test]
    fn parse_cargo_test_simple() {
        let s = "test result: ok. 5 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out";
        let r = parse_cargo_test(s);
        assert_eq!(r.passed, 5);
        assert_eq!(r.failed, 0);
        assert_eq!(r.ignored, 1);
    }

    #[test]
    fn parse_cargo_test_failure() {
        let s = "test result: FAILED. 3 passed; 2 failed; 0 ignored; 0 measured";
        let r = parse_cargo_test(s);
        assert_eq!(r.passed, 3);
        assert_eq!(r.failed, 2);
    }

    #[test]
    fn parse_cargo_test_sums_multiple_binaries() {
        let s = "test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n\
                 test result: ok. 3 passed; 1 failed; 2 ignored; 0 measured; 0 filtered out";
        let r = parse_cargo_test(s);
        assert_eq!(r.passed, 8);
        assert_eq!(r.failed, 1);
        assert_eq!(r.ignored, 2);
    }

    #[test]
    fn parse_pytest_summary() {
        let s = "============== 12 passed, 2 failed, 1 skipped in 1.23s ==============";
        let r = parse_pytest(s);
        assert_eq!(r.passed, 12);
        assert_eq!(r.failed, 2);
        assert_eq!(r.ignored, 1);
    }

    #[test]
    fn parse_npm_jest_summary() {
        let s = "Tests:       1 failed, 5 passed, 6 total";
        let r = parse_npm_test(s);
        assert_eq!(r.passed, 5);
        assert_eq!(r.failed, 1);
    }

    #[test]
    fn parse_npm_mocha_summary() {
        let s = "  10 passing (1.2s)\n  2 failing\n  3 pending\n";
        let r = parse_npm_test(s);
        assert_eq!(r.passed, 10);
        assert_eq!(r.failed, 2);
        assert_eq!(r.ignored, 3);
    }

    #[test]
    fn parse_go_test_summary() {
        let s = "--- PASS: TestA (0.01s)\n--- FAIL: TestB (0.02s)\n--- PASS: TestC (0.00s)";
        let r = parse_go_test(s);
        assert_eq!(r.passed, 2);
        assert_eq!(r.failed, 1);
    }

    #[test]
    fn truncate_keeps_head_and_tail_for_oversize_input() {
        let body = "a".repeat(50_000);
        let out = truncate_for_model(&body, 1000);
        assert!(out.len() < body.len());
        assert!(out.contains("chars omitted"));
        // The very-first and very-last chars are still there.
        assert!(out.starts_with("a"));
        assert!(out.ends_with("a"));
    }

    #[test]
    fn truncate_passes_through_short_input() {
        let body = "small output";
        assert_eq!(truncate_for_model(body, 1000), body);
    }

    #[test]
    fn execute_returns_error_when_no_runner_detected() {
        let d = temp_dir("none");
        let tool = RunTestsTool::new();
        let err = tool
            .execute(serde_json::json!({"workdir": d.to_string_lossy()}))
            .expect_err("should fail");
        assert!(err.to_string().contains("Could not detect"));
    }
}
