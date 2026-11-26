//! End-to-end: the real binary, real child process, replay lane.
//!
//! Unix only: fixtures and verify commands are POSIX `sh`.
#![cfg(unix)]

use serde_json::json;
use std::path::Path;
use std::process::Command;

fn call(i: usize, name: &str, args: serde_json::Value) -> String {
    json!({"seq": i, "response": {"id": format!("r{i}"), "model": "eval-model",
        "message": {"role": "assistant", "content": null,
            "tool_calls": [{"id": format!("c{i}"), "type": "function",
                "function": {"name": name, "arguments": args.to_string()}}]},
        "finish_reason": "tool_calls",
        "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}}})
    .to_string()
}

fn text(i: usize, t: &str) -> String {
    json!({"seq": i, "response": {"id": format!("r{i}"), "model": "eval-model",
        "message": {"role": "assistant", "content": t, "tool_calls": []},
        "finish_reason": "stop", "usage": null}})
    .to_string()
}

fn make_task(root: &Path, id: &str, forbid: &[&str], cassette: &[String]) {
    let dir = root.join(id);
    std::fs::create_dir_all(dir.join("repo")).unwrap();
    std::fs::write(dir.join("repo/value.txt"), "bad\n").unwrap();
    std::fs::write(dir.join("repo/guard.txt"), "keep\n").unwrap();
    std::fs::write(
        dir.join("task.json"),
        json!({"id": id, "prompt": "fix value.txt",
            "verify": {"command": "grep -q good value.txt", "forbid_paths": forbid}})
        .to_string(),
    )
    .unwrap();
    std::fs::write(dir.join("cassette.jsonl"), cassette.join("\n")).unwrap();
}

fn run(tasks: &Path, extra: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_takka-eval"))
        .args(["run", "--tasks"])
        .arg(tasks)
        .args(extra)
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn edit_steps() -> Vec<String> {
    vec![
        call(0, "read_file", json!({"path": "value.txt"})),
        call(
            1,
            "edit_file",
            json!({"file_path": "value.txt", "old_string": "bad", "new_string": "good"}),
        ),
        text(2, "done"),
    ]
}

#[test]
fn passing_task_reports_ok_and_writes_jsonl() {
    let tmp = tempfile::tempdir().unwrap();
    make_task(tmp.path(), "t-pass", &[], &edit_steps());
    let out = tmp.path().join("results.jsonl");
    let (code, stdout, stderr) = run(tmp.path(), &["--out", out.to_str().unwrap()]);
    assert_eq!(code, 0, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("1/1 passed"), "{stdout}");
    let line = std::fs::read_to_string(out).unwrap();
    let v: serde_json::Value = serde_json::from_str(line.lines().next().unwrap()).unwrap();
    assert_eq!(v["passed"], true);
    assert_eq!(v["metrics"]["tool_calls"]["edit_file"], 1);
    assert_eq!(v["files_changed"], 1);
}

#[test]
fn agent_that_does_not_fix_fails_verification() {
    let tmp = tempfile::tempdir().unwrap();
    make_task(tmp.path(), "t-nofix", &[], &[text(0, "I did nothing")]);
    let (code, stdout, _) = run(tmp.path(), &[]);
    assert_eq!(code, 1, "{stdout}");
    assert!(stdout.contains("verify command exited with"), "{stdout}");
}

#[test]
fn editing_a_forbidden_path_fails_even_if_verify_passes() {
    let tmp = tempfile::tempdir().unwrap();
    // write_file refuses unread files (staleness guard), so read it first.
    let mut steps = edit_steps();
    steps.insert(2, call(2, "read_file", json!({"path": "guard.txt"})));
    steps.insert(
        3,
        call(
            3,
            "write_file",
            json!({"path": "guard.txt", "content": "tampered\n"}),
        ),
    );
    make_task(tmp.path(), "t-forbid", &["guard.txt"], &steps);
    let (code, stdout, _) = run(tmp.path(), &[]);
    assert_eq!(code, 1, "{stdout}");
    assert!(stdout.contains("forbidden paths: guard.txt"), "{stdout}");
}

#[test]
fn exhausted_cassette_is_reported_not_hung() {
    let tmp = tempfile::tempdir().unwrap();
    make_task(
        tmp.path(),
        "t-short",
        &[],
        &[call(0, "read_file", json!({"path": "value.txt"}))],
    );
    let (code, stdout, _) = run(tmp.path(), &[]);
    assert_eq!(code, 1, "{stdout}");
    assert!(stdout.contains("FAIL"), "{stdout}");
}

#[test]
fn missing_cassette_fails_the_task_clearly() {
    let tmp = tempfile::tempdir().unwrap();
    make_task(tmp.path(), "t-nocass", &[], &edit_steps());
    std::fs::remove_file(tmp.path().join("t-nocass/cassette.jsonl")).unwrap();
    let (code, stdout, _) = run(tmp.path(), &[]);
    assert_eq!(code, 1);
    assert!(stdout.contains("missing cassette"), "{stdout}");
}

#[test]
fn shipped_seed_tasks_pass_in_replay() {
    let seed = Path::new(env!("CARGO_MANIFEST_DIR")).join("../evals/tasks");
    let (code, stdout, stderr) = run(&seed, &[]);
    assert_eq!(code, 0, "stdout:\n{stdout}\nstderr:\n{stderr}");
}

#[test]
fn baseline_compare_passes_against_itself_and_fails_on_regression() {
    let tmp = tempfile::tempdir().unwrap();
    make_task(tmp.path(), "t-base", &[], &edit_steps());
    let base = tmp.path().join("base.jsonl");
    let (code, _, _) = run(tmp.path(), &["--out", base.to_str().unwrap()]);
    assert_eq!(code, 0);

    // Same run against its own baseline: no regression.
    let (code, stdout, _) = run(tmp.path(), &["--baseline", base.to_str().unwrap()]);
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains("no regressions"), "{stdout}");

    // Break the cassette: the task now fails, which the baseline flags.
    std::fs::write(tmp.path().join("t-base/cassette.jsonl"), text(0, "nothing")).unwrap();
    let (code, stdout, _) = run(tmp.path(), &["--baseline", base.to_str().unwrap()]);
    assert_eq!(code, 1, "{stdout}");
    assert!(stdout.contains("REGRESSION"), "{stdout}");
    assert!(
        stdout.contains("passed in the baseline but fails now"),
        "{stdout}"
    );
}

#[test]
fn repeated_runs_report_pass_fraction() {
    let tmp = tempfile::tempdir().unwrap();
    make_task(tmp.path(), "t-rep", &[], &edit_steps());
    let out = tmp.path().join("r.jsonl");
    let (code, stdout, stderr) = run(tmp.path(), &["--n", "3", "--out", out.to_str().unwrap()]);
    assert_eq!(code, 0, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("3/3"), "{stdout}");
    let v: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(out)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        (v["runs"].as_u64(), v["pass_count"].as_u64()),
        (Some(3), Some(3))
    );
}

#[test]
fn live_lane_requires_config_and_never_touches_the_network_without_it() {
    let tmp = tempfile::tempdir().unwrap();
    make_task(tmp.path(), "t-live", &[], &edit_steps());
    let (code, _, stderr) = run(tmp.path(), &["--lane", "live"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("--config"), "{stderr}");
}

/// Ablation: switching a feature off must change the outcome of the task
/// that depends on it, and an unknown feature name must be rejected.
#[test]
fn disable_switch_turns_off_the_feature_a_task_expects() {
    let tasks = Path::new(env!("CARGO_MANIFEST_DIR")).join("../evals/tasks");

    let (code, stdout, _) = run(&tasks, &["--filter", "loop-nudge"]);
    assert_eq!(code, 0, "baseline run must pass: {stdout}");

    let (code, stdout, _) = run(
        &tasks,
        &["--filter", "loop-nudge", "--disable", "loop-nudge"],
    );
    assert_ne!(code, 0, "task must fail without the nudge: {stdout}");
    assert!(stdout.contains("saw 0"), "{stdout}");

    let (code, _, stderr) = run(&tasks, &["--filter", "loop-nudge", "--disable", "nonsense"]);
    assert_eq!(code, 2, "unknown feature is a usage error: {stderr}");
}

/// A live-only task has no cassette; the replay lane must skip it rather
/// than fail on the missing file.
#[test]
fn replay_lane_skips_live_only_tasks() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("hard");
    std::fs::create_dir_all(dir.join("repo")).unwrap();
    std::fs::write(
        dir.join("task.json"),
        json!({"id": "hard", "prompt": "p", "lanes": ["live"],
            "verify": {"command": "true"}})
        .to_string(),
    )
    .unwrap();
    make_task(
        tmp.path(),
        "easy",
        &[],
        &[
            call(
                0,
                "write_file",
                json!({"path": "value.txt", "content": "good\n"}),
            ),
            text(1, "done"),
        ],
    );
    let (code, stdout, stderr) = run(tmp.path(), &[]);
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    assert!(stdout.contains("easy"), "{stdout}");
    assert!(!stdout.contains("hard"), "{stdout}");
    assert!(stderr.contains("skipping 1 task"), "{stderr}");
}

/// A task with `setup` gets a live service: the verify command sees its port
/// in `EVAL_PORT` and can reach it. (`{port}` substitution in the prompt is
/// covered by a unit test; a static cassette cannot observe it.)
#[test]
fn setup_service_is_reachable_from_verify() {
    if Command::new("python3").arg("--version").output().is_err() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("svc");
    std::fs::create_dir_all(dir.join("repo")).unwrap();
    std::fs::write(
        dir.join("task.json"),
        json!({"id": "svc", "prompt": "do nothing at {port}",
            "setup": {"command": "python3 -m http.server $EVAL_PORT --bind 127.0.0.1",
                "ready_timeout_secs": 30},
            "verify": {"command": "python3 -c \"import os,urllib.request; urllib.request.urlopen('http://127.0.0.1:%s/' % os.environ['EVAL_PORT'])\""}})
        .to_string(),
    )
    .unwrap();
    std::fs::write(dir.join("cassette.jsonl"), text(0, "ok")).unwrap();
    let (code, stdout, stderr) = run(tmp.path(), &[]);
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    assert!(stdout.contains("1/1 passed"), "{stdout}");
}

/// A service that never comes up fails the task with a clear reason and
/// never runs the agent.
#[test]
fn setup_service_that_never_starts_fails_the_task() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("svc");
    std::fs::create_dir_all(dir.join("repo")).unwrap();
    std::fs::write(
        dir.join("task.json"),
        json!({"id": "svc", "prompt": "p",
            "setup": {"command": "sleep 30", "ready_timeout_secs": 1},
            "verify": {"command": "true"}})
        .to_string(),
    )
    .unwrap();
    std::fs::write(dir.join("cassette.jsonl"), text(0, "done")).unwrap();
    let (code, stdout, _) = run(tmp.path(), &[]);
    assert_eq!(code, 1, "{stdout}");
    assert!(stdout.contains("not ready"), "{stdout}");
}

/// A run killed by the wall-clock timeout still reports what it did before
/// being killed (iterations, tool calls), so a slow run can be diagnosed.
#[test]
fn timed_out_run_keeps_partial_metrics() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("slow");
    std::fs::create_dir_all(dir.join("repo")).unwrap();
    std::fs::write(
        dir.join("task.json"),
        json!({"id": "slow", "prompt": "p", "timeout_secs": 2,
            "verify": {"command": "true"}})
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.join("cassette.jsonl"),
        [
            call(0, "run_command", json!({"command": "sleep 30"})),
            text(1, "done"),
        ]
        .join("\n"),
    )
    .unwrap();
    let out = tmp.path().join("results.jsonl");
    let (code, stdout, _) = run(tmp.path(), &["--out", out.to_str().unwrap()]);
    assert_eq!(code, 1, "{stdout}");
    let line = std::fs::read_to_string(out).unwrap();
    let v: serde_json::Value = serde_json::from_str(line.lines().next().unwrap()).unwrap();
    assert!(v["failure"].as_str().unwrap().contains("timed out"), "{v}");
    assert_eq!(v["metrics"]["iterations"], 1, "{v}");
    assert_eq!(v["metrics"]["tool_calls"]["run_command"], 1, "{v}");
    assert_eq!(v["run_details"][0]["iterations"], 1, "{v}");
}
