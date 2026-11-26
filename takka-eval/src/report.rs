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
        "{:<id_w$}  {:<4}  {:>5}  {:>5}  {:>9}  {:>6}  {:>6}  {:>6}  {}\n",
        "task", "pass", "iter", "tools", "cost_usd", "cache", "nudge", "retry", "note"
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
        out.push_str(&format!(
            "{:<id_w$}  {:<4}  {:>5}  {:>5}  {:>9}  {:>6}  {:>6}  {:>6}  {}\n",
            r.id,
            if r.passed { "ok" } else { "FAIL" },
            r.metrics.iterations,
            r.metrics.total_tool_calls(),
            cost,
            format!("{:.0}%", r.metrics.cache_hit_ratio() * 100.0),
            r.metrics.loop_nudges,
            r.metrics.auto_retries,
            note
        ));
    }
    let passed = results.iter().filter(|r| r.passed).count();
    out.push_str(&format!("\n{passed}/{} passed\n", results.len()));
    out
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
    fn table_shows_status_notes_and_totals() {
        let t = summary_table(&[result("alpha", true), result("beta", false)]);
        assert!(t.contains("alpha"));
        assert!(t.contains("FAIL"));
        assert!(t.contains("verify command exited with 1"));
        assert!(t.contains("1/2 passed"));
    }
}
