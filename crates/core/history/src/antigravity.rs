//! Google Antigravity: the `agy` CLI, the Antigravity app and the Antigravity IDE keep
//! their conversations under `~/.gemini`: `antigravity` (the app and the CLI share it),
//! `antigravity-cli` and `antigravity-ide`. In each, `brain/<conversation id>/
//! .system_generated/logs/` holds the conversation as JSON lines, one step a line
//! (`step_index`, `source`, `type`, `status`, `created_at`, `content`, `thinking`,
//! `tool_calls`):
//!
//! - `transcript.jsonl` has the steps with long fields cut (`truncated_fields`);
//! - `transcript_full.jsonl` has them whole, sometimes only for the last steps;
//! - `chunks/transcript/*.jsonl` and `chunks/transcript_full/*.jsonl` are pieces of the two,
//!   cut anywhere, even inside a line or a character, and read joined.
//!
//! The steps are put together by `step_index`, the whole ones over the cut ones. A long
//! conversation may begin late in all of them: its first steps are then only in its
//! database, and the window shows it from where the files begin. A model
//! step with `tool_calls` is followed by one step per call, in their order, with its
//! result. A prompt is wrapped in `<USER_REQUEST>`.
//!
//! Titles and folders are in `conversation_summaries.db` (SQLite, opened read-only, only
//! the table `conversation_summaries`); the IDE keeps none, and a host's is not read: there
//! the title is the first prompt. The conversations' own databases (`conversations/*.db`)
//! are protocol buffers and are not read, so the model and the tokens of a conversation
//! are not known. Written from the files of Antigravity 1.5 on the development machine;
//! the tests use synthetic files.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io;
use std::path::Path;

use envs::{Environment, FileMeta, ReadRange};
use model::{
    Block, Entry, HarnessKind, Role, SessionKey, SessionSummary, SourceStamp, ToolKind, ToolResult,
    Transcript, TurnState,
};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value};

use crate::time::parse_rfc3339_ms;
use crate::{HistorySource, KnownSummaries};

pub struct AntigravitySource;

/// The folders of `~/.gemini` that hold conversations.
const DATA_DIRS: [&str; 3] = ["antigravity", "antigravity-cli", "antigravity-ide"];

/// The tool names of Antigravity, in the kinds the window draws.
fn kind_of(name: &str) -> ToolKind {
    match name {
        "view_file" | "list_dir" | "grep_search" | "find_by_name" | "view_file_outline" | "view_code_item" => ToolKind::Read,
        "write_to_file" | "replace_file_content" | "multi_replace_file_content" => ToolKind::Edit,
        "run_command" | "command_status" | "send_command_input" => ToolKind::Shell,
        "search_web" | "read_url_content" => ToolKind::Web,
        "browser_subagent" => ToolKind::Agent,
        other => ToolKind::classify(other),
    }
}

/// The arguments of a call with the names the window reads (`command`, `file_path`,
/// `path`, `query`, `url`, `description`) beside Antigravity's own.
fn input_of(args: &Value) -> Value {
    let mut input: Map<String, Value> = args.as_object().cloned().unwrap_or_default();
    let mut alias = |to: &str, from: &[&str]| {
        if input.contains_key(to) {
            return;
        }
        if let Some(value) = from.iter().find_map(|key| input.get(*key).filter(|v| v.is_string()).cloned()) {
            input.insert(to.to_owned(), value);
        }
    };
    alias("command", &["CommandLine"]);
    alias("file_path", &["TargetFile", "AbsolutePath"]);
    alias("path", &["DirectoryPath", "SearchPath"]);
    alias("query", &["Query", "query"]);
    alias("url", &["Url"]);
    alias("description", &["toolSummary", "Description", "TaskSummary"]);
    Value::Object(input)
}

/// The prompt inside `<USER_REQUEST>`, or the whole text.
fn prompt_of(content: &str) -> String {
    match (content.find("<USER_REQUEST>"), content.find("</USER_REQUEST>")) {
        (Some(start), Some(end)) if end > start => content[start + "<USER_REQUEST>".len()..end].trim().to_owned(),
        _ => content.trim().to_owned(),
    }
}

