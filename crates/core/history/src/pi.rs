//! Pi (pi.dev): one JSON-lines file per session,
//! `~/.pi/agent/sessions/--<folder>--/<timestamp>_<session id>.jsonl`. The first line is a
//! `session` header (`id`, `cwd`, `timestamp`, `parentSession` as the path of the file it
//! was forked from); every other line is an entry with `id` and `parentId`, so a file is a
//! tree: going back in `/tree` starts a branch beside the old one. The window shows the
//! branch of the last entry, the one Pi goes on from.
//!
//! Entries: `message` (roles `user`, `assistant`, `toolResult`, `system`, …), `compaction`,
//! `branch_summary`, `session_info` (the name), `model_change`, `usage` and others the
//! window passes over. Written from the format documentation of Pi 1.6.1
//! (`docs/session-format.md`, `docs/message-types.md`).

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;

use envs::{Environment, ReadRange};
use model::{
    Block, Entry, HarnessKind, Role, SessionKey, SessionSummary, SourceStamp, ToolKind, ToolResult,
    Transcript, TurnState, Usage,
};
use serde_json::Value;

use crate::time::parse_rfc3339_ms;
use crate::{HistorySource, KnownSummaries, text_of};

pub struct PiSource;

fn usage_of(usage: &Value) -> Usage {
    let field = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
    Usage {
        input: field("input"),
        output: field("output"),
        cache_read: field("cacheRead"),
        cache_write: field("cacheWrite"),
        // A part of `cacheWrite`, kept an hour.
        cache_write_long: field("cacheWrite1h"),
    }
}

fn cost_of(usage: &Value) -> f64 {
    usage.pointer("/cost/total").and_then(Value::as_f64).unwrap_or(0.0)
}

/// The blocks of a message's `content`: a string, or text, thinking, image and tool-call
/// blocks.
fn content_blocks(content: &Value) -> Vec<Block> {
    let Some(items) = content.as_array() else {
        let text = text_of(content);
        return if text.trim().is_empty() { Vec::new() } else { vec![Block::Text { text }] };
    };
    items
        .iter()
        .filter_map(|item| match item.get("type").and_then(Value::as_str)? {
            "text" => {
                let text = item.get("text").and_then(Value::as_str).unwrap_or("").to_owned();
                (!text.trim().is_empty()).then_some(Block::Text { text })
            }
            "thinking" => {
                let text = item.get("thinking").and_then(Value::as_str).unwrap_or("").to_owned();
                (!text.trim().is_empty()).then_some(Block::Thinking { text })
            }
            "image" => Some(Block::Image {
                media_type: item.get("mimeType").and_then(Value::as_str).unwrap_or("image").to_owned(),
                data: item.get("data").and_then(Value::as_str).map(str::to_owned),
            }),
            "toolCall" => {
                let name = item.get("name").and_then(Value::as_str).unwrap_or("tool").to_owned();
                Some(Block::ToolCall {
                    id: item.get("id").and_then(Value::as_str).unwrap_or("").to_owned(),
                    kind: ToolKind::classify(&name),
                    name,
                    input: item.get("arguments").cloned().unwrap_or(Value::Null),
                    result: None,
                })
            }
            _ => None,
        })
        .collect()
}

/// The session id in a file name: `<timestamp>_<id>.jsonl`.
pub fn id_of_file(path: &str) -> Option<String> {
    let name = path.rsplit(['/', '\\']).next()?;
    let stem = name.strip_suffix(".jsonl")?;
    let id = stem.split_once('_').map_or(stem, |(_, id)| id);
    (!id.is_empty()).then(|| id.to_owned())
}

/// What a session file holds: its header facts and the transcript of its current branch.
pub struct Parsed {
    pub id: Option<String>,
    pub cwd: Option<String>,
    pub parent: Option<String>,
    pub created_ms: Option<i64>,
    pub updated_ms: Option<i64>,
    pub title: Option<String>,
    pub first_prompt: Option<String>,
    pub transcript: Transcript,
    /// Where the turn stands after the last message: a prompt or a tool's result means the
    /// model works, an answer that calls tools waits on them, any other answer is the
    /// user's turn.
    pub turn: Option<TurnState>,
}

