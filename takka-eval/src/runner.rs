//! Parent side of an eval run: prepare a workdir, spawn the agent child,
//! verify the outcome independently of the agent, and collect metrics.

use crate::child::{ChildOutcome, ProviderSource};
use crate::metrics::{self, Metrics};
use crate::task::{ExpectSpec, Task};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use takka_core::runtime::pipe_id::fnv1a64;

const VERIFY_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Replay,
    Live,
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub lane: Lane,
    /// Binary to re-exec as the agent child (normally `current_exe`).
    pub exe: PathBuf,
    pub model: Option<String>,
    /// Config file for the live lane.
    pub config: Option<PathBuf>,
    /// Per-run USD ceiling for tasks that do not set `max_cost_usd`.
    pub default_max_cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    pub id: String,
    pub tags: Vec<String>,
    pub passed: bool,
    /// Why the task failed, when it did.
    pub failure: Option<String>,
    pub verify_exit: Option<i32>,
    pub agent_error: Option<String>,
    /// Modified files that were declared off-limits.
    pub forbidden_edits: Vec<String>,
    pub files_changed: usize,
    pub wall_ms: u64,
    pub model: String,
    pub metrics: Metrics,
    /// How many times the task ran (`--n`). Older result files omit it.
    #[serde(default = "one")]
    pub runs: u32,
    /// How many of those runs passed. `passed` is the majority outcome.
    #[serde(default)]
    pub pass_count: u32,
}

fn one() -> u32 {
    1
}

pub fn run_task(task: &Task, opts: &RunOptions) -> Result<TaskResult> {
    let tmp = tempfile::tempdir().context("creating temp dir")?;
    let work = tmp.path().join("work");
    copy_dir(&task.repo_dir(), &work)
        .with_context(|| format!("copying fixture {}", task.repo_dir().display()))?;
    let out_file = tmp.path().join("outcome.json");

    let source = match opts.lane {
        Lane::Replay => {
            let c = task.cassette_path();
            if !c.is_file() {
                return Ok(failed_result(
                    task,
                    &opts.model,
                    format!("missing cassette {}", c.display()),
                ));
            }
            ProviderSource::Cassette(std::fs::canonicalize(c)?)
        }
        Lane::Live => ProviderSource::Live {
            config_path: opts
                .config
                .clone()
                .context("the live lane needs --config <path>")?,
        },
    };

    let before = snapshot(&work)?;

    let mut cmd = Command::new(&opts.exe);
    cmd.arg("child")
        .arg("--spec")
        .arg(std::fs::canonicalize(task.dir.join("task.json"))?)
        .arg("--out")
        .arg(&out_file);
    match &source {
        ProviderSource::Cassette(p) => {
            cmd.arg("--cassette").arg(p);
        }
        ProviderSource::Live { config_path } => {
            cmd.arg("--live-config")
                .arg(std::fs::canonicalize(config_path)?);
        }
    }
    if let Some(m) = &opts.model {
        cmd.arg("--model").arg(m);
    }
    if let Some(c) = opts.default_max_cost_usd {
        cmd.arg("--default-max-cost-usd").arg(c.to_string());
    }
    cmd.current_dir(&work)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let started = Instant::now();
    let mut child = cmd.spawn().context("spawning agent child")?;
    let timeout = Duration::from_secs(task.spec.timeout_secs);
    let status = wait_timeout(&mut child, timeout)?;
    let child_stderr = read_stderr(&mut child);

    let mut outcome: Option<ChildOutcome> = None;
    let mut failure: Option<String> = None;
    match status {
        None => failure = Some(format!("agent timed out after {}s", timeout.as_secs())),
        Some(s) if !s.success() => {
            failure = Some(format!(
                "agent child exited with {s}: {}",
                child_stderr.trim()
            ));
        }
        Some(_) => match std::fs::read(&out_file) {
            Ok(bytes) => outcome = serde_json::from_slice(&bytes).ok(),
            Err(e) => failure = Some(format!("no outcome file: {e}")),
        },
    }
    let wall_ms = started.elapsed().as_millis() as u64;

    let after = snapshot(&work)?;
    let changed = changed_paths(&before, &after);
    let forbidden = forbidden_edits(&changed, &task.spec.verify.forbid_paths);

    let (verify_exit, verify_ok) = if failure.is_none() {
        let code = run_verify(&task.spec.verify.command, &work)?;
        (code, code == Some(0))
    } else {
        (None, false)
    };

    if failure.is_none() {
        if !forbidden.is_empty() {
            failure = Some(format!(
                "modified forbidden paths: {}",
                forbidden.join(", ")
            ));
        } else if !verify_ok {
            failure = Some(match verify_exit {
                Some(c) => format!("verify command exited with {c}"),
                None => "verify command timed out or was killed".to_string(),
            });
        }
    }

    let model = outcome
        .as_ref()
        .map(|o| o.model.clone())
        .or_else(|| opts.model.clone())
        .or_else(|| task.spec.model.clone())
        .unwrap_or_else(|| crate::child::DEFAULT_REPLAY_MODEL.to_string());
    let metrics = outcome
        .as_ref()
        .map(|o| metrics::from_events(&o.events, &model))
        .unwrap_or_default();

    if failure.is_none() {
        failure = check_expectations(&metrics, &task.spec.expect);
    }

    let failure_is_none = failure.is_none();
    Ok(TaskResult {
        id: task.spec.id.clone(),
        tags: task.spec.tags.clone(),
        passed: failure_is_none,
        failure,
        verify_exit,
        agent_error: outcome.as_ref().and_then(|o| o.error.clone()),
        forbidden_edits: forbidden,
        files_changed: changed.len(),
        wall_ms,
        model,
        metrics,
        runs: 1,
        pass_count: u32::from(failure_is_none),
    })
}