/// Lines of JSON from bytes that may be cut anywhere: what does not read is passed over.
fn steps_of(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(bytes).lines().filter_map(|line| serde_json::from_str::<Value>(line).ok()).filter(|step| step.get("step_index").is_some()).collect()
}

/// The steps of a conversation from its files, by `step_index`, each source over the ones
/// before it: the cut pieces, the cut transcript, the whole pieces, the whole transcript.
pub fn merge_steps(sources: &[&[u8]]) -> Vec<Value> {
    let mut steps: BTreeMap<i64, Value> = BTreeMap::new();
    for bytes in sources {
        for step in steps_of(bytes) {
            if let Some(index) = step.get("step_index").and_then(Value::as_i64) {
                steps.insert(index, step);
            }
        }
    }
    steps.into_values().collect()
}

pub struct Parsed {
    pub first_prompt: Option<String>,
    pub created_ms: Option<i64>,
    pub updated_ms: Option<i64>,
    pub transcript: Transcript,
    pub turn: Option<TurnState>,
}

pub fn parse(steps: &[Value]) -> Parsed {
    let mut parsed = Parsed { first_prompt: None, created_ms: None, updated_ms: None, transcript: Transcript::default(), turn: None };
    let transcript = &mut parsed.transcript;
    // The calls of the last model step that wait for their results, in order.
    let mut waiting: VecDeque<(usize, usize)> = VecDeque::new();
    for step in steps {
        let text = |key: &str| step.get(key).and_then(Value::as_str).unwrap_or("");
        let time = parse_rfc3339_ms(text("created_at")).unwrap_or(0);
        if time > 0 {
            parsed.created_ms.get_or_insert(time);
            parsed.updated_ms = Some(parsed.updated_ms.map_or(time, |t| t.max(time)));
        }
        let index = step.get("step_index").and_then(Value::as_i64).unwrap_or(0);
        match (text("source"), text("type")) {
            ("USER_EXPLICIT", "USER_INPUT") => {
                waiting.clear();
                let prompt = prompt_of(text("content"));
                if prompt.is_empty() {
                    continue;
                }
                parsed.first_prompt.get_or_insert_with(|| model::shorten(&prompt, 500));
                parsed.turn = Some(TurnState::Working);
                transcript.entries.push(Entry {
                    id: index.to_string(),
                    role: Role::User,
                    timestamp_ms: time,
                    blocks: vec![Block::Text { text: prompt }],
                    model: None,
                    usage: None,
                    compact_summary: false,
                });
            }
            ("MODEL", "PLANNER_RESPONSE") => {
                waiting.clear();
                let mut blocks = Vec::new();
                if !text("thinking").trim().is_empty() {
                    blocks.push(Block::Thinking { text: text("thinking").to_owned() });
                }
                if !text("content").trim().is_empty() {
                    blocks.push(Block::Text { text: text("content").to_owned() });
                }
                let calls = step.get("tool_calls").and_then(Value::as_array).cloned().unwrap_or_default();
                for (n, call) in calls.iter().enumerate() {
                    let name = call.get("name").and_then(Value::as_str).unwrap_or("tool").to_owned();
                    blocks.push(Block::ToolCall {
                        id: format!("{index}-{n}"),
                        kind: kind_of(&name),
                        name,
                        input: input_of(call.get("args").unwrap_or(&Value::Null)),
                        result: None,
                    });
                }
                parsed.turn = Some(if calls.is_empty() { TurnState::Idle } else { TurnState::ToolPending });
                if blocks.is_empty() {
                    continue;
                }
                let joins = transcript.entries.last().is_some_and(|last| last.role == Role::Assistant);
                if !joins {
                    transcript.entries.push(Entry {
                        id: index.to_string(),
                        role: Role::Assistant,
                        timestamp_ms: time,
                        blocks: Vec::new(),
                        model: None,
                        usage: None,
                        compact_summary: false,
                    });
                }
                let at = transcript.entries.len() - 1;
                let entry = &mut transcript.entries[at];
                for block in blocks {
                    if matches!(block, Block::ToolCall { .. }) {
                        waiting.push_back((at, entry.blocks.len()));
                    }
                    entry.blocks.push(block);
                }
            }
            ("MODEL", _) => {
                // The result of the next call that waits for one.
                let Some((entry, block)) = waiting.pop_front() else { continue };
                parsed.turn = Some(if waiting.is_empty() { TurnState::Working } else { TurnState::ToolPending });
                if text("status") == "RUNNING" {
                    continue;
                }
                if let Some(Block::ToolCall { result, .. }) = transcript.entries[entry].blocks.get_mut(block) {
                    *result = Some(ToolResult { text: text("content").to_owned(), is_error: text("status") == "ERROR" });
                }
            }
            ("SYSTEM", "ERROR_MESSAGE") if !text("content").trim().is_empty() => {
                // An error ends the turn: the user goes on.
                waiting.clear();
                parsed.turn = Some(TurnState::Idle);
                transcript.entries.push(Entry {
                    id: index.to_string(),
                    role: Role::System,
                    timestamp_ms: time,
                    blocks: vec![Block::Notice { text: text("content").trim().to_owned() }],
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

/// A folder from a `file://` URI: `file:///c%3A/Users/me/app` is `C:\Users\me\app` on
/// Windows, `file:///home/me/app` is `/home/me/app`.
pub fn folder_of_uri(uri: &str, windows: bool) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = rest.get(i + 1..i + 3).and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            decoded.push(byte);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    let path = String::from_utf8(decoded).ok()?;
    let drive = path.strip_prefix('/').filter(|p| p.as_bytes().get(1) == Some(&b':'));
    Some(match drive {
        Some(path) if windows => {
            let mut chars = path.chars();
            let letter = chars.next()?.to_ascii_uppercase();
            format!("{letter}{}", chars.as_str().replace('/', "\\"))
        }
        _ => path,
    })
}

/// What the index of an app keeps of a conversation.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Indexed {
    pub title: String,
    pub cwd: Option<String>,
    pub updated_ms: Option<i64>,
    pub parent: Option<String>,
}

/// The conversations the indexes of this computer name, by id. The first non-empty title
/// wins: an app sets it, the CLI's index may hold an empty one.
fn read_indexes(env: &dyn Environment, gemini: &str) -> HashMap<String, Indexed> {
    let mut found: HashMap<String, Indexed> = HashMap::new();
    if !env.id().is_local() {
        return found;
    }
    for dir in DATA_DIRS {
        let db = env.join(&env.join(gemini, dir), "conversation_summaries.db");
        if !Path::new(&db).is_file() {
            continue;
        }
        let Ok(connection) = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX) else { continue };
        let Ok(mut statement) = connection.prepare(
            "SELECT conversation_id, title, workspace_uris, last_modified_time, parent_conversation_id FROM conversation_summaries",
        ) else {
            continue;
        };
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            ))
        });
        let Ok(rows) = rows else { continue };
        for (id, title, uris, modified, parent) in rows.flatten() {
            let cwd = serde_json::from_str::<Vec<String>>(&uris)
                .ok()
                .and_then(|uris| uris.first().and_then(|uri| folder_of_uri(uri, env.is_windows())));
            let entry = found.entry(id).or_default();
            if entry.title.trim().is_empty() {
                entry.title = title.trim().to_owned();
            }
            entry.cwd = entry.cwd.take().or(cwd);
            let modified = parse_rfc3339_ms(&modified);
            entry.updated_ms = match (entry.updated_ms, modified) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
            if entry.parent.is_none() && !parent.trim().is_empty() {
                entry.parent = Some(parent);
            }
        }
    }
    found
}

