//! Bowyer's memory for harnesses: an MCP server on standard input and output
//! (`bowyer mcp`). A harness asks for what it needs instead of carrying it in its context:
//! past sessions found by words, the conversation of one of them, and notes kept per
//! project. Everything stays in Bowyer's own `library.db`; nothing goes over the network.
//!
//! The server answers for the project it was started in (the harness starts it in its
//! working folder): searching and reading other projects takes `all_projects: true`.
//! The switch `memory.enabled` in the library is read on every call, so turning the memory
//! off in the window takes effect at once; `memory.off_projects` turns it off for some
//! projects only.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use envs::{Environment, LocalEnvironment, SshEnvironment};
use library::Library;
use model::{Block, EnvId, Role, SessionKey, SessionSummary};
use serde_json::{Value, json};

/// The most text one answer carries.
const MAX_ANSWER: usize = 60_000;
const DEFAULT_READ: usize = 20_000;
const PROTOCOL: &str = "2025-06-18";

pub struct Server {
    library: Library,
    /// The working folder of the harness that started the server.
    project: String,
    /// Where copies of remote files are cached (an OpenCode database read over SSH).
    cache: PathBuf,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A folder in one spelling: forward slashes, no trailing one, lower case on Windows. Notes
/// are filed under it.
pub fn normalize(path: &str) -> String {
    let path = path.replace('\\', "/");
    let path = path.trim_end_matches('/');
    if cfg!(windows) { path.to_lowercase() } else { path.to_owned() }
}

/// The setting that lists the folders whose memory is off, one per line.
pub const OFF_PROJECTS: &str = "memory.off_projects";

/// The folders whose memory is off, in the spelling of [`normalize`].
pub fn off_projects(library: &Library) -> Vec<String> {
    library.setting(OFF_PROJECTS).unwrap_or_default().lines().map(str::trim).filter(|line| !line.is_empty()).map(normalize).collect()
}

/// Whether the memory answers in `project`: on as a whole, and not off for the project or
/// a folder that holds it.
pub fn memory_on(library: &Library, project: &str) -> bool {
    let enabled = library.setting("memory.enabled").is_none_or(|value| value != "false");
    enabled && !off_projects(library).iter().any(|off| in_project(Some(project), off))
}

/// Turns the memory of one project folder on or off.
pub fn set_project_memory(library: &Library, project: &str, on: bool) -> io::Result<()> {
    let project = normalize(project);
    let mut off = off_projects(library);
    off.retain(|folder| folder != &project);
    if !on {
        off.push(project);
    }
    library.set_setting(OFF_PROJECTS, &off.join("\n")).map_err(|e| io::Error::other(e.to_string()))
}

/// Whether `cwd` is the project or a folder inside it.
fn in_project(cwd: Option<&str>, project: &str) -> bool {
    let Some(cwd) = cwd else { return false };
    let (cwd, project) = (normalize(cwd), normalize(project));
    cwd == project || cwd.starts_with(&format!("{project}/"))
}

fn cut(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… (cut at {limit} characters)", &text[..end])
}

fn tools() -> Value {
    json!([
        {
            "name": "search_sessions",
            "description": "Find past AI coding sessions (Claude Code, OpenCode, Codex) by words in their titles, first prompts and folders, and in the conversations opened in Bowyer before. Answers with keys for read_session. Only this project's sessions unless all_projects is true.",
            "inputSchema": {"type": "object", "properties": {
                "query": {"type": "string", "description": "Words to look for."},
                "limit": {"type": "integer", "description": "At most this many sessions (default 10, at most 30)."},
                "all_projects": {"type": "boolean", "description": "Search every project, not only this one."}
            }, "required": ["query"]}
        },
        {
            "name": "read_session",
            "description": "The conversation of a past session found with search_sessions: prompts and answers, tool calls in one line each, tool output left out. Long sessions come cut; ask for less with max_chars.",
            "inputSchema": {"type": "object", "properties": {
                "key": {"type": "string", "description": "The key search_sessions gave."},
                "max_chars": {"type": "integer", "description": "At most this many characters (default 20000, at most 60000)."},
                "from_end": {"type": "boolean", "description": "Keep the end of a long session instead of its start."},
                "all_projects": {"type": "boolean", "description": "Allow a session of another project."}
            }, "required": ["key"]}
        },
        {
            "name": "save_note",
            "description": "Keep a short note for this project: a decision, a fact, a command that works. Notes outlive sessions and cost no context until searched.",
            "inputSchema": {"type": "object", "properties": {
                "text": {"type": "string", "description": "The note."}
            }, "required": ["text"]}
        },
        {
            "name": "search_notes",
            "description": "The notes of this project that hold all the given words, newest first; with no words, the latest notes.",
            "inputSchema": {"type": "object", "properties": {
                "query": {"type": "string", "description": "Words the notes must hold."},
                "all_projects": {"type": "boolean", "description": "Notes of every project."}
            }}
        },
        {
            "name": "delete_note",
            "description": "Remove a note by its id from search_notes.",
            "inputSchema": {"type": "object", "properties": {
                "id": {"type": "integer"}
            }, "required": ["id"]}
        }
    ])
}

impl Server {
    pub fn new(library: Library, project: impl Into<String>, cache: impl AsRef<Path>) -> Self {
        Self { library, project: project.into(), cache: cache.as_ref().to_path_buf() }
    }

