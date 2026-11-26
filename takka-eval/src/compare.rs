//! Baseline comparison: did this run get worse than a stored one?

use crate::runner::TaskResult;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    /// Largest tolerated drop in pass rate (0.0..=1.0). Default 0: any drop fails.
    pub max_pass_drop: f64,
    /// Largest tolerated increase of the median iterations / median cost,
    /// as a percentage of the baseline median.
    pub max_regress_pct: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            max_pass_drop: 0.0,
            max_regress_pct: 25.0,
        }
    }
}

#[derive(Debug, Default)]
pub struct Comparison {
    /// Reasons the run counts as a regression (non-empty => exit non-zero).
    pub regressions: Vec<String>,
    /// Informational lines (improvements, new/missing tasks, medians).
    pub notes: Vec<String>,
}

impl Comparison {
    pub fn is_regression(&self) -> bool {
        !self.regressions.is_empty()
    }

    pub fn render(&self) -> String {
        let mut out = String::from("baseline comparison\n");
        for n in &self.notes {
            out.push_str(&format!("  note: {n}\n"));
        }
        for r in &self.regressions {
            out.push_str(&format!("  REGRESSION: {r}\n"));
        }
        if self.regressions.is_empty() {
            out.push_str("  no regressions\n");
        }
        out
    }
}

fn pass_rate(rs: &[&TaskResult]) -> f64 {
    if rs.is_empty() {
        1.0
    } else {
        rs.iter().filter(|r| r.passed).count() as f64 / rs.len() as f64
    }
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    })
}

fn pct_increase(base: f64, now: f64) -> f64 {
    if base <= 0.0 {
        if now > 0.0 {
            f64::INFINITY
        } else {
            0.0
        }
    } else {
        (now - base) / base * 100.0
    }
}