/// The entries on the path from the last entry back to the root, in file order. A file
/// whose entries have no ids (the first format version) is one line of entries.
fn current_branch(entries: &[Value]) -> Vec<&Value> {
    let id_of = |entry: &Value| entry.get("id").and_then(Value::as_str).map(str::to_owned);
    if entries.iter().all(|entry| id_of(entry).is_none()) {
        return entries.iter().collect();
    }
    let by_id: HashMap<String, &Value> = entries.iter().filter_map(|entry| Some((id_of(entry)?, entry))).collect();
    let mut on_branch: HashSet<String> = HashSet::new();
    let mut at = entries.iter().rev().find_map(id_of);
    while let Some(id) = at {
        if !on_branch.insert(id.clone()) {
            break; // A loop of parents.
        }
        at = by_id.get(&id).and_then(|entry| entry.get("parentId")).and_then(Value::as_str).map(str::to_owned);
    }
    entries.iter().filter(|entry| id_of(entry).is_some_and(|id| on_branch.contains(&id))).collect()
}

fn notice(text: String, time: i64) -> Entry {
    Entry {
        id: String::new(),
        role: Role::System,
        timestamp_ms: time,
        blocks: vec![Block::Notice { text }],
        model: None,
        usage: None,
        compact_summary: false,
    }
}

