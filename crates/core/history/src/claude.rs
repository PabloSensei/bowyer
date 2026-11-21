//! Claude Code: one JSONL file per session in `~/.claude/projects/<folder>/<id>.jsonl`.
//! The folder name is the working folder with separators replaced, which loses
//! information, so the working folder comes from the `cwd` of the lines. Titles are
//! written again and again as the session goes on: the last one counts, and it sits near
//! the end of the file. Files grow to hundreds of megabytes, so a scan reads the head and
//! the tail only.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use envs::{Environment, ReadRange};
use model::{
    Block, Entry, HarnessKind, Role, SessionKey, SessionSummary, SourceStamp, ToolKind,
    ToolResult, Transcript, TurnState, Usage,
};
use serde_json::Value;

use crate::{HistorySource, KnownSummaries, parse_rfc3339_ms, text_of};

pub struct ClaudeSource;

const HEAD_BYTES: u64 = 96 * 1024;
const TAIL_BYTES: u64 = 256 * 1024;

/// Lines that hold nothing the window draws. Checked on the raw text, before parsing: some
/// of them are the largest lines in the file.
const SKIPPED_TYPES: [&str; 6] = [
    "\"type\":\"api-request",
    "\"type\":\"file-history",
    "\"type\":\"queue-operation\"",
    "\"type\":\"atis-latch\"",
    "\"type\":\"artifact-",
    "\"type\":\"frame-link\"",
];

fn skipped(line: &str) -> bool {
    let mut end = line.len().min(160);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    let head = &line[..end];
    SKIPPED_TYPES.iter().any(|marker| head.contains(marker))
}

/// What a scan learns from the lines it reads.
#[derive(Default)]
struct Facts {
    custom_title: Option<String>,
    ai_title: Option<String>,
    first_prompt: Option<String>,
    cwd: Option<String>,
    branch: Option<String>,
    model: Option<String>,
    first_ms: Option<i64>,
    last_ms: Option<i64>,
    turn: Option<TurnState>,
}

impl Facts {
    fn take(&mut self, value: &Value, from_tail: bool) {
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "custom-title" => {
                if let Some(title) = value.get("customTitle").and_then(Value::as_str) {
                    self.custom_title = Some(title.to_owned());
                }
                return;
            }
            "ai-title" => {
                if let Some(title) = value.get("aiTitle").and_then(Value::as_str) {
                    self.ai_title = Some(title.to_owned());
                }
                return;
            }
            "summary" => {
                if let Some(title) = value.get("summary").and_then(Value::as_str) {
                    self.ai_title.get_or_insert_with(|| title.to_owned());
                }
                return;
            }
            "user" | "assistant" | "system" | "attachment" => {}
            _ => return,
        }
        let cwd = value.get("cwd").and_then(Value::as_str);
        let branch = value.get("gitBranch").and_then(Value::as_str).filter(|b| !b.is_empty());
        if from_tail {
            if let Some(cwd) = cwd {
                self.cwd = Some(cwd.to_owned());
            }
            if let Some(branch) = branch {
                self.branch = Some(branch.to_owned());
            }
        } else {
            if self.cwd.is_none() {
                self.cwd = cwd.map(str::to_owned);
            }
            if self.branch.is_none() {
                self.branch = branch.map(str::to_owned);
            }
        }
        if let Some(ms) = value.get("timestamp").and_then(Value::as_str).and_then(parse_rfc3339_ms) {
            self.first_ms = Some(self.first_ms.map_or(ms, |first| first.min(ms)));
            self.last_ms = Some(self.last_ms.map_or(ms, |last| last.max(ms)));
        }
        if from_tail {
            self.turn = turn_after(value).or(self.turn);
        }
        if kind == "assistant" && self.model.is_none() {
            self.model = value
                .pointer("/message/model")
                .and_then(Value::as_str)
                .filter(|model| !model.starts_with('<'))
                .map(str::to_owned);
        }
        if kind == "user" && self.first_prompt.is_none() && !from_tail && is_human_prompt(value) {
            let text = without_reminders(&text_of(value.pointer("/message/content").unwrap_or(&Value::Null)));
            if !text.is_empty() {
                self.first_prompt = Some(model::shorten(&text, 500));
            }
        }
    }
}