    /// Serves requests line by line until the input ends.
    pub fn serve(&self, input: impl BufRead, mut output: impl Write) -> io::Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let Some(reply) = self.handle(&line) else { continue };
            writeln!(output, "{reply}")?;
            output.flush()?;
        }
        Ok(())
    }

    /// The answer to one JSON-RPC message; `None` for a notification.
    pub fn handle(&self, line: &str) -> Option<Value> {
        let message: Value = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(error) => {
                return Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": error.to_string()}}));
            }
        };
        let id = message.get("id").cloned()?; // No id: a notification, which gets no answer.
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "bowyer", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "Bowyer's memory: search past sessions and keep notes for this project. Look here before asking the user what was decided earlier."
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => Ok(self.call(&params)),
            _ => Err((-32601, format!("unknown method {method}"))),
        };
        Some(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
        })
    }

    fn call(&self, params: &Value) -> Value {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let args = params.get("arguments").cloned().unwrap_or(Value::Null);
        let outcome = if !memory_on(&self.library, &self.project) {
            Err("Bowyer's memory is turned off in the Bowyer window.".to_owned())
        } else {
            match name {
                "search_sessions" => self.search_sessions(&args),
                "read_session" => self.read_session(&args),
                "save_note" => self.save_note(&args),
                "search_notes" => self.search_notes(&args),
                "delete_note" => self.delete_note(&args),
                _ => Err(format!("unknown tool {name}")),
            }
        };
        match outcome {
            Ok(text) => json!({"content": [{"type": "text", "text": cut(&text, MAX_ANSWER)}], "isError": false}),
            Err(error) => json!({"content": [{"type": "text", "text": error}], "isError": true}),
        }
    }

    fn all_projects(args: &Value) -> bool {
        args.get("all_projects").and_then(Value::as_bool).unwrap_or(false)
    }

    /// The environment a session lives in: this computer, or an SSH host added in the
    /// window, with its key.
    fn env_for(&self, id: &EnvId) -> Result<Box<dyn Environment>, String> {
        if id.is_local() {
            return Ok(Box::new(LocalEnvironment::new()));
        }
        let records = self.library.ssh_environments().map_err(|e| e.to_string())?;
        records
            .into_iter()
            .filter(|record| record.enabled)
            .map(|record| SshEnvironment::new(&record.host).with_identity(record.identity.map(PathBuf::from)))
            .find(|ssh| &ssh.id() == id)
            .map(|ssh| {
                let password = envs::secret::exists(&ssh.host);
                Box::new(ssh.with_password(password)) as Box<dyn Environment>
            })
            .ok_or_else(|| "the SSH host of this session is not set up or is turned off in Bowyer".to_owned())
    }

    fn summary(&self, key: &SessionKey) -> Option<SessionSummary> {
        self.library.summaries().ok()?.into_iter().find(|s| &s.key == key)
    }

    fn search_sessions(&self, args: &Value) -> Result<String, String> {
        let query = args.get("query").and_then(Value::as_str).unwrap_or("").trim();
        if query.is_empty() {
            return Err("query is empty".into());
        }
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10).clamp(1, 30) as usize;
        let all = Self::all_projects(args);
        let hits = self.library.search(query, 200).map_err(|e| e.to_string())?;
        let summaries = self.library.summaries().map_err(|e| e.to_string())?;
        let mut lines = Vec::new();
        for hit in hits {
            let Some(summary) = summaries.iter().find(|s| s.key == hit.key) else { continue };
            if !all && !in_project(summary.cwd.as_deref(), &self.project) {
                continue;
            }
            lines.push(format!(
                "- {title}\n  key: {key}\n  {harness}, {folder}, updated {updated}\n  {snippet}",
                title = summary.display_title(),
                key = summary.key.encode(),
                harness = summary.key.harness,
                folder = summary.cwd.as_deref().unwrap_or("?"),
                updated = model::format_ms_utc(summary.updated_ms),
                snippet = hit.snippet.replace('\n', " "),
            ));
            if lines.len() >= limit {
                break;
            }
        }
        if lines.is_empty() {
            let scope = if all { "any project" } else { "this project (try all_projects: true)" };
            return Ok(format!("No sessions of {scope} match \"{query}\"."));
        }
        Ok(lines.join("\n"))
    }

    fn read_session(&self, args: &Value) -> Result<String, String> {
        let key = args.get("key").and_then(Value::as_str).and_then(SessionKey::decode).ok_or("no such key")?;
        let summary = self.summary(&key).ok_or("no session with this key")?;
        if !Self::all_projects(args) && !in_project(summary.cwd.as_deref(), &self.project) {
            return Err("this session belongs to another project; pass all_projects: true to read it".into());
        }
        let limit = args.get("max_chars").and_then(Value::as_u64).unwrap_or(DEFAULT_READ as u64).clamp(500, MAX_ANSWER as u64) as usize;
        let source = history::source_for(&key.harness).ok_or("no reader for this harness")?;
        // A session on an SSH host is read over a connection of its own, with the host's key.
        let env = self.env_for(&key.env)?;
        let transcript = source.load(&*env, &summary, &self.cache.join("remote")).map_err(|e| e.to_string())?;
        let mut text = format!("# {}\n{} · {}\n\n", summary.display_title(), key.harness, summary.cwd.as_deref().unwrap_or("?"));
        for entry in &transcript.entries {
            let who = match entry.role {
                Role::User => "User",
                Role::Assistant => "Assistant",
                Role::System => "Note",
            };
            for block in &entry.blocks {
                match block {
                    Block::Text { text: part } if !part.trim().is_empty() => {
                        text.push_str(&format!("{who}: {}\n\n", part.trim()));
                    }
                    Block::ToolCall { name, input, .. } => {
                        let what = ["command", "file_path", "path", "pattern", "description", "url"]
                            .iter()
                            .find_map(|field| input.get(*field).and_then(Value::as_str))
                            .unwrap_or("");
                        text.push_str(&format!("[{name} {}]\n", model::shorten(what, 160)));
                    }
                    _ => {}
                }
            }
        }
        let from_end = args.get("from_end").and_then(Value::as_bool).unwrap_or(false);
        if from_end && text.len() > limit {
            let mut start = text.len() - limit;
            while !text.is_char_boundary(start) {
                start += 1;
            }
            return Ok(format!("… (the start is left out)\n{}", &text[start..]));
        }
        Ok(cut(&text, limit))
    }

    fn save_note(&self, args: &Value) -> Result<String, String> {
        let text = args.get("text").and_then(Value::as_str).unwrap_or("").trim();
        if text.is_empty() {
            return Err("the note is empty".into());
        }
        let text = cut(text, 4000);
        let id = self.library.add_note(None, Some(&normalize(&self.project)), &text, now_ms()).map_err(|e| e.to_string())?;
        Ok(format!("Saved note {id}."))
    }

    fn search_notes(&self, args: &Value) -> Result<String, String> {
        let query = args.get("query").and_then(Value::as_str).unwrap_or("").to_lowercase();
        let words: Vec<&str> = query.split_whitespace().collect();
        let project = normalize(&self.project);
        let notes = self.library.notes((!Self::all_projects(args)).then_some(project.as_str())).map_err(|e| e.to_string())?;
        let found: Vec<String> = notes
            .iter()
            .filter(|note| {
                let lower = note.text.to_lowercase();
                words.iter().all(|word| lower.contains(word))
            })
            .take(if words.is_empty() { 20 } else { 50 })
            .map(|note| format!("- [{}] {} ({})", note.id, note.text, model::format_ms_utc(note.created_ms)))
            .collect();
        if found.is_empty() {
            return Ok("No notes found.".into());
        }
        Ok(found.join("\n"))
    }

    fn delete_note(&self, args: &Value) -> Result<String, String> {
        let id = args.get("id").and_then(Value::as_i64).ok_or("no id")?;
        self.library.delete_note(id).map_err(|e| e.to_string())?;
        Ok(format!("Deleted note {id}."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::{EnvId, HarnessKind, SourceStamp};

    fn summary(id: &str, title: &str, cwd: &str) -> SessionSummary {
        SessionSummary {
            key: SessionKey { env: EnvId::local(), harness: HarnessKind::Claude, id: id.into() },
            title: title.into(),
            first_prompt: Some(format!("{title} please")),
            cwd: Some(cwd.into()),
            git_branch: None,
            model: None,
            created_ms: 1,
            updated_ms: 2,
            parent: None,
            source: SourceStamp::default(),
            turn: None,
        }
    }

    fn server() -> Server {
        let mut library = Library::in_memory().unwrap();
        library
            .store_scan("local", "claude", &[summary("a", "Parser fix", "/w/app"), summary("b", "Parser docs", "/w/other")])
            .unwrap();
        Server::new(library, "/w/app", std::env::temp_dir())
    }

    fn call(server: &Server, name: &str, arguments: Value) -> (String, bool) {
        let line = json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": name, "arguments": arguments}});
        let reply = server.handle(&line.to_string()).unwrap();
        let result = &reply["result"];
        (result["content"][0]["text"].as_str().unwrap().to_owned(), result["isError"].as_bool().unwrap())
    }

    #[test]
    fn the_handshake_lists_tools_and_notifications_get_no_answer() {
        let server = server();
        let init = server.handle(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#).unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert!(server.handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
        let list = server.handle(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).unwrap();
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 5);
        let unknown = server.handle(r#"{"jsonrpc":"2.0","id":3,"method":"nope"}"#).unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
    }

    #[test]
    fn search_stays_in_the_project_unless_asked() {
        let server = server();
        let (text, error) = call(&server, "search_sessions", json!({"query": "parser"}));
        assert!(!error);
        assert!(text.contains("Parser fix") && !text.contains("Parser docs"), "{text}");
        let (text, _) = call(&server, "search_sessions", json!({"query": "parser", "all_projects": true}));
        assert!(text.contains("Parser docs"));
        let (text, error) = call(&server, "read_session", json!({"key": "local|claude|b"}));
        assert!(error && text.contains("another project"));
    }

    #[test]
    fn a_session_of_a_host_that_is_not_set_up_says_so() {
        let mut library = Library::in_memory().unwrap();
        let mut remote = summary("r", "Remote fix", "/w/app");
        remote.key.env = EnvId::ssh("nobody@nowhere");
        library.store_scan(&remote.key.env.0, "claude", &[remote.clone()]).unwrap();
        let server = Server::new(library, "/w/app", std::env::temp_dir());
        let (text, error) = call(&server, "read_session", json!({"key": remote.key.encode()}));
        assert!(error && text.contains("not set up"), "{text}");
    }

    /// A Claude session on a real host is read over SSH: a synthetic session file is put
    /// in a temporary folder there. `BOWYER_SSH_HOST=user@host BOWYER_SSH_KEY=key cargo
    /// test -p mcp -- --ignored remote`.
    #[test]
    #[ignore = "needs an SSH host"]
    fn a_remote_session_is_read_over_ssh() {
        let host = std::env::var("BOWYER_SSH_HOST").expect("BOWYER_SSH_HOST");
        let key = std::env::var("BOWYER_SSH_KEY").ok();
        let ssh = SshEnvironment::new(&host).with_identity(key.clone().map(PathBuf::from));
        let dir = format!("/tmp/bowyer-mcp-{}", std::process::id());
        let path = format!("{dir}/s1.jsonl");
        let lines = [
            json!({"type": "user", "uuid": "u1", "sessionId": "s1", "cwd": "/w/app", "timestamp": "2026-11-21T10:00:00Z",
                   "message": {"role": "user", "content": "Where is the remote parser?"}}),
            json!({"type": "assistant", "uuid": "a1", "parentUuid": "u1", "sessionId": "s1", "timestamp": "2026-11-21T10:00:05Z",
                   "message": {"role": "assistant", "content": [{"type": "text", "text": "It lives in crates/remote."}]}}),
        ]
        .map(|line| line.to_string())
        .join("\n");
        ssh.run_with_input("sh", &["-c", &format!("mkdir -p {dir} && cat > {path}")], None, lines.as_bytes()).unwrap();

        let library = Library::in_memory().unwrap();
        library.add_ssh_environment(&host, &host, key.as_deref()).unwrap();
        let mut library = library;
        let mut remote = summary("s1", "Remote parser", "/w/app");
        remote.key.env = ssh.id();
        remote.source.path = path.clone();
        remote.source.size = lines.len() as u64;
        library.store_scan(&remote.key.env.0, "claude", &[remote.clone()]).unwrap();
        let server = Server::new(library, "/w/app", std::env::temp_dir().join(format!("bowyer-mcp-{}", std::process::id())));
        let (text, error) = call(&server, "read_session", json!({"key": remote.key.encode()}));
        let _ = ssh.run("rm", &["-rf", &dir], None);
        assert!(!error, "{text}");
        assert!(text.contains("User: Where is the remote parser?") && text.contains("Assistant: It lives in crates/remote."), "{text}");
    }

    #[test]
    fn notes_are_kept_per_project_and_the_switch_turns_them_off() {
        let server = server();
        let (saved, _) = call(&server, "save_note", json!({"text": "Release builds use thin LTO"}));
        assert!(saved.starts_with("Saved note"));
        let (found, _) = call(&server, "search_notes", json!({"query": "lto release"}));
        assert!(found.contains("thin LTO"));
        let (none, _) = call(&server, "search_notes", json!({"query": "kotlin"}));
        assert_eq!(none, "No notes found.");
        server.library.set_setting("memory.enabled", "false").unwrap();
        let (off, error) = call(&server, "search_notes", json!({}));
        assert!(error && off.contains("turned off"));
    }

    #[test]
    fn memory_turns_off_for_one_project_and_its_folders() {
        let server = server();
        let (_, error) = call(&server, "search_notes", json!({}));
        assert!(!error);
        set_project_memory(&server.library, &server.project, false).unwrap();
        let (off, error) = call(&server, "search_notes", json!({}));
        assert!(error && off.contains("turned off"));
        assert!(!memory_on(&server.library, &format!("{}/src", server.project)), "a folder inside");
        assert!(memory_on(&server.library, "/somewhere/else"), "other projects keep it");
        set_project_memory(&server.library, &server.project, true).unwrap();
        assert!(memory_on(&server.library, &server.project));
        assert!(off_projects(&server.library).is_empty());
    }

    #[test]
    fn the_server_reads_lines_and_writes_one_answer_each() {
        let server = server();
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n\n";
        let mut output = Vec::new();
        server.serve(&input[..], &mut output).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("\"result\":{}"));
    }

    #[test]
    fn projects_match_by_folder() {
        assert!(in_project(Some("C:\\W\\App\\src"), "c:/w/app") == cfg!(windows));
        assert!(in_project(Some("/w/app/src"), "/w/app"));
        assert!(!in_project(Some("/w/application"), "/w/app"));
        assert!(!in_project(None, "/w/app"));
    }
}
