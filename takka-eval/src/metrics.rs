//! Metrics derived purely from the orchestrator's event stream, so the
//! harness never needs orchestrator changes.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use takka_core::Event;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    /// Provider round trips (`ModelRequest` events).
    pub iterations: u32,
    /// Tool executions by name (`ToolStarted`).
    pub tool_calls: BTreeMap<String, u32>,
    /// Tool executions that reported `success: false`.
    pub tool_failures: u32,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    /// `None` when the model has no entry in the price table (never "free").
    pub cost_usd: Option<f64>,
    /// Steering nudges from the semantic loop detector.
    pub loop_nudges: u32,
    /// Tool results annotated by the transient auto-retry.
    pub auto_retries: u32,
    /// Tool calls refused by safety or confirmation.
    pub denied_tools: u32,
    /// `"max_tokens"`, `"max_turn_requests"` or `"cost"` when a limit ended the run.
    pub limit_hit: Option<String>,
    /// Highest graduated tool-budget stage reached: 0 none, 1 read-only tools
    /// stripped (80%), 2 only mutating tools left (90%).
    #[serde(default)]
    pub budget_stage: u8,
    /// The run hit the iteration cap and was forced to finalize.
    #[serde(default)]
    pub finalized: bool,
}

impl Metrics {
    pub fn total_tool_calls(&self) -> u32 {
        self.tool_calls.values().sum()
    }

    /// Share of prompt-side tokens served from cache, 0.0 when none reported.
    pub fn cache_hit_ratio(&self) -> f64 {
        let denom = self.cache_read_tokens + self.cache_creation_tokens + self.prompt_tokens;
        if denom == 0 {
            0.0
        } else {
            self.cache_read_tokens as f64 / denom as f64
        }
    }
}

pub fn from_events(events: &[Event], model: &str) -> Metrics {
    let mut m = Metrics::default();
    for e in events {
        match e {
            Event::ModelRequest { .. } => m.iterations += 1,
            Event::ToolStarted { tool_name, .. } => {
                *m.tool_calls.entry(tool_name.clone()).or_insert(0) += 1;
            }
            Event::ToolOutput {
                output, success, ..
            } => {
                if !success {
                    m.tool_failures += 1;
                }
                if output.to_string().contains("[auto-retry]") {
                    m.auto_retries += 1;
                }
            }
            Event::Usage {
                prompt_tokens,
                completion_tokens,
                cache_read_input_tokens,
                cache_creation_input_tokens,
            } => {
                m.prompt_tokens += u64::from(*prompt_tokens);
                m.completion_tokens += u64::from(*completion_tokens);
                m.cache_read_tokens += u64::from(cache_read_input_tokens.unwrap_or(0));
                m.cache_creation_tokens += u64::from(cache_creation_input_tokens.unwrap_or(0));
            }
            Event::System { message } => {
                // Heuristics tied to the orchestrator's wording; the
                // integration tests in takka-core assert the same strings.
                if message.contains("re-reading") {
                    m.loop_nudges += 1;
                }
                if message.contains("Tool budget 90%") {
                    m.budget_stage = m.budget_stage.max(2);
                } else if message.contains("Tool budget 80%") {
                    m.budget_stage = m.budget_stage.max(1);
                }
                if message.contains("finalizing answer") {
                    m.finalized = true;
                }
                if message.contains("denied") || message.contains("confirmation denied") {
                    m.denied_tools += 1;
                }
            }
            Event::TurnLimitExceeded { kind } => {
                m.limit_hit = Some(
                    match kind {
                        takka_core::events::TurnLimitKind::MaxTokens => "max_tokens",
                        takka_core::events::TurnLimitKind::MaxTurnRequests => "max_turn_requests",
                    }
                    .to_string(),
                );
            }
            Event::CostLimitExceeded { .. } => m.limit_hit = Some("cost".to_string()),
            _ => {}
        }
    }
    if m.prompt_tokens + m.completion_tokens + m.cache_read_tokens + m.cache_creation_tokens > 0 {
        m.cost_usd = takka_core::pricing::lookup(model).map(|p| {
            p.cost_usd(
                saturate(m.prompt_tokens),
                saturate(m.completion_tokens),
                saturate(m.cache_read_tokens),
                saturate(m.cache_creation_tokens),
            )
        });
    }
    m
}