/// `Some(reason)` when the run did not exhibit the behaviour a task expects.
pub fn check_expectations(m: &Metrics, e: &ExpectSpec) -> Option<String> {
    if m.loop_nudges < e.min_loop_nudges {
        return Some(format!(
            "expected at least {} loop nudge(s), saw {}",
            e.min_loop_nudges, m.loop_nudges
        ));
    }
    if m.auto_retries < e.min_auto_retries {
        return Some(format!(
            "expected at least {} auto-retry annotation(s), saw {}",
            e.min_auto_retries, m.auto_retries
        ));
    }
    if let Some(max) = e.max_iterations_used {
        if m.iterations > max {
            return Some(format!(
                "expected at most {max} iterations, used {}",
                m.iterations
            ));
        }
    }
    if let Some(want) = &e.limit_hit {
        if m.limit_hit.as_deref() != Some(want.as_str()) {
            return Some(format!("expected limit {want:?}, saw {:?}", m.limit_hit));
        }
    }
    None
}

fn failed_result(task: &Task, model: &Option<String>, reason: String) -> TaskResult {
    TaskResult {
        id: task.spec.id.clone(),
        tags: task.spec.tags.clone(),
        passed: false,
        failure: Some(reason),
        verify_exit: None,
        agent_error: None,
        forbidden_edits: vec![],
        files_changed: 0,
        wall_ms: 0,
        model: model
            .clone()
            .or_else(|| task.spec.model.clone())
            .unwrap_or_else(|| crate::child::DEFAULT_REPLAY_MODEL.to_string()),
        metrics: Metrics::default(),
        runs: 1,
        pass_count: 0,
    }
}

/// Run a task `n` times and fold the runs into one result. Stops repeating
/// once `remaining_budget` (USD) is spent. Returns the result and the USD
/// spent across the runs that were priced.
pub fn run_task_n(
    task: &Task,
    opts: &RunOptions,
    n: u32,
    remaining_budget: Option<f64>,
) -> Result<(TaskResult, f64)> {
    let mut runs = Vec::new();
    let mut spent = 0.0;
    for i in 0..n.max(1) {
        if i > 0 && remaining_budget.is_some_and(|b| spent >= b) {
            break;
        }
        let r = run_task(task, opts)?;
        spent += r.metrics.cost_usd.unwrap_or(0.0);
        runs.push(r);
    }
    Ok((aggregate(runs), spent))
}

/// Fold several runs of one task: `passed` is the majority outcome and the
/// metrics come from the run with the median iteration count.
pub fn aggregate(mut runs: Vec<TaskResult>) -> TaskResult {
    assert!(!runs.is_empty(), "aggregate needs at least one run");
    let total = runs.len() as u32;
    let pass_count = runs.iter().filter(|r| r.passed).count() as u32;
    let failure = runs.iter().find_map(|r| r.failure.clone());
    runs.sort_by_key(|r| r.metrics.iterations);
    let mut rep = runs.swap_remove(runs.len() / 2);
    rep.runs = total;
    rep.pass_count = pass_count;
    rep.passed = pass_count * 2 > total;
    rep.failure = if rep.passed { None } else { failure };
    rep
}

fn run_verify(command: &str, work: &Path) -> Result<Option<i32>> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(work)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawning verify command")?;
    Ok(wait_timeout(&mut child, VERIFY_TIMEOUT)?.and_then(|s| s.code()))
}

/// Wait for `child`; kill it and return `None` when `timeout` elapses.
fn wait_timeout(child: &mut Child, timeout: Duration) -> Result<Option<ExitStatus>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn read_stderr(child: &mut Child) -> String {
    use std::io::Read;
    let mut s = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut s);
    }
    s
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    if !from.is_dir() {
        return Ok(()); // a task with no fixture starts from an empty workdir
    }
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

/// relative path -> content hash, skipping VCS and build directories.
pub fn snapshot(root: &Path) -> Result<BTreeMap<String, u64>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, u64>) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            if entry.file_type()?.is_dir() {
                if name == ".git" || name == "target" {
                    continue;
                }
                walk(root, &path, out)?;
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(rel, fnv1a64(&std::fs::read(&path)?));
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out)?;
    Ok(out)
}

