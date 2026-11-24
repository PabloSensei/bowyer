//! One shape for every harness: a session as the sidebar sees it, and a transcript as the
//! centre pane draws it. History readers and live sessions both produce these types, so the
//! window renders a replayed Claude session and a running OpenCode one with the same code.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

mod context;
pub mod pricing;
mod tokens;

pub use context::{ClaudeContext, ContextBreakdown, ContextPart, parse_claude_context};
pub use tokens::estimate_tokens;

/// The harnesses the application knows how to read and start.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessKind {
    Claude,
    Codex,
    OpenCode,
    DeepSeek,
    /// A harness from a plugin, by its id.
    Other(String),
}

impl HarnessKind {
    pub fn id(&self) -> &str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::DeepSeek => "deepseek",
            Self::Other(id) => id,
        }
    }

    pub fn from_id(id: &str) -> Self {
        match id {
            "claude" => Self::Claude,
            "codex" => Self::Codex,
            "opencode" => Self::OpenCode,
            "deepseek" => Self::DeepSeek,
            other => Self::Other(other.to_owned()),
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::OpenCode => "OpenCode",
            Self::DeepSeek => "DeepSeek",
            Self::Other(id) => id,
        }
    }
}

impl fmt::Display for HarnessKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Where a session lives: this computer or a host reached over SSH.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EnvId(pub String);

impl EnvId {
    pub const LOCAL: &'static str = "local";

    pub fn local() -> Self {
        Self(Self::LOCAL.to_owned())
    }

    pub fn ssh(host: &str) -> Self {
        Self(format!("ssh:{host}"))
    }

    pub fn is_local(&self) -> bool {
        self.0 == Self::LOCAL
    }

    /// The host of an SSH environment.
    pub fn ssh_host(&self) -> Option<&str> {
        self.0.strip_prefix("ssh:")
    }
}

impl fmt::Display for EnvId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A session, unique across environments and harnesses.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SessionKey {
    pub env: EnvId,
    pub harness: HarnessKind,
    pub id: String,
}

impl SessionKey {
    /// A stable text form for the database: `env|harness|id`.
    pub fn encode(&self) -> String {
        format!("{}|{}|{}", self.env.0, self.harness.id(), self.id)
    }

    pub fn decode(text: &str) -> Option<Self> {
        let mut parts = text.splitn(3, '|');
        let env = EnvId(parts.next()?.to_owned());
        let harness = HarnessKind::from_id(parts.next()?);
        let id = parts.next()?.to_owned();
        Some(Self { env, harness, id })
    }
}

/// Where a session's history was read from, to know when to read it again.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceStamp {
    /// A file path, or `db:<path>#<id>` for sessions inside a database.
    pub path: String,
    pub size: u64,
    /// Modification time in milliseconds since the Unix epoch.
    pub modified_ms: i64,
}

/// What the sidebar needs about a session, cheap to read for hundreds of them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub key: SessionKey,
    pub title: String,
    /// The first prompt, for search and for a title when the harness gave none.
    pub first_prompt: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub model: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
    /// The session this one was forked from or runs under, such as a subagent.
    pub parent: Option<String>,
    pub source: SourceStamp,
    /// Where the last turn stands, as the end of the history tells.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<TurnState>,
}

/// Where the last turn of a session stands. With the time of the last write it tells a
/// session at work from one that waits for its user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    /// A prompt or a tool result went in: the model is answering.
    Working,
    /// The model asked for a tool and no result came back yet: a command runs, or the
    /// harness waits for a permission.
    ToolPending,
    /// The model answered, or the user stopped it: the next move is the user's.
    Idle,
}

impl SessionSummary {
    /// The title, or the first prompt cut short, or the id.
    pub fn display_title(&self) -> String {
        if !self.title.trim().is_empty() {
            return self.title.trim().to_owned();
        }
        if let Some(prompt) = &self.first_prompt {
            let line = prompt.lines().find(|line| !line.trim().is_empty()).unwrap_or("");
            if !line.is_empty() {
                return shorten(line.trim(), 80);
            }
        }
        self.key.id.chars().take(8).collect()
    }
}

/// Cuts text at a character boundary and adds an ellipsis when it was longer.
pub fn shorten(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    System,
}

/// Tokens of one model call or a whole session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    /// Every token written to the cache, of either lifetime.
    pub cache_write: u64,
    /// The part of `cache_write` kept for an hour rather than five minutes.
    #[serde(default)]
    pub cache_write_long: u64,
}