fn saturate(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use takka_core::events::TurnLimitKind;

    fn started(name: &str) -> Event {
        Event::ToolStarted {
            tool_call_id: "id".into(),
            tool_name: name.into(),
            arguments: json!({}),
        }
    }
    fn output(ok: bool, out: serde_json::Value) -> Event {
        Event::ToolOutput {
            tool_name: "t".into(),
            output: out,
            error: None,
            tool_call_id: "id".into(),
            duration_ms: 1,
            success: ok,
            truncated: false,
        }
    }
    fn usage(p: u32, c: u32, cr: Option<u32>) -> Event {
        Event::Usage {
            prompt_tokens: p,
            completion_tokens: c,
            cache_read_input_tokens: cr,
            cache_creation_input_tokens: None,
        }
    }

    #[test]
    fn counts_iterations_tools_and_failures() {
        let ev = vec![
            Event::ModelRequest {
                model: "m".into(),
                message_count: 1,
            },
            started("read_file"),
            output(true, json!("ok")),
            started("read_file"),
            output(false, json!("boom")),
            started("edit_file"),
            Event::ModelRequest {
                model: "m".into(),
                message_count: 3,
            },
        ];
        let m = from_events(&ev, "unknown-model");
        assert_eq!(m.iterations, 2);
        assert_eq!(m.tool_calls["read_file"], 2);
        assert_eq!(m.tool_calls["edit_file"], 1);
        assert_eq!(m.total_tool_calls(), 3);
        assert_eq!(m.tool_failures, 1);
    }

    #[test]
    fn sums_usage_and_computes_cache_ratio() {
        let m = from_events(
            &[usage(100, 10, Some(300)), usage(100, 10, None)],
            "unknown-model",
        );
        assert_eq!(m.prompt_tokens, 200);
        assert_eq!(m.completion_tokens, 20);
        assert_eq!(m.cache_read_tokens, 300);
        assert!((m.cache_hit_ratio() - 0.6).abs() < 1e-9);
        // Unknown model: no price, never reported as free.
        assert_eq!(m.cost_usd, None);
    }

    #[test]
    fn cost_is_none_without_usage_and_some_for_known_model() {
        assert_eq!(from_events(&[], "gpt-4o-mini").cost_usd, None);
        let m = from_events(&[usage(1_000_000, 0, None)], "gpt-4o-mini");
        assert!(m.cost_usd.unwrap() > 0.0);
    }

    #[test]
    fn tracks_highest_budget_stage_and_forced_finalization() {
        let sys = |s: &str| Event::System { message: s.into() };
        assert_eq!(from_events(&[], "m").budget_stage, 0);
        assert!(!from_events(&[], "m").finalized);
        let m = from_events(
            &[
                sys("\u{26a0}\u{fe0f} Approaching iteration limit (6/7)"),
                sys("\u{26a0}\u{fe0f} Tool budget 80% (iteration 6/7) \u{2014} read-only tools disabled"),
            ],
            "m",
        );
        assert_eq!(m.budget_stage, 1, "approaching alone is not a stage");
        assert!(!m.finalized);
        let m = from_events(
            &[
                sys("Tool budget 80% (iteration 8/10) - read-only tools disabled"),
                sys("Tool budget 90% (iteration 9/10) - only write/edit tools remain"),
                sys("\u{1f6d1} Iteration limit reached (10) \u{2014} finalizing answer"),
            ],
            "m",
        );
        assert_eq!(m.budget_stage, 2);
        assert!(m.finalized);
    }

    #[test]
    fn detects_nudges_retries_denials_and_limits() {
        let ev = vec![
            Event::System {
                message: "you keep re-reading thrash.txt; decide or ask".into(),
            },
            output(
                true,
                json!("[auto-retry] first attempt failed: timeout; retry succeeded"),
            ),
            Event::System {
                message: "Tool run_command denied: blocked".into(),
            },
            Event::TurnLimitExceeded {
                kind: TurnLimitKind::MaxTurnRequests,
            },
        ];
        let m = from_events(&ev, "m");
        assert_eq!(m.loop_nudges, 1);
        assert_eq!(m.auto_retries, 1);
        assert_eq!(m.denied_tools, 1);
        assert_eq!(m.limit_hit.as_deref(), Some("max_turn_requests"));

        let c = from_events(
            &[Event::CostLimitExceeded {
                max_cost_usd: 1.0,
                spent_usd: 1.2,
            }],
            "m",
        );
        assert_eq!(c.limit_hit.as_deref(), Some("cost"));
    }
}
