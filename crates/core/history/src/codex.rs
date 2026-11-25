//! Codex CLI: rollout files in `~/.codex/sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl`.
//! Each line is `{timestamp, type, payload}`: `session_meta` first, then `response_item`
//! lines in the Responses API shape, `event_msg` with token counts and `turn_context`.
//!
//! UNVERIFIED: written from the published format; no Codex session existed on the
//! development machine. The tests use a synthetic file of that shape.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use envs::{Environment, ReadRange};
use model::{
    Block, Entry, HarnessKind, Role, SessionKey, SessionSummary, SourceStamp, ToolKind, ToolResult,
    Transcript, TurnState, Usage,
};
use serde_json::Value;

use crate::{HistorySource, KnownSummaries, parse_rfc3339_ms, text_of};

pub struct CodexSource;

const HEAD_BYTES: u64 = 64 * 1024;
/// How much is read when the first prompt is not in [`HEAD_BYTES`].
const LONG_HEAD_BYTES: u64 = 16 * 1024 * 1024;

/// Text the CLI injects as user messages: instructions and the environment.
fn injected(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with("<environment_context>")
        || text.starts_with("<user_instructions>")
        || text.starts_with("# AGENTS.md")
        || text.starts_with("<permissions")
}

fn file_id(path: &str) -> String {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let name = name.strip_suffix(".jsonl").unwrap_or(name);
    // rollout-2025-01-01T10-00-00-<uuid>: the uuid is the last 36 characters (counted as
    // characters: a name of another kind may hold letters of more than one byte).
    match name.char_indices().rev().nth(35) {
        Some((start, _)) if start > 0 => name[start..].to_owned(),
        _ => name.to_owned(),
    }
}

/// What the start of a session file says of it.
fn summary_of_head(env: &dyn Environment, meta: &envs::FileMeta, head: &str) -> SessionSummary {
    let mut summary = SessionSummary {
        key: SessionKey { env: env.id(), harness: HarnessKind::Codex, id: file_id(&meta.path) },
        title: String::new(),
        first_prompt: None,
        cwd: None,
        git_branch: None,
        model: None,
        created_ms: meta.modified_ms,
        updated_ms: meta.modified_ms,
        turn: None,
        parent: None,
        source: SourceStamp { path: meta.path.clone(), size: meta.size, modified_ms: meta.modified_ms },
    };
    for line in head.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else { continue };
        let payload = value.get("payload").unwrap_or(&Value::Null);
        match value.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                if let Some(id) = payload.get("id").and_then(Value::as_str) {
                    summary.key.id = id.to_owned();
                }
                summary.cwd = payload.get("cwd").and_then(Value::as_str).map(str::to_owned);
                summary.git_branch = payload.pointer("/git/branch").and_then(Value::as_str).map(str::to_owned);
                if let Some(ms) = payload.get("timestamp").and_then(Value::as_str).and_then(parse_rfc3339_ms) {
                    summary.created_ms = ms;
                }
            }
            Some("turn_context") if summary.model.is_none() => {
                summary.model = payload.get("model").and_then(Value::as_str).map(str::to_owned);
            }
            Some("response_item") if summary.first_prompt.is_none() => {
                if payload.get("role").and_then(Value::as_str) == Some("user") {
                    let text = text_of(payload.get("content").unwrap_or(&Value::Null));
                    if !text.trim().is_empty() && !injected(&text) {
                        summary.first_prompt = Some(model::shorten(text.trim(), 500));
                    }
                }
            }
            _ => {}
        }
    }
    summary
}

impl HistorySource for CodexSource {
    fn harness(&self) -> HarnessKind {
        HarnessKind::Codex
    }