impl Usage {
    /// The size of the context the model saw on this call.
    pub fn context(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }

    pub fn add(&mut self, other: &Usage) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.cache_write_long += other.cache_write_long;
    }

    pub fn subtract(&mut self, other: &Usage) {
        self.input = self.input.saturating_sub(other.input);
        self.output = self.output.saturating_sub(other.output);
        self.cache_read = self.cache_read.saturating_sub(other.cache_read);
        self.cache_write = self.cache_write.saturating_sub(other.cache_write);
        self.cache_write_long = self.cache_write_long.saturating_sub(other.cache_write_long);
    }

    pub fn is_empty(&self) -> bool {
        *self == Usage::default()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    /// Reads files or searches: no side effects.
    Read,
    Edit,
    Shell,
    /// Starts another agent.
    Agent,
    Web,
    Mcp,
    Other,
}

impl ToolKind {
    /// A best guess from the tool name, the same for every harness.
    pub fn classify(name: &str) -> Self {
        let lower = name.to_ascii_lowercase();
        if lower.starts_with("mcp__") || lower.contains("mcp") {
            return Self::Mcp;
        }
        match lower.as_str() {
            "read" | "glob" | "grep" | "ls" | "list" | "view" | "notebookread" | "read_file"
            | "list_dir" | "search" | "codebase_search" | "find" => Self::Read,
            "edit" | "write" | "multiedit" | "notebookedit" | "apply_patch" | "patch"
            | "write_file" | "edit_file" | "str_replace" | "create_file" => Self::Edit,
            "bash" | "powershell" | "shell" | "exec_command" | "local_shell" | "run_command"
            | "terminal" | "bashoutput" | "killshell" => Self::Shell,
            "task" | "agent" | "subagent" | "spawn_agent" | "workflow" => Self::Agent,
            "webfetch" | "websearch" | "web_search" | "fetch" => Self::Web,
            _ => Self::Other,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub text: String,
    pub is_error: bool,
}

/// A piece of a message. Text is Markdown.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        kind: ToolKind,
        input: Value,
        result: Option<ToolResult>,
    },
    Image {
        media_type: String,
        /// The picture in base64, when the history keeps it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<String>,
    },
    /// A note from the harness itself: a hook, a compaction, an error.
    Notice {
        text: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub role: Role,
    pub timestamp_ms: i64,
    pub blocks: Vec<Block>,
    pub model: Option<String>,
    pub usage: Option<Usage>,
    /// A summary the harness wrote when it compacted the context.
    pub compact_summary: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Created,
    Modified,
    Deleted,
}

/// A file an agent changed, summed over the session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub kind: ChangeKind,
    pub added: u32,
    pub removed: u32,
    pub edits: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Running,
    Done,
    Failed,
}

/// A command or an agent a session started.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRun {
    pub id: String,
    pub kind: ToolKind,
    pub title: String,
    pub status: TaskStatus,
    pub started_ms: i64,
    /// The command line of a shell call, to find its processes; `None` for an agent.
    #[serde(default)]
    pub command: Option<String>,
}

/// A whole session, ready to draw.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    pub entries: Vec<Entry>,
    /// Tokens summed over every call.
    pub total_usage: Usage,
    /// Usage of the last call: what is in the context now.
    pub last_usage: Option<Usage>,
    pub cost_usd: Option<f64>,
    pub model: Option<String>,
    /// MCP servers and plugins the harness reported for the session, by name.
    pub tools_seen: Vec<String>,
    /// Tokens per model, to price a session that switched models.
    pub usage_by_model: Vec<(String, Usage)>,
    /// When the last model call answered, for the state of the prompt cache.
    pub last_call_ms: Option<i64>,
    /// Tokens per UTC day (days since the Unix epoch) and bucket, to sum a part of the
    /// session such as this month's.
    #[serde(default)]
    pub usage_by_day: Vec<(i64, String, Usage)>,
    /// The last prompt of a Claude Code session when the model never answered it (only API
    /// errors came back): the harness can resume without it.
    #[serde(default)]
    pub failed_turn: Option<FailedTurn>,
    /// The calls the harness named (Claude's message ids), each with its bucket, tokens and
    /// time: a session resumed into a new file repeats the calls of the one it came from,
    /// and a sum over sessions counts each of them once.
    #[serde(default)]
    pub named_calls: Vec<NamedCall>,
}

