//! Task specs: `evals/tasks/<id>/{task.json, repo/, cassette.jsonl}`.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

fn default_max_iterations() -> u32 {
    20
}
fn default_timeout_secs() -> u64 {
    300
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifySpec {
    /// Run with `sh -c` in the task workdir after the agent finishes.
    /// Exit status 0 means the task succeeded.
    pub command: String,
    /// Paths (relative to the workdir; a file or a directory prefix) the
    /// agent must not modify. Touching one fails the task.
    #[serde(default)]
    pub forbid_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSpec {
    pub id: String,
    pub prompt: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    #[serde(default)]
    pub max_cost_usd: Option<f64>,
    /// Model id sent to the provider. Replay cassettes are recorded against
    /// one model; live runs may override it with `--model`.
    #[serde(default)]
    pub model: Option<String>,
    /// Wall-clock cap for the agent child process.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    pub verify: VerifySpec,
}

#[derive(Debug, Clone)]
pub struct Task {
    pub spec: TaskSpec,
    pub dir: PathBuf,
}

impl Task {
    pub fn repo_dir(&self) -> PathBuf {
        self.dir.join("repo")
    }
    pub fn cassette_path(&self) -> PathBuf {
        self.dir.join("cassette.jsonl")
    }
}

pub fn load_task(dir: &Path) -> Result<Task> {
    let spec_path = dir.join("task.json");
    let text = std::fs::read_to_string(&spec_path)
        .with_context(|| format!("reading {}", spec_path.display()))?;
    let spec: TaskSpec =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", spec_path.display()))?;
    let dir_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if spec.id != dir_name {
        bail!(
            "{}: task id {:?} must match its directory name {:?}",
            spec_path.display(),
            spec.id,
            dir_name
        );
    }
    Ok(Task {
        spec,
        dir: dir.to_path_buf(),
    })
}

/// Load every task under `root`, sorted by id. `filter` keeps tasks whose id
/// contains it or that carry it as a tag.
pub fn load_tasks(root: &Path, filter: Option<&str>) -> Result<Vec<Task>> {
    let mut tasks = Vec::new();
    for entry in std::fs::read_dir(root).with_context(|| format!("reading {}", root.display()))? {
        let path = entry?.path();
        if path.join("task.json").is_file() {
            tasks.push(load_task(&path)?);
        }
    }
    tasks.sort_by(|a, b| a.spec.id.cmp(&b.spec.id));
    if let Some(f) = filter {
        tasks.retain(|t| t.spec.id.contains(f) || t.spec.tags.iter().any(|tag| tag == f));
    }
    Ok(tasks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_task(root: &Path, id: &str, json: &str) {
        let d = root.join(id);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("task.json"), json).unwrap();
    }

    #[test]
    fn loads_with_defaults_and_sorts() {
        let tmp = tempfile::tempdir().unwrap();
        let min =
            |id: &str| format!(r#"{{"id":"{id}","prompt":"p","verify":{{"command":"true"}}}}"#);
        write_task(tmp.path(), "b-task", &min("b-task"));
        write_task(tmp.path(), "a-task", &min("a-task"));
        let tasks = load_tasks(tmp.path(), None).unwrap();
        let ids: Vec<_> = tasks.iter().map(|t| t.spec.id.as_str()).collect();
        assert_eq!(ids, vec!["a-task", "b-task"]);
        assert_eq!(tasks[0].spec.max_iterations, 20);
        assert_eq!(tasks[0].spec.timeout_secs, 300);
        assert!(tasks[0].spec.verify.forbid_paths.is_empty());
    }

    #[test]
    fn id_must_match_directory() {
        let tmp = tempfile::tempdir().unwrap();
        write_task(
            tmp.path(),
            "dir-name",
            r#"{"id":"other","prompt":"p","verify":{"command":"true"}}"#,
        );
        let err = load_tasks(tmp.path(), None).unwrap_err().to_string();
        assert!(err.contains("must match"), "{err}");
    }

    #[test]
    fn filter_matches_id_substring_or_tag() {
        let tmp = tempfile::tempdir().unwrap();
        write_task(
            tmp.path(),
            "fix-one",
            r#"{"id":"fix-one","prompt":"p","tags":["bugfix"],"verify":{"command":"true"}}"#,
        );
        write_task(
            tmp.path(),
            "add-two",
            r#"{"id":"add-two","prompt":"p","tags":["feature"],"verify":{"command":"true"}}"#,
        );
        assert_eq!(load_tasks(tmp.path(), Some("fix")).unwrap().len(), 1);
        assert_eq!(load_tasks(tmp.path(), Some("feature")).unwrap().len(), 1);
        assert_eq!(load_tasks(tmp.path(), Some("nope")).unwrap().len(), 0);
    }
}