/// Where a turn stands after this line of the history, or `None` when the line says nothing
/// about it (a meta line, a system note).
fn turn_after(value: &Value) -> Option<TurnState> {
    let kind = value.get("type").and_then(Value::as_str)?;
    if value.get("isMeta").and_then(Value::as_bool) == Some(true) || value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let content = value.pointer("/message/content")?;
    let items: Vec<&Value> = match content {
        Value::Array(items) => items.iter().collect(),
        _ => Vec::new(),
    };
    let has = |wanted: &str| items.iter().any(|item| item.get("type").and_then(Value::as_str) == Some(wanted));
    match kind {
        "assistant" if has("tool_use") => Some(TurnState::ToolPending),
        "assistant" if has("text") => Some(TurnState::Idle),
        "assistant" => None, // Thinking alone: the answer is still coming.
        "user" => {
            let text = text_of(content);
            if text.starts_with("[Request interrupted") || text.starts_with("<local-command") || text.starts_with("<command-") {
                Some(TurnState::Idle)
            } else if has("tool_result") || !text.trim().is_empty() || has("image") {
                Some(TurnState::Working)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Where the turn stands after the last bytes of a history, as a scan would say: the
/// first line, likely cut, is skipped.
pub fn turn_of_tail(tail: &[u8]) -> Option<TurnState> {
    let text = String::from_utf8_lossy(tail);
    parse_lines(&text, true, false)
        .filter(|value| matches!(value.get("type").and_then(Value::as_str), Some("user" | "assistant")))
        .fold(None, |turn, value| turn_after(&value).or(turn))
}

/// A prompt a person typed, not a tool result, a meta line or a command echo.
fn is_human_prompt(value: &Value) -> bool {
    if value.get("isMeta").and_then(Value::as_bool) == Some(true)
        || value.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
        || value.get("toolUseResult").is_some()
    {
        return false;
    }
    match value.pointer("/message/content") {
        Some(Value::String(text)) => command_notice(text).is_none(),
        Some(Value::Array(items)) => items
            .iter()
            .any(|item| item.get("type").and_then(Value::as_str) == Some("text")),
        _ => false,
    }
}

fn parse_lines(text: &str, skip_first: bool, skip_last: bool) -> impl Iterator<Item = Value> + '_ {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if skip_last {
        lines.pop();
    }
    let start = usize::from(skip_first && !lines.is_empty());
    lines
        .into_iter()
        .skip(start)
        .filter(|line| !line.trim().is_empty() && !skipped(line))
        .filter_map(|line| serde_json::from_str(line).ok())
}

fn stem(path: &str) -> String {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    name.strip_suffix(".jsonl").unwrap_or(name).to_owned()
}

impl ClaudeSource {
    /// The reads a summary of this file needs: the whole file when it is small, else its
    /// head and its tail.
    fn requests(meta: &envs::FileMeta) -> Vec<(String, ReadRange)> {
        if meta.size <= HEAD_BYTES + TAIL_BYTES {
            vec![(meta.path.clone(), ReadRange::All)]
        } else {
            vec![(meta.path.clone(), ReadRange::Head(HEAD_BYTES)), (meta.path.clone(), ReadRange::Tail(TAIL_BYTES))]
        }
    }

    fn summarize(env: &dyn Environment, meta: &envs::FileMeta, mut parts: Vec<Vec<u8>>) -> SessionSummary {
        let mut facts = Facts::default();
        if parts.len() == 1 {
            let text = String::from_utf8_lossy(&parts.remove(0)).into_owned();
            let values: Vec<Value> = parse_lines(&text, false, false).collect();
            for value in &values {
                facts.take(value, false);
            }
            for value in &values {
                if matches!(value.get("type").and_then(Value::as_str), Some("user" | "assistant")) {
                    facts.take(value, true);
                }
            }
        } else {
            let head = String::from_utf8_lossy(&parts[0]).into_owned();
            for value in parse_lines(&head, false, true) {
                facts.take(&value, false);
            }
            let tail = String::from_utf8_lossy(parts.get(1).map_or(&[][..], Vec::as_slice)).into_owned();
            for value in parse_lines(&tail, true, false) {
                facts.take(&value, true);
            }
        }
        let title = facts.custom_title.or(facts.ai_title).unwrap_or_default();
        SessionSummary {
            key: SessionKey { env: env.id(), harness: HarnessKind::Claude, id: stem(&meta.path) },
            title,
            first_prompt: facts.first_prompt,
            cwd: facts.cwd,
            git_branch: facts.branch,
            model: facts.model,
            created_ms: facts.first_ms.unwrap_or(meta.modified_ms),
            updated_ms: facts.last_ms.unwrap_or(meta.modified_ms).max(meta.modified_ms),
            parent: None,
            source: SourceStamp { path: meta.path.clone(), size: meta.size, modified_ms: meta.modified_ms },
            turn: facts.turn,
        }
    }
}

/// Files read per batch: a remote host answers each batch in one round trip.
const BATCH: usize = 64;

impl HistorySource for ClaudeSource {
    fn harness(&self) -> HarnessKind {
        HarnessKind::Claude
    }

    fn scan(&self, env: &dyn Environment, known: &dyn KnownSummaries, _cache: &Path) -> io::Result<Vec<SessionSummary>> {
        let root = env.join(&env.join(&env.home()?, ".claude"), "projects");
        let files = env.find_files(&root, ".jsonl", 2)?;
        let mut sessions = Vec::with_capacity(files.len());
        let mut changed = Vec::new();
        for meta in files {
            match known.known(&meta.path) {
                Some(summary)
                    if summary.source.size == meta.size && summary.source.modified_ms == meta.modified_ms =>
                {
                    sessions.push(summary)
                }
                _ => changed.push(meta),
            }
        }
        for chunk in changed.chunks(BATCH) {
            let requests: Vec<Vec<(String, ReadRange)>> = chunk.iter().map(Self::requests).collect();
            let flat: Vec<(String, ReadRange)> = requests.iter().flatten().cloned().collect();
            let mut answers = env.read_many(&flat).into_iter();
            for (meta, wanted) in chunk.iter().zip(&requests) {
                let parts: io::Result<Vec<Vec<u8>>> = answers.by_ref().take(wanted.len()).collect();
                match parts {
                    Ok(parts) => {
                        let summary = Self::summarize(env, meta, parts);
                        // A session without a single prompt has nothing to show.
                        if summary.first_prompt.is_some() || !summary.title.is_empty() {
                            sessions.push(summary);
                        }
                    }
                    Err(error) => eprintln!("claude history {}: {error}", meta.path),
                }
            }
        }
        Ok(sessions)
    }

    fn load(&self, env: &dyn Environment, summary: &SessionSummary, _cache: &Path) -> io::Result<Transcript> {
        let bytes = env.read(&summary.source.path, ReadRange::All)?;
        Ok(parse_transcript(&String::from_utf8_lossy(&bytes)))
    }
}

fn usage_of(value: &Value) -> Option<Usage> {
    let usage = value.pointer("/message/usage")?;
    let field = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
    Some(Usage {
        input: field("input_tokens"),
        output: field("output_tokens"),
        cache_read: field("cache_read_input_tokens"),
        cache_write: field("cache_creation_input_tokens"),
        cache_write_long: usage
            .pointer("/cache_creation/ephemeral_1h_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    })
}

/// Reads Claude Code lines one at a time: the history file and the `stream-json` output
/// of `claude -p` have the same shape, so a live turn uses the same reader.
#[derive(Default, Clone)]
pub struct ClaudeParser {
    transcript: Transcript,
    /// Where each tool call sits, to attach its result when the user line brings it.
    calls: HashMap<String, (usize, usize)>,
    /// One message spans several lines that repeat its usage: count it once, at its last line.
    calls_by_message: HashMap<String, (Option<String>, Usage, i64)>,
    call_order: Vec<String>,
    /// The size of the context after the last compaction, and when it happened.
    post_compact: Option<(i64, u64)>,
}

impl ClaudeParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, value: &Value) {
    let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
    let timestamp_ms = value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_ms)
        .unwrap_or(0);
    let id = value.get("uuid").and_then(Value::as_str).unwrap_or("").to_owned();
    match kind {
        "assistant" => {
            let message_id = value.pointer("/message/id").and_then(Value::as_str).unwrap_or("").to_owned();
            let model = value.pointer("/message/model").and_then(Value::as_str).map(str::to_owned);
            let usage = usage_of(&value);
            if let Some(usage) = usage {
                if !self.calls_by_message.contains_key(&message_id) {
                    self.call_order.push(message_id.clone());
                }
                self.calls_by_message.insert(message_id.clone(), (model.clone(), usage, timestamp_ms));
            }
            if let Some(model) = &model
                && !model.starts_with('<')
            {
                self.transcript.model = Some(model.clone());
            }
            let blocks = assistant_blocks(&value);
            // Claude Code writes one line per content block of the same message.
            let same_message = self.transcript.entries.last().is_some_and(|last| {
                last.role == Role::Assistant && !message_id.is_empty() && last.id == message_id
            });
            if !same_message {
                self.transcript.entries.push(Entry {
                    id: message_id,
                    role: Role::Assistant,
                    timestamp_ms,
                    blocks: Vec::new(),
                    model,
                    usage,
                    compact_summary: false,
                });
            }
            let entry_index = self.transcript.entries.len() - 1;
            let entry = &mut self.transcript.entries[entry_index];
            entry.usage = usage.or(entry.usage);
            for block in blocks {
                if let Block::ToolCall { id, .. } = &block {
                    self.calls.insert(id.clone(), (entry_index, entry.blocks.len()));
                }
                entry.blocks.push(block);
            }
        }
        "user" => {
            if value.get("isMeta").and_then(Value::as_bool) == Some(true) {
                return;
            }
            let compact = value.get("isCompactSummary").and_then(Value::as_bool) == Some(true);
            let content = value.pointer("/message/content").cloned().unwrap_or(Value::Null);
            // A slash command and what it printed come as tagged user text: a notice.
            if let Value::String(text) = &content
                && let Some(notice) = command_notice(text)
            {
                if let Some(text) = notice {
                    self.transcript.entries.push(Entry {
                        id,
                        role: Role::System,
                        timestamp_ms,
                        blocks: vec![Block::Notice { text }],
                        model: None,
                        usage: None,
                        compact_summary: false,
                    });
                }
                return;
            }
            let mut blocks = Vec::new();
            match &content {
                Value::String(text) => {
                    let text = without_reminders(text);
                    if !text.is_empty() {
                        blocks.push(Block::Text { text });
                    }
                }
                Value::Array(items) => {
                    for item in items {
                        match item.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                let text = without_reminders(item.get("text").and_then(Value::as_str).unwrap_or(""));
                                if !text.is_empty() {
                                    blocks.push(Block::Text { text });
                                }
                            }
                            Some("image") => blocks.push(Block::Image {
                                media_type: item
                                    .pointer("/source/media_type")
                                    .and_then(Value::as_str)
                                    .unwrap_or("image")
                                    .to_owned(),
                                data: item.pointer("/source/data").and_then(Value::as_str).map(str::to_owned),
                            }),
                            Some("tool_result") => {
                                let tool_id = item.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                                let result = ToolResult {
                                    text: text_of(item.get("content").unwrap_or(&Value::Null)),
                                    is_error: item.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                                };
                                if let Some(&(entry, block)) = self.calls.get(tool_id)
                                    && let Some(Block::ToolCall { result: slot, .. }) =
                                        self.transcript.entries[entry].blocks.get_mut(block)
                                {
                                    *slot = Some(result);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            if !blocks.is_empty() {
                self.transcript.entries.push(Entry {
                    id,
                    role: Role::User,
                    timestamp_ms,
                    blocks,
                    model: None,
                    usage: None,
                    compact_summary: compact,
                });
            }
        }
        "system" => {
            let subtype = value.get("subtype").and_then(Value::as_str).unwrap_or("");
            let text = match subtype {
                "compact_boundary" => {
                    // The history file writes camelCase, the stream of `claude -p` snake_case.
                    let tokens = value
                        .pointer("/compactMetadata/postTokens")
                        .or_else(|| value.pointer("/compact_metadata/post_tokens"))
                        .and_then(Value::as_u64);
                    if let Some(tokens) = tokens {
                        self.post_compact = Some((timestamp_ms, tokens));
                    }
                    Some("Context compacted".to_owned())
                }
                "api_error" => value.get("content").and_then(Value::as_str).map(str::to_owned),
                _ if value.get("level").and_then(Value::as_str) == Some("error") => {
                    value.get("content").and_then(Value::as_str).map(str::to_owned)
                }
                _ => None,
            };
            if let Some(text) = text {
                self.transcript.entries.push(Entry {
                    id,
                    role: Role::System,
                    timestamp_ms,
                    blocks: vec![Block::Notice { text }],
                    model: None,
                    usage: None,
                    compact_summary: false,
                });
            }
        }
        "result" => {
            // The end of a turn of `claude -p`: the cost of this run.
            if let Some(cost) = value.get("total_cost_usd").and_then(Value::as_f64) {
                *self.transcript.cost_usd.get_or_insert(0.0) += cost;
            }
        }
        "cost-state" => {
            self.transcript.cost_usd = value.get("totalCostUSD").and_then(Value::as_f64);
        }
        "tools-used" => {
            if let Some(tools) = value.get("tools").and_then(Value::as_array) {
                self.transcript.tools_seen =
                    tools.iter().filter_map(Value::as_str).map(str::to_owned).collect();
            }
        }
        _ => {}
    }
    }

    /// The transcript so far, with every model call counted.
    pub fn snapshot(&self) -> Transcript {
        self.clone().finish()
    }

    pub fn finish(mut self) -> Transcript {
        for id in std::mem::take(&mut self.call_order) {
            if let Some((model, usage, timestamp_ms)) = self.calls_by_message.remove(&id) {
                self.transcript.add_call(model.as_deref().filter(|m| !m.starts_with('<')), usage, timestamp_ms);
            }
        }
        // A compaction after the last call: the context is its summary until the next call.
        if let Some((at, tokens)) = self.post_compact
            && self.transcript.last_call_ms.is_none_or(|last| at >= last)
        {
            self.transcript.last_usage = Some(Usage { input: tokens, ..Usage::default() });
        }
        self.transcript
    }
}

/// A session file read up to its last whole line, which reads on from there when the file
/// grows: a session that runs in a terminal while it is open costs the new lines, not the
/// whole file again.
#[derive(Clone)]
pub struct ClaudeFollower {
    path: String,
    parser: ClaudeParser,
    /// The bytes read and fed, always up to the end of a line.
    offset: u64,
    /// Where the turn stands after the last line that tells.
    turn: Option<TurnState>,
}

impl ClaudeFollower {
    /// Reads the whole file.
    pub fn open(env: &dyn Environment, path: &str) -> io::Result<Self> {
        let mut follower = Self { path: path.to_owned(), parser: ClaudeParser::new(), offset: 0, turn: None };
        follower.feed(&env.read(path, ReadRange::All)?);
        Ok(follower)
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// The bytes read so far.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Reads what was written since the last time. A file that got shorter was written
    /// anew and is read from the start. Returns whether anything new came.
    pub fn read_on(&mut self, env: &dyn Environment) -> io::Result<bool> {
        if env.stat(&self.path)?.size < self.offset {
            *self = Self::open(env, &self.path)?;
            return Ok(true);
        }
        let before = self.offset;
        self.feed(&env.read(&self.path, ReadRange::From(self.offset))?);
        Ok(self.offset > before)
    }

    /// Feeds the whole lines of `bytes`; a line still being written waits for its end.
    fn feed(&mut self, bytes: &[u8]) {
        let Some(end) = bytes.iter().rposition(|&b| b == b'\n') else { return };
        let text = String::from_utf8_lossy(&bytes[..=end]);
        for value in parse_lines(&text, false, false) {
            self.turn = turn_after(&value).or(self.turn);
            self.parser.feed(&value);
        }
        self.offset += end as u64 + 1;
    }

    /// Where the turn stands, as a scan of the file would say.
    pub fn turn(&self) -> Option<TurnState> {
        self.turn
    }

    /// The transcript so far.
    pub fn transcript(&self) -> Transcript {
        self.parser.snapshot()
    }
}

/// A user's text without the `<system-reminder>` blocks the harness adds to it for the model
/// (where a worktree is, what changed in a file): they are not what the user wrote.
fn without_reminders(text: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    if !text.contains(OPEN) {
        return text.trim().to_owned();
    }
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        match rest[start..].find(CLOSE) {
            Some(end) => rest = &rest[start + end + CLOSE.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_owned()
}

/// The text between `<name>` and `</name>`.
fn tag<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let start = text.find(&format!("<{name}>"))? + name.len() + 2;
    let end = start + text[start..].find(&format!("</{name}>"))?;
    Some(&text[start..end])
}

/// What a line of a slash command shows: `Some(Some(notice))` for the command and its
/// output, `Some(None)` for a line to leave out, `None` for an ordinary message.
fn command_notice(text: &str) -> Option<Option<String>> {
    let text = text.trim_start();
    if let Some(name) = tag(text, "command-name") {
        let args = tag(text, "command-args").unwrap_or("").trim();
        let line = if args.is_empty() { name.trim().to_owned() } else { format!("{} {args}", name.trim()) };
        return Some(Some(format!("⌘ {line}")));
    }
    for name in ["local-command-stdout", "local-command-stderr"] {
        if text.starts_with(&format!("<{name}>")) {
            let output = tag(text, name).unwrap_or("").trim();
            return Some((!output.is_empty()).then(|| output.to_owned()));
        }
    }
    // The harness tells the model that a background command or agent finished: one line.
    if text.starts_with("<task-notification>") {
        let summary = tag(text, "summary").map(str::trim).filter(|summary| !summary.is_empty());
        let status = tag(text, "status").map(str::trim).unwrap_or_default();
        return Some(Some(match summary {
            Some(summary) => format!("◷ {summary}"),
            None => format!("◷ task {status}").trim_end().to_owned(),
        }));
    }
    text.starts_with("<local-command-caveat>").then_some(None)
}

/// The whole transcript of a session file.
pub fn parse_transcript(text: &str) -> Transcript {
    let mut parser = ClaudeParser::new();
    for value in parse_lines(text, false, false) {
        parser.feed(&value);
    }
    parser.finish()
}

fn assistant_blocks(value: &Value) -> Vec<Block> {
    let Some(items) = value.pointer("/message/content").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let text = |name: &str| item.get(name).and_then(Value::as_str).unwrap_or("").to_owned();
            match item.get("type").and_then(Value::as_str)? {
                "text" => Some(Block::Text { text: text("text") }),
                "thinking" => {
                    let thinking = text("thinking");
                    (!thinking.is_empty()).then_some(Block::Thinking { text: thinking })
                }
                "tool_use" | "server_tool_use" => {
                    let name = text("name");
                    Some(Block::ToolCall {
                        id: text("id"),
                        kind: ToolKind::classify(&name),
                        name,
                        input: item.get("input").cloned().unwrap_or(Value::Null),
                        result: None,
                    })
                }
                _ => None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use envs::LocalEnvironment;
    use test_support::TempDir;

    #[test]
    fn the_end_of_a_history_tells_where_the_turn_stands() {
        let line = |text: &str| serde_json::from_str::<Value>(text).unwrap();
        let call = line(r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t","name":"Bash","input":{}}]}}"#);
        let result = line(r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]}}"#);
        let answer = line(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Done."}]}}"#);
        let thinking = line(r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hm"}]}}"#);
        let stop = line(r#"{"type":"user","message":{"content":"[Request interrupted by user]"}}"#);
        let meta = line(r#"{"type":"user","isMeta":true,"message":{"content":"caveat"}}"#);
        assert_eq!(turn_after(&call), Some(TurnState::ToolPending));
        assert_eq!(turn_after(&result), Some(TurnState::Working));
        assert_eq!(turn_after(&answer), Some(TurnState::Idle));
        assert_eq!(turn_after(&thinking), None);
        assert_eq!(turn_after(&stop), Some(TurnState::Idle));
        assert_eq!(turn_after(&meta), None);

        let tail = b"t\":\"cut\"}}\n{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"Bash\",\"input\":{}}]}}\n{\"type\":\"user\",\"isMeta\":true,\"message\":{\"content\":\"caveat\"}}\n";
        assert_eq!(turn_of_tail(tail), Some(TurnState::ToolPending), "a meta line keeps the state");
        assert_eq!(turn_of_tail(b""), None);
    }

    /// A synthetic session in the shape Claude Code writes.
    const SESSION: &str = r#"{"type":"queue-operation","operation":"enqueue","content":"hi","sessionId":"s1"}
{"parentUuid":null,"type":"user","message":{"role":"user","content":"Fix the failing test in parser.rs"},"uuid":"u1","timestamp":"2026-11-01T10:00:00.000Z","cwd":"/work/app","gitBranch":"main","sessionId":"s1"}
{"type":"assistant","message":{"id":"m1","model":"claude-opus-5-5","content":[{"type":"thinking","thinking":"Let me look."}],"usage":{"input_tokens":5,"output_tokens":10,"cache_read_input_tokens":1000,"cache_creation_input_tokens":200}},"uuid":"a1","timestamp":"2026-11-01T10:00:01.000Z","cwd":"/work/app"}
{"type":"assistant","message":{"id":"m1","model":"claude-opus-5-5","content":[{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"/work/app/parser.rs","old_string":"a","new_string":"b\nc"}}],"usage":{"input_tokens":5,"output_tokens":10,"cache_read_input_tokens":1000,"cache_creation_input_tokens":200}},"uuid":"a2","timestamp":"2026-11-01T10:00:02.000Z"}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]},"toolUseResult":{},"uuid":"u2","timestamp":"2026-11-01T10:00:03.000Z"}
{"type":"assistant","message":{"id":"m2","model":"claude-opus-5-5","content":[{"type":"text","text":"Fixed."}],"usage":{"input_tokens":1,"output_tokens":3,"cache_read_input_tokens":1500,"cache_creation_input_tokens":0}},"uuid":"a3","timestamp":"2026-11-01T10:00:04.000Z","gitBranch":"fix/parser"}
{"type":"custom-title","customTitle":"Parser fix","sessionId":"s1"}
{"type":"cost-state","totalCostUSD":0.25}
"#;

    #[test]
    fn a_transcript_merges_split_messages_and_attaches_results() {
        let transcript = parse_transcript(SESSION);
        assert_eq!(transcript.entries.len(), 3);
        let first = &transcript.entries[1];
        assert_eq!(first.blocks.len(), 2, "thinking and the edit share one message");
        let Block::ToolCall { result, kind, .. } = &first.blocks[1] else { panic!("a call") };
        assert_eq!(*kind, ToolKind::Edit);
        assert_eq!(result.as_ref().map(|r| r.text.as_str()), Some("ok"));
        assert_eq!(transcript.last_usage.map(|u| u.context()), Some(1501));
        assert_eq!(transcript.total_usage.output, 13, "a split message counts once");
        assert_eq!(transcript.cost_usd, Some(0.25));
        assert_eq!(transcript.changes()[0].added, 2);
    }

    #[test]
    fn a_follower_reads_on_as_the_file_grows() {
        let dir = TempDir::new("claude_follow");
        let path = dir.path().join("s1.jsonl");
        let path_text = path.to_string_lossy().into_owned();
        let env = LocalEnvironment::new();
        // The file ends in the middle of the edit line, as a harness writing it would leave it.
        let cut = SESSION.find("\"name\":\"Edit\"").unwrap();
        std::fs::write(&path, &SESSION[..cut]).unwrap();
        let mut follower = ClaudeFollower::open(&env, &path_text).unwrap();
        let start = follower.offset();
        assert!(start > 0 && start < cut as u64, "the half line waits");
        assert!(!follower.read_on(&env).unwrap(), "nothing new");

        std::fs::write(&path, SESSION).unwrap();
        assert!(follower.read_on(&env).unwrap());
        assert_eq!(follower.offset(), SESSION.len() as u64);
        assert_eq!(follower.turn(), Some(TurnState::Idle), "the answer ends the turn");
        let whole = parse_transcript(SESSION);
        let followed = follower.transcript();
        assert_eq!(followed.entries.len(), whole.entries.len());
        assert_eq!(followed.total_usage.output, whole.total_usage.output, "the split message still counts once");
        assert_eq!(followed.changes()[0].added, 2);

        // Written anew and shorter: read from the start.
        let first = SESSION.lines().take(2).map(|line| format!("{line}\n")).collect::<String>();
        std::fs::write(&path, &first).unwrap();
        assert!(follower.read_on(&env).unwrap());
        assert_eq!(follower.offset(), first.len() as u64);
        assert_eq!(follower.transcript().entries.len(), 1);
    }

    /// The same over SSH, on a temporary file of a real host:
    /// `BOWYER_SSH_HOST=user@host BOWYER_SSH_KEY=~/.ssh/key cargo test -p history -- --ignored`.
    #[test]
    #[ignore = "needs an SSH host"]
    fn a_follower_reads_on_over_ssh() {
        let host = std::env::var("BOWYER_SSH_HOST").expect("BOWYER_SSH_HOST");
        let key = std::env::var_os("BOWYER_SSH_KEY").map(std::path::PathBuf::from);
        let env = envs::SshEnvironment::new(&host).with_identity(key);
        let path = env.run("mktemp", &["-t", "bowyer-follow.XXXXXX"], None).unwrap().trim().to_owned();
        let write = |text: &str, append: bool| {
            let script = if append { "cat >> \"$1\"" } else { "cat > \"$1\"" };
            env.run_with_input("sh", &["-c", script, "sh", &path], None, text.as_bytes()).unwrap();
        };
        let cut = SESSION.find("\"name\":\"Edit\"").unwrap();
        write(&SESSION[..cut], false);
        let mut follower = ClaudeFollower::open(&env, &path).unwrap();
        let start = follower.offset();
        assert!(start > 0 && start < cut as u64);
        assert!(!follower.read_on(&env).unwrap());
        write(&SESSION[cut..], true);
        assert!(follower.read_on(&env).unwrap());
        assert_eq!(follower.offset(), SESSION.len() as u64);
        let whole = parse_transcript(SESSION);
        assert_eq!(follower.transcript().entries.len(), whole.entries.len());
        assert_eq!(follower.transcript().total_usage.output, whole.total_usage.output);
        write("{\"type\":\"queue-operation\"}\n", false);
        assert!(follower.read_on(&env).unwrap(), "shorter: read anew");
        assert_eq!(follower.transcript().entries.len(), 0);
        env.run("rm", &["-f", &path], None).unwrap();
    }

    #[test]
    fn a_compaction_after_the_last_call_sets_the_context() {
        let boundary = r#"{"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"manual","preTokens":50021,"postTokens":10066},"timestamp":"2026-11-01T10:05:00.000Z"}"#;
        let compacted = parse_transcript(&format!("{SESSION}{boundary}\n"));
        assert_eq!(compacted.last_usage.map(|u| u.context()), Some(10066));
        let after = r#"{"type":"assistant","message":{"id":"m9","model":"claude-opus-5-5","content":[{"type":"text","text":"Next."}],"usage":{"input_tokens":3,"output_tokens":2,"cache_read_input_tokens":12000,"cache_creation_input_tokens":0}},"timestamp":"2026-11-01T10:06:00.000Z"}"#;
        let answered = parse_transcript(&format!("{SESSION}{boundary}\n{after}\n"));
        assert_eq!(answered.last_usage.map(|u| u.context()), Some(12003), "a later call measures it again");
    }

    #[test]
    fn slash_commands_and_their_output_become_notices() {
        assert_eq!(
            command_notice("<command-name>/compact</command-name>\n<command-message>compact</command-message>\n<command-args>keep the plan</command-args>"),
            Some(Some("⌘ /compact keep the plan".to_owned()))
        );
        assert_eq!(command_notice("<local-command-stdout>Compacted </local-command-stdout>"), Some(Some("Compacted".to_owned())));
        assert_eq!(command_notice("<local-command-stdout></local-command-stdout>"), Some(None));
        assert_eq!(command_notice("<local-command-caveat>Caveat: ...</local-command-caveat>"), Some(None));
        assert_eq!(command_notice("Fix the <command-name> parser"), None);
        assert_eq!(without_reminders("<system-reminder>
You are in a worktree.
</system-reminder>
Task 0.16: previews"), "Task 0.16: previews");
        assert_eq!(without_reminders("Fix it<system-reminder>a</system-reminder> now<system-reminder>cut"), "Fix it now");
        assert_eq!(without_reminders("<system-reminder>only</system-reminder>"), "");
        assert_eq!(
            command_notice("<task-notification>
<task-id>b1</task-id>
<status>completed</status>
<summary>Background command \"Run checks\" completed (exit code 0)</summary>
</task-notification>"),
            Some(Some("◷ Background command \"Run checks\" completed (exit code 0)".to_owned()))
        );
        assert_eq!(command_notice("<task-notification><status>failed</status></task-notification>"), Some(Some("◷ task failed".to_owned())));

        let line = r#"{"type":"user","message":{"role":"user","content":"<command-name>/model</command-name>\n<command-args>haiku</command-args>"},"timestamp":"2026-11-01T10:07:00.000Z"}"#;
        let transcript = parse_transcript(&format!("{SESSION}{line}\n"));
        let last = transcript.entries.last().unwrap();
        assert_eq!(last.role, Role::System);
        assert!(matches!(&last.blocks[0], Block::Notice { text } if text == "⌘ /model haiku"));
    }

    #[test]
    fn a_scan_takes_the_last_title_and_branch() {
        let dir = TempDir::new("claude_scan");
        let projects = dir.path().join(".claude/projects/-work-app");
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::write(projects.join("s1.jsonl"), SESSION).unwrap();
        std::fs::write(projects.join("empty.jsonl"), "{\"type\":\"queue-operation\"}\n").unwrap();
        let env = LocalEnvironment::with_home(dir.path());
        let sessions = ClaudeSource.scan(&env, &(), dir.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.title, "Parser fix");
        assert_eq!(session.key.id, "s1");
        assert_eq!(session.cwd.as_deref(), Some("/work/app"));
        assert_eq!(session.git_branch.as_deref(), Some("fix/parser"));
        assert_eq!(session.first_prompt.as_deref(), Some("Fix the failing test in parser.rs"));
        assert_eq!(session.model.as_deref(), Some("claude-opus-5-5"));
    }
}