/// Compare over the tasks present in both runs; tasks only in one are noted.
pub fn compare(baseline: &[TaskResult], current: &[TaskResult], t: Thresholds) -> Comparison {
    let mut c = Comparison::default();
    let base: BTreeMap<&str, &TaskResult> = baseline.iter().map(|r| (r.id.as_str(), r)).collect();
    let cur: BTreeMap<&str, &TaskResult> = current.iter().map(|r| (r.id.as_str(), r)).collect();

    for id in base.keys().filter(|id| !cur.contains_key(*id)) {
        c.notes
            .push(format!("task {id} is in the baseline but was not run"));
    }
    for id in cur.keys().filter(|id| !base.contains_key(*id)) {
        c.notes
            .push(format!("task {id} is new (not in the baseline)"));
    }

    let shared: Vec<(&TaskResult, &TaskResult)> = base
        .iter()
        .filter_map(|(id, b)| cur.get(id).map(|n| (*b, *n)))
        .collect();
    if shared.is_empty() {
        c.notes
            .push("no tasks in common; nothing to compare".into());
        return c;
    }

    for (b, n) in &shared {
        if b.passed && !n.passed {
            c.regressions.push(format!(
                "task {} passed in the baseline but fails now ({})",
                n.id,
                n.failure.as_deref().unwrap_or("no reason recorded")
            ));
        } else if !b.passed && n.passed {
            c.notes
                .push(format!("task {} now passes (improvement)", n.id));
        }
    }

    let bs: Vec<&TaskResult> = shared.iter().map(|(b, _)| *b).collect();
    let ns: Vec<&TaskResult> = shared.iter().map(|(_, n)| *n).collect();
    let (bp, np) = (pass_rate(&bs), pass_rate(&ns));
    c.notes.push(format!(
        "pass rate {:.0}% -> {:.0}%",
        bp * 100.0,
        np * 100.0
    ));
    if bp - np > t.max_pass_drop + 1e-9 {
        c.regressions.push(format!(
            "pass rate dropped {:.0}% -> {:.0}% (allowed drop {:.0}%)",
            bp * 100.0,
            np * 100.0,
            t.max_pass_drop * 100.0
        ));
    }

    let iters =
        |rs: &[&TaskResult]| median(rs.iter().map(|r| f64::from(r.metrics.iterations)).collect());
    if let (Some(bi), Some(ni)) = (iters(&bs), iters(&ns)) {
        c.notes
            .push(format!("median iterations {bi:.1} -> {ni:.1}"));
        let inc = pct_increase(bi, ni);
        if inc > t.max_regress_pct {
            c.regressions.push(format!(
                "median iterations rose {bi:.1} -> {ni:.1} (+{inc:.0}%, allowed +{:.0}%)",
                t.max_regress_pct
            ));
        }
    }

    // Cost only where both runs could price the task.
    let priced: Vec<(f64, f64)> = shared
        .iter()
        .filter_map(|(b, n)| Some((b.metrics.cost_usd?, n.metrics.cost_usd?)))
        .collect();
    if let (Some(bc), Some(nc)) = (
        median(priced.iter().map(|p| p.0).collect()),
        median(priced.iter().map(|p| p.1).collect()),
    ) {
        c.notes.push(format!(
            "median cost ${bc:.4} -> ${nc:.4} ({} priced tasks)",
            priced.len()
        ));
        let inc = pct_increase(bc, nc);
        if inc > t.max_regress_pct {
            c.regressions.push(format!(
                "median cost rose ${bc:.4} -> ${nc:.4} (+{inc:.0}%, allowed +{:.0}%)",
                t.max_regress_pct
            ));
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::Metrics;

    fn r(id: &str, passed: bool, iters: u32, cost: Option<f64>) -> TaskResult {
        TaskResult {
            id: id.into(),
            tags: vec![],
            passed,
            failure: (!passed).then(|| "verify command exited with 1".to_string()),
            verify_exit: None,
            agent_error: None,
            forbidden_edits: vec![],
            files_changed: 0,
            wall_ms: 0,
            model: "m".into(),
            metrics: Metrics {
                iterations: iters,
                cost_usd: cost,
                ..Default::default()
            },
        }
    }

    #[test]
    fn identical_runs_have_no_regressions() {
        let a = vec![r("a", true, 4, Some(0.1)), r("b", true, 6, Some(0.2))];
        let c = compare(&a, &a, Thresholds::default());
        assert!(!c.is_regression(), "{}", c.render());
    }

    #[test]
    fn pass_to_fail_is_a_regression_even_within_pass_rate_tolerance() {
        let base = vec![r("a", true, 4, None), r("b", true, 4, None)];
        let cur = vec![r("a", true, 4, None), r("b", false, 4, None)];
        let t = Thresholds {
            max_pass_drop: 0.9,
            ..Default::default()
        };
        let c = compare(&base, &cur, t);
        assert!(c.is_regression());
        assert!(c.regressions[0].contains("task b passed in the baseline"));
    }

    #[test]
    fn pass_rate_drop_respects_threshold() {
        let base = vec![r("a", true, 4, None), r("b", false, 4, None)];
        // a fails now, b still fails: 50% -> 0%.
        let cur = vec![r("a", false, 4, None), r("b", false, 4, None)];
        let strict = compare(&base, &cur, Thresholds::default());
        assert!(strict
            .regressions
            .iter()
            .any(|x| x.contains("pass rate dropped")));
        let lax = compare(
            &base,
            &cur,
            Thresholds {
                max_pass_drop: 0.6,
                ..Default::default()
            },
        );
        assert!(!lax
            .regressions
            .iter()
            .any(|x| x.contains("pass rate dropped")));
    }

    #[test]
    fn median_iterations_and_cost_thresholds() {
        let base = vec![r("a", true, 4, Some(0.10)), r("b", true, 4, Some(0.10))];
        let slower = vec![r("a", true, 8, Some(0.10)), r("b", true, 8, Some(0.10))];
        let c = compare(&base, &slower, Thresholds::default());
        assert!(c
            .regressions
            .iter()
            .any(|x| x.contains("median iterations rose")));

        let pricier = vec![r("a", true, 4, Some(0.20)), r("b", true, 4, Some(0.20))];
        let c = compare(&base, &pricier, Thresholds::default());
        assert!(c.regressions.iter().any(|x| x.contains("median cost rose")));

        let ok = vec![r("a", true, 5, Some(0.12)), r("b", true, 5, Some(0.12))];
        assert!(!compare(&base, &ok, Thresholds::default()).is_regression());
    }

    #[test]
    fn unpriced_tasks_are_excluded_from_cost() {
        let base = vec![r("a", true, 4, None)];
        let cur = vec![r("a", true, 4, Some(5.0))];
        assert!(!compare(&base, &cur, Thresholds::default()).is_regression());
    }

    #[test]
    fn new_and_missing_tasks_are_notes_not_regressions() {
        let base = vec![r("old", true, 4, None), r("both", true, 4, None)];
        let cur = vec![r("new", true, 4, None), r("both", true, 4, None)];
        let c = compare(&base, &cur, Thresholds::default());
        assert!(!c.is_regression());
        assert!(c
            .notes
            .iter()
            .any(|n| n.contains("old") && n.contains("not run")));
        assert!(c
            .notes
            .iter()
            .any(|n| n.contains("new") && n.contains("is new")));
    }

    #[test]
    fn improvement_is_noted() {
        let base = vec![r("a", false, 4, None)];
        let cur = vec![r("a", true, 4, None)];
        let c = compare(&base, &cur, Thresholds::default());
        assert!(!c.is_regression());
        assert!(c.notes.iter().any(|n| n.contains("improvement")));
    }

    #[test]
    fn median_even_and_odd() {
        assert_eq!(median(vec![3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(vec![4.0, 1.0, 2.0, 3.0]), Some(2.5));
        assert_eq!(median(vec![]), None);
    }
}