/// A model call with the name the harness gave it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NamedCall {
    pub id: String,
    pub bucket: String,
    pub usage: Usage,
    pub at_ms: i64,
}

/// The tokens of sessions summed per bucket, each named call once.
#[derive(Default)]
pub struct UsageSum {
    pub buckets: Vec<(String, Usage)>,
    seen: std::collections::HashSet<String>,
}

impl UsageSum {
    pub fn add(&mut self, bucket: &str, usage: &Usage) {
        match self.buckets.iter_mut().find(|(name, _)| name == bucket) {
            Some((_, sum)) => sum.add(usage),
            None => self.buckets.push((bucket.to_owned(), *usage)),
        }
    }

    /// Adds what a session spent from `since_ms` on, as [`Transcript::spent_since`] gave it.
    pub fn add_session(&mut self, spent: &SpentSince) {
        for (bucket, usage) in &spent.unnamed {
            self.add(bucket, usage);
        }
        for call in &spent.named {
            if self.seen.insert(call.id.clone()) {
                self.add(&call.bucket, &call.usage);
            }
        }
    }
}

/// What a session spent in a span of time: its named calls, and the tokens of the others
/// per bucket.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpentSince {
    pub named: Vec<NamedCall>,
    pub unnamed: Vec<(String, Usage)>,
}

impl SpentSince {
    pub fn is_empty(&self) -> bool {
        self.named.is_empty() && self.unnamed.is_empty()
    }
}

/// A prompt that only got API errors, at the end of a Claude Code session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedTurn {
    /// The history line of the prompt (`uuid`).
    pub prompt: String,
    /// The line before it: the session goes on from there without the prompt.
    pub kept: String,
    /// The first entry of the turn in [`Transcript::entries`].
    pub start: usize,
}

impl Transcript {
    /// Counts one model call: the totals, the tokens of its model, and the last call.
    /// [`Self::add_call`] for a call the harness named.
    pub fn add_named_call(&mut self, id: &str, model: Option<&str>, usage: Usage, timestamp_ms: i64) {
        self.add_call(model, usage, timestamp_ms);
        if !id.is_empty() {
            let bucket = pricing::bucket(model.unwrap_or("unknown"), &usage, timestamp_ms);
            self.named_calls.push(NamedCall { id: id.to_owned(), bucket, usage, at_ms: timestamp_ms });
        }
    }

    pub fn add_call(&mut self, model: Option<&str>, usage: Usage, timestamp_ms: i64) {
        self.total_usage.add(&usage);
        let bucket = pricing::bucket(model.unwrap_or("unknown"), &usage, timestamp_ms);
        let day = timestamp_ms.div_euclid(DAY_MS);
        match self.usage_by_day.iter_mut().find(|(at, name, _)| *at == day && *name == bucket) {
            Some((_, _, sum)) => sum.add(&usage),
            None => self.usage_by_day.push((day, bucket.clone(), usage)),
        }
        match self.usage_by_model.iter_mut().find(|(name, _)| *name == bucket) {
            Some((_, sum)) => sum.add(&usage),
            None => self.usage_by_model.push((bucket, usage)),
        }
        if usage.context() > 0 {
            self.last_usage = Some(usage);
            self.last_call_ms = Some(self.last_call_ms.map_or(timestamp_ms, |last| last.max(timestamp_ms)));
        }
    }

    /// Leaves out the failed turn of `prompt` when it is the transcript's: the user dropped
    /// it and the next turn resumes before it.
    pub fn hide_failed_turn(&mut self, prompt: &str) -> bool {
        match &self.failed_turn {
            Some(failed) if failed.prompt == prompt && failed.start <= self.entries.len() => {
                self.entries.truncate(failed.start);
                true
            }
            _ => false,
        }
    }

    /// Tokens per bucket of the calls from the UTC day of `since_ms` on.
    pub fn usage_since(&self, since_ms: i64) -> Vec<(String, Usage)> {
        let first = since_ms.div_euclid(DAY_MS);
        let mut sums: Vec<(String, Usage)> = Vec::new();
        for (_, bucket, usage) in self.usage_by_day.iter().filter(|(day, _, _)| *day >= first) {
            match sums.iter_mut().find(|(name, _)| name == bucket) {
                Some((_, sum)) => sum.add(usage),
                None => sums.push((bucket.clone(), *usage)),
            }
        }
        sums
    }