pub fn parse(text: &str) -> Parsed {
    let mut parsed = Parsed {
        id: None,
        cwd: None,
        parent: None,
        created_ms: None,
        updated_ms: None,
        title: None,
        first_prompt: None,
        transcript: Transcript::default(),
        turn: None,
    };
    let mut entries: Vec<Value> = Vec::new();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else { continue };
        if value.get("type").and_then(Value::as_str) == Some("session") {
            parsed.id = value.get("id").and_then(Value::as_str).map(str::to_owned);
            parsed.cwd = value.get("cwd").and_then(Value::as_str).map(str::to_owned);
            parsed.created_ms = value.get("timestamp").and_then(Value::as_str).and_then(parse_rfc3339_ms);
            parsed.parent = value.get("parentSession").and_then(Value::as_str).and_then(id_of_file);
        } else {
            entries.push(value);
        }
    }
    let transcript = &mut parsed.transcript;
    let mut cost = 0.0;
    let mut calls: HashMap<String, (usize, usize)> = HashMap::new();
    for entry in current_branch(&entries) {
        let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
        // A message streamed by a turn has only its own time, in milliseconds.
        let time = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_ms)
            .or_else(|| entry.pointer("/message/timestamp").and_then(Value::as_i64))
            .unwrap_or(0);
        if time > 0 {
            parsed.updated_ms = Some(parsed.updated_ms.map_or(time, |t| t.max(time)));
        }
        let id = entry.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
        match kind {
            "session_info" => parsed.title = entry.get("name").and_then(Value::as_str).map(str::to_owned).filter(|name| !name.trim().is_empty()),
            "model_change" => {
                if let Some(model) = entry.get("modelId").and_then(Value::as_str) {
                    transcript.model = Some(model.to_owned());
                }
            }
            "usage" => {
                if let Some(usage) = entry.get("usage") {
                    transcript.add_call(entry.get("model").and_then(Value::as_str), usage_of(usage), time);
                    cost += cost_of(usage);
                }
            }
            "compaction" => {
                if let Some(usage) = entry.get("usage") {
                    cost += cost_of(usage);
                }
                transcript.entries.push(notice("Context compacted".into(), time));
                if let Some(summary) = entry.get("summary").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
                    transcript.entries.push(Entry {
                        id,
                        role: Role::User,
                        timestamp_ms: time,
                        blocks: vec![Block::Text { text: summary.to_owned() }],
                        model: None,
                        usage: None,
                        compact_summary: true,
                    });
                }
            }
            "branch_summary" => {
                if let Some(usage) = entry.get("usage") {
                    cost += cost_of(usage);
                }
                let summary = entry.get("summary").and_then(Value::as_str).unwrap_or("").trim();
                transcript.entries.push(notice(if summary.is_empty() { "Branch summary".into() } else { format!("Branch summary: {summary}") }, time));
            }
            "message" => {
                let message = entry.get("message").unwrap_or(&Value::Null);
                match message.get("role").and_then(Value::as_str).unwrap_or("") {
                    "user" => {
                        let blocks = content_blocks(message.get("content").unwrap_or(&Value::Null));
                        if parsed.first_prompt.is_none()
                            && let Some(Block::Text { text }) = blocks.iter().find(|b| matches!(b, Block::Text { .. }))
                        {
                            parsed.first_prompt = Some(model::shorten(text.trim(), 500));
                        }
                        parsed.turn = Some(TurnState::Working);
                        if !blocks.is_empty() {
                            transcript.entries.push(Entry { id, role: Role::User, timestamp_ms: time, blocks, model: None, usage: None, compact_summary: false });
                        }
                    }
                    "assistant" => {
                        let model = message.get("model").and_then(Value::as_str).map(str::to_owned);
                        if let Some(model) = &model {
                            transcript.model = Some(model.clone());
                        }
                        let usage = message.get("usage").map(usage_of);
                        if let (Some(usage), Some(raw)) = (usage, message.get("usage")) {
                            transcript.add_call(model.as_deref(), usage, time);
                            cost += cost_of(raw);
                        }
                        let mut blocks = content_blocks(message.get("content").unwrap_or(&Value::Null));
                        let stop = message.get("stopReason").and_then(Value::as_str).unwrap_or("");
                        if matches!(stop, "error" | "aborted")
                            && let Some(error) = message.get("errorMessage").and_then(Value::as_str).filter(|e| !e.trim().is_empty())
                        {
                            blocks.push(Block::Notice { text: error.to_owned() });
                        }
                        let calls_tools = blocks.iter().any(|block| matches!(block, Block::ToolCall { .. }));
                        parsed.turn = Some(if calls_tools && !matches!(stop, "error" | "aborted") { TurnState::ToolPending } else { TurnState::Idle });
                        if blocks.is_empty() {
                            continue;
                        }
                        let joins = transcript.entries.last().is_some_and(|last| last.role == Role::Assistant);
                        if !joins {
                            transcript.entries.push(Entry { id, role: Role::Assistant, timestamp_ms: time, blocks: Vec::new(), model, usage, compact_summary: false });
                        }
                        let index = transcript.entries.len() - 1;
                        let entry = &mut transcript.entries[index];
                        for block in blocks {
                            if let Block::ToolCall { id, .. } = &block {
                                calls.insert(id.clone(), (index, entry.blocks.len()));
                            }
                            entry.blocks.push(block);
                        }
                    }
                    "toolResult" => {
                        parsed.turn = Some(TurnState::Working);
                        let call = message.get("toolCallId").and_then(Value::as_str).unwrap_or("");
                        let text = text_of(message.get("content").unwrap_or(&Value::Null));
                        let is_error = message.get("isError").and_then(Value::as_bool) == Some(true);
                        if let Some(&(entry, block)) = calls.get(call)
                            && let Some(Block::ToolCall { result, .. }) = transcript.entries[entry].blocks.get_mut(block)
                        {
                            *result = Some(ToolResult { text, is_error });
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    if cost > 0.0 {
        transcript.cost_usd = Some(cost);
    }
    parsed
}

/// The transcript of messages as a turn of Pi streams them (the `message` of
/// `message_end` in JSON and RPC modes), read as entries of one branch.
pub fn transcript_of_messages(messages: &[Value]) -> Transcript {
    let entries: Vec<Value> = messages
        .iter()
        .enumerate()
        .map(|(n, message)| {
            let parent = if n == 0 { Value::Null } else { Value::String(format!("live-{}", n - 1)) };
            serde_json::json!({"type": "message", "id": format!("live-{n}"), "parentId": parent, "message": message})
        })
        .collect();
    let text: String = entries.iter().map(|entry| format!("{entry}\n")).collect();
    parse(&text).transcript
}

fn sessions_dir(env: &dyn Environment) -> io::Result<String> {
    let home = env.home()?;
    Ok(env.join(&env.join(&env.join(&home, ".pi"), "agent"), "sessions"))
}

impl HistorySource for PiSource {
    fn harness(&self) -> HarnessKind {
        HarnessKind::Pi
    }

    fn scan(&self, env: &dyn Environment, known: &dyn KnownSummaries, _cache: &Path) -> io::Result<Vec<SessionSummary>> {
        let root = sessions_dir(env)?;
        let mut sessions = Vec::new();
        for meta in env.find_files(&root, ".jsonl", 2)? {
            let known = known.known(&meta.path);
            if let Some(summary) = &known
                && summary.source.size == meta.size
                && summary.source.modified_ms == meta.modified_ms
            {
                sessions.push(summary.clone());
                continue;
            }
            // One file that does not read stops nothing else; it keeps what was known of it.
            let bytes = match env.read(&meta.path, ReadRange::All) {
                Ok(bytes) => bytes,
                Err(error) => {
                    eprintln!("pi history {}: {error}", meta.path);
                    sessions.extend(known);
                    continue;
                }
            };
            let parsed = parse(&String::from_utf8_lossy(&bytes));
            if parsed.first_prompt.is_none() && parsed.title.is_none() {
                continue;
            }
            let Some(id) = parsed.id.clone().or_else(|| id_of_file(&meta.path)) else { continue };
            sessions.push(SessionSummary {
                key: SessionKey { env: env.id(), harness: HarnessKind::Pi, id },
                title: parsed.title.unwrap_or_default(),
                first_prompt: parsed.first_prompt,
                cwd: parsed.cwd,
                git_branch: None,
                model: parsed.transcript.model.clone(),
                created_ms: parsed.created_ms.unwrap_or(meta.modified_ms),
                updated_ms: parsed.updated_ms.unwrap_or(meta.modified_ms),
                turn: parsed.turn,
                parent: parsed.parent,
                source: SourceStamp { path: meta.path.clone(), size: meta.size, modified_ms: meta.modified_ms },
            });
        }
        Ok(sessions)
    }

    fn load(&self, env: &dyn Environment, summary: &SessionSummary, _cache: &Path) -> io::Result<Transcript> {
        let bytes = env.read(&summary.source.path, ReadRange::All)?;
        Ok(parse(&String::from_utf8_lossy(&bytes)).transcript)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use envs::LocalEnvironment;
    use test_support::TempDir;

    /// A session that went back in its tree: the first answer was left for another branch.
    const SESSION: &str = r#"{"type":"session","version":3,"id":"0aa00000-1111-7222-8333-444455556666","timestamp":"2026-03-01T10:00:00.000Z","cwd":"/work/app"}
{"type":"model_change","id":"m1","parentId":null,"timestamp":"2026-03-01T10:00:00.100Z","provider":"anthropic","modelId":"claude-sonnet-5-5"}
{"type":"message","id":"u1","parentId":"m1","timestamp":"2026-03-01T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"List the files"}],"timestamp":1772359201000}}
{"type":"message","id":"a0","parentId":"u1","timestamp":"2026-03-01T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"An abandoned answer"}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-5-5","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0,"totalTokens":15,"cost":{"total":0.001}},"stopReason":"stop","timestamp":1772359202000}}
{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-03-01T10:00:03.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Use ls."},{"type":"toolCall","id":"call_1","name":"bash","arguments":{"command":"ls"}}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-5-5","usage":{"input":900,"output":40,"cacheRead":3000,"cacheWrite":100,"cacheWrite1h":100,"totalTokens":4040,"cost":{"total":0.01}},"stopReason":"toolUse","timestamp":1772359203000}}
{"type":"message","id":"r1","parentId":"a1","timestamp":"2026-03-01T10:00:04.000Z","message":{"role":"toolResult","toolCallId":"call_1","toolName":"bash","content":[{"type":"text","text":"src\nCargo.toml"}],"isError":false,"timestamp":1772359204000}}
{"type":"message","id":"a2","parentId":"r1","timestamp":"2026-03-01T10:00:05.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Two entries."}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-5-5","usage":{"input":20,"output":5,"cacheRead":3950,"cacheWrite":0,"totalTokens":3975,"cost":{"total":0.002}},"stopReason":"stop","timestamp":1772359205000}}
{"type":"session_info","id":"n1","parentId":"a2","timestamp":"2026-03-01T10:00:06.000Z","name":"Listing files"}"#;

    #[test]
    fn the_current_branch_becomes_the_transcript() {
        let parsed = parse(SESSION);
        assert_eq!(parsed.id.as_deref(), Some("0aa00000-1111-7222-8333-444455556666"));
        assert_eq!(parsed.cwd.as_deref(), Some("/work/app"));
        assert_eq!(parsed.title.as_deref(), Some("Listing files"));
        assert_eq!(parsed.first_prompt.as_deref(), Some("List the files"));
        assert_eq!(parsed.turn, Some(TurnState::Idle), "an answer without calls is the user's turn");
        let transcript = &parsed.transcript;
        let text = format!("{:?}", transcript.entries);
        assert!(!text.contains("abandoned"), "the other branch is left out");
        assert_eq!(transcript.entries.len(), 2, "the steps of a turn join: {text}");
        assert_eq!(transcript.tasks(false)[0].title, "ls");
        assert_eq!(transcript.tasks(false)[0].status, model::TaskStatus::Done);
        assert_eq!(transcript.model.as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(transcript.last_usage.map(|u| u.context()), Some(3970));
        assert_eq!(transcript.total_usage.cache_write_long, 100);
        assert!((transcript.cost_usd.unwrap() - 0.012).abs() < 1e-9, "the branch's calls only");
    }

    #[test]
    fn a_call_waits_and_an_aborted_answer_says_why() {
        let header = r#"{"type":"session","version":3,"id":"s","timestamp":"2026-03-01T10:00:00.000Z","cwd":"/w"}"#;
        let user = r#"{"type":"message","id":"u","parentId":null,"timestamp":"2026-03-01T10:00:01.000Z","message":{"role":"user","content":"go","timestamp":1}}"#;
        let calling = r#"{"type":"message","id":"a","parentId":"u","timestamp":"2026-03-01T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"c","name":"read","arguments":{"path":"a.rs"}}],"usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"total":0}},"stopReason":"toolUse","timestamp":2}}"#;
        assert_eq!(parse(&format!("{header}\n{user}")).turn, Some(TurnState::Working));
        assert_eq!(parse(&format!("{header}\n{user}\n{calling}")).turn, Some(TurnState::ToolPending));
        let aborted = r#"{"type":"message","id":"a","parentId":"u","timestamp":"2026-03-01T10:00:02.000Z","message":{"role":"assistant","content":[],"usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"total":0}},"stopReason":"aborted","errorMessage":"Operation aborted","timestamp":2}}"#;
        let parsed = parse(&format!("{header}\n{user}\n{aborted}"));
        assert_eq!(parsed.turn, Some(TurnState::Idle));
        assert!(matches!(parsed.transcript.entries.last().unwrap().blocks[0], Block::Notice { ref text } if text == "Operation aborted"));
    }

    #[test]
    fn a_fork_names_its_parent_by_the_file_name() {
        assert_eq!(id_of_file("/h/.pi/agent/sessions/--w--/2026-03-01T10-00-00-000Z_abc-123.jsonl").as_deref(), Some("abc-123"));
        let forked = r#"{"type":"session","version":3,"id":"child","timestamp":"2026-03-01T10:00:00.000Z","cwd":"/w","parentSession":"/h/.pi/agent/sessions/--w--/2026-03-01T09-00-00-000Z_parent-1.jsonl"}"#;
        assert_eq!(parse(forked).parent.as_deref(), Some("parent-1"));
    }

    #[test]
    fn the_sessions_folder_is_scanned() {
        let dir = TempDir::new("pi_scan");
        let folder = dir.path().join(".pi/agent/sessions/--work-app--");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("2026-03-01T10-00-00-000Z_0aa00000-1111-7222-8333-444455556666.jsonl"), SESSION).unwrap();
        std::fs::write(folder.join("2026-03-01T11-00-00-000Z_empty.jsonl"), "{\"type\":\"session\",\"id\":\"empty\",\"cwd\":\"/w\"}\n").unwrap();
        let env = LocalEnvironment::with_home(dir.path());
        let sessions = PiSource.scan(&env, &(), dir.path()).unwrap();
        assert_eq!(sessions.len(), 1, "a session without a prompt is left out");
        assert_eq!(sessions[0].key.harness, HarnessKind::Pi);
        assert_eq!(sessions[0].title, "Listing files");
        let transcript = PiSource.load(&env, &sessions[0], dir.path()).unwrap();
        assert_eq!(transcript.entries.len(), 2);
    }
}
