//! The history every harness keeps on disk, read into the shared model.
//!
//! A source lists sessions cheaply (head and tail of each file, one query of a database)
//! and reads a whole transcript only when the window opens it. Reading never writes to the
//! harness's files. Sources that read a database open it read-only and select session
//! tables only: OpenCode keeps account tokens in the same file.

use std::io;
use std::path::Path;

use envs::Environment;
use model::{HarnessKind, SessionSummary, Transcript};

mod claude;
mod commands;
mod codex;
mod dsh;
mod export;
mod memory_files;
mod opencode;
#[cfg(test)]
mod test_env;
mod time;
mod trash;
mod turns;

pub use claude::{ClaudeFollower, ClaudeParser, ClaudeSource, turn_of_tail};
pub use commands::{CommandSource, HarnessCommand, claude_commands};
pub use codex::{CodexSource, codex_turn_of_tail};
pub use dsh::DshSource;
pub use export::{ExportLabels, export_file_name, export_markdown};
pub use memory_files::{memory_file_paths, memory_tokens};
pub use opencode::OpenCodeSource;
pub use time::parse_rfc3339_ms;
pub use trash::{TRASH_DIR, move_to_trash, session_files};
pub use turns::{TurnFileState, file_history_dir, read_turn, undo_turn};

/// Summaries already known, by the path of their source, so a scan reads only what changed.
pub trait KnownSummaries {
    fn known(&self, source_path: &str) -> Option<SessionSummary>;
}

impl KnownSummaries for () {
    fn known(&self, _: &str) -> Option<SessionSummary> {
        None
    }
}

impl<F: Fn(&str) -> Option<SessionSummary>> KnownSummaries for F {
    fn known(&self, source_path: &str) -> Option<SessionSummary> {
        self(source_path)
    }
}

pub trait HistorySource: Send + Sync {
    fn harness(&self) -> HarnessKind;

    /// Every session the harness kept in this environment. `cache` is a folder for copies of
    /// remote databases.
    fn scan(
        &self,
        env: &dyn Environment,
        known: &dyn KnownSummaries,
        cache: &Path,
    ) -> io::Result<Vec<SessionSummary>>;

    /// The whole session.
    fn load(
        &self,
        env: &dyn Environment,
        summary: &SessionSummary,
        cache: &Path,
    ) -> io::Result<Transcript>;
}

/// The sources of the built-in harnesses.
pub fn builtin_sources() -> Vec<Box<dyn HistorySource>> {
    vec![Box::new(ClaudeSource), Box::new(CodexSource), Box::new(OpenCodeSource), Box::new(DshSource)]
}

/// The source of a harness, when one is built in.
pub fn source_for(harness: &HarnessKind) -> Option<Box<dyn HistorySource>> {
    builtin_sources().into_iter().find(|source| &source.harness() == harness)
}

/// Text of a JSON value that is a string or a list of `{type: text, text}` blocks.
pub(crate) fn text_of(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|item| match item {
                serde_json::Value::String(text) => Some(text.as_str()),
                other => other.get("text").and_then(serde_json::Value::as_str),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Milliseconds since the Unix epoch.
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}