    /// What the session spent from the UTC day of `since_ms` on: its named calls apart, so
    /// that a sum over sessions counts a call two files share once ([`UsageSum`]).
    pub fn spent_since(&self, since_ms: i64) -> SpentSince {
        let first = since_ms.div_euclid(DAY_MS);
        let named: Vec<NamedCall> = self.named_calls.iter().filter(|call| call.at_ms.div_euclid(DAY_MS) >= first).cloned().collect();
        let mut unnamed = self.usage_since(since_ms);
        for call in &named {
            if let Some((_, sum)) = unnamed.iter_mut().find(|(bucket, _)| *bucket == call.bucket) {
                sum.subtract(&call.usage);
            }
        }
        unnamed.retain(|(_, usage)| !usage.is_empty());
        SpentSince { named, unnamed }
    }

    /// Files the session changed, from its edit and write calls, in first-touch order.
    pub fn changes(&self) -> Vec<FileChange> {
        let mut changes: Vec<FileChange> = Vec::new();
        for block in self.entries.iter().flat_map(|entry| &entry.blocks) {
            let Block::ToolCall { name, kind: ToolKind::Edit, input, result, .. } = block else {
                continue;
            };
            if result.as_ref().is_some_and(|result| result.is_error) {
                continue;
            }
            for (path, kind, added, removed) in edit_effects(name, input) {
                match changes.iter_mut().find(|change| change.path == path) {
                    Some(change) => {
                        change.added += added;
                        change.removed += removed;
                        change.edits += 1;
                        if kind == ChangeKind::Deleted {
                            change.kind = ChangeKind::Deleted;
                        }
                    }
                    None => changes.push(FileChange { path, kind, added, removed, edits: 1 }),
                }
            }
        }
        changes
    }

    /// Shell commands and agents the session started. A call without a result still runs
    /// when the session is live, and was cut off otherwise.
    pub fn tasks(&self, live: bool) -> Vec<TaskRun> {
        let mut tasks = Vec::new();
        for entry in &self.entries {
            for block in &entry.blocks {
                let Block::ToolCall { id, name, kind, input, result } = block else {
                    continue;
                };
                if !matches!(kind, ToolKind::Shell | ToolKind::Agent) {
                    continue;
                }
                let title = task_title(name, input);
                let status = match result {
                    Some(result) if result.is_error => TaskStatus::Failed,
                    Some(_) => TaskStatus::Done,
                    None if live => TaskStatus::Running,
                    None => TaskStatus::Failed,
                };
                tasks.push(TaskRun {
                    id: id.clone(),
                    kind: *kind,
                    title,
                    status,
                    started_ms: entry.timestamp_ms,
                    command: (*kind == ToolKind::Shell).then(|| str_field(input, &["command", "cmd", "script"]).map(str::to_owned)).flatten(),
                });
            }
        }
        tasks
    }

    /// The titles of the sessions this one started in a separate session of its host app
    /// (the `start_session` tool of Claude's desktop app): the new session takes the title,
    /// and nothing in its own history names the session that started it.
    pub fn started_sessions(&self) -> Vec<String> {
        let mut titles: Vec<String> = Vec::new();
        for entry in &self.entries {
            for block in &entry.blocks {
                if let Block::ToolCall { name, input, .. } = block
                    && name.ends_with("start_session")
                    && let Some(title) = input.get("title").and_then(Value::as_str).map(str::trim).filter(|title| !title.is_empty())
                    && !titles.iter().any(|known| known == title)
                {
                    titles.push(title.to_owned());
                }
            }
        }
        titles
    }

    /// The text of the session for the search index: prompts and answers, no tool output.
    /// The text for the search index of a long session: its start and its end, about
    /// `limit` bytes in all, so both how it began and what it came to are found.
    pub fn search_text_ends(&self, limit: usize) -> String {
        let all = self.search_text(usize::MAX);
        if all.len() <= limit {
            return all;
        }
        let half = limit / 2;
        let mut head = half;
        while !all.is_char_boundary(head) {
            head -= 1;
        }
        let mut tail = all.len() - half;
        while !all.is_char_boundary(tail) {
            tail += 1;
        }
        format!("{}\n…\n{}", &all[..head], &all[tail..])
    }