/// Paths added, removed or modified between two snapshots.
pub fn changed_paths(before: &BTreeMap<String, u64>, after: &BTreeMap<String, u64>) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for (p, h) in after {
        if before.get(p) != Some(h) {
            v.push(p.clone());
        }
    }
    for p in before.keys() {
        if !after.contains_key(p) {
            v.push(p.clone());
        }
    }
    v.sort();
    v
}

pub fn forbidden_edits(changed: &[String], forbid: &[String]) -> Vec<String> {
    changed
        .iter()
        .filter(|c| {
            forbid.iter().any(|f| {
                let f = f.trim_end_matches('/');
                *c == f || c.starts_with(&format!("{f}/"))
            })
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(pairs: &[(&str, u64)]) -> BTreeMap<String, u64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn changed_paths_reports_add_modify_remove() {
        let before = snap(&[("a", 1), ("b", 2), ("c", 3)]);
        let after = snap(&[("a", 1), ("b", 9), ("d", 4)]);
        assert_eq!(changed_paths(&before, &after), vec!["b", "c", "d"]);
    }

    #[test]
    fn forbidden_matches_file_and_directory_prefix_only() {
        let changed = vec![
            "tests/a.sh".to_string(),
            "tests_extra/b.sh".to_string(),
            "src/x".to_string(),
        ];
        let hit = forbidden_edits(&changed, &["tests/".to_string()]);
        assert_eq!(hit, vec!["tests/a.sh"]);
        let hit = forbidden_edits(&changed, &["src/x".to_string()]);
        assert_eq!(hit, vec!["src/x"]);
    }

    #[test]
    fn wait_timeout_kills_a_hung_process() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let started = Instant::now();
        let status = wait_timeout(&mut child, Duration::from_millis(100)).unwrap();
        assert!(status.is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn expectations_pass_by_default_and_fail_when_unmet() {
        let m = Metrics {
            iterations: 5,
            loop_nudges: 1,
            ..Default::default()
        };
        assert_eq!(check_expectations(&m, &ExpectSpec::default()), None);
        let ok = ExpectSpec {
            min_loop_nudges: 1,
            max_iterations_used: Some(5),
            ..Default::default()
        };
        assert_eq!(check_expectations(&m, &ok), None);
        let nudges = ExpectSpec {
            min_loop_nudges: 2,
            ..Default::default()
        };
        assert!(check_expectations(&m, &nudges)
            .unwrap()
            .contains("loop nudge"));
        let retries = ExpectSpec {
            min_auto_retries: 1,
            ..Default::default()
        };
        assert!(check_expectations(&m, &retries)
            .unwrap()
            .contains("auto-retry"));
        let iters = ExpectSpec {
            max_iterations_used: Some(4),
            ..Default::default()
        };
        assert!(check_expectations(&m, &iters)
            .unwrap()
            .contains("at most 4"));
        let limit = ExpectSpec {
            limit_hit: Some("cost".into()),
            ..Default::default()
        };
        assert!(check_expectations(&m, &limit).unwrap().contains("limit"));
    }

    fn res(passed: bool, iters: u32) -> TaskResult {
        TaskResult {
            id: "t".into(),
            tags: vec![],
            passed,
            failure: (!passed).then(|| "boom".to_string()),
            verify_exit: None,
            agent_error: None,
            forbidden_edits: vec![],
            files_changed: 0,
            wall_ms: 0,
            model: "m".into(),
            metrics: Metrics {
                iterations: iters,
                ..Default::default()
            },
            runs: 1,
            pass_count: u32::from(passed),
        }
    }

    #[test]
    fn aggregate_is_majority_with_median_metrics() {
        let agg = aggregate(vec![res(true, 9), res(true, 3), res(false, 5)]);
        assert!(agg.passed);
        assert_eq!((agg.runs, agg.pass_count), (3, 2));
        assert_eq!(agg.metrics.iterations, 5);
        assert_eq!(agg.failure, None);

        let agg = aggregate(vec![res(true, 3), res(false, 4), res(false, 5)]);
        assert!(!agg.passed);
        assert_eq!(agg.pass_count, 1);
        assert_eq!(agg.failure.as_deref(), Some("boom"));

        // An even split is not a pass.
        assert!(!aggregate(vec![res(true, 1), res(false, 1)]).passed);
        // Single run passes through unchanged.
        let one = aggregate(vec![res(true, 7)]);
        assert_eq!((one.runs, one.pass_count, one.passed), (1, 1, true));
    }

    #[test]
    fn snapshot_skips_git_and_target() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        std::fs::create_dir_all(tmp.path().join("target")).unwrap();
        std::fs::write(tmp.path().join(".git/x"), "1").unwrap();
        std::fs::write(tmp.path().join("target/y"), "1").unwrap();
        std::fs::write(tmp.path().join("keep"), "1").unwrap();
        let s = snapshot(tmp.path()).unwrap();
        assert_eq!(s.keys().collect::<Vec<_>>(), vec!["keep"]);
    }
}