/// The logs folder of a transcript file, and the conversation id two folders up:
/// `brain/<id>/.system_generated/logs/transcript.jsonl`.
fn conversation_of(path: &str) -> Option<(&str, &str)> {
    let logs = &path[..path.rfind(['/', '\\'])?];
    let generated = &logs[..logs.rfind(['/', '\\'])?];
    let conversation = &generated[..generated.rfind(['/', '\\'])?];
    let id = conversation.rsplit(['/', '\\']).next()?;
    Some((logs, id))
}

/// The steps of the conversation whose logs are in `logs`.
fn read_steps(env: &dyn Environment, logs: &str) -> io::Result<Vec<Value>> {
    let read = |name: &str| env.read(&env.join(logs, name), ReadRange::All).unwrap_or_default();
    let joined = |name: &str| {
        let mut pieces: Vec<FileMeta> = env.find_files(&env.join(&env.join(logs, "chunks"), name), ".jsonl", 1).unwrap_or_default();
        pieces.sort_by(|a, b| a.path.cmp(&b.path));
        let mut bytes = Vec::new();
        for piece in pieces {
            bytes.extend(env.read(&piece.path, ReadRange::All).unwrap_or_default());
        }
        bytes
    };
    let sources = [joined("transcript"), read("transcript.jsonl"), joined("transcript_full"), read("transcript_full.jsonl")];
    if sources.iter().all(Vec::is_empty) {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("no transcript in {logs}")));
    }
    Ok(merge_steps(&sources.iter().map(Vec::as_slice).collect::<Vec<_>>()))
}