    pub fn search_text(&self, limit: usize) -> String {
        let mut text = String::new();
        for entry in &self.entries {
            for block in &entry.blocks {
                if let Block::Text { text: part } = block {
                    if text.len() + part.len() > limit {
                        return text;
                    }
                    text.push_str(part);
                    text.push('\n');
                }
            }
        }
        text
    }
}

const DAY_MS: i64 = 86_400_000;

/// The start of the UTC month holding `ms`, in milliseconds since the Unix epoch.
pub fn month_start_utc_ms(ms: i64) -> i64 {
    let days = ms.div_euclid(DAY_MS);
    let (_, _, day) = civil_from_days(days);
    (days - (day - 1)) * DAY_MS
}

/// The year, month and day of a count of days since 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

/// A time as `2026-11-20 18:42 UTC`, from milliseconds since the Unix epoch.
pub fn format_ms_utc(ms: i64) -> String {
    let seconds = ms.div_euclid(1000);
    let (days, rest) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02} UTC", rest / 3600, rest % 3600 / 60)
}

fn str_field<'a>(input: &'a Value, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|name| input.get(*name).and_then(Value::as_str))
}

fn line_count(text: &str) -> u32 {
    if text.is_empty() { 0 } else { text.lines().count() as u32 }
}

/// What one edit call did to which files: path, kind, lines added and removed.
fn edit_effects(name: &str, input: &Value) -> Vec<(String, ChangeKind, u32, u32)> {
    let lower = name.to_ascii_lowercase();
    if lower == "apply_patch" || lower == "patch" {
        let patch = str_field(input, &["input", "patch", "command"]).unwrap_or_default();
        return patch_effects(patch);
    }
    let Some(path) = str_field(input, &["file_path", "path", "filePath", "notebook_path"]) else {
        return Vec::new();
    };
    let path = path.to_owned();
    match lower.as_str() {
        "write" | "write_file" | "create_file" => {
            let content = str_field(input, &["content", "contents"]).unwrap_or_default();
            vec![(path, ChangeKind::Created, line_count(content), 0)]
        }
        "multiedit" => {
            let (mut added, mut removed) = (0, 0);
            if let Some(edits) = input.get("edits").and_then(Value::as_array) {
                for edit in edits {
                    added += line_count(str_field(edit, &["new_string"]).unwrap_or_default());
                    removed += line_count(str_field(edit, &["old_string"]).unwrap_or_default());
                }
            }
            vec![(path, ChangeKind::Modified, added, removed)]
        }
        _ => {
            let new = str_field(input, &["new_string", "newString", "new_str"]).unwrap_or_default();
            let old = str_field(input, &["old_string", "oldString", "old_str"]).unwrap_or_default();
            vec![(path, ChangeKind::Modified, line_count(new), line_count(old))]
        }
    }
}

/// The files of a patch in the `*** Begin Patch` format of Codex.
fn patch_effects(patch: &str) -> Vec<(String, ChangeKind, u32, u32)> {
    let mut effects: Vec<(String, ChangeKind, u32, u32)> = Vec::new();
    for line in patch.lines() {
        let header = [
            ("*** Add File: ", ChangeKind::Created),
            ("*** Update File: ", ChangeKind::Modified),
            ("*** Delete File: ", ChangeKind::Deleted),
        ]
        .into_iter()
        .find_map(|(prefix, kind)| line.strip_prefix(prefix).map(|path| (path, kind)));
        if let Some((path, kind)) = header {
            effects.push((path.trim().to_owned(), kind, 0, 0));
            continue;
        }
        let Some(current) = effects.last_mut() else { continue };
        if line.starts_with('+') {
            current.2 += 1;
        } else if line.starts_with('-') {
            current.3 += 1;
        }
    }
    effects
}

