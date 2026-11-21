//! DeepSeek Harness (`dsh`, github.com/deepseek-ai/deepseek-harness): one folder per
//! session, `$DSH_HOME/sessions/--<project key>--/<session id>/session.v4.jsonl.zstd`.
//! The file is a run of zstd frames (a header frame, then one per appended batch) that
//! decode to JSON lines: a `session` header, then envelopes `{type, seq, time, data}` such as
//! `user/message`, `assistant/message`, `tool/result` and `session/title`. Older format
//! versions may sit next to the current one; the highest `vN` wins.
//!
//! UNVERIFIED: written from the source of the harness (format version 4, commit 2e39e70);
//! no dsh session existed on the development machine. The tests use synthetic files.

use std::collections::HashMap;
use std::io::{self, Cursor, Read};
use std::path::Path;

use envs::{Environment, ReadRange};
use model::{
    Block, Entry, HarnessKind, Role, SessionKey, SessionSummary, SourceStamp, ToolKind, ToolResult,
    Transcript, TurnState, Usage,
};
use serde_json::Value;

use crate::{HistorySource, KnownSummaries, text_of};

pub struct DshSource;

/// Decodes concatenated zstd frames; plain JSON lines pass through.
pub fn decode(bytes: &[u8]) -> io::Result<Vec<u8>> {
    const MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
    if !bytes.starts_with(&MAGIC) {
        return Ok(bytes.to_vec());
    }
    let mut cursor = Cursor::new(bytes);
    let mut out = Vec::new();
    while (cursor.position() as usize) < bytes.len() {
        let before = cursor.position();
        let mut decoder =
            ruzstd::decoding::StreamingDecoder::new(&mut cursor).map_err(|e| io::Error::other(e.to_string()))?;
        decoder.read_to_end(&mut out)?;
        if cursor.position() == before {
            break;
        }
    }
    Ok(out)
}

/// The format version in a file name: `session.v4.jsonl.zstd` is 4, `session.jsonl` is 0.
fn version_of(path: &str) -> u32 {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    name.strip_prefix("session.v")
        .and_then(|rest| rest.split('.').next())
        .and_then(|digits| digits.parse().ok())
        .unwrap_or(0)
}

fn session_dir(path: &str) -> &str {
    path.rfind(['/', '\\']).map_or(path, |index| &path[..index])
}

fn usage_of(value: &Value) -> Option<Usage> {
    let usage = value.get("usage")?;
    let field = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
    Some(Usage {
        input: field("inputTokens"),
        output: field("outputTokens"),
        cache_read: field("cacheReadTokens"),
        cache_write: field("cacheWriteTokens"),
        cache_write_long: 0,
    })
}

fn content_blocks(content: &Value) -> Vec<Block> {
    let Some(items) = content.as_array() else {
        let text = text_of(content);
        return if text.is_empty() { Vec::new() } else { vec![Block::Text { text }] };
    };
    items
        .iter()
        .filter_map(|item| {
            let text = || item.get("text").and_then(Value::as_str).unwrap_or("").to_owned();
            match item.get("type").and_then(Value::as_str)? {
                "text" => Some(Block::Text { text: text() }),
                "reasoning" => Some(Block::Thinking { text: text() }),
                "image" => Some(Block::Image { media_type: "image".into(), data: None }),
                "tool-call" => {
                    let name = item.get("name").and_then(Value::as_str).unwrap_or("tool").to_owned();
                    let input = match item.get("arguments") {
                        Some(Value::String(raw)) => serde_json::from_str(raw).unwrap_or(Value::String(raw.clone())),
                        Some(other) => other.clone(),
                        None => Value::Null,
                    };
                    Some(Block::ToolCall {
                        id: item.get("id").and_then(Value::as_str).unwrap_or("").to_owned(),
                        kind: ToolKind::classify(&name),
                        name,
                        input,
                        result: None,
                    })
                }
                _ => None,
            }
        })
        .collect()
}

