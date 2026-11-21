//! OpenCode: one SQLite database, `~/.local/share/opencode/opencode.db`, on every system.
//! Version 2 keeps sessions in `session_v2` and their messages in `session_message`
//! (`type` and a JSON `data`); older versions used `session`, `message` and `part`. Both
//! are read, newer first.
//!
//! The same file holds account and credential tables. The connection is read-only and the
//! queries name the session tables only. On an SSH host the file is not copied whole:
//! Python there opens it read-only and copies only the session rows a read needs into a
//! small file, so credentials stay on the host (see [`EXTRACT_SCRIPT`]).

use std::io;
use std::path::{Path, PathBuf};

use base64::Engine as _;

use envs::Environment;
use model::{
    Block, Entry, HarnessKind, Role, SessionKey, SessionSummary, SourceStamp, ToolKind, ToolResult,
    Transcript, TurnState, Usage,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;

use crate::{HistorySource, KnownSummaries, text_of};

pub struct OpenCodeSource;

fn db_path(env: &dyn Environment) -> io::Result<String> {
    let home = env.home()?;
    let mut path = home;
    for part in [".local", "share", "opencode", "opencode.db"] {
        path = env.join(&path, part);
    }
    Ok(path)
}

/// What a read of a remote database needs.
#[derive(Clone, Copy)]
enum Extract<'a> {
    /// Every session, with the first prompt and the last message of each.
    Scan,
    /// One session with all its messages.
    Session(&'a str),
}

/// Python on the host opens the database read-only and copies the session rows a read
/// needs into a temporary file, sent back in base64 and removed. Arguments: the database,
/// `scan` or `session`, the session id.
const EXTRACT_SCRIPT: &str = r#"
import base64, os, sqlite3, sys, tempfile
db, mode, sid = sys.argv[1], sys.argv[2], sys.argv[3]
src = sqlite3.connect('file:' + db + '?mode=ro', uri=True)
tables = {row[0] for row in src.execute("SELECT name FROM sqlite_master WHERE type = 'table'")}
fd, out = tempfile.mkstemp(suffix='.db')
os.close(fd)
try:
    dst = sqlite3.connect(out)
    def copy(table, select=None, args=(), slim=None):
        if table not in tables:
            return
        cols = [row[1] for row in src.execute('PRAGMA table_info("%s")' % table)]
        dst.execute('CREATE TABLE "%s" (%s)' % (table, ', '.join('"%s"' % c for c in cols)))
        rows = None
        if slim:
            # Only the fields a read looks at; whole rows where SQLite has no JSON functions.
            listed = ', '.join(slim.get(c, 'm."%s"' % c) for c in cols)
            try:
                rows = src.execute((select or 'SELECT * FROM "%s"' % table).replace('m.*', listed, 1), args).fetchall()
            except sqlite3.OperationalError:
                rows = None
        if rows is None:
            rows = src.execute(select or 'SELECT * FROM "%s"' % table, args)
        dst.executemany('INSERT INTO "%s" VALUES (%s)' % (table, ', '.join('?' * len(cols))), rows)
    if mode == 'scan':
        copy('session_v2')
        copy('session')
        # The first prompt gives the title of an unnamed session; the last message tells
        # where the turn stands.
        copy('session_message', """SELECT m.* FROM session_message m JOIN (
            SELECT session_id, min(CASE WHEN type = 'user' THEN seq END) AS first,
                   max(CASE WHEN type <> 'system' THEN seq END) AS last
            FROM session_message GROUP BY session_id) g
          ON m.session_id = g.session_id AND (m.seq = g.first OR m.seq = g.last)""",
          slim={'data': """json_object('text', substr(json_extract(m.data, '$.text'), 1, 600),
            'finish', json_extract(m.data, '$.finish'),
            'time', json_object('completed', json_extract(m.data, '$.time.completed')),
            'content', (SELECT json_group_array(json_object('state', json_object('status', json_extract(value, '$.state.status'))))
                        FROM json_each(m.data, '$.content')))"""})
    else:
        copy('session_v2', 'SELECT * FROM session_v2 WHERE id = ?', (sid,))
        copy('session', 'SELECT * FROM session WHERE id = ?', (sid,))
        copy('session_message', 'SELECT * FROM session_message WHERE session_id = ?', (sid,))
        copy('message', 'SELECT * FROM message WHERE session_id = ?', (sid,))
        copy('part', 'SELECT * FROM part WHERE message_id IN (SELECT id FROM message WHERE session_id = ?)', (sid,))
    dst.commit()
    dst.close()
    with open(out, 'rb') as extract:
        sys.stdout.write(base64.b64encode(extract.read()).decode())
finally:
    os.remove(out)
"#;

/// A file the reader opens: the database itself on this computer, a copy of what a read
/// needs from a remote one. A temporary copy is removed when dropped.
struct Readable {
    path: PathBuf,
    temporary: bool,
}

impl Drop for Readable {
    fn drop(&mut self) {
        if self.temporary {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// The database to read: local as it is; remote through [`EXTRACT_SCRIPT`], or copied
/// whole when the host has no `python3`.
fn readable(env: &dyn Environment, db: &str, extract: Extract, cache: &Path) -> io::Result<Readable> {
    if env.id().is_local() {
        return Ok(Readable { path: env.local_copy(db, cache)?, temporary: false });
    }
    let (mode, id) = match extract {
        Extract::Scan => ("scan", ""),
        Extract::Session(id) => ("session", id),
    };
    // Only a host without Python gets the whole file copied: any other failure (a dropped
    // connection, a locked database) is an error, so the credentials never travel by accident.
    let probe = r##"command -v python3 >/dev/null 2>&1 || { echo "#no-python#"; exit 0; }; exec python3 -c "$0" "$@""##;
    match env.run("sh", &["-c", probe, EXTRACT_SCRIPT, db, mode, id], None) {
        Ok(out) if out.trim() == "#no-python#" => Ok(Readable { path: env.local_copy(db, cache)?, temporary: false }),
        Ok(out) => {
            std::fs::create_dir_all(cache)?;
            let name: String = format!("{}-opencode-{mode}-{id}-{}.db", env.id().0, std::process::id())
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
                .collect();
            let path = cache.join(name);
            std::fs::write(&path, decode_extract(&out)?)?;
            Ok(Readable { path, temporary: true })
        }
        Err(error) => Err(error),
    }
}

fn decode_extract(out: &str) -> io::Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(out.trim()).map_err(|error| io::Error::other(format!("the extract of the database: {error}")))
}

fn open(path: &Path) -> io::Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(io::Error::other)
}

fn has_table(connection: &Connection, name: &str) -> bool {
    connection
        .query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1", [name], |_| Ok(()))
        .optional()
        .ok()
        .flatten()
        .is_some()
}

fn sql_error(error: rusqlite::Error) -> io::Error {
    io::Error::other(error)
}

fn model_id(text: Option<String>) -> Option<String> {
    let text = text?;
    match serde_json::from_str::<Value>(&text) {
        Ok(value) => value.get("id").and_then(Value::as_str).map(str::to_owned),
        Err(_) => Some(text),
    }
}

impl HistorySource for OpenCodeSource {
    fn harness(&self) -> HarnessKind {
        HarnessKind::OpenCode
    }

    fn scan(&self, env: &dyn Environment, _known: &dyn KnownSummaries, cache: &Path) -> io::Result<Vec<SessionSummary>> {
        let db = db_path(env)?;
        if env.stat(&db).is_err() {
            return Ok(Vec::new());
        }
        let local = readable(env, &db, Extract::Scan, cache)?;
        let connection = open(&local.path)?;
        let mut sessions = Vec::new();
        let summary = |id: String, parent: Option<String>, dir: Option<String>, title: Option<String>,
                       model: Option<String>, created: i64, updated: i64, prompt: Option<String>| SessionSummary {
            key: SessionKey { env: env.id(), harness: HarnessKind::OpenCode, id: id.clone() },
            title: title.unwrap_or_default(),
            first_prompt: prompt.map(|p| model::shorten(p.trim(), 500)),
            cwd: dir,
            git_branch: None,
            model: model_id(model),
            created_ms: created,
            updated_ms: updated,
            parent,
            source: SourceStamp { path: format!("{db}#{id}"), size: 0, modified_ms: updated },
            turn: None,
        };
        if has_table(&connection, "session_v2") {
            let mut statement = connection
                .prepare(
                    "SELECT s.id, s.parent_id, s.directory, s.title, s.model, s.time_created, s.time_updated,
                       (SELECT json_extract(m.data, '$.text') FROM session_message m
                         WHERE m.session_id = s.id AND m.type = 'user' ORDER BY m.seq LIMIT 1)
                     FROM session_v2 s WHERE s.time_archived IS NULL",
                )
                .map_err(sql_error)?;
            let rows = statement
                .query_map([], |row| {
                    Ok(summary(row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?))
                })
                .map_err(sql_error)?;
            sessions.extend(rows.flatten());
        }
        if has_table(&connection, "session") {
            let known: std::collections::HashSet<String> = sessions.iter().map(|s| s.key.id.clone()).collect();
            let mut statement = connection
                .prepare("SELECT id, parent_id, directory, title, model, time_created, time_updated FROM session")
                .map_err(sql_error)?;
            let rows = statement
                .query_map([], |row| {
                    Ok(summary(row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, None))
                })
                .map_err(sql_error)?;
            sessions.extend(rows.flatten().filter(|s| !known.contains(&s.key.id)));
        }
        // Where the turn stands, for the sessions written lately; older ones show no state.
        if has_table(&connection, "session_message") {
            let recent = now_ms() - TURN_WINDOW_MS;
            let mut statement = connection
                .prepare(
                    "SELECT type, json_extract(data, '$.finish'), json_extract(data, '$.time.completed'),
                       (SELECT count(*) FROM json_each(data, '$.content')
                         WHERE json_extract(value, '$.state.status') IN ('running', 'pending'))
                     FROM session_message WHERE session_id = ?1 AND type <> 'system'
                     ORDER BY seq DESC LIMIT 1",
                )
                .map_err(sql_error)?;
            for session in sessions.iter_mut().filter(|s| s.updated_ms >= recent) {
                session.turn = statement
                    .query_row([&session.key.id], |row| {
                        Ok(turn_of(&row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?.as_deref(), row.get::<_, Option<i64>>(2)?.is_some(), row.get::<_, i64>(3)? > 0))
                    })
                    .optional()
                    .ok()
                    .flatten()
                    .flatten();
            }
        }
        // OpenCode fills an unnamed session's title with zeros.
        for session in &mut sessions {
            if session.title.chars().all(|c| c == '0') {
                session.title.clear();
            }
        }
        Ok(sessions)
    }

    fn load(&self, env: &dyn Environment, summary: &SessionSummary, cache: &Path) -> io::Result<Transcript> {
        let db = db_path(env)?;
        let id = &summary.key.id;
        let local = readable(env, &db, Extract::Session(id), cache)?;
        let connection = open(&local.path)?;
        let mut transcript = if has_table(&connection, "session_message") {
            let mut statement = connection
                .prepare("SELECT type, data, time_created FROM session_message WHERE session_id = ?1 ORDER BY seq")
                .map_err(sql_error)?;
            let rows: Vec<(String, String, i64)> = statement
                .query_map([id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .map_err(sql_error)?
                .flatten()
                .collect();
            if rows.is_empty() { None } else { Some(parse_v2(&rows)) }
        } else {
            None
        };
        if transcript.is_none() && has_table(&connection, "part") {
            transcript = Some(load_v1(&connection, id)?);
        }
        let mut transcript = transcript.unwrap_or_default();
        if transcript.model.is_none() {
            transcript.model = summary.model.clone();
        }
        Ok(transcript)
    }
}

fn tokens_of(value: &Value) -> Option<Usage> {
    let tokens = value.get("tokens")?;
    let field = |pointer: &str| tokens.pointer(pointer).and_then(Value::as_u64).unwrap_or(0);
    Some(Usage {
        input: field("/input"),
        output: field("/output") + field("/reasoning"),
        cache_read: field("/cache/read"),
        cache_write: field("/cache/write"),
        cache_write_long: 0,
    })
}

fn tool_block(item: &Value, id_field: &str, name_field: &str) -> Block {
    let name = item.get(name_field).and_then(Value::as_str).unwrap_or("tool").to_owned();
    let state = item.get("state").unwrap_or(&Value::Null);
    let result = match state.get("status").and_then(Value::as_str) {
        Some("completed") => Some(ToolResult {
            text: state
                .get("output")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| text_of(state.get("content").unwrap_or(&Value::Null))),
            is_error: false,
        }),
        Some("error") => Some(ToolResult {
            text: state
                .pointer("/error/message")
                .or_else(|| state.get("error"))
                .map(|error| error.as_str().map(str::to_owned).unwrap_or_else(|| error.to_string()))
                .unwrap_or_default(),
            is_error: true,
        }),
        _ => None,
    };
    Block::ToolCall {
        id: item.get(id_field).and_then(Value::as_str).unwrap_or("").to_owned(),
        kind: ToolKind::classify(&name),
        name,
        input: state.get("input").cloned().unwrap_or(Value::Null),
        result,
    }
}

/// Rows of `session_message` in order: `(type, data, time_created)`.
pub fn parse_v2(rows: &[(String, String, i64)]) -> Transcript {
    let mut transcript = Transcript::default();
    for (kind, data, created) in rows {
        let Ok(value) = serde_json::from_str::<Value>(data) else { continue };
        let next_id = transcript.entries.len().to_string();
        let entry = |role, blocks, compact| Entry {
            id: next_id.clone(),
            role,
            timestamp_ms: *created,
            blocks,
            model: None,
            usage: None,
            compact_summary: compact,
        };
        match kind.as_str() {
            "user" => {
                let text = value.get("text").and_then(Value::as_str).unwrap_or("").to_owned();
                let mut blocks = vec![Block::Text { text }];
                for key in ["files", "attachments"] {
                    for file in value.get(key).and_then(Value::as_array).into_iter().flatten() {
                        blocks.push(file_block(file));
                    }
                }
                transcript.entries.push(entry(Role::User, blocks, false));
            }
            "compaction" => {
                let text = value.get("summary").and_then(Value::as_str).unwrap_or("").to_owned();
                transcript.entries.push(entry(Role::User, vec![Block::Text { text }], true));
            }
            "system" | "synthetic" => {
                let text = value
                    .get("description")
                    .or_else(|| value.get("text"))
                    .and_then(Value::as_str)
                    .map(|text| model::shorten(text, 300))
                    .unwrap_or_default();
                if !text.is_empty() {
                    transcript.entries.push(entry(Role::System, vec![Block::Notice { text }], false));
                }
            }
            "assistant" => {
                let mut blocks = Vec::new();
                for item in value.get("content").and_then(Value::as_array).into_iter().flatten() {
                    match item.get("type").and_then(Value::as_str) {
                        Some("text") => blocks.push(Block::Text { text: text_of(item.get("text").unwrap_or(&Value::Null)) }),
                        Some("reasoning") => {
                            let text = text_of(item.get("text").unwrap_or(&Value::Null));
                            if !text.trim().is_empty() {
                                blocks.push(Block::Thinking { text });
                            }
                        }
                        Some("tool") => blocks.push(tool_block(item, "id", "name")),
                        _ => {}
                    }
                }
                let usage = tokens_of(&value);
                if let Some(model) = value.pointer("/model/id").and_then(Value::as_str) {
                    transcript.model = Some(model.to_owned());
                }
                if let Some(usage) = usage {
                    let at = value.pointer("/time/completed").and_then(Value::as_i64).unwrap_or(*created);
                    let model = transcript.model.clone();
                    transcript.add_call(model.as_deref(), usage, at);
                }
                if let Some(cost) = value.get("cost").and_then(Value::as_f64).filter(|cost| *cost > 0.0) {
                    *transcript.cost_usd.get_or_insert(0.0) += cost;
                }
                // Every step of a turn is a message of its own; the window shows the turn.
                match transcript.entries.last_mut() {
                    Some(last) if last.role == Role::Assistant => {
                        last.blocks.extend(blocks);
                        last.usage = usage.or(last.usage);
                    }
                    _ => {
                        let mut new = entry(Role::Assistant, blocks, false);
                        new.usage = usage;
                        transcript.entries.push(new);
                    }
                }
            }
            _ => {}
        }
    }
    transcript
}

/// How far back a session's turn is read; the window shows no state for older ones.
const TURN_WINDOW_MS: i64 = 3 * 3_600_000;

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// Where a turn stands after the last message of a session: its type, and for a step of
/// the model its finish reason, whether it completed and whether a tool of it still runs.
fn turn_of(kind: &str, finish: Option<&str>, completed: bool, tool_running: bool) -> Option<TurnState> {
    match kind {
        "idle" => Some(TurnState::Idle),
        "user" | "synthetic" | "compaction" => Some(TurnState::Working),
        "assistant" if tool_running => Some(TurnState::ToolPending),
        "assistant" if !completed => Some(TurnState::Working),
        // The tools of the step ran; the next step is on its way.
        "assistant" if finish == Some("tool-calls") => Some(TurnState::Working),
        "assistant" => Some(TurnState::Idle),
        _ => None,
    }
}

/// The layout before version 2: messages with their parts in a table of their own.
fn load_v1(connection: &Connection, id: &str) -> io::Result<Transcript> {
    let mut transcript = Transcript::default();
    let mut messages = connection
        .prepare("SELECT id, data, time_created FROM message WHERE session_id = ?1 ORDER BY time_created")
        .map_err(sql_error)?;
    let messages: Vec<(String, String, i64)> = messages
        .query_map([id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .map_err(sql_error)?
        .flatten()
        .collect();
    let mut parts = connection
        .prepare("SELECT data FROM part WHERE message_id = ?1 ORDER BY time_created, id")
        .map_err(sql_error)?;
    for (message_id, data, created) in messages {
        let value: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
        let role = match value.get("role").and_then(Value::as_str) {
            Some("user") => Role::User,
            _ => Role::Assistant,
        };
        let mut blocks = Vec::new();
        let rows: Vec<String> = parts.query_map([&message_id], |row| row.get(0)).map_err(sql_error)?.flatten().collect();
        for part in rows {
            let part: Value = serde_json::from_str(&part).unwrap_or(Value::Null);
            match part.get("type").and_then(Value::as_str) {
                Some("text") => blocks.push(Block::Text { text: text_of(part.get("text").unwrap_or(&Value::Null)) }),
                Some("reasoning") => blocks.push(Block::Thinking { text: text_of(part.get("text").unwrap_or(&Value::Null)) }),
                Some("tool") => blocks.push(tool_block(&part, "callID", "tool")),
                Some("file") => blocks.push(file_block(&part)),
                _ => {}
            }
        }
        let usage = tokens_of(&value);
        if let Some(model) = value.get("modelID").and_then(Value::as_str) {
            transcript.model = Some(model.to_owned());
        }
        if let Some(usage) = usage {
            let model = transcript.model.clone();
            transcript.add_call(model.as_deref(), usage, created);
        }
        transcript.entries.push(Entry { id: message_id, role, timestamp_ms: created, blocks, model: None, usage, compact_summary: false });
    }
    Ok(transcript)
}

#[cfg(test)]
mod tests {
    use super::*;
    use envs::LocalEnvironment;
    use test_support::TempDir;

    fn fixture(home: &Path) {
        let folder = home.join(".local/share/opencode");
        std::fs::create_dir_all(&folder).unwrap();
        let connection = Connection::open(folder.join("opencode.db")).unwrap();
        connection
            .execute_batch(
                r#"
                CREATE TABLE credential (id TEXT, value TEXT);
                INSERT INTO credential VALUES ('c', 'secret');
                CREATE TABLE session_v2 (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT, title TEXT,
                  model TEXT, time_created INTEGER, time_updated INTEGER, time_archived INTEGER);
                CREATE TABLE session_message (id TEXT, session_id TEXT, type TEXT, seq INTEGER,
                  time_created INTEGER, data TEXT);
                INSERT INTO session_v2 VALUES ('ses_1', NULL, 'C:/work/web', '0000000000',
                  '{"id":"deepseek-v4.1-flash","providerID":"x"}', 1000, 5000, NULL);
                INSERT INTO session_v2 VALUES ('ses_old', NULL, 'C:/work/web', 'Archived', NULL, 1, 2, 3);
                INSERT INTO session_message VALUES ('m1', 'ses_1', 'user', 1, 1000,
                  '{"text":"Run the tests","files":[{"data":"iVBORw0K","mime":"image/png","source":{"type":"inline"},"name":"shot.png"}]}');
                INSERT INTO session_message VALUES ('m2', 'ses_1', 'assistant', 2, 1100,
                  '{"content":[{"type":"reasoning","text":"thinking"},{"type":"tool","id":"t1","name":"shell","state":{"status":"completed","input":{"command":"npm test"},"content":[{"type":"text","text":"3 passed"}]}}],"tokens":{"input":10,"output":5,"reasoning":1,"cache":{"read":400,"write":0}}}');
                INSERT INTO session_message VALUES ('m3', 'ses_1', 'assistant', 3, 1200,
                  '{"content":[{"type":"text","text":"All green."}],"model":{"id":"deepseek-v4.1-flash"},"tokens":{"input":2,"output":3,"reasoning":0,"cache":{"read":420,"write":0}}}');
                INSERT INTO session_message VALUES ('m4', 'ses_1', 'idle', 4, 1300, '{}');
                "#,
            )
            .unwrap();
    }

    #[test]
    fn the_last_message_tells_where_the_turn_stands() {
        assert_eq!(turn_of("idle", None, false, false), Some(TurnState::Idle));
        assert_eq!(turn_of("user", None, false, false), Some(TurnState::Working));
        assert_eq!(turn_of("assistant", None, false, false), Some(TurnState::Working), "still streaming");
        assert_eq!(turn_of("assistant", Some("tool-calls"), true, true), Some(TurnState::ToolPending));
        assert_eq!(turn_of("assistant", Some("tool-calls"), true, false), Some(TurnState::Working));
        assert_eq!(turn_of("assistant", Some("stop"), true, false), Some(TurnState::Idle));
        assert_eq!(turn_of("session_inbox", None, false, false), None);

        let dir = TempDir::new("opencode_turn");
        fixture(dir.path());
        let db = dir.path().join(".local/share/opencode/opencode.db");
        let connection = Connection::open(&db).unwrap();
        let now = now_ms();
        connection.execute("UPDATE session_v2 SET time_updated = ?1 WHERE id = 'ses_1'", [now]).unwrap();
        drop(connection);
        let env = LocalEnvironment::with_home(dir.path());
        let sessions = OpenCodeSource.scan(&env, &(), dir.path()).unwrap();
        assert_eq!(sessions[0].turn, Some(TurnState::Idle), "the turn ended with idle");

        let connection = Connection::open(&db).unwrap();
        connection
            .execute(
                "INSERT INTO session_message VALUES ('m5', 'ses_1', 'assistant', 5, 1400,
                  '{\"content\":[{\"type\":\"tool\",\"id\":\"t2\",\"name\":\"shell\",\"state\":{\"status\":\"running\"}}],\"finish\":\"tool-calls\",\"time\":{\"completed\":1}}')",
                [],
            )
            .unwrap();
        drop(connection);
        let sessions = OpenCodeSource.scan(&env, &(), dir.path()).unwrap();
        assert_eq!(sessions[0].turn, Some(TurnState::ToolPending), "a tool still runs");
    }

    /// The extract a host sends: only session tables, and for a scan only the first
    /// prompt and the last message of each session. Runs the script with the Python of
    /// this computer, if there is one.
    #[test]
    fn the_extract_keeps_session_rows_only() {
        let python = ["python3", "python"].into_iter().find(|python| {
            std::process::Command::new(python).arg("--version").output().is_ok_and(|out| out.status.success())
        });
        let Some(python) = python else { return };
        let dir = TempDir::new("opencode_extract");
        fixture(dir.path());
        let db = dir.path().join(".local/share/opencode/opencode.db");
        let extract = |mode: &str, id: &str| {
            let out = std::process::Command::new(python).args(["-c", EXTRACT_SCRIPT, db.to_str().unwrap(), mode, id]).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            let path = dir.path().join(format!("{mode}.db"));
            std::fs::write(&path, decode_extract(&String::from_utf8(out.stdout).unwrap()).unwrap()).unwrap();
            Connection::open(path).unwrap()
        };
        let ids = |connection: &Connection| -> Vec<String> {
            let mut statement = connection.prepare("SELECT id FROM session_message ORDER BY seq").unwrap();
            statement.query_map([], |row| row.get(0)).unwrap().flatten().collect()
        };
        let scan = extract("scan", "");
        assert!(!has_table(&scan, "credential"), "credentials stay on the host");
        assert_eq!(scan.query_row("SELECT count(*) FROM session_v2", [], |row| row.get::<_, i64>(0)).unwrap(), 2);
        assert_eq!(ids(&scan), ["m1", "m4"], "the first prompt and the last message");
        let session = extract("session", "ses_1");
        assert!(!has_table(&session, "credential"));
        assert_eq!(ids(&session), ["m1", "m2", "m3", "m4"]);
    }

    /// A remote scan and load through the extract, on a real host with Python: the
    /// synthetic database goes into a temporary home there. `BOWYER_SSH_HOST=user@host
    /// BOWYER_SSH_KEY=key cargo test -p history -- --ignored opencode_over_ssh`.
    #[test]
    #[ignore = "needs an SSH host"]
    fn opencode_over_ssh_reads_through_the_extract() {
        let host = std::env::var("BOWYER_SSH_HOST").expect("BOWYER_SSH_HOST");
        let key = std::env::var_os("BOWYER_SSH_KEY").map(std::path::PathBuf::from);
        let ssh = envs::SshEnvironment::new(&host).with_identity(key);
        let dir = TempDir::new("opencode_ssh");
        fixture(dir.path());
        let bytes = std::fs::read(dir.path().join(".local/share/opencode/opencode.db")).unwrap();
        let remote = format!("/tmp/bowyer-oc-{}", std::process::id());
        let db = format!("{remote}/opencode.db");
        ssh.run_with_input("sh", &["-c", &format!("mkdir -p {remote} && cat > {db}")], None, &bytes).unwrap();
        let cache = dir.path().join("cache");
        let scanned = readable(&ssh, &db, Extract::Scan, &cache).map(|readable| {
            let connection = open(&readable.path).unwrap();
            (has_table(&connection, "credential"), connection.query_row("SELECT count(*) FROM session_message", [], |row| row.get::<_, i64>(0)).unwrap())
        });
        let loaded = readable(&ssh, &db, Extract::Session("ses_1"), &cache).map(|readable| {
            let connection = open(&readable.path).unwrap();
            connection.query_row("SELECT count(*) FROM session_message", [], |row| row.get::<_, i64>(0)).unwrap()
        });
        let _ = ssh.run("rm", &["-rf", &remote], None);
        assert_eq!(scanned.unwrap(), (false, 2));
        assert_eq!(loaded.unwrap(), 4);
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 0, "the extracts are removed after reading");
    }

    #[test]
    fn sessions_and_turns_come_from_version_2() {
        let dir = TempDir::new("opencode");
        fixture(dir.path());
        let env = LocalEnvironment::with_home(dir.path());
        let sessions = OpenCodeSource.scan(&env, &(), dir.path()).unwrap();
        assert_eq!(sessions.len(), 1, "archived sessions stay out");
        let session = &sessions[0];
        assert_eq!(session.title, "", "a title of zeros is no title");
        assert_eq!(session.display_title(), "Run the tests");
        assert_eq!(session.model.as_deref(), Some("deepseek-v4.1-flash"));

        let transcript = OpenCodeSource.load(&env, session, dir.path()).unwrap();
        assert_eq!(transcript.entries.len(), 2, "steps of a turn join");
        assert!(matches!(
            &transcript.entries[0].blocks[1],
            Block::Image { media_type, data: Some(data) } if media_type == "image/png" && data == "iVBORw0K"
        ));
        assert_eq!(transcript.entries[1].blocks.len(), 3);
        assert_eq!(transcript.last_usage.map(|u| u.context()), Some(422));
        assert_eq!(transcript.tasks(false)[0].title, "npm test");
    }
}

/// An attached file. Version 2 keeps a picture as `{name, mime, data}` with the bytes in
/// base64 (text files go into the prompt itself); version 1 as `{filename, mime, url}` with a
/// data URL.
fn file_block(file: &Value) -> Block {
    let field = |key: &str| file.get(key).and_then(Value::as_str).unwrap_or("");
    let mime = field("mime");
    if mime.starts_with("image/") {
        let data = Some(field("data"))
            .filter(|data| !data.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                let url = field("url").strip_prefix("data:")?;
                url.split_once(";base64,").map(|(_, data)| data.to_owned())
            });
        return Block::Image { media_type: mime.to_owned(), data };
    }
    let url = field("url");
    let name = [field("name"), field("filename")]
        .into_iter()
        .find(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| url.rsplit(['/', '\\']).next().unwrap_or(url).to_owned());
    Block::Notice { text: format!("📎 {name}") }
}
