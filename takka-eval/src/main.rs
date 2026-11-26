//! `takka-eval`: run a fixed task suite headlessly and report how the agent
//! loop did. See docs/superpowers/plans/2026-11-26-eval-harness-and-record-replay.md.

mod child;
mod compare;
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
        } => {
            let source = match (cassette, live_config) {
                (Some(c), None) => child::ProviderSource::Cassette(c),
                (None, Some(config_path)) => child::ProviderSource::Live { config_path },
                _ => bail!("child needs exactly one of --cassette or --live-config"),
            };
            child::run_child(&spec, &source, model.as_deref(), &out)
        }
        Command::Run {
            tasks,
            lane,
            filter,
            model,
            config,
            out,
            baseline,
            max_pass_drop,
            max_regress_pct,
        } => {
            let loaded = task::load_tasks(&tasks, filter.as_deref())?;
            if loaded.is_empty() {
                bail!("no tasks found under {}", tasks.display());
            }
            let opts = runner::RunOptions {
                lane: match lane {
                    LaneArg::Replay => runner::Lane::Replay,
                    LaneArg::Live => runner::Lane::Live,
                },
                exe: std::env::current_exe().context("locating own executable")?,
                model,
                config,
            };
            let mut results = Vec::new();
            for t in &loaded {
                eprintln!("running {} ...", t.spec.id);
                results.push(runner::run_task(t, &opts)?);
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
            if regression || results.iter().any(|r| !r.passed) {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}
