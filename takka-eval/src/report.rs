//! Result output: JSONL for machines, a plain-text table for people.

use crate::runner::TaskResult;
use anyhow::Result;
use std::io::Write;
use std::path::Path;

pub fn write_jsonl(path: &Path, results: &[TaskResult]) -> Result<()> {
    let mut f = std::fs::File::create(path)?;
    for r in results {
        writeln!(f, "{}", serde_json::to_string(r)?)?;
    }
    Ok(())
}

pub fn read_jsonl(path: &Path) -> Result<Vec<TaskResult>> {
    let text = std::fs::read_to_string(path)?;
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| Ok(serde_json::from_str(l)?))
        .collect()
}

pub fn summary_table(results: &[TaskResult]) -> String {
    let id_w = results.iter().map(|r| r.id.len()).max().unwrap_or(2).max(4);
    let mut out = format!(
        "{:<id_w$}  {:<5}  {:>5}  {:>5}  {:>9}  {:>6}  {:>6}  {:>6}  {:<30}  {}\n",
        "task",
        "pass",
        "iter",
        "tools",
        "cost_usd",
        "cache",
        "nudge",
        "retry",
        "fired (runs)",
        "note"
    );
    for r in results {
        let cost = r
            .metrics
            .cost_usd
            .map(|c| format!("{c:.4}"))
            .unwrap_or_else(|| "-".to_string());
        let note = r
            .failure
            .clone()
            .or_else(|| r.metrics.limit_hit.as_ref().map(|l| format!("limit:{l}")))
            .unwrap_or_default();
        let verdict = if r.runs > 1 {
            format!("{}/{}", r.pass_count, r.runs)
        } else if r.passed {
            "ok".to_string()
        } else {
            "FAIL".to_string()
        };
        out.push_str(&format!(
            "{:<id_w$}  {:<5}  {:>5}  {:>5}  {:>9}  {:>6}  {:>6}  {:>6}  {:<30}  {}\n",
            r.id,
            verdict,
            r.metrics.iterations,
            r.metrics.total_tool_calls(),
            cost,
            format!("{:.0}%", r.metrics.cache_hit_ratio() * 100.0),
            r.metrics.loop_nudges,
            r.metrics.auto_retries,
            fired_summary(r),
            note
        ));
    }
    let passed = results.iter().filter(|r| r.passed).count();
    out.push_str(&format!("\n{passed}/{} passed\n", results.len()));
    out
}

/// In how many runs each feature fired, e.g. `nudge 3/5 retry 0/5 shape 2/5`.
/// Empty for results without per-run records (older files).
fn fired_summary(r: &TaskResult) -> String {
    let d = &r.run_details;
    if d.is_empty() {
        return String::new();
    }
    let n = d.len();
    let nudge = d.iter().filter(|x| x.loop_nudges > 0).count();
    let retry = d.iter().filter(|x| x.auto_retries > 0).count();
    let shape = d.iter().filter(|x| x.budget_stage > 0).count();
    format!("nudge {nudge}/{n} retry {retry}/{n} shape {shape}/{n}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::Metrics;

    pub fn result(id: &str, passed: bool) -> TaskResult {
        TaskResult {
            id: id.into(),
            tags: vec![],
            passed,
            failure: if passed {
                None
            } else {
                Some("verify command exited with 1".into())
            },
            verify_exit: Some(if passed { 0 } else { 1 }),
            agent_error: None,
            forbidden_edits: vec![],
            files_changed: 1,
            wall_ms: 5,
            model: "m".into(),
            metrics: Metrics::default(),
            runs: 1,
            pass_count: u32::from(passed),
            run_details: Vec::new(),
        }
    }

    #[test]
    fn jsonl_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("r.jsonl");
        write_jsonl(&p, &[result("a", true), result("b", false)]).unwrap();
        let back = read_jsonl(&p).unwrap();
        assert_eq!(back.len(), 2);
        assert!(!back[1].passed);
    }

    #[test]
    fn table_shows_how_often_each_feature_fired() {
        let mut r = result("t", true);
        r.runs = 2;
        r.run_details = vec![
            crate::runner::RunDetail {
                passed: true,
                failure: None,
                iterations: 5,
                loop_nudges: 1,
                auto_retries: 0,
                budget_stage: 2,
                finalized: false,
                cost_usd: None,
                wall_ms: 1,
            },
            crate::runner::RunDetail {
                passed: true,
                failure: None,
                iterations: 4,
                loop_nudges: 0,
                auto_retries: 0,
                budget_stage: 0,
                finalized: false,
                cost_usd: None,
                wall_ms: 1,
            },
        ];
        let t = summary_table(&[r]);
        assert!(t.contains("nudge 1/2 retry 0/2 shape 1/2"), "{t}");
    }

    #[test]
    fn old_result_files_without_run_details_still_load() {
        let old = r#"{"id":"a","tags":[],"passed":true,"failure":null,"verify_exit":0,"agent_error":null,"forbidden_edits":[],"files_changed":1,"wall_ms":5,"model":"m","metrics":{"iterations":3,"tool_calls":{},"tool_failures":0,"prompt_tokens":0,"completion_tokens":0,"cache_read_tokens":0,"cache_creation_tokens":0,"cost_usd":null,"loop_nudges":0,"auto_retries":0,"denied_tools":0,"limit_hit":null},"runs":1,"pass_count":1}"#;
        let r: TaskResult = serde_json::from_str(old).unwrap();
        assert!(r.run_details.is_empty());
        assert_eq!(r.metrics.budget_stage, 0);
    }

    #[test]
    fn table_shows_pass_fraction_for_repeated_runs() {
        let mut r = result("flaky", true);
        r.runs = 3;
        r.pass_count = 2;
        assert!(summary_table(&[r]).contains("2/3"));
    }

    #[test]
    fn table_shows_status_notes_and_totals() {
        let t = summary_table(&[result("alpha", true), result("beta", false)]);
        assert!(t.contains("alpha"));
        assert!(t.contains("FAIL"));
        assert!(t.contains("verify command exited with 1"));
        assert!(t.contains("1/2 passed"));
    }
}