/// What a session file holds: its header facts and the transcript.
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
    /// model works, an answer with calls of tools waits on them, an answer without is the
    /// user's turn.
    pub turn: Option<TurnState>,
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
    let transcript = &mut parsed.transcript;
    let mut calls: HashMap<String, (usize, usize)> = HashMap::new();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else { continue };
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        let time = value.get("time").and_then(Value::as_i64).unwrap_or(0);
        if time > 0 {
            parsed.updated_ms = Some(parsed.updated_ms.map_or(time, |t| t.max(time)));
        }
        let data = value.get("data").unwrap_or(&Value::Null);
        match kind {
            "session" => {
                parsed.id = value.get("id").and_then(Value::as_str).map(str::to_owned);
                parsed.cwd = value.get("cwd").and_then(Value::as_str).map(str::to_owned);
                parsed.created_ms = value.get("createdAt").and_then(Value::as_i64);
                parsed.parent = value
                    .get("parentSession")
                    .and_then(|p| p.as_str().map(str::to_owned).or_else(|| p.get("id").and_then(Value::as_str).map(str::to_owned)));
            }
            "session/title" => parsed.title = data.get("title").and_then(Value::as_str).map(str::to_owned),
            "user/message" => {
                let blocks = content_blocks(data.get("content").unwrap_or(&Value::Null));
                if parsed.first_prompt.is_none()
                    && let Some(Block::Text { text }) = blocks.first()
                {
                    parsed.first_prompt = Some(model::shorten(text.trim(), 500));
                }
                parsed.turn = Some(TurnState::Working);
                if !blocks.is_empty() {
                    transcript.entries.push(Entry {
                        id: data.get("id").and_then(Value::as_str).unwrap_or("").to_owned(),
                        role: Role::User,
                        timestamp_ms: time,
                        blocks,
                        model: None,
                        usage: None,
                        compact_summary: false,
                    });
                }
            }
            "assistant/message" => {
                let message = data.get("message").unwrap_or(&Value::Null);
                let model = message.pointer("/source/model").and_then(Value::as_str).map(str::to_owned);
                if let Some(model) = &model {
                    transcript.model = Some(model.clone());
                }
                let usage = usage_of(data);
                if let Some(usage) = usage {
                    transcript.add_call(model.as_deref(), usage, time);
                }
                let blocks = content_blocks(message.get("content").unwrap_or(&Value::Null));
                let calls_tools = blocks.iter().any(|block| matches!(block, Block::ToolCall { .. }));
                parsed.turn = Some(if calls_tools { TurnState::ToolPending } else { TurnState::Idle });
                let joins = transcript.entries.last().is_some_and(|last| last.role == Role::Assistant);
                if !joins {
                    transcript.entries.push(Entry {
                        id: message.get("id").and_then(Value::as_str).unwrap_or("").to_owned(),
                        role: Role::Assistant,
                        timestamp_ms: time,
                        blocks: Vec::new(),
                        model,
                        usage,
                        compact_summary: false,
                    });
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
            "tool/result" => {
                parsed.turn = Some(TurnState::Working);
                let message = data.get("message").unwrap_or(&Value::Null);
                let call = message.get("toolCallId").and_then(Value::as_str).unwrap_or("");
                let mut text = text_of(message.get("content").unwrap_or(&Value::Null));
                if text.is_empty()
                    && let Some(error) = data.get("error")
                {
                    text = error.get("reason").or_else(|| error.get("name")).and_then(Value::as_str).unwrap_or("error").to_owned();
                }
                let is_error = message.get("isError").and_then(Value::as_bool) == Some(true) || data.get("error").is_some();
                if let Some(&(entry, block)) = calls.get(call)
                    && let Some(Block::ToolCall { result, .. }) = transcript.entries[entry].blocks.get_mut(block)
                {
                    *result = Some(ToolResult { text, is_error });
                }
            }
            kind if kind.starts_with("compaction/") && kind.ends_with("end") => {
                transcript.entries.push(Entry {
                    id: String::new(),
                    role: Role::System,
                    timestamp_ms: time,
                    blocks: vec![Block::Notice { text: "Context compacted".into() }],
                    model: None,
                    usage: None,
                    compact_summary: false,
                });
            }
            _ => {}
        }
    }
    parsed
}

fn dsh_home(env: &dyn Environment) -> io::Result<String> {
    Ok(env.join(&env.home()?, ".dsh"))
}

impl HistorySource for DshSource {
    fn harness(&self) -> HarnessKind {
        HarnessKind::DeepSeek
    }

