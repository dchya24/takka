//! Spend-ceiling display helpers (`agent.max_cost_usd`).
//!
//! One place for the two user-facing strings so the inline REPL, the TUI
//! status bar and the event renderer cannot drift: the budget chip that sits
//! in a status line, and the stop notice that explains what to change.
//!
//! Amounts are printed per run, because the ceiling bounds one user turn —
//! see `Orchestrator::record_run_cost`.

/// The ceiling itself, as it is written in the config file.
pub fn format_usd(amount: f64) -> String {
    format!("${amount:.2}")
}

/// Remaining budget for the run, given the ceiling and what has been spent.
///
/// Clamps at zero: a run that already crossed the ceiling shows `$0.00 left`
/// rather than a negative number, which is what the loop is doing at that
/// point anyway (it stops at the turn boundary).
pub fn remaining_usd(max_cost_usd: f64, spent_usd: f64) -> f64 {
    (max_cost_usd - spent_usd).max(0.0)
}

/// Status-bar chip, e.g. `💵 $0.12 of $0.50 left`. Returns `None` when no
/// ceiling is configured (the default), so unlimited runs show no chip at
/// all.
pub fn budget_chip(max_cost_usd: Option<f64>, spent_usd: f64) -> Option<String> {
    let max = max_cost_usd?;
    Some(format!(
        "💵 {} of {} left",
        format_usd(remaining_usd(max, spent_usd)),
        format_usd(max)
    ))
}

/// The stop notice for `Event::CostLimitExceeded`: what happened, plus the
/// concrete way out (raise or remove the key).
pub fn stop_notice(max_cost_usd: f64, spent_usd: f64) -> String {
    format!(
        "Spend ceiling reached — this run spent {} of {}. \
         Raise or remove `agent.max_cost_usd` in your takka config to continue.",
        format_usd(spent_usd),
        format_usd(max_cost_usd)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_ceiling_means_no_chip() {
        assert_eq!(budget_chip(None, 3.0), None);
    }

    #[test]
    fn chip_reports_the_remaining_budget() {
        assert_eq!(
            budget_chip(Some(0.50), 0.12).as_deref(),
            Some("💵 $0.38 of $0.50 left")
        );
    }

    #[test]
    fn remaining_never_goes_negative() {
        assert_eq!(remaining_usd(0.10, 0.25), 0.0);
        assert_eq!(
            budget_chip(Some(0.10), 0.25).as_deref(),
            Some("💵 $0.00 of $0.10 left")
        );
    }

    #[test]
    fn stop_notice_names_the_config_key() {
        let notice = stop_notice(0.50, 0.62);
        assert!(notice.contains("$0.62"), "got {notice}");
        assert!(notice.contains("$0.50"), "got {notice}");
        assert!(notice.contains("agent.max_cost_usd"), "got {notice}");
    }
}