    fn scan(&self, env: &dyn Environment, known: &dyn KnownSummaries, _cache: &Path) -> io::Result<Vec<SessionSummary>> {
        let codex = env.join(&env.home()?, ".codex");
        let mut files = env.find_files(&env.join(&codex, "sessions"), ".jsonl", 4)?;
        files.extend(env.find_files(&env.join(&codex, "archived_sessions"), ".jsonl", 4)?);
        let mut sessions = Vec::new();
        for meta in files {
            let known = known.known(&meta.path);
            if let Some(summary) = &known
                && summary.source.size == meta.size
                && summary.source.modified_ms == meta.modified_ms
            {
                sessions.push(summary.clone());
                continue;
            }
            // One file that does not read stops nothing else; it keeps what was known of it.
            // The first prompt is mostly near the start; a long first line (the instructions,
            // a picture) pushes it further, and then more of the file is read.
            let mut read = HEAD_BYTES;
            let summary = loop {
                let head = match env.read(&meta.path, ReadRange::Head(read)) {
                    Ok(head) => head,
                    Err(error) => {
                        eprintln!("codex history {}: {error}", meta.path);
                        break None;
                    }
                };
                let summary = summary_of_head(env, &meta, &String::from_utf8_lossy(&head));
                if summary.first_prompt.is_some() || meta.size <= read || read >= LONG_HEAD_BYTES {
                    break Some(summary);
                }
                read = LONG_HEAD_BYTES;
            };
            let Some(mut summary) = summary else {
                sessions.extend(known);
                continue;
            };
            if summary.first_prompt.is_some() {
                // Where the turn stands, for a session written lately: from the end of its file.
                if meta.modified_ms >= crate::now_ms() - TURN_WINDOW_MS
                    && let Ok(tail) = env.read(&meta.path, ReadRange::Tail(TAIL_BYTES))
                {
                    summary.turn = codex_turn_of_tail(&tail);
                }
                sessions.push(summary);
            }
        }
        Ok(sessions)
    }

    fn load(&self, env: &dyn Environment, summary: &SessionSummary, _cache: &Path) -> io::Result<Transcript> {
        let bytes = env.read(&summary.source.path, ReadRange::All)?;
        Ok(parse_transcript(&String::from_utf8_lossy(&bytes)))
    }
}

fn usage_of(value: &Value) -> Usage {
    let field = |name: &str| value.get(name).and_then(Value::as_u64).unwrap_or(0);
    let cached = field("cached_input_tokens");
    Usage {
        input: field("input_tokens").saturating_sub(cached),
        output: field("output_tokens"),
        cache_read: cached,
        cache_write: 0,
        cache_write_long: 0,
    }
}

/// The end of a Codex session from its last bytes, for a preview: a line cut at the start or
/// still being written does not read and is left out. Entries are named by when they were
/// written, so that the same entry keeps its name in a later, longer or shorter, read.
pub fn codex_tail(text: &str) -> Transcript {
    let mut transcript = parse_transcript(text);
    let mut seen: HashMap<i64, usize> = HashMap::new();
    for entry in &mut transcript.entries {
        let n = seen.entry(entry.timestamp_ms).or_default();
        entry.id = format!("{}#{n}", entry.timestamp_ms);
        *n += 1;
    }
    transcript
}