    fn scan(&self, env: &dyn Environment, known: &dyn KnownSummaries, _cache: &Path) -> io::Result<Vec<SessionSummary>> {
        let root = env.join(&dsh_home(env)?, "sessions");
        let mut files = env.find_files(&root, ".jsonl.zstd", 3)?;
        files.extend(env.find_files(&root, ".jsonl", 3)?);
        // The newest format version of each session folder.
        let mut newest: HashMap<String, envs::FileMeta> = HashMap::new();
        for file in files {
            let dir = session_dir(&file.path).to_owned();
            match newest.get(&dir) {
                Some(current) if version_of(&current.path) >= version_of(&file.path) => {}
                _ => {
                    newest.insert(dir, file);
                }
            }
        }
        let mut sessions = Vec::new();
        for meta in newest.into_values() {
            if let Some(summary) = known.known(&meta.path)
                && summary.source.size == meta.size
                && summary.source.modified_ms == meta.modified_ms
            {
                sessions.push(summary);
                continue;
            }
            let bytes = decode(&env.read(&meta.path, ReadRange::All)?)?;
            let parsed = parse(&String::from_utf8_lossy(&bytes));
            if parsed.first_prompt.is_none() && parsed.title.is_none() {
                continue;
            }
            let dir = session_dir(&meta.path);
            let fallback_id = dir.rsplit(['/', '\\']).next().unwrap_or(dir).to_owned();
            sessions.push(SessionSummary {
                key: SessionKey { env: env.id(), harness: HarnessKind::DeepSeek, id: parsed.id.unwrap_or(fallback_id) },
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
        let bytes = decode(&env.read(&summary.source.path, ReadRange::All)?)?;
        Ok(parse(&String::from_utf8_lossy(&bytes)).transcript)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use envs::LocalEnvironment;
    use test_support::TempDir;

    const HEADER: &str = r#"{"type":"session","version":4,"id":"session-11111111-2222-3333-4444-555555555555","createdAt":1787472000000,"cwd":"/work/app","isSeeded":false,"delegationDepth":0}"#;
    const BODY: &str = r#"{"type":"user/message","data":{"id":"u1","role":"user","content":[{"type":"text","text":"List the files"}],"source":{"kind":"user"}},"surfaceOp":"append","seq":1,"time":1787472000001}
{"type":"assistant/message","data":{"turn":1,"step":1,"message":{"id":"a1","role":"assistant","content":[{"type":"reasoning","text":"Use ls."},{"type":"tool-call","id":"c1","name":"shell","arguments":"{\"command\":\"ls\"}"}],"source":{"kind":"model","provider":"deepseek-official","model":"deepseek-flash"}},"usage":{"inputTokens":900,"outputTokens":40,"cacheReadTokens":3000,"cacheWriteTokens":0}},"seq":2,"time":1787472000500}
{"type":"tool/result","data":{"turn":1,"step":1,"message":{"id":"r1","role":"tool","toolCallId":"c1","content":[{"type":"text","text":"src\nCargo.toml"}],"source":{"kind":"tool","callId":"c1"}}},"seq":3,"time":1787472000600}
{"type":"assistant/message","data":{"turn":1,"step":2,"message":{"id":"a2","role":"assistant","content":[{"type":"text","text":"Two entries."}],"source":{"kind":"model","provider":"deepseek-official","model":"deepseek-flash"}},"usage":{"inputTokens":20,"outputTokens":5,"cacheReadTokens":3950,"cacheWriteTokens":0}},"seq":4,"time":1787472001000}
{"type":"session/title","data":{"title":"Listing files","messageSeqs":[1],"source":{"kind":"provider"}},"seq":5,"time":1787472001100}"#;

    #[test]
    fn concatenated_frames_decode_to_lines() {
        use ruzstd::encoding::{CompressionLevel, compress_to_vec};
        let mut bytes = compress_to_vec(format!("{HEADER}\n").as_bytes(), CompressionLevel::Fastest);
        bytes.extend(compress_to_vec(format!("{BODY}\n").as_bytes(), CompressionLevel::Fastest));
        let text = String::from_utf8(decode(&bytes).unwrap()).unwrap();
        assert!(text.starts_with(HEADER));
        assert!(text.contains("session/title"));
        assert_eq!(decode(b"{\"plain\":1}").unwrap(), b"{\"plain\":1}");
    }

    #[test]
    fn a_session_folder_becomes_a_summary_and_a_transcript() {
        use ruzstd::encoding::{CompressionLevel, compress_to_vec};
        let dir = TempDir::new("dsh_scan");
        let session = dir.path().join(".dsh/sessions/---work-app--/session-11111111-2222-3333-4444-555555555555");
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(session.join("session.v3.jsonl"), "{\"type\":\"session\",\"id\":\"old\"}\n").unwrap();
        let mut bytes = compress_to_vec(format!("{HEADER}\n").as_bytes(), CompressionLevel::Fastest);
        bytes.extend(compress_to_vec(format!("{BODY}\n").as_bytes(), CompressionLevel::Fastest));
        std::fs::write(session.join("session.v4.jsonl.zstd"), bytes).unwrap();

        let env = LocalEnvironment::with_home(dir.path());
        let sessions = DshSource.scan(&env, &(), dir.path()).unwrap();
        assert_eq!(sessions.len(), 1, "the highest version wins");
        let summary = &sessions[0];
        assert_eq!(summary.key.id, "session-11111111-2222-3333-4444-555555555555");
        assert_eq!(summary.title, "Listing files");
        assert_eq!(summary.cwd.as_deref(), Some("/work/app"));
        assert_eq!(summary.model.as_deref(), Some("deepseek-flash"));
        assert_eq!(summary.turn, Some(model::TurnState::Idle), "an answer without calls is the user's turn");
        let mut parsed = parse(r#"{"type":"user/message","data":{"content":[{"type":"text","text":"go"}]},"seq":1,"time":1}"#);
        assert_eq!(parsed.turn, Some(model::TurnState::Working));
        parsed = parse(&format!("{}
{}", r#"{"type":"user/message","data":{"content":[]},"seq":1,"time":1}"#,
            r#"{"type":"assistant/message","data":{"message":{"content":[{"type":"tool-call","id":"c","name":"shell","arguments":"{}"}]}},"seq":2,"time":2}"#));
        assert_eq!(parsed.turn, Some(model::TurnState::ToolPending), "a call waits on its tool");

        let transcript = DshSource.load(&env, summary, dir.path()).unwrap();
        assert_eq!(transcript.entries.len(), 2, "the steps of a turn join");
        assert_eq!(transcript.tasks(false)[0].title, "ls");
        assert_eq!(transcript.tasks(false)[0].status, model::TaskStatus::Done);
        assert_eq!(transcript.last_usage.map(|u| u.context()), Some(3970));
    }
}
