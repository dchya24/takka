//! `takka-eval`: run a fixed task suite headlessly and report how the agent
//! loop did. See docs/superpowers/plans/2026-11-26-eval-harness-and-record-replay.md.

mod child;
mod compare;
mod live;
mod metrics;
mod report;
mod runner;
mod task;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "takka-eval", about = "Eval harness for the takka agent loop")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum LaneArg {
    Replay,
    Live,
}

#[derive(Subcommand)]
enum Command {
    /// Run the task suite and print a summary.
    Run {
        #[arg(long, default_value = "evals/tasks")]
        tasks: PathBuf,
        #[arg(long, value_enum, default_value = "replay")]
        lane: LaneArg,
        /// Keep tasks whose id contains this text or that carry it as a tag.
        #[arg(long)]
        filter: Option<String>,
        /// Model id (live lane: provider default when unset).
        #[arg(long)]
        model: Option<String>,
        /// Config file with provider credentials (live lane only).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Write per-task results as JSONL.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Run each task this many times; `passed` is the majority outcome and
        /// the table shows pass/runs, so flaky tasks are visible.
        #[arg(long, default_value_t = 1)]
        n: u32,
        /// Live lane: per-run USD ceiling for tasks without `max_cost_usd`.
        #[arg(long, default_value_t = 0.50)]
        max_cost_usd: f64,
        /// Live lane: stop after this much total USD (the run then exits non-zero).
        #[arg(long, default_value_t = 5.0)]
        budget_usd: f64,
        /// Live lane: run a model that is missing from the price table (its
        /// cost ceilings cannot be enforced).
        #[arg(long)]
        allow_unpriced: bool,
        /// Compare against a JSONL results file from an earlier run and exit
        /// non-zero on regression.
        #[arg(long)]
        baseline: Option<PathBuf>,
        /// Largest tolerated drop in pass rate, as a fraction (0.1 = 10 points).
        #[arg(long, default_value_t = 0.0)]
        max_pass_drop: f64,
        /// Largest tolerated rise (percent) of median iterations or median cost.
        #[arg(long, default_value_t = 25.0)]
        max_regress_pct: f64,
    },
    /// List the tasks that would run.
    List {
        #[arg(long, default_value = "evals/tasks")]
        tasks: PathBuf,
        #[arg(long)]
        filter: Option<String>,
    },
    /// Internal: run one task's agent in the current directory.
    #[command(hide = true)]
    Child {
        #[arg(long)]
        spec: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, conflicts_with = "live_config")]
        cassette: Option<PathBuf>,
        #[arg(long)]
        live_config: Option<PathBuf>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        default_max_cost_usd: Option<f64>,
    },
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("error: {e:#}");
        std::process::exit(2);
    }
}

fn real_main() -> Result<()> {
    match Cli::parse().command {
        Command::List { tasks, filter } => {
            for t in task::load_tasks(&tasks, filter.as_deref())? {
                println!("{}\t{}", t.spec.id, t.spec.tags.join(","));
            }
            Ok(())
        }
        Command::Child {
            spec,
            out,
            cassette,
            live_config,
            model,
            default_max_cost_usd,
        } => {
            let source = match (cassette, live_config) {
                (Some(c), None) => child::ProviderSource::Cassette(c),
                (None, Some(config_path)) => child::ProviderSource::Live { config_path },
                _ => bail!("child needs exactly one of --cassette or --live-config"),
            };
            child::run_child(&spec, &source, model.as_deref(), default_max_cost_usd, &out)
        }
        Command::Run {
            tasks,
            lane,
            filter,
            model,
            config,
            out,
            n,
            max_cost_usd,
            budget_usd,
            allow_unpriced,
            baseline,
            max_pass_drop,
            max_regress_pct,
        } => {
            let loaded = task::load_tasks(&tasks, filter.as_deref())?;
            if loaded.is_empty() {
                bail!("no tasks found under {}", tasks.display());
            }
            let live = matches!(lane, LaneArg::Live);
            let opts = runner::RunOptions {
                lane: if live {
                    runner::Lane::Live
                } else {
                    runner::Lane::Replay
                },
                exe: std::env::current_exe().context("locating own executable")?,
                model,
                default_max_cost_usd: live.then_some(max_cost_usd),
                config,
            };
            if live {
                // Resolve the model now: validates the config before any
                // spend, and lets us refuse models we cannot price.
                let config_path = opts
                    .config
                    .clone()
                    .context("the live lane needs --config <path>")?;
                let (_, model_name) = child::build_provider(
                    &child::ProviderSource::Live { config_path },
                    opts.model.as_deref(),
                )?;
                live::check_priced(&model_name, allow_unpriced)?;
                eprintln!(
                    "live lane: model {model_name}, <= ${max_cost_usd:.2}/run, <= ${budget_usd:.2} total"
                );
            }
            let mut budget = live::Budget::new(budget_usd);
            let mut results = Vec::new();
            let mut skipped = Vec::new();
            for t in &loaded {
                if live && budget.exhausted() {
                    skipped.push(t.spec.id.clone());
                    continue;
                }
                eprintln!("running {} ...", t.spec.id);
                let remaining = live.then(|| budget.remaining());
                let (result, spent) = runner::run_task_n(t, &opts, n, remaining)?;
                budget.record(spent);
                results.push(result);
            }
            if !skipped.is_empty() {
                eprintln!(
                    "budget of ${budget_usd:.2} exhausted (spent ${:.4}); not run: {}",
                    budget.spent(),
                    skipped.join(", ")
                );
            }
            print!("{}", report::summary_table(&results));
            if let Some(path) = out {
                report::write_jsonl(&path, &results)?;
            }
            let mut regression = false;
            if let Some(path) = baseline {
                let base = report::read_jsonl(&path)
                    .with_context(|| format!("reading baseline {}", path.display()))?;
                let cmp = compare::compare(
                    &base,
                    &results,
                    compare::Thresholds {
                        max_pass_drop,
                        max_regress_pct,
                    },
                );
                print!("\n{}", cmp.render());
                regression = cmp.is_regression();
            }
            if regression || !skipped.is_empty() || results.iter().any(|r| !r.passed) {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}