pub fn parse_transcript(text: &str) -> Transcript {
    let mut transcript = Transcript::default();
    let mut calls: HashMap<String, (usize, usize)> = HashMap::new();
    let mut last_total: Option<Value> = None;
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else { continue };
        let timestamp_ms = value.get("timestamp").and_then(Value::as_str).and_then(parse_rfc3339_ms).unwrap_or(0);
        let payload = value.get("payload").unwrap_or(&Value::Null);
        let payload_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
        match value.get("type").and_then(Value::as_str).unwrap_or("") {
            "turn_context" => {
                if let Some(model) = payload.get("model").and_then(Value::as_str) {
                    transcript.model = Some(model.to_owned());
                }
            }
            "compacted" => push(&mut transcript, Role::User, timestamp_ms, Block::Text {
                text: payload.get("message").and_then(Value::as_str).unwrap_or("").to_owned(),
            }, true),
            "event_msg" if payload_type == "token_count" => {
                // Codex repeats its last count when only the rate limits changed: a call is
                // a count whose total moved.
                let total = payload.pointer("/info/total_token_usage");
                if total.is_some() && total == last_total.as_ref() {
                    continue;
                }
                last_total = total.cloned();
                if let Some(last) = payload.pointer("/info/last_token_usage") {
                    // The per-call record prices the session and its days.
                    let model = transcript.model.clone();
                    transcript.add_call(model.as_deref(), usage_of(last), timestamp_ms);
                }
                // The totals come from the harness.
                if let Some(total) = total {
                    transcript.total_usage = usage_of(total);
                }
            }
            "response_item" => match payload_type {
                "message" => {
                    let role = match payload.get("role").and_then(Value::as_str) {
                        Some("user") => Role::User,
                        Some("assistant") => Role::Assistant,
                        _ => continue,
                    };
                    let text = text_of(payload.get("content").unwrap_or(&Value::Null));
                    if text.trim().is_empty() || (role == Role::User && injected(&text)) {
                        continue;
                    }
                    push(&mut transcript, role, timestamp_ms, Block::Text { text }, false);
                }
                "reasoning" => {
                    let text = text_of(payload.get("summary").unwrap_or(&Value::Null));
                    if !text.is_empty() {
                        push(&mut transcript, Role::Assistant, timestamp_ms, Block::Thinking { text }, false);
                    }
                }
                "function_call" | "custom_tool_call" | "local_shell_call" => {
                    let name = payload.get("name").and_then(Value::as_str).unwrap_or("shell").to_owned();
                    let input = match payload.get("arguments").and_then(Value::as_str) {
                        Some(arguments) => serde_json::from_str(arguments).unwrap_or(Value::String(arguments.to_owned())),
                        None => match payload.get("input") {
                            Some(Value::String(text)) => serde_json::json!({ "input": text }),
                            Some(other) => other.clone(),
                            None => payload.get("action").cloned().unwrap_or(Value::Null),
                        },
                    };
                    let id = payload.get("call_id").and_then(Value::as_str).unwrap_or("").to_owned();
                    let block = Block::ToolCall { id: id.clone(), kind: ToolKind::classify(&name), name, input, result: None };
                    push(&mut transcript, Role::Assistant, timestamp_ms, block, false);
                    let entry = transcript.entries.len() - 1;
                    calls.insert(id, (entry, transcript.entries[entry].blocks.len() - 1));
                }
                "function_call_output" | "custom_tool_call_output" => {
                    let id = payload.get("call_id").and_then(Value::as_str).unwrap_or("");
                    let raw = payload.get("output").cloned().unwrap_or(Value::Null);
                    // Shell output comes as JSON text with an exit code inside.
                    let parsed = raw.as_str().and_then(|text| serde_json::from_str::<Value>(text).ok());
                    let (text, is_error) = match &parsed {
                        Some(object) if object.get("output").is_some() => (
                            text_of(object.get("output").unwrap_or(&Value::Null)),
                            object.pointer("/metadata/exit_code").and_then(Value::as_i64).is_some_and(|code| code != 0),
                        ),
                        _ => (text_of(&raw), false),
                    };
                    if let Some(&(entry, block)) = calls.get(id)
                        && let Some(Block::ToolCall { result, .. }) = transcript.entries[entry].blocks.get_mut(block)
                    {
                        *result = Some(ToolResult { text, is_error });
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    transcript
}

/// Appends a block to the last entry when the role matches, or starts an entry.
fn push(transcript: &mut Transcript, role: Role, timestamp_ms: i64, block: Block, compact: bool) {
    let joins = !compact
        && role == Role::Assistant
        && transcript.entries.last().is_some_and(|last| last.role == role && !last.compact_summary);
    if joins {
        transcript.entries.last_mut().unwrap().blocks.push(block);
        return;
    }
    transcript.entries.push(Entry {
        id: format!("{}", transcript.entries.len()),
        role,
        timestamp_ms,
        blocks: vec![block],
        model: None,
        usage: None,
        compact_summary: compact,
    });
}

/// A session written this long ago or later shows where its turn stands.
const TURN_WINDOW_MS: i64 = 3 * 3_600_000;
/// The end of a session read for its turn.
const TAIL_BYTES: u64 = 64 * 1024;

/// Where the turn of a Codex session stands, from the end of its file: the last line that
/// tells. The end of a task (`task_complete`, `turn_aborted`) or an answer of the model is
/// the user's turn; a call of a tool or a request for approval waits on it; a prompt, a
/// tool's output or reasoning means the model works.
///
/// UNVERIFIED on real sessions: the event names come from the documentation of Codex.
pub fn codex_turn_of_tail(tail: &[u8]) -> Option<TurnState> {
    let text = String::from_utf8_lossy(tail);
    for line in text.lines().rev() {
        let Ok(value) = serde_json::from_str::<Value>(line) else { continue };
        let payload = value.get("payload").unwrap_or(&Value::Null);
        let kind = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let verdict = match value.get("type").and_then(Value::as_str) {
            Some("event_msg") => match kind {
                "task_complete" | "turn_aborted" => Some(TurnState::Idle),
                "task_started" => Some(TurnState::Working),
                "exec_approval_request" | "apply_patch_approval_request" => Some(TurnState::ToolPending),
                _ => None,
            },
            Some("response_item") => match kind {
                "message" => match payload.get("role").and_then(Value::as_str) {
                    Some("assistant") => Some(TurnState::Idle),
                    Some("user") => Some(TurnState::Working),
                    _ => None,
                },
                "function_call" | "custom_tool_call" | "local_shell_call" => Some(TurnState::ToolPending),
                "function_call_output" | "custom_tool_call_output" | "reasoning" => Some(TurnState::Working),
                _ => None,
            },
            _ => None,
        };
        if verdict.is_some() {
            return verdict;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    /// The end of a session read twice, the second time from further on: an entry keeps its
    /// name, and a line cut at the start is left out.
    #[test]
    fn an_entry_keeps_its_name_from_one_read_of_the_end_to_the_next() {
        use super::codex_tail;
        let user = |at: &str, text: &str| format!(r#"{{"timestamp":"{at}","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{text}"}}]}}}}"#);
        let answer = |at: &str, text: &str| format!(r#"{{"timestamp":"{at}","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"{text}"}}]}}}}"#);
        let lines = [user("2026-01-01T10:00:00Z", "one"), answer("2026-01-01T10:00:05Z", "two"), user("2026-01-01T10:01:00Z", "three")];
        let whole = codex_tail(&lines.join("\n"));
        let cut = codex_tail(&format!("stamp\":\"2026-01-01\"}}\n{}\n{}", lines[1], lines[2]));
        assert_eq!(whole.entries.len(), 3);
        assert_eq!(cut.entries.len(), 2, "the cut line is left out");
        assert_eq!(whole.entries[1].id, cut.entries[0].id);
        assert_eq!(whole.entries[2].id, cut.entries[1].id);
    }

    #[test]
    fn a_file_name_of_any_letters_gives_an_id() {
        use super::file_id;
        let uuid = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";
        assert_eq!(file_id(&format!("/h/.codex/sessions/2025/01/01/rollout-2025-01-01T10-00-00-{uuid}.jsonl")), uuid);
        // 38 letters in 63 bytes: the 36th byte from the end is the second half of a `ü`.
        let umlauts = format!("/h/.codex/sessions/{}{}.jsonl", "ü".repeat(25), "a".repeat(13));
        assert_eq!(file_id(&umlauts).chars().count(), 36, "no panic in a letter");
        assert_eq!(file_id("/h/.codex/sessions/kürzer.jsonl"), "kürzer");
    }

    #[test]
    fn the_last_line_that_tells_gives_the_turn() {
        use super::codex_turn_of_tail;
        use model::TurnState;
        let line = |kind: &str, payload: &str| format!(r#"{{"timestamp":"2026-11-21T10:00:00Z","type":"{kind}","payload":{payload}}}"#);
        let prompt = line("response_item", r#"{"type":"message","role":"user","content":[]}"#);
        let call = line("response_item", r#"{"type":"function_call","name":"shell","arguments":"{}","call_id":"c1"}"#);
        let output = line("response_item", r#"{"type":"function_call_output","call_id":"c1","output":"ok"}"#);
        let answer = line("response_item", r#"{"type":"message","role":"assistant","content":[]}"#);
        let tokens = line("event_msg", r#"{"type":"token_count"}"#);
        let done = line("event_msg", r#"{"type":"task_complete"}"#);
        let turn = |lines: &[&String]| codex_turn_of_tail(lines.iter().map(|l| format!("{l}\n")).collect::<String>().as_bytes());
        assert_eq!(turn(&[&prompt]), Some(TurnState::Working));
        assert_eq!(turn(&[&prompt, &call]), Some(TurnState::ToolPending));
        assert_eq!(turn(&[&prompt, &call, &output]), Some(TurnState::Working));
        assert_eq!(turn(&[&prompt, &call, &output, &answer, &tokens]), Some(TurnState::Idle), "a token count tells nothing");
        assert_eq!(turn(&[&answer, &prompt, &done]), Some(TurnState::Idle));
        assert_eq!(turn(&[&tokens]), None);
        // A tail starts inside a line: the broken first line is skipped.
        assert_eq!(codex_turn_of_tail(format!("ent\":[]}}}}\n{answer}\n").as_bytes()), Some(TurnState::Idle));
    }

    use super::*;
    use envs::LocalEnvironment;
    use test_support::TempDir;

    const ROLLOUT: &str = r#"{"timestamp":"2026-10-01T09:00:00.000Z","type":"session_meta","payload":{"id":"0199aaaa-bbbb-cccc-dddd-eeeeffff0000","timestamp":"2026-10-01T09:00:00.000Z","cwd":"/work/api","git":{"branch":"dev"}}}
{"timestamp":"2026-10-01T09:00:00.100Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>cwd</environment_context>"}]}}
{"timestamp":"2026-10-01T09:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Add a health endpoint"}]}}
{"timestamp":"2026-10-01T09:00:01.500Z","type":"turn_context","payload":{"cwd":"/work/api","model":"gpt-5-codex"}}
{"timestamp":"2026-10-01T09:00:02.000Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"bash\",\"-lc\",\"ls\"]}","call_id":"c1"}}
{"timestamp":"2026-10-01T09:00:03.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"{\"output\":\"src\\n\",\"metadata\":{\"exit_code\":0}}"}}
{"timestamp":"2026-10-01T09:00:04.000Z","type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch","call_id":"c2","input":"*** Begin Patch\n*** Add File: src/health.rs\n+pub fn ok() {}\n*** End Patch"}}
{"timestamp":"2026-10-01T09:00:05.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done."}]}}
{"timestamp":"2026-10-01T09:00:05.100Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":9000,"cached_input_tokens":8000,"output_tokens":300},"last_token_usage":{"input_tokens":5000,"cached_input_tokens":4000,"output_tokens":100}}}}
"#;

    #[test]
    fn a_rollout_becomes_a_transcript() {
        let transcript = parse_transcript(ROLLOUT);
        assert_eq!(transcript.entries.len(), 2, "the injected context is left out");
        assert_eq!(transcript.model.as_deref(), Some("gpt-5-codex"));
        assert_eq!(transcript.last_usage.map(|u| u.context()), Some(5000));
        let tasks = transcript.tasks(false);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "bash -lc ls");
        assert_eq!(transcript.changes()[0].path, "src/health.rs");
    }

    #[test]
    fn a_scan_reads_the_meta_line() {
        let dir = TempDir::new("codex_scan");
        let day = dir.path().join(".codex/sessions/2026/10/01");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("rollout-2026-10-01T09-00-00-0199aaaa-bbbb-cccc-dddd-eeeeffff0000.jsonl"), ROLLOUT).unwrap();
        let sessions = CodexSource.scan(&LocalEnvironment::with_home(dir.path()), &(), dir.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].key.id, "0199aaaa-bbbb-cccc-dddd-eeeeffff0000");
        assert_eq!(sessions[0].first_prompt.as_deref(), Some("Add a health endpoint"));
        assert_eq!(sessions[0].git_branch.as_deref(), Some("dev"));
        assert_eq!(sessions[0].cwd.as_deref(), Some("/work/api"));
    }

    /// Codex repeats its last count when only the rate limits change: the call counts once,
    /// and its day is known.
    #[test]
    fn a_repeated_token_count_is_one_call() {
        let count = ROLLOUT.lines().find(|line| line.contains("token_count")).unwrap();
        let once = parse_transcript(ROLLOUT);
        let twice = parse_transcript(&ROLLOUT.replacen(count, &format!("{count}\n{count}"), 1));
        assert_eq!(twice.usage_by_model, once.usage_by_model);
        assert_eq!(once.usage_by_model.iter().map(|(_, usage)| usage.output).sum::<u64>(), 100);
        assert_eq!(once.usage_since(0), once.usage_by_model, "the calls have their days");
        assert_eq!(once.total_usage.output, 300, "the total is the harness's");
    }

    /// Long instructions in the first line push the first prompt past the usual head: the
    /// session is still found, with its folder and prompt.
    #[test]
    fn a_first_prompt_far_in_the_file_is_found() {
        let dir = TempDir::new("codex_scan_long");
        let day = dir.path().join(".codex/sessions/2026/10/01");
        std::fs::create_dir_all(&day).unwrap();
        let long = "x".repeat(HEAD_BYTES as usize + 1000);
        let rollout = ROLLOUT.replacen(r#""git":{"branch":"dev"}"#, &format!(r#""git":{{"branch":"dev"}},"instructions":"{long}""#), 1);
        assert_ne!(rollout, ROLLOUT);
        std::fs::write(day.join("rollout-2026-10-01T09-00-00-0199aaaa-bbbb-cccc-dddd-eeeeffff0000.jsonl"), rollout).unwrap();
        let sessions = CodexSource.scan(&LocalEnvironment::with_home(dir.path()), &(), dir.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].first_prompt.as_deref(), Some("Add a health endpoint"));
        assert_eq!(sessions[0].cwd.as_deref(), Some("/work/api"));
    }

    /// A file that does not read leaves the others in the scan.
    #[test]
    fn one_file_that_does_not_read_stops_nothing_else() {
        let dir = TempDir::new("codex_scan_one_bad");
        let day = dir.path().join(".codex/sessions/2026/10/01");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("rollout-2026-10-01T09-00-00-0199aaaa-bbbb-cccc-dddd-eeeeffff0000.jsonl"), ROLLOUT).unwrap();
        std::fs::write(day.join("rollout-2026-10-01T10-00-00-broken.jsonl"), ROLLOUT).unwrap();
        let env = crate::test_env::Unreadable::paths_with(LocalEnvironment::with_home(dir.path()), "broken");
        let sessions = CodexSource.scan(&env, &(), dir.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].key.id, "0199aaaa-bbbb-cccc-dddd-eeeeffff0000");
    }
}
