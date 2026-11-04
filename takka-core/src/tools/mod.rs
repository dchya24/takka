pub mod apply_patch;
pub mod edit_file;
pub mod fetch;
pub mod git_query;
pub mod glob;
pub mod grep;
pub mod list_files;
pub mod question;
pub mod read_file;
pub mod run_command;
pub mod run_script;
pub mod run_tests;
pub mod search_files;
pub mod spawn_subagent;
pub mod todowrite;
pub mod update_memory;
pub mod web_search;
pub mod write_file;

pub use apply_patch::ApplyPatchTool;
pub use edit_file::EditFileTool;
pub use fetch::FetchTool;
pub use git_query::{GitDiffTool, GitStatusTool};
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use list_files::ListFilesTool;
pub use question::{
    install_question_handler, QuestionAnswer, QuestionHandler, QuestionPrompt, QuestionTool,
};
pub use read_file::ReadFileTool;
pub use run_command::RunCommandTool;
pub use run_script::RunScriptTool;
pub use run_tests::RunTestsTool;
pub use search_files::SearchFilesTool;
pub use spawn_subagent::{SpawnSubagentTool, SubagentPolicy, DEFAULT_SUBAGENT_MAX_ITERATIONS};
pub use todowrite::{
    clear_todos, current_todos, TodoChangeHandler, TodoItem, TodoPriority, TodoStatus,
    TodowriteTool,
};
pub use update_memory::UpdateMemoryTool;
pub use web_search::WebSearchTool;
pub use write_file::WriteFileTool;

use crate::file_tracker::FileTracker;
use crate::safety::UrlPolicy;
use std::sync::Arc;

#[derive(Default)]
pub struct ToolDeps {
    pub tracker: Option<Arc<FileTracker>>,
    pub url_policy: UrlPolicy,
    /// Per-instance question UI handler (dev API: `Arc`).
    pub question_handler: Option<Arc<dyn QuestionHandler>>,
    /// Per-instance todo-change UI handler.
    pub todo_handler: Option<Box<dyn TodoChangeHandler>>,
}

impl ToolDeps {
    pub fn new() -> Self {
        Self {
            tracker: Some(Arc::new(FileTracker::new())),
            url_policy: UrlPolicy::default(),
            question_handler: None,
            todo_handler: None,
        }
    }

    pub fn with_tracker(mut self, tracker: Arc<FileTracker>) -> Self {
        self.tracker = Some(tracker);
        self
    }

    pub fn with_url_policy(mut self, url_policy: UrlPolicy) -> Self {
        self.url_policy = url_policy;
        self
    }

    /// Wire the interactive question handler (dev API: `Arc`).
    pub fn with_question_handler(mut self, handler: Arc<dyn QuestionHandler>) -> Self {
        self.question_handler = Some(handler);
        self
    }

    /// Wire the todo-change renderer (owned by the tool instance).
    pub fn with_todo_handler(mut self, handler: Box<dyn TodoChangeHandler>) -> Self {
        self.todo_handler = Some(handler);
        self
    }
}

pub fn builtin_tools() -> Vec<Box<dyn crate::tool::Tool + Send + Sync>> {
    builtin_tools_with_deps(ToolDeps::new())
}

/// Build the standard tool set, sharing a single [`FileTracker`] instance
/// between read_file and edit_file so staleness detection works end-to-end.
pub fn builtin_tools_with_tracker(
    tracker: Arc<FileTracker>,
) -> Vec<Box<dyn crate::tool::Tool + Send + Sync>> {
    builtin_tools_with_deps(ToolDeps::new().with_tracker(tracker))
}

/// Build the standard tool set with both a shared [`FileTracker`] and a
/// URL allowlist policy applied to URL-taking tools (`fetch`, `web_search`).
///
/// When `url_policy.is_unrestricted()` returns `true` (the default), this
/// is equivalent to `builtin_tools_with_tracker`.
pub fn builtin_tools_with(
    tracker: Arc<FileTracker>,
    url_policy: UrlPolicy,
) -> Vec<Box<dyn crate::tool::Tool + Send + Sync>> {
    builtin_tools_with_deps(
        ToolDeps::new()
            .with_tracker(tracker)
            .with_url_policy(url_policy),
    )
}

pub fn builtin_tools_with_deps(deps: ToolDeps) -> Vec<Box<dyn crate::tool::Tool + Send + Sync>> {
    let tracker = deps.tracker.unwrap_or_else(|| Arc::new(FileTracker::new()));
    let question = match deps.question_handler {
        Some(handler) => QuestionTool::new().with_handler(handler),
        None => QuestionTool::new(),
    };
    let todowrite = match deps.todo_handler {
        Some(handler) => TodowriteTool::new().with_change_handler(handler),
        None => TodowriteTool::new(),
    };
    let url_policy = deps.url_policy;

    vec![
        Box::new(RunCommandTool::new()),
        Box::new(ReadFileTool::with_tracker(tracker.clone())),
        Box::new(WriteFileTool::new()),
        Box::new(EditFileTool::with_tracker(tracker.clone())),
        Box::new(ApplyPatchTool::with_tracker(tracker.clone())),
        Box::new(ListFilesTool::new()),
        Box::new(GlobTool::new()),
        Box::new(GrepTool::new()),
        Box::new(SearchFilesTool::new()),
        Box::new(RunScriptTool::new()),
        Box::new(RunTestsTool::new()),
        Box::new(GitStatusTool::new()),
        Box::new(GitDiffTool::new()),
        Box::new(UpdateMemoryTool::new()),
        Box::new(question),
        Box::new(todowrite),
        Box::new(FetchTool::new().with_url_policy(url_policy.clone())),
        Box::new(WebSearchTool::new().with_url_policy(url_policy)),
    ]
}
