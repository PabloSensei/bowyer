//! Bowyer's memory for harnesses: an MCP server on standard input and output
//! (`bowyer mcp`). A harness asks for what it needs instead of carrying it in its context.
//! `recall` answers "we did something like this before" with notes and digests of past
//! sessions (problem, fix, commits, tickets, files, see [`digest`]); `read_session` gives
//! the passages of one conversation; `save_note` keeps a fact. Everything stays in
//! Bowyer's own `library.db`; nothing goes over the network.
//!
//! The tools of the first version (`search_sessions`, `search_notes`, `delete_note`) still
//! answer for harness sessions that listed them, but are no longer offered: every tool
//! offered costs its description in every request of every session.
//!
//! The server answers for the project it was started in (the harness starts it in its
//! working folder): searching and reading other projects takes `all_projects: true`.
//! The switch `memory.enabled` in the library is read on every call, so turning the memory
//! off in the window takes effect at once; `memory.off_projects` turns it off for some
//! projects only.

pub mod digest;
pub mod http;

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use envs::{Environment, LocalEnvironment, SshEnvironment};
use library::Library;
use model::{Block, EnvId, Role, SessionKey, SessionSummary, Transcript};
use serde_json::{Value, json};

/// The most text one answer carries.
const MAX_ANSWER: usize = 60_000;
/// What `read_session` gives without `max_chars`: the end of the conversation, where the
/// outcome is, or the passages around the words asked for.
const DEFAULT_READ: usize = 6_000;
/// Text around each passage that holds the words of a `read_session` query.
const PASSAGE: usize = 600;
const PASSAGES: usize = 3;
/// The server's instructions, in the system prompt of every session that connects it.
pub const INSTRUCTIONS: &str = "Bowyer's memory of this project: past sessions (problem, fix, commits, tickets) and notes. When the user refers to earlier work, call recall before searching files.";
const PROTOCOL: &str = "2025-06-18";