fn gemini_dir(env: &dyn Environment) -> io::Result<String> {
    Ok(env.join(&env.home()?, ".gemini"))
}

impl HistorySource for AntigravitySource {
    fn harness(&self) -> HarnessKind {
        HarnessKind::Antigravity
    }

    fn scan(&self, env: &dyn Environment, known: &dyn KnownSummaries, _cache: &Path) -> io::Result<Vec<SessionSummary>> {
        let gemini = gemini_dir(env)?;
        let mut files: HashMap<String, FileMeta> = HashMap::new();
        for dir in DATA_DIRS {
            let brain = env.join(&env.join(&gemini, dir), "brain");
            let mut found = env.find_files(&brain, "transcript.jsonl", 4).unwrap_or_default();
            found.extend(env.find_files(&brain, "transcript_full.jsonl", 4).unwrap_or_default());
            for meta in found {
                let Some((_, id)) = conversation_of(&meta.path) else { continue };
                // One file stands for the conversation: the cut transcript, which has every
                // step, when there is one; the newest of a conversation in two folders.
                let better = match files.get(id) {
                    None => true,
                    Some(current) => {
                        let cut = |m: &FileMeta| m.path.ends_with("transcript.jsonl");
                        (cut(&meta), meta.modified_ms) > (cut(current), current.modified_ms)
                    }
                };
                if better {
                    files.insert(id.to_owned(), meta);
                }
            }
        }
        let index = read_indexes(env, &gemini);
        let mut sessions = Vec::new();
        for (id, meta) in files {
            let indexed = index.get(&id);
            let known = known.known(&meta.path);
            if let Some(summary) = &known
                && summary.source.size == meta.size
                && summary.source.modified_ms == meta.modified_ms
            {
                let mut summary = summary.clone();
                // A title set in the app after the last change of the transcript.
                if let Some(indexed) = indexed.filter(|i| !i.title.is_empty()) {
                    summary.title = indexed.title.clone();
                }
                sessions.push(summary);
                continue;
            }
            let Some((logs, _)) = conversation_of(&meta.path) else { continue };
            let steps = match read_steps(env, logs) {
                Ok(steps) => steps,
                Err(error) => {
                    eprintln!("antigravity history {}: {error}", meta.path);
                    sessions.extend(known);
                    continue;
                }
            };
            let parsed = parse(&steps);
            if parsed.first_prompt.is_none() {
                continue;
            }
            let updated = parsed.updated_ms.into_iter().chain(indexed.and_then(|i| i.updated_ms)).max();
            sessions.push(SessionSummary {
                key: SessionKey { env: env.id(), harness: HarnessKind::Antigravity, id: id.clone() },
                title: indexed.map(|i| i.title.clone()).unwrap_or_default(),
                first_prompt: parsed.first_prompt,
                cwd: indexed.and_then(|i| i.cwd.clone()),
                git_branch: None,
                model: None,
                created_ms: parsed.created_ms.unwrap_or(meta.modified_ms),
                updated_ms: updated.unwrap_or(meta.modified_ms),
                turn: parsed.turn,
                parent: indexed.and_then(|i| i.parent.clone()),
                source: SourceStamp { path: meta.path.clone(), size: meta.size, modified_ms: meta.modified_ms },
            });
        }
        Ok(sessions)
    }

