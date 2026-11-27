//! Parent side of an eval run: prepare a workdir, spawn the agent child,
//! verify the outcome independently of the agent, and collect metrics.

use crate::child::{ChildOutcome, ProviderSource};
use crate::metrics::{self, Metrics};
use crate::service;
use crate::task::{ExpectSpec, Task};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use takka_core::runtime::pipe_id::fnv1a64;

const VERIFY_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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
    /// Features switched off for this run (ablation).
    pub disabled: Vec<crate::child::Feature>,
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
    /// One record per run, in run order, so a failure or a feature trigger
    /// can be traced even when the representative metrics come from the
    /// median run. Older result files omit it.
    #[serde(default)]
    pub run_details: Vec<RunDetail>,
}

/// What happened in one run of a task (see `TaskResult::run_details`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunDetail {
    pub passed: bool,
    pub failure: Option<String>,
    pub iterations: u32,
    pub loop_nudges: u32,
    pub auto_retries: u32,
    pub budget_stage: u8,
    pub finalized: bool,
    pub cost_usd: Option<f64>,
    pub wall_ms: u64,
}

impl From<&TaskResult> for RunDetail {
    fn from(r: &TaskResult) -> Self {
        RunDetail {
            passed: r.passed,
            failure: r.failure.clone(),
            iterations: r.metrics.iterations,
            loop_nudges: r.metrics.loop_nudges,
            auto_retries: r.metrics.auto_retries,
            budget_stage: r.metrics.budget_stage,
            finalized: r.metrics.finalized,
            cost_usd: r.metrics.cost_usd,
            wall_ms: r.wall_ms,
        }
    }
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

    // Optional background service; killed when `_service` drops at the end
    // of this function, after verification.
    let task_dir = std::fs::canonicalize(&task.dir)?;
    let mut port: Option<u16> = None;
    let mut _service = None;
    if let Some(setup) = &task.spec.setup {
        let p = service::free_port()?;
        match service::start(setup, &task_dir, &work, p) {
            Ok(s) => {
                port = Some(p);
                _service = Some(s);
            }
            Err(e) => return Ok(failed_result(task, &opts.model, format!("setup: {e:#}"))),
        }
    }
    // The child reads a resolved copy of the spec so `{port}` in the prompt
    // points at this run's service.
    let mut spec = task.spec.clone();
    if let Some(p) = port {
        spec.prompt = service::resolve_prompt(&spec.prompt, p);
    }
    let spec_file = tmp.path().join("task.json");
    std::fs::write(&spec_file, serde_json::to_vec_pretty(&spec)?)?;

    let mut cmd = Command::new(&opts.exe);
    cmd.arg("child")
        .arg("--spec")
        .arg(&spec_file)
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
    for f in &opts.disabled {
        cmd.arg("--disable").arg(f.name());
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
    // Events streamed before a timeout or crash; used when there is no outcome.
    let partial_events = if matches!(&status, Some(s) if s.success()) {
        Vec::new()
    } else {
        crate::child::read_events_log(&crate::child::events_log_path(&out_file))
    };
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
        let code = run_verify(&task.spec.verify.command, &work, port)?;
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
        .unwrap_or_else(|| metrics::from_events(&partial_events, &model));

    if failure.is_none() {
        failure = expectation_failure(opts.lane, &metrics, &task.spec.expect);
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
        run_details: Vec::new(),
    })
}

/// Lane-aware wrapper: `expect` only constrains the replay lane. Its checks
/// (a loop nudge, an auto-retry, a limit hit) describe behaviour a scripted
/// cassette forces, which a real model may legitimately never exhibit, so in
/// the live lane they would fail correct runs. Live runs still report the
/// same metrics; only the pass/fail verdict ignores `expect`.
pub fn expectation_failure(lane: Lane, m: &Metrics, e: &ExpectSpec) -> Option<String> {
    match lane {
        Lane::Replay => check_expectations(m, e),
        Lane::Live => None,
    }
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
        run_details: Vec::new(),
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
    let details: Vec<RunDetail> = runs.iter().map(RunDetail::from).collect();
    let pass_count = runs.iter().filter(|r| r.passed).count() as u32;
    let failure = runs.iter().find_map(|r| r.failure.clone());
    runs.sort_by_key(|r| r.metrics.iterations);
    let mut rep = runs.swap_remove(runs.len() / 2);
    rep.runs = total;
    rep.pass_count = pass_count;
    rep.run_details = details;
    rep.passed = pass_count * 2 > total;
    rep.failure = if rep.passed { None } else { failure };
    rep
}

fn run_verify(command: &str, work: &Path, port: Option<u16>) -> Result<Option<i32>> {
    let mut verify = Command::new("sh");
    verify.arg("-c").arg(command).current_dir(work);
    if let Some(p) = port {
        verify.env("EVAL_PORT", p.to_string());
    }
    let mut child = verify
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

    #[cfg(unix)]
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

    #[test]
    fn expectations_apply_to_replay_only() {
        // A real model that never thrashes must not "fail" a task whose
        // expectations only make sense for a scripted cassette.
        let m = Metrics::default();
        let e = ExpectSpec {
            min_loop_nudges: 1,
            min_auto_retries: 1,
            ..Default::default()
        };
        assert!(expectation_failure(Lane::Replay, &m, &e).is_some());
        assert_eq!(expectation_failure(Lane::Live, &m, &e), None);
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
            run_details: Vec::new(),
        }
    }

    #[test]
    fn aggregate_keeps_one_detail_per_run_in_order() {
        let mut a = res(false, 9);
        a.failure = Some("verify command exited with 1".into());
        a.metrics.budget_stage = 2;
        a.metrics.finalized = true;
        let mut b = res(true, 4);
        b.metrics.loop_nudges = 1;
        let c = res(true, 6);
        let agg = aggregate(vec![a, b, c]);
        let d = &agg.run_details;
        assert_eq!(d.len(), 3);
        // Run order is preserved, not the median ordering used for metrics.
        assert_eq!(d[0].iterations, 9);
        assert!(!d[0].passed);
        assert_eq!(
            d[0].failure.as_deref(),
            Some("verify command exited with 1")
        );
        assert_eq!((d[0].budget_stage, d[0].finalized), (2, true));
        assert_eq!(d[1].loop_nudges, 1);
        assert_eq!(d[2].iterations, 6);
        assert_eq!(agg.runs, 3);
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