fn task_title(name: &str, input: &Value) -> String {
    let text = str_field(input, &["description", "command", "cmd", "prompt", "script"])
        .map(str::to_owned)
        .or_else(|| {
            input.get("command").and_then(Value::as_array).map(|parts| {
                parts.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")
            })
        })
        .unwrap_or_else(|| name.to_owned());
    shorten(text.lines().next().unwrap_or(""), 120)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_month_starts_on_its_first_utc_day() {
        // 2026-11-23T18:42:00Z and 2024-02-29T00:00:00Z (a leap day).
        assert_eq!(super::month_start_utc_ms(1_795_459_320_000), 1_793_491_200_000);
        assert_eq!(super::format_ms_utc(super::month_start_utc_ms(1_795_459_320_000)), "2026-11-01 00:00 UTC");
        assert_eq!(super::format_ms_utc(super::month_start_utc_ms(1_709_164_800_000)), "2024-02-01 00:00 UTC");
        assert_eq!(super::month_start_utc_ms(1_793_491_200_000), 1_793_491_200_000, "the first moment is its own start");
    }

    #[test]
    fn usage_since_a_day_keeps_the_calls_from_that_day_on() {
        let mut transcript = super::Transcript::default();
        let call = |output| super::Usage { output, ..Default::default() };
        transcript.add_call(Some("claude-haiku-4-5"), call(10), 1_793_400_000_000); // 2026-10-30
        transcript.add_call(Some("claude-haiku-4-5"), call(5), 1_793_500_000_000); // 2026-11-01
        transcript.add_call(Some("claude-opus-4-5"), call(7), 1_793_600_000_000);
        let month = transcript.usage_since(super::month_start_utc_ms(1_795_459_320_000));
        assert_eq!(month.iter().map(|(name, usage)| (name.as_str(), usage.output)).collect::<Vec<_>>(), vec![("claude-haiku-4-5", 5), ("claude-opus-4-5", 7)]);
        assert_eq!(transcript.usage_since(0), transcript.usage_by_model, "all of it is the whole session");
    }

    use super::*;

    #[test]
    fn times_format_as_utc_dates() {
        assert_eq!(format_ms_utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_ms_utc(1_795_196_570_000), "2026-11-20 17:42 UTC");
        assert_eq!(format_ms_utc(951_782_400_000), "2000-02-29 00:00 UTC");
    }
    use serde_json::json;

    fn call(name: &str, input: Value, result: Option<&str>) -> Block {
        Block::ToolCall {
            id: format!("id-{name}"),
            name: name.to_owned(),
            kind: ToolKind::classify(name),
            input,
            result: result.map(|text| ToolResult { text: text.to_owned(), is_error: false }),
        }
    }

    fn entry(blocks: Vec<Block>) -> Entry {
        Entry {
            id: "e".into(),
            role: Role::Assistant,
            timestamp_ms: 0,
            blocks,
            model: None,
            usage: None,
            compact_summary: false,
        }
    }

    #[test]
    fn edits_add_up_per_file() {
        let transcript = Transcript {
            entries: vec![entry(vec![
                call("Write", json!({"file_path": "a.rs", "content": "1\n2\n3"}), Some("ok")),
                call("Edit", json!({"file_path": "a.rs", "old_string": "1", "new_string": "x\ny"}), Some("ok")),
                call("apply_patch", json!({"input": "*** Begin Patch\n*** Update File: b.rs\n-old\n+new\n+more\n*** End Patch"}), Some("ok")),
            ])],
            ..Default::default()
        };
        let changes = transcript.changes();
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].path, "a.rs");
        assert_eq!((changes[0].added, changes[0].removed, changes[0].edits), (5, 1, 2));
        assert_eq!(changes[0].kind, ChangeKind::Created);
        assert_eq!((changes[1].added, changes[1].removed), (2, 1));
    }

    #[test]
    fn an_unanswered_shell_call_runs_only_in_a_live_session() {
        let transcript = Transcript {
            entries: vec![entry(vec![call("Bash", json!({"command": "cargo test"}), None)])],
            ..Default::default()
        };
        assert_eq!(transcript.tasks(true)[0].status, TaskStatus::Running);
        assert_eq!(transcript.tasks(false)[0].status, TaskStatus::Failed);
        assert_eq!(transcript.tasks(true)[0].title, "cargo test");
    }

    #[test]
    fn keys_survive_the_database() {
        let key = SessionKey {
            env: EnvId::ssh("dev-vm"),
            harness: HarnessKind::OpenCode,
            id: "ses|1".into(),
        };
        assert_eq!(SessionKey::decode(&key.encode()), Some(key));
    }

    #[test]
    fn titles_fall_back_to_the_first_prompt() {
        let summary = SessionSummary {
            key: SessionKey { env: EnvId::local(), harness: HarnessKind::Claude, id: "abcdef123".into() },
            title: String::new(),
            first_prompt: Some("\n  fix the build\nplease".into()),
            cwd: None,
            git_branch: None,
            model: None,
            created_ms: 0,
            updated_ms: 0,
            turn: None,
            parent: None,
            source: SourceStamp::default(),
        };
        assert_eq!(summary.display_title(), "fix the build");
    }
}