pub struct Server {
    library: Library,
    /// The working folder of the harness that started the server.
    project: String,
    /// Where copies of remote files are cached (an OpenCode database read over SSH).
    cache: PathBuf,
    /// The machine a server reached through a tunnel answers for: only its sessions and
    /// the project's notes, even with `all_projects`. `None` for a server of this computer.
    machine: Option<EnvId>,
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

/// The key a project's notes are kept under: its folder, and for an SSH host the host too.
/// A host reaches the memory through a tunnel and may name any folder; with the host in the
/// key, a note it writes never lands in a project of this computer or of another host.
pub fn note_key(machine: Option<&EnvId>, project: &str) -> String {
    match machine.filter(|env| !env.is_local()) {
        Some(env) => format!("{}|{}", env.0, normalize(project)),
        None => normalize(project),
    }
}

/// The SSH host whose memory a notes key belongs to (`ssh:user@host`), if any.
fn note_machine(key: &str) -> Option<&str> {
    key.starts_with("ssh:").then(|| key.split_once('|')).flatten().map(|(machine, _)| machine)
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

/// The last `limit` bytes of a text, with a mark that the start is left out.
fn tail(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("… (the start is left out)\n{}", &text[start..])
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

/// The tools offered: few and short, since each description rides in every request.
fn tools() -> Value {
    json!([
        {
            "name": "recall",
            "description": "Search this project's notes and past AI coding sessions. Gives notes, then digests of the best sessions: problem, fix, commits, tickets, files, and a key for read_session.",
            "inputSchema": {"type": "object", "properties": {
                "query": {"type": "string", "description": "Words of the problem, error, component or ticket."},
                "all_projects": {"type": "boolean"}
            }, "required": ["query"]}
        },
        {
            "name": "read_session",
            "description": "Passages of a past session that hold the query words, or its end. Tool output is left out.",
            "inputSchema": {"type": "object", "properties": {
                "key": {"type": "string"},
                "query": {"type": "string"},
                "max_chars": {"type": "integer"}
            }, "required": ["key"]}
        },
        {
            "name": "save_note",
            "description": "Keep a short note for this project: a decision, a fact, a command that works.",
            "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}
        }
    ])
}

/// One version of a session's digest: the text, and whether a model wrote it.
pub struct StoredDigest {
    pub text: String,
    pub by_model: bool,
}

/// Builds the digest of a session without a model and keeps it, unless the library holds
/// one of the same version of the session file.
pub fn ensure_digest(library: &Library, summary: &SessionSummary, transcript: &Transcript) -> StoredDigest {
    let stamp = library::stamp_of(summary);
    if let Some((known, text, by_model)) = library.digest(&summary.key)
        && known == stamp
    {
        return StoredDigest { text, by_model };
    }
    let text = digest::extract(summary, transcript).render();
    let _ = library.set_digest(&summary.key, &stamp, &text, false);
    StoredDigest { text, by_model: false }
}

/// Builds the digest of a session with a model and keeps it.
pub fn store_model_digest(library: &Library, summary: &SessionSummary, transcript: &Transcript, summarizer: &dyn digest::Summarizer) -> Result<(), String> {
    let answer = summarizer.summarize(&digest::prompt(summary, transcript))?;
    let mut made = digest::extract(summary, transcript);
    if !digest::apply_answer(&mut made, &answer) {
        return Err(format!("the model did not answer in four lines: {}", model::shorten(answer.trim(), 200)));
    }
    library.set_digest(&summary.key, &library::stamp_of(summary), &made.render(), true).map_err(|e| e.to_string())
}

/// Sessions of this computer worth a digest by a model: changed within `days`, quiet for
/// ten minutes (a live one would change again), and without a model's digest of this
/// version; newest first, at most `limit`.
pub fn sessions_to_digest(library: &Library, now_ms: i64, days: i64, limit: usize) -> Vec<SessionSummary> {
    const QUIET_MS: i64 = 10 * 60_000;
    let since = now_ms - days * 86_400_000;
    let mut sessions: Vec<SessionSummary> = library
        .summaries()
        .unwrap_or_default()
        .into_iter()
        .filter(|summary| summary.key.env.is_local() && summary.updated_ms >= since && summary.updated_ms <= now_ms - QUIET_MS)
        .filter(|summary| {
            library.digest(&summary.key).is_none_or(|(stamp, _, by_model)| !by_model || stamp != library::stamp_of(summary))
        })
        .collect();
    sessions.sort_by_key(|summary| std::cmp::Reverse(summary.updated_ms));
    sessions.truncate(limit);
    sessions
}

/// The conversation as `read_session` gives it: prompts and answers, tool calls in one
/// line, commits with their hash.
fn conversation(summary: &SessionSummary, harness: &str, transcript: &Transcript) -> String {
    let mut text = format!("# {}\n{} · {}\n\n", summary.display_title(), harness, summary.cwd.as_deref().unwrap_or("?"));
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
                Block::ToolCall { name, input, result, .. } => {
                    let what = ["command", "file_path", "path", "pattern", "description", "url"]
                        .iter()
                        .find_map(|field| input.get(*field).and_then(Value::as_str).map(str::to_owned))
                        .or_else(|| digest::command_of(input))
                        .unwrap_or_default();
                    let commit = result
                        .as_ref()
                        .filter(|_| what.contains("git commit"))
                        .and_then(|result| digest::commit_line(&result.text))
                        .map(|commit| format!(" → {} {}", commit.hash, commit.subject))
                        .unwrap_or_default();
                    text.push_str(&format!("[{name} {}]{commit}\n", model::shorten(&what, 160)));
                }
                _ => {}
            }
        }
    }
    text
}