    fn load(&self, env: &dyn Environment, summary: &SessionSummary, _cache: &Path) -> io::Result<Transcript> {
        let (logs, _) = conversation_of(&summary.source.path).ok_or_else(|| io::Error::other("not a transcript path"))?;
        Ok(parse(&read_steps(env, logs)?).transcript)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use envs::LocalEnvironment;
    use test_support::TempDir;

    fn line(value: Value) -> String {
        format!("{value}\n")
    }

    fn conversation() -> Vec<Value> {
        vec![
            serde_json::json!({"step_index": 0, "source": "USER_EXPLICIT", "type": "USER_INPUT", "status": "DONE", "created_at": "2026-03-01T10:00:00Z", "content": "<USER_REQUEST>\nList the files\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nnow\n</ADDITIONAL_METADATA>"}),
            serde_json::json!({"step_index": 1, "source": "SYSTEM", "type": "CONVERSATION_HISTORY", "status": "DONE", "created_at": "2026-03-01T10:00:00Z"}),
            serde_json::json!({"step_index": 2, "source": "MODEL", "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-03-01T10:00:01Z", "thinking": "Look first.", "tool_calls": [
                {"name": "list_dir", "args": {"DirectoryPath": "/work/app", "toolAction": "Listing", "toolSummary": "the folder"}},
                {"name": "run_command", "args": {"CommandLine": "cargo test", "Cwd": "/work/app", "toolAction": "Running", "toolSummary": "the tests"}}
            ]}),
            serde_json::json!({"step_index": 3, "source": "MODEL", "type": "LIST_DIRECTORY", "status": "DONE", "created_at": "2026-03-01T10:00:02Z", "content": "src\nCargo.toml"}),
            serde_json::json!({"step_index": 4, "source": "MODEL", "type": "RUN_COMMAND", "status": "ERROR", "created_at": "2026-03-01T10:00:03Z", "content": "1 failed"}),
            serde_json::json!({"step_index": 5, "source": "MODEL", "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-03-01T10:00:04Z", "content": "Two entries; one test fails."}),
        ]
    }

    #[test]
    fn steps_become_a_transcript_with_results_in_order() {
        let parsed = parse(&conversation());
        assert_eq!(parsed.first_prompt.as_deref(), Some("List the files"), "the prompt without its wrapping");
        assert_eq!(parsed.turn, Some(TurnState::Idle));
        let entries = &parsed.transcript.entries;
        assert_eq!(entries.len(), 2, "the steps of a turn join");
        let Block::ToolCall { kind, input, result, .. } = &entries[1].blocks[1] else { panic!("{:?}", entries[1].blocks) };
        assert_eq!(*kind, ToolKind::Read);
        assert_eq!(input["path"], "/work/app");
        assert_eq!(result.as_ref().unwrap().text, "src\nCargo.toml");
        let Block::ToolCall { kind, input, result, .. } = &entries[1].blocks[2] else { panic!() };
        assert_eq!(*kind, ToolKind::Shell);
        assert_eq!(input["command"], "cargo test");
        assert!(result.as_ref().unwrap().is_error);
        assert!(matches!(&entries[1].blocks[3], Block::Text { text } if text.starts_with("Two entries")));
        assert_eq!(parse(&conversation()[..3]).turn, Some(TurnState::ToolPending), "calls wait on their results");
    }

    #[test]
    fn whole_steps_win_over_cut_ones_and_broken_lines_are_passed_over() {
        let cut = line(serde_json::json!({"step_index": 0, "source": "USER_EXPLICIT", "type": "USER_INPUT", "content": "<USER_REQUEST>short</USER_REQUEST>", "truncated_fields": ["content"]}))
            + &line(serde_json::json!({"step_index": 1, "source": "MODEL", "type": "PLANNER_RESPONSE", "content": "first"}));
        let whole = line(serde_json::json!({"step_index": 0, "source": "USER_EXPLICIT", "type": "USER_INPUT", "content": "<USER_REQUEST>the whole prompt</USER_REQUEST>"}));
        let mut pieces = line(serde_json::json!({"step_index": 2, "source": "MODEL", "type": "PLANNER_RESPONSE", "content": "second"})).into_bytes();
        pieces.extend_from_slice("{\"step_index\": 3, \"content\": \"cut in the mid".as_bytes());
        let steps = merge_steps(&[cut.as_bytes(), &pieces, whole.as_bytes()]);
        assert_eq!(steps.len(), 3);
        assert_eq!(parse(&steps).first_prompt.as_deref(), Some("the whole prompt"));
    }

    #[test]
    fn a_workspace_uri_is_a_folder() {
        assert_eq!(folder_of_uri("file:///c%3A/Users/me/my%20app", true).as_deref(), Some("C:\\Users\\me\\my app"));
        assert_eq!(folder_of_uri("file:///d:/work", true).as_deref(), Some("D:\\work"));
        assert_eq!(folder_of_uri("file:///home/me/app", false).as_deref(), Some("/home/me/app"));
        assert_eq!(folder_of_uri("/plain", false), None);
    }

    #[test]
    fn the_brain_folders_and_the_index_make_the_list() {
        let dir = TempDir::new("antigravity_scan");
        let gemini = dir.path().join(".gemini");
        let logs = gemini.join("antigravity/brain/11111111-2222-3333-4444-555555555555/.system_generated/logs");
        std::fs::create_dir_all(&logs).unwrap();
        let text: String = conversation().into_iter().map(line).collect();
        std::fs::write(logs.join("transcript.jsonl"), &text).unwrap();
        let ide = gemini.join("antigravity-ide/brain/99999999-2222-3333-4444-555555555555/.system_generated/logs");
        std::fs::create_dir_all(&ide).unwrap();
        std::fs::write(ide.join("transcript_full.jsonl"), line(serde_json::json!({"step_index": 0, "source": "USER_EXPLICIT", "type": "USER_INPUT", "created_at": "2026-03-02T10:00:00Z", "content": "From the IDE"}))).unwrap();
        let index = Connection::open(gemini.join("antigravity/conversation_summaries.db")).unwrap();
        index
            .execute_batch(
                "CREATE TABLE conversation_summaries (conversation_id text, title text, preview text, workspace_uris text, last_modified_time datetime, parent_conversation_id text);
                 INSERT INTO conversation_summaries VALUES ('11111111-2222-3333-4444-555555555555', 'Listing files', '', '[\"file:///work/app\"]', '2026-03-01 10:05:00.1234567+00:00', '');",
            )
            .unwrap();
        drop(index);
        let env = LocalEnvironment::with_home(dir.path());
        let mut sessions = AntigravitySource.scan(&env, &(), dir.path()).unwrap();
        sessions.sort_by(|a, b| a.key.id.cmp(&b.key.id));
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].title, "Listing files");
        assert!(sessions[0].cwd.as_deref().is_some_and(|cwd| cwd.ends_with("work/app") || cwd.ends_with("work\\app")), "{:?}", sessions[0].cwd);
        assert_eq!(sessions[0].updated_ms, parse_rfc3339_ms("2026-03-01T10:05:00.123Z").unwrap());
        assert_eq!(sessions[1].title, "", "the IDE keeps no index: the first prompt names it");
        assert_eq!(sessions[1].first_prompt.as_deref(), Some("From the IDE"));
        let transcript = AntigravitySource.load(&env, &sessions[0], dir.path()).unwrap();
        assert_eq!(transcript.entries.len(), 2);
    }
}