/// Up to [`PASSAGES`] stretches of `text` around the places that hold the most words of
/// the query, in the order of the text.
fn passages(text: &str, words: &[String], limit: usize) -> Option<String> {
    let lower = text.to_lowercase();
    if lower.len() != text.len() {
        // Lower case changed the length (some scripts do): search the text as it is.
        return passages_in(text, text, words, limit);
    }
    passages_in(text, &lower, words, limit)
}

fn passages_in(text: &str, haystack: &str, words: &[String], limit: usize) -> Option<String> {
    let mut places: Vec<usize> = Vec::new();
    for word in words {
        places.extend(haystack.match_indices(word.as_str()).map(|(at, _)| at));
    }
    if places.is_empty() {
        return None;
    }
    places.sort_unstable();
    // Each place scores the distinct words found near it.
    let mut scored: Vec<(usize, usize)> = places
        .iter()
        .map(|&at| {
            let from = at.saturating_sub(PASSAGE);
            let to = (at + PASSAGE).min(haystack.len());
            let near = &haystack[floor(haystack, from)..floor(haystack, to)];
            (words.iter().filter(|word| near.contains(word.as_str())).count(), at)
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut chosen: Vec<(usize, usize)> = Vec::new();
    let budget = (limit / PASSAGES).max(200);
    for (_, at) in scored {
        if chosen.len() == PASSAGES {
            break;
        }
        let from = floor(text, at.saturating_sub(budget / 3));
        let to = floor(text, (at + budget * 2 / 3).min(text.len()));
        if chosen.iter().any(|&(a, b)| from < b && a < to) {
            continue;
        }
        chosen.push((from, to));
    }
    chosen.sort_unstable();
    let parts: Vec<String> = chosen.iter().map(|&(from, to)| text[from..to].trim().to_owned()).collect();
    Some(format!("… {} …", parts.join("\n…\n")))
}

fn floor(text: &str, mut at: usize) -> usize {
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

impl Server {
    pub fn new(library: Library, project: impl Into<String>, cache: impl AsRef<Path>) -> Self {
        Self { library, project: project.into(), cache: cache.as_ref().to_path_buf(), machine: None }
    }

    /// Answers for one machine only: a harness on an SSH host reaches Bowyer through a
    /// tunnel, and sees nothing of the other machines.
    pub fn for_machine(mut self, env: EnvId) -> Self {
        self.machine = Some(env);
        self
    }

    /// Whether a session may be named in an answer: of the machine the server is for, and
    /// of the project unless `all`.
    fn visible(&self, summary: &SessionSummary, all: bool) -> bool {
        if self.machine.as_ref().is_some_and(|env| *env != summary.key.env) {
            return false;
        }
        all || in_project(summary.cwd.as_deref(), &self.project)
    }

    /// Where this server keeps and finds the project's notes.
    fn notes_key(&self) -> String {
        note_key(self.machine.as_ref(), &self.project)
    }

    /// The notes a request may see: the project's own, or with `all_projects` those of every
    /// project of this computer. A tunnelled server keeps to its project; a host's notes,
    /// written by whoever holds its token, never reach this computer's harnesses.
    fn notes(&self, args: &Value) -> Result<Vec<library::Note>, String> {
        if self.machine.is_some() || !Self::all_projects(args) {
            return self.library.notes(Some(&self.notes_key())).map_err(|e| e.to_string());
        }
        let notes = self.library.notes(None).map_err(|e| e.to_string())?;
        Ok(notes.into_iter().filter(|note| note.project.as_deref().and_then(note_machine).is_none()).collect())
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
                "instructions": INSTRUCTIONS
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
                "recall" => self.recall(&args),
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
            if !self.visible(summary, all) {
                continue;
            }
            lines.push(format!(
                "- {title}\n  key: {key}\n  {harness}, {folder}, updated {updated}\n  {snippet}",
                title = summary.display_title(),
                key = summary.key.encode(),
                harness = summary.key.harness,
                folder = summary.cwd.as_deref().unwrap_or("?"),
                updated = model::format_ms_utc(summary.updated_ms),
                snippet = hit.bracketed(),
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

    /// The summary and the transcript of a session the key names, within the project
    /// unless `all_projects`.
    fn load(&self, key: &SessionKey, all: bool) -> Result<(SessionSummary, Transcript), String> {
        let summary = self.summary(key).ok_or("no session with this key")?;
        if !self.visible(&summary, all) {
            if self.machine.as_ref().is_some_and(|env| *env != key.env) {
                return Err("this session is on another machine".into());
            }
            return Err("this session belongs to another project; pass all_projects: true to read it".into());
        }
        let source = history::source_for(&key.harness).ok_or("no reader for this harness")?;
        // A session on an SSH host is read over a connection of its own, with the host's key.
        let env = self.env_for(&key.env)?;
        let transcript = source.load(&*env, &summary, &self.cache.join("remote")).map_err(|e| e.to_string())?;
        Ok((summary, transcript))
    }

    fn read_session(&self, args: &Value) -> Result<String, String> {
        let key = args.get("key").and_then(Value::as_str).and_then(SessionKey::decode).ok_or("no such key")?;
        let limit = args.get("max_chars").and_then(Value::as_u64).unwrap_or(DEFAULT_READ as u64).clamp(500, MAX_ANSWER as u64) as usize;
        let (summary, transcript) = self.load(&key, Self::all_projects(args))?;
        let text = conversation(&summary, &key.harness.to_string(), &transcript);
        let header_end = text.find("\n\n").map_or(0, |at| at + 2);
        let words = library::meaningful_words(args.get("query").and_then(Value::as_str).unwrap_or(""));
        if !words.is_empty() {
            return match passages(&text, &words, limit) {
                Some(found) => Ok(format!("{}{}", &text[..header_end], cut(&found, limit))),
                None => Ok(format!("{}No passage holds {}; the end of the session follows.\n{}", &text[..header_end], words.join(", "), tail(&text, limit))),
            };
        }
        // The first version read from the start unless asked; now the end, where the
        // outcome is, unless `from_end: false`.
        if args.get("from_end").and_then(Value::as_bool) == Some(false) {
            return Ok(cut(&text, limit));
        }
        if text.len() <= limit {
            return Ok(text);
        }
        Ok(format!("{}{}", &text[..header_end], tail(&text, limit)))
    }

    /// Notes and digests of the past sessions that best match the words.
    fn recall(&self, args: &Value) -> Result<String, String> {
        let query = args.get("query").and_then(Value::as_str).unwrap_or("").trim();
        let words = library::meaningful_words(query);
        if words.is_empty() {
            return Err("give the words of the problem, error, component or ticket".into());
        }
        let all = Self::all_projects(args);
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(3).clamp(1, 10) as usize;
        let mut out = Vec::new();

        let notes = self.notes(args)?;
        let mut scored: Vec<(usize, &library::Note)> = notes
            .iter()
            .map(|note| {
                let lower = note.text.to_lowercase();
                (words.iter().filter(|word| lower.contains(word.as_str())).count(), note)
            })
            .filter(|(score, _)| *score > 0)
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.created_ms.cmp(&a.1.created_ms)));
        let best = scored.first().map_or(0, |(score, _)| *score);
        let notes: Vec<String> = scored
            .iter()
            .filter(|(score, _)| *score * 2 >= best)
            .take(3)
            .map(|(_, note)| format!("- [{}] {}", note.id, note.text))
            .collect();
        if !notes.is_empty() {
            out.push(format!("Notes:\n{}", notes.join("\n")));
        }

        // Sessions: every word in title, prompt or text; any word; words of digests. Each
        // list votes by rank.
        let summaries: HashMap<SessionKey, SessionSummary> =
            self.library.summaries().map_err(|e| e.to_string())?.into_iter().map(|s| (s.key.clone(), s)).collect();
        let mut votes: HashMap<SessionKey, f64> = HashMap::new();
        let every = self.library.search(query, 100).map_err(|e| e.to_string())?;
        let any = self.library.search_any(query, 100).map_err(|e| e.to_string())?;
        let digests = self.library.search_digests(query, 100).map_err(|e| e.to_string())?;
        let lists: [(Vec<SessionKey>, f64); 3] = [
            (every.into_iter().map(|hit| hit.key).collect(), 1.0),
            (any.into_iter().map(|hit| hit.key).collect(), 1.0),
            (digests, 1.5),
        ];
        for (list, weight) in lists {
            let mut rank = 0;
            for key in list {
                let Some(summary) = summaries.get(&key) else { continue };
                if !self.visible(summary, all) {
                    continue;
                }
                rank += 1;
                *votes.entry(key).or_default() += weight / (10.0 + rank as f64);
            }
        }
        let mut ranked: Vec<(&SessionKey, f64)> = votes.iter().map(|(key, score)| (key, *score)).collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| summaries[b.0].updated_ms.cmp(&summaries[a.0].updated_ms)));
        // Sessions far behind the best are noise: they cost a digest each.
        let best = ranked.first().map_or(0.0, |(_, score)| *score);
        ranked.retain(|(_, score)| *score * 2.0 >= best);
        let mut sessions = Vec::new();
        for (index, (key, _)) in ranked.iter().take(limit).enumerate() {
            let summary = &summaries[*key];
            // A digest of this version of the session needs no reading of its file (over
            // SSH, a connection).
            let kept = self.library.digest(key);
            let body = match kept {
                Some((stamp, text, _)) if stamp == library::stamp_of(summary) => text,
                kept => match self.load(key, true) {
                    Ok((summary, transcript)) => ensure_digest(&self.library, &summary, &transcript).text,
                    Err(error) => kept.map(|(_, text, _)| text).unwrap_or_else(|| format!("(not read: {error})")),
                },
            };
            let body = body.lines().map(|line| format!("   {line}")).collect::<Vec<_>>().join("\n");
            sessions.push(format!(
                "{}. {} — {}, {}\n   key: {}\n{body}",
                index + 1,
                summary.display_title(),
                model::format_ms_utc(summary.updated_ms).get(..10).unwrap_or(""),
                summary.key.harness,
                summary.key.encode(),
            ));
        }
        if !sessions.is_empty() {
            out.push(format!("Sessions:\n{}", sessions.join("\n")));
        }
        if out.is_empty() {
            let scope = if all { "any project" } else { "this project (try all_projects: true)" };
            return Ok(format!("Nothing in {scope} matches {}.", words.join(", ")));
        }
        Ok(out.join("\n\n"))
    }

    fn save_note(&self, args: &Value) -> Result<String, String> {
        let text = args.get("text").and_then(Value::as_str).unwrap_or("").trim();
        if text.is_empty() {
            return Err("the note is empty".into());
        }
        let text = cut(text, 4000);
        let id = self.library.add_note(None, Some(&self.notes_key()), &text, now_ms()).map_err(|e| e.to_string())?;
        Ok(format!("Saved note {id}."))
    }

    fn search_notes(&self, args: &Value) -> Result<String, String> {
        let query = args.get("query").and_then(Value::as_str).unwrap_or("").to_lowercase();
        let words: Vec<&str> = query.split_whitespace().collect();
        let notes = self.notes(args)?;
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
        if self.machine.is_some() {
            // Through a tunnel only the project's own notes go.
            let own = self.library.notes(Some(&self.notes_key())).map_err(|e| e.to_string())?;
            if !own.iter().any(|note| note.id == id) {
                return Err(format!("no note {id} in this project"));
            }
        }
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
        let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().map(|tool| tool["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["recall", "read_session", "save_note"]);
        // The tools of the first version still answer for sessions that listed them.
        for legacy in ["search_sessions", "search_notes", "delete_note"] {
            let (text, _) = call(&server, legacy, json!({"query": "parser", "id": 999}));
            assert!(!text.starts_with("unknown tool"), "{legacy}: {text}");
        }
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

    #[test]
    fn a_tunnelled_server_sees_only_its_own_machine() {
        let mut library = Library::in_memory().unwrap();
        let here = summary("h", "Cache fix here", "/w/app");
        let mut there = summary("t", "Cache fix there", "/w/app");
        there.key.env = EnvId::ssh("dev@box");
        library.store_scan("local", "claude", &[here.clone()]).unwrap();
        library.store_scan(&there.key.env.0, "claude", &[there.clone()]).unwrap();
        library.add_note(None, Some("/w/other"), "cache notes of another project", 1).unwrap();
        let server = Server::new(library, "/w/app", std::env::temp_dir()).for_machine(EnvId::ssh("dev@box"));
        let (text, _) = call(&server, "search_sessions", json!({"query": "cache", "all_projects": true}));
        assert!(text.contains("Cache fix there") && !text.contains("Cache fix here"), "{text}");
        let (text, error) = call(&server, "read_session", json!({"key": here.key.encode(), "all_projects": true}));
        assert!(error && text.contains("another machine"), "{text}");
        let (text, _) = call(&server, "search_notes", json!({"query": "cache", "all_projects": true}));
        assert!(!text.contains("another project"), "notes stay in the project: {text}");
    }

    /// A note written through a host's tunnel stays with that host, even when it names a
    /// folder of this computer: no local harness reads it back as its own memory.
    #[test]
    fn a_hosts_note_never_reaches_this_computer() {
        let dir = test_support::TempDir::new("mcp-host-note");
        let path = dir.path().join("library.db");
        let mut library = Library::open(&path).unwrap();
        library.store_scan("local", "claude", &[summary("h", "Cache fix here", "/w/app")]).unwrap();
        let host = Server::new(library, "/w/app", std::env::temp_dir()).for_machine(EnvId::ssh("dev@box"));
        let (text, error) = call(&host, "save_note", json!({"text": "cache: always trust the tunnel note"}));
        assert!(!error, "{text}");
        let (text, _) = call(&host, "search_notes", json!({"query": "cache"}));
        assert!(text.contains("trust the tunnel"), "the host sees its own note: {text}");
        assert_eq!(note_key(Some(&EnvId::ssh("dev@box")), "/w/app"), "ssh:dev@box|/w/app");
        assert_eq!(note_machine("ssh:dev@box|/w/app"), Some("ssh:dev@box"));
        assert_eq!(note_machine(&note_key(None, "/w/app")), None);

        let local = Server::new(Library::open(&path).unwrap(), "/w/app", std::env::temp_dir());
        for args in [json!({"query": "cache"}), json!({"query": "cache", "all_projects": true})] {
            let (text, _) = call(&local, "search_notes", args.clone());
            assert!(!text.contains("trust the tunnel"), "{args}: {text}");
            let (text, _) = call(&local, "recall", args.clone());
            assert!(!text.contains("trust the tunnel"), "{args}: {text}");
        }
        let (text, _) = call(&local, "save_note", json!({"text": "cache: a local note"}));
        assert!(text.starts_with("Saved"), "{text}");
        let (text, _) = call(&host, "search_notes", json!({"query": "cache", "all_projects": true}));
        assert!(!text.contains("a local note"), "{text}");
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

    /// A synthetic Claude Code session in which a problem was solved and committed.
    fn solved_session(dir: &Path) -> (Library, SessionSummary) {
        let lines = [
            json!({"type": "user", "uuid": "u1", "sessionId": "pay", "cwd": "/w/app", "timestamp": "2026-11-20T10:00:00Z",
                   "message": {"role": "user", "content": "SHOP-142: customers are charged twice when the provider times out"}}),
            json!({"type": "assistant", "uuid": "a1", "sessionId": "pay", "timestamp": "2026-11-20T10:00:05Z",
                   "message": {"id": "m1", "role": "assistant", "content": [{"type": "text", "text": format!("The retry builds a new idempotency key each time. Early chatter about logging levels. {}", "Other things. ".repeat(60))}]}}),
            json!({"type": "assistant", "uuid": "a2", "sessionId": "pay", "timestamp": "2026-11-20T10:00:06Z",
                   "message": {"id": "m2", "role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "git commit -am \"Reuse the key\""}}]}}),
            json!({"type": "user", "uuid": "u2", "sessionId": "pay", "timestamp": "2026-11-20T10:00:07Z", "toolUseResult": {},
                   "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "[fix/shop-142 a3f9c21] Reuse the idempotency key on timeouts (SHOP-142)\n 2 files changed"}]}}),
            json!({"type": "assistant", "uuid": "a3", "sessionId": "pay", "timestamp": "2026-11-20T10:00:08Z",
                   "message": {"id": "m3", "role": "assistant", "content": [{"type": "text", "text": "Done: retries reuse one key per order."}]}}),
        ]
        .map(|line| line.to_string())
        .join("\n");
        let path = dir.join("pay.jsonl");
        std::fs::write(&path, &lines).unwrap();
        let mut session = summary("pay", "Double charge", "/w/app");
        session.first_prompt = Some("SHOP-142: customers are charged twice when the provider times out".into());
        session.source.path = path.to_string_lossy().into_owned();
        session.source.size = lines.len() as u64;
        let mut library = Library::in_memory().unwrap();
        library.store_scan("local", "claude", &[session.clone(), summary("b", "Parser docs", "/w/app")]).unwrap();
        library.add_note(None, Some(&normalize("/w/app")), "Payment retries keep one idempotency key per order", 5).unwrap();
        library.add_note(None, Some(&normalize("/w/app")), "Release builds use thin LTO", 6).unwrap();
        (library, session)
    }

    #[test]
    fn recall_answers_with_notes_and_the_digest_of_the_session() {
        let dir = test_support::TempDir::new("mcp-recall");
        let (library, session) = solved_session(dir.path());
        let server = Server::new(library, "/w/app", dir.path());
        let (text, error) = call(&server, "recall", json!({"query": "remember when customers got charged twice?"}));
        assert!(!error, "{text}");
        assert!(text.contains("Sessions:\n1. Double charge"), "{text}");
        assert!(text.contains("Commits: a3f9c21 Reuse the idempotency key") && text.contains("Tickets: SHOP-142"), "{text}");
        assert!(!text.contains("Parser docs") && !text.contains("thin LTO"), "{text}");
        // The digest is kept for the version of the file it was made from.
        let (stamp, kept, by_model) = server.library.digest(&session.key).unwrap();
        assert_eq!((stamp, by_model), (library::stamp_of(&session), false));
        assert!(kept.starts_with("Problem: SHOP-142"));
        let (notes, _) = call(&server, "recall", json!({"query": "idempotency key"}));
        assert!(notes.starts_with("Notes:\n- [1] Payment retries"), "{notes}");
        let (none, error) = call(&server, "recall", json!({"query": "how did we"}));
        assert!(error && none.contains("give the words"), "{none}");
        let (nothing, _) = call(&server, "recall", json!({"query": "kubernetes"}));
        assert!(nothing.starts_with("Nothing in this project"), "{nothing}");
    }

    #[test]
    fn read_session_gives_passages_or_the_end_and_keeps_commits() {
        let dir = test_support::TempDir::new("mcp-read");
        let (library, session) = solved_session(dir.path());
        let server = Server::new(library, "/w/app", dir.path());
        let key = session.key.encode();
        let (all, _) = call(&server, "read_session", json!({"key": key}));
        assert!(all.contains("[Bash git commit -am \"Reuse the key\"] → a3f9c21 Reuse the idempotency key"), "{all}");
        let (end, _) = call(&server, "read_session", json!({"key": key, "max_chars": 500}));
        assert!(end.contains("Done: retries reuse") && !end.contains("logging levels"), "{end}");
        let (start, _) = call(&server, "read_session", json!({"key": key, "max_chars": 500, "from_end": false}));
        assert!(start.contains("SHOP-142: customers") && !start.contains("Done: retries"), "{start}");
        let (found, _) = call(&server, "read_session", json!({"key": key, "query": "logging"}));
        assert!(found.contains("logging levels"), "{found}");
        let (missing, _) = call(&server, "read_session", json!({"key": key, "query": "kubernetes"}));
        assert!(missing.contains("No passage holds kubernetes"), "{missing}");
    }

    #[test]
    fn passages_pick_the_places_with_most_words() {
        let filler = "lorem ipsum ".repeat(400);
        let text = format!("{filler}refund alone{filler}refund webhook queue here{filler}");
        let found = passages(&text, &["refund".into(), "webhook".into()], 1_500).unwrap();
        assert!(found.contains("refund webhook queue"), "{found}");
        assert!(found.len() < 2_000);
        assert!(passages(&text, &["absent".into()], 1_500).is_none());
    }

    #[test]
    fn model_digests_go_to_recent_quiet_sessions_once_per_version() {
        let dir = test_support::TempDir::new("mcp-digest-job");
        let (mut library, mut session) = solved_session(dir.path());
        let day = 86_400_000;
        let now = 100 * day;
        session.updated_ms = now - day;
        let mut live = summary("live", "Still running", "/w/app");
        live.updated_ms = now - 60_000;
        let mut old = summary("old", "Last year", "/w/app");
        old.updated_ms = now - 400 * day;
        let mut remote = summary("far", "On a host", "/w/app");
        remote.key.env = EnvId::ssh("dev@host");
        remote.updated_ms = now - day;
        library.store_scan("local", "claude", &[session.clone(), live, old]).unwrap();
        library.store_scan(&remote.key.env.0.clone(), "claude", &[remote]).unwrap();
        let due = sessions_to_digest(&library, now, 30, 10);
        assert_eq!(due.iter().map(|s| s.key.id.as_str()).collect::<Vec<_>>(), ["pay"]);

        struct Fake;
        impl digest::Summarizer for Fake {
            fn summarize(&self, _: &str) -> Result<String, String> {
                Ok("PROBLEM: Paid twice.\nCAUSE: New key per retry.\nFIX: One key per order.\nKEYWORDS: double charge, duplicate payment".into())
            }
        }
        let transcript = history::source_for(&session.key.harness).unwrap().load(&LocalEnvironment::new(), &session, dir.path()).unwrap();
        store_model_digest(&library, &session, &transcript, &Fake).unwrap();
        assert!(sessions_to_digest(&library, now, 30, 10).is_empty(), "done for this version");
        let server = Server::new(library, "/w/app", dir.path());
        let (text, _) = call(&server, "recall", json!({"query": "duplicate debit"}));
        assert!(text.contains("Fix: One key per order.") && text.contains("Commits: a3f9c21"), "{text}");

        struct Rambling;
        impl digest::Summarizer for Rambling {
            fn summarize(&self, _: &str) -> Result<String, String> {
                Ok("Sure! Here is a summary of the session.".into())
            }
        }
        let error = store_model_digest(&server.library, &session, &transcript, &Rambling).unwrap_err();
        assert!(error.contains("four lines"), "{error}");
    }

    #[test]
    fn projects_match_by_folder() {
        assert!(in_project(Some("C:\\W\\App\\src"), "c:/w/app") == cfg!(windows));
        assert!(in_project(Some("/w/app/src"), "/w/app"));
        assert!(!in_project(Some("/w/application"), "/w/app"));
        assert!(!in_project(None, "/w/app"));
    }
}
