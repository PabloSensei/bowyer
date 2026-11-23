//! Live turns. A harness runs headless for one prompt and its structured output becomes the
//! same `Transcript` the history readers make, so the window draws a running turn with the
//! code that draws history:
//!
//! - Claude Code: `claude -p --input-format stream-json --output-format stream-json`; its
//!   lines have the shape of its history files and go through [`history::ClaudeParser`], and
//!   its questions before an action go to the window ([`claude_ask`]).
//! - OpenCode: `opencode acp`, the Agent Client Protocol ([`acp`]): its updates become
//!   blocks, and its questions before a command or an edit go to the window.
//!
//! The prompt goes in on standard input, never on the command line, so no shell or `cmd.exe`
//! quoting touches it. A turn ends when the process exits; [`Turn::stop`] ends it early,
//! with every process it started.

use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;

use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use history::ClaudeParser;
use model::{Block, Entry, HarnessKind, Role, ToolKind, ToolResult, Transcript, Usage};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};

mod acp;
mod claude_ask;
mod ide;
mod kill;
mod resolve;
pub mod usage;

pub use acp::{Answer, AskQuestion, PermissionAsk};

pub use ide::{IdeLink, claude_config_home, find_ide, ide_for};
pub use resolve::resolve_program;

/// How a headless turn treats actions that would ask for permission.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Permission {
    /// What the user's own harness settings say.
    #[default]
    Settings,
    /// Edits go through, commands still follow the settings.
    AcceptEdits,
    /// Everything goes through.
    Bypass,
}

impl Permission {
    pub const ALL: [Permission; 3] = [Self::Settings, Self::AcceptEdits, Self::Bypass];

    pub fn id(self) -> &'static str {
        match self {
            Self::Settings => "settings",
            Self::AcceptEdits => "accept_edits",
            Self::Bypass => "bypass",
        }
    }
}

#[derive(Clone, Debug)]
pub struct TurnSpec {
    pub harness: HarnessKind,
    /// The session to continue, or `None` for a new one.
    pub session: Option<String>,
    pub cwd: String,
    pub prompt: String,
    /// `haiku`, `opencode-go/deepseek-v4.1-flash`; `None` keeps the harness default.
    pub model: Option<String>,
    /// How hard the model thinks: `low` … `max` for Claude Code, `minimal` … `high` for
    /// Codex; `None` keeps the harness default. Other harnesses take none.
    pub effort: Option<String>,
    pub permission: Permission,
    /// The host the turn runs on; `None` is this computer.
    pub ssh: Option<envs::SshEnvironment>,
    /// Pictures and files sent with the prompt.
    pub attachments: Vec<Attachment>,
    /// The IDE that has the folder open, for Claude Code on this computer: see [`ide`].
    pub ide: Option<IdeLink>,
    /// A failed turn of the Claude Code session to leave out: the line the session goes on
    /// from and the prompt dropped (`--resume-session-at`, `--resume-drops-turn`).
    pub drop_turn: Option<(String, String)>,
}

/// A file sent with a prompt.
#[derive(Clone, Debug, Default)]
pub struct Attachment {
    /// Where the harness reads it: a path on the machine the turn runs on.
    pub path: String,
    /// `image/png` and the like for a picture, with its bytes: Claude takes pictures inline.
    pub image: Option<(String, std::sync::Arc<Vec<u8>>)>,
}

/// The media type of a picture by its file name, for the formats the models read.
pub fn image_type(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    let ext = lower.rsplit('.').next()?;
    Some(match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    })
}

#[derive(Clone, Debug)]
pub enum LiveEvent {
    /// The harness named the session, at once for OpenCode and on its first line for Claude.
    Session(String),
    /// The turn so far: the prompt and everything the agent did since.
    Update(Transcript),
    /// The last lines the harness wrote to its error output.
    Stderr(String),
    /// The harness waits for the user to allow or refuse an action.
    Permission(PermissionAsk),
    Finished { ok: bool, error: Option<String> },
}

pub struct Turn {
    stop: StopHandle,
    pub events: UnboundedReceiver<LiveEvent>,
}

impl Turn {
    /// Ends the turn and every process the harness started.
    pub fn stop(&self) {
        self.stop.stop();
    }

    pub fn pid(&self) -> u32 {
        self.stop.pid
    }

    /// The stop handle and the events apart, for a reader that runs elsewhere.
    pub fn into_parts(self) -> (StopHandle, UnboundedReceiver<LiveEvent>) {
        (self.stop, self.events)
    }
}

/// Stops a turn whose events are read somewhere else.
#[derive(Clone, Debug)]
pub struct StopHandle {
    /// The local process: the harness, or the SSH connection that follows it.
    pid: u32,
    /// A turn on an SSH host runs detached from the connection: its host and run.
    remote: Option<(envs::SshEnvironment, String)>,
    /// Claude's input and tasks, for `stop_task`.
    claude: Option<(claude_ask::Input, claude_ask::Tasks)>,
    /// Whether the processes of the harness can be found.
    processes: bool,
}

impl StopHandle {
    pub fn stop(&self) {
        if let Some((ssh, run)) = &self.remote {
            let _ = ssh.stop_detached(run);
        }
        stop_tree(self.pid);
    }

    /// Whether single commands of this turn can be stopped.
    pub fn stops_commands(&self) -> bool {
        self.claude.is_some() || self.processes
    }

    /// Stops one command or agent of the turn, the tool call `call` that runs `command`; the
    /// turn goes on. Claude stops its own task; for other harnesses the processes that run
    /// the command end. Blocks for a listing of processes or a round trip to the host.
    pub fn stop_command(&self, call: &str, command: Option<&str>) -> io::Result<()> {
        if let Some((input, tasks)) = &self.claude
            && claude_ask::stop_task(input, tasks, call)
        {
            return Ok(());
        }
        let not_found = || io::Error::new(io::ErrorKind::NotFound, "no process of this command was found");
        let command = command.filter(|_| self.processes).ok_or_else(not_found)?;
        let found = match &self.remote {
            Some((ssh, run)) => kill::stop_remote(ssh, run, command)?,
            None => kill::stop_local(self.pid, command)?,
        };
        if found == 0 { Err(not_found()) } else { Ok(()) }
    }
}

#[cfg(windows)]
fn stop_tree(pid: u32) {
    use std::os::windows::process::CommandExt;
    let _ = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .creation_flags(0x0800_0000)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(windows))]
fn stop_tree(pid: u32) {
    // The harness leads its own process group (see `spawn`), so the group goes with it.
    let _ = Command::new("kill").args(["-TERM", &format!("-{pid}")]).status();
    let _ = Command::new("kill").args(["-TERM", &pid.to_string()]).status();
}

/// The command line of a turn, without the prompt.
pub fn command_line(spec: &TurnSpec) -> io::Result<(String, Vec<String>)> {
    let mut args: Vec<String> = Vec::new();
    let program = match spec.harness {
        HarnessKind::Claude => {
            args.extend(
                ["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--include-partial-messages"]
                    .map(String::from),
            );
            if let Some(session) = &spec.session {
                args.extend(["--resume".into(), session.clone()]);
                // Claude Code checks that only the dropped turn goes, and refuses otherwise.
                if let Some((kept, prompt)) = &spec.drop_turn {
                    args.extend(["--resume-session-at".into(), kept.clone(), "--resume-drops-turn".into(), prompt.clone()]);
                }
            }
            if let Some(model) = &spec.model {
                args.extend(["--model".into(), model.clone()]);
            }
            if let Some(effort) = &spec.effort {
                args.extend(["--effort".into(), effort.clone()]);
            }
            match spec.permission {
                Permission::Settings => {}
                Permission::AcceptEdits => args.extend(["--permission-mode".into(), "acceptEdits".into()]),
                Permission::Bypass => args.extend(["--permission-mode".into(), "bypassPermissions".into()]),
            }
            if claude_answered(spec) {
                // Questions come as `control_request` lines, answers go on standard input.
                args.extend(["--permission-prompt-tool".into(), "stdio".into()]);
            }
            "claude"
        }
        HarnessKind::OpenCode => {
            // Session, model, prompt and files go over the protocol.
            args.push("acp".into());
            "opencode"
        }
        HarnessKind::Codex => {
            // Options go before `resume`; `-` takes the prompt from standard input. Pictures of
            // a new session go first: there `--image` takes every value up to the next flag.
            args.push("exec".into());
            let images: Vec<String> = spec.attachments.iter().filter(|a| a.image.is_some()).map(|a| a.path.clone()).collect();
            if spec.session.is_none() {
                for image in &images {
                    args.extend(["--image".into(), image.clone()]);
                }
            }
            args.extend(["--json", "--skip-git-repo-check"].map(String::from));
            if let Some(model) = &spec.model {
                args.extend(["--model".into(), model.clone()]);
            }
            if let Some(effort) = &spec.effort {
                args.extend(["-c".into(), format!("model_reasoning_effort={effort}")]);
            }
            match spec.permission {
                Permission::Settings => {}
                // `codex exec` 0.167 has no `--full-auto`: edits in the folder need its sandbox.
                Permission::AcceptEdits => args.extend(["--sandbox".into(), "workspace-write".into()]),
                Permission::Bypass => args.push("--dangerously-bypass-approvals-and-sandbox".into()),
            }
            if let Some(session) = &spec.session {
                args.extend(["resume".into(), session.clone()]);
                // `resume --image` takes one value each.
                for image in &images {
                    args.extend(["--image".into(), image.clone()]);
                }
            }
            args.push("-".into());
            "codex"
        }
        HarnessKind::DeepSeek => {
            // `-` takes the task from standard input.
            args.extend(["headless", "--json"].map(String::from));
            if let Some(session) = &spec.session {
                args.extend(["--session-id".into(), session.clone()]);
            }
            if spec.permission == Permission::Bypass {
                args.extend(["--approval-policy".into(), "never".into()]);
            }
            args.push("-".into());
            "dsh"
        }
        ref other => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("{other} has no headless mode in this version"),
            ));
        }
    };
    Ok((program.to_owned(), args))
}

/// Whether a Claude turn asks the window before an action. Everywhere but a turn on an SSH
/// host that allows everything: that one keeps the plain detached run, nothing to answer.
fn claude_answered(spec: &TurnSpec) -> bool {
    spec.harness == HarnessKind::Claude && !(spec.ssh.is_some() && spec.permission == Permission::Bypass)
}

/// The prompt with the paths of the files the harness reads from disk: every attachment
/// but the pictures Claude takes inline, and nothing for OpenCode, which gets `--file`.
fn prompt_text(spec: &TurnSpec) -> String {
    let listed: Vec<&str> = spec
        .attachments
        .iter()
        .filter(|a| match spec.harness {
            HarnessKind::Claude | HarnessKind::Codex => a.image.is_none(),
            HarnessKind::OpenCode => false,
            _ => true,
        })
        .map(|a| a.path.as_str())
        .collect();
    if listed.is_empty() {
        return spec.prompt.clone();
    }
    let mut text = spec.prompt.clone();
    text.push_str("\n\nAttached files:");
    for path in listed {
        text.push_str("\n- ");
        text.push_str(path);
    }
    text
}

/// The prompt as the harness reads it from standard input.
fn stdin_payload(spec: &TurnSpec) -> String {
    let text = prompt_text(spec);
    match spec.harness {
        HarnessKind::Claude => {
            if !spec.attachments.iter().any(|a| a.image.is_some()) {
                let line = json!({"type": "user", "message": {"role": "user", "content": text}});
                return format!("{line}\n");
            }
            // An empty text block is an error to the API; a picture alone is fine.
            let mut content: Vec<Value> = Vec::new();
            if !text.is_empty() {
                content.push(json!({"type": "text", "text": text}));
            }
            for (media_type, bytes) in spec.attachments.iter().filter_map(|a| a.image.as_ref()) {
                content.push(json!({
                    "type": "image",
                    "source": {"type": "base64", "media_type": media_type, "data": BASE64.encode(bytes.as_slice())},
                }));
            }
            let line = json!({"type": "user", "message": {"role": "user", "content": content}});
            format!("{line}\n")
        }
        _ => text,
    }
}

/// The prompt as the window shows it while the turn runs: the text and the pictures.
fn prompt_blocks(spec: &TurnSpec) -> Vec<Block> {
    let mut blocks = vec![Block::Text { text: prompt_text(spec) }];
    for (media_type, bytes) in spec.attachments.iter().filter_map(|a| a.image.as_ref()) {
        blocks.push(Block::Image { media_type: media_type.clone(), data: Some(BASE64.encode(bytes.as_slice())) });
    }
    blocks
}

fn prompt_entry(prompt: &[Block], now_ms: i64) -> Entry {
    Entry {
        id: "live-prompt".into(),
        role: Role::User,
        timestamp_ms: now_ms,
        blocks: prompt.to_vec(),
        model: None,
        usage: None,
        compact_summary: false,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Starts a turn on this computer.
pub fn start(spec: TurnSpec) -> io::Result<Turn> {
    let (program, args) = command_line(&spec)?;
    // A turn on an SSH host runs detached from the connection, so a dropped link does not
    // end it; the remote login shell finds the harness, the prompt still goes on stdin.
    // An ACP agent talks both ways for the whole turn, so it cannot run detached.
    let acp = spec.harness == HarnessKind::OpenCode;
    // Either way the run has a name on the host, under which its process id is kept.
    let run = spec.ssh.as_ref().map(|_| format!("{}-{}", now_ms(), std::process::id()));
    let (mut command, resolved) = match &spec.ssh {
        Some(ssh) if acp => {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let run = run.as_deref().unwrap_or("run");
            (ssh.headless_run(&program, &args, Some(&spec.cwd), run), PathBuf::from(format!("{}:{program}", ssh.host)))
        }
        Some(ssh) => {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let run = run.as_deref().unwrap_or("run");
            let command = if claude_answered(&spec) {
                // Answers go into the run's input file over connections of their own.
                ssh.detached_answered(&program, &args, Some(&spec.cwd), run)
            } else {
                ssh.detached(&program, &args, Some(&spec.cwd), run)
            };
            (command, PathBuf::from(format!("{}:{program}", ssh.host)))
        }
        None => {
            let (resolved, prefix) = resolve_program(&program).unwrap_or_else(|| (PathBuf::from(&program), Vec::new()));
            let mut command = Command::new(&resolved);
            command.args(&prefix).args(&args).current_dir(&spec.cwd);
            if spec.harness == HarnessKind::Claude
                && let Some(ide) = &spec.ide
            {
                command.envs(ide.environment());
            }
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x0800_0000); // No console window.
            }
            (command, resolved)
        }
    };
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", resolved.display())))?;
    let pid = child.id();
    let (sender, events) = unbounded();

    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut acp_stdin = None;
    // Where Claude's answers go: its standard input, kept open until the turn's result, or
    // the input file of its detached run on an SSH host.
    let mut answers = claude_ask::Input::closed();
    let tasks = claude_ask::Tasks::default();
    if acp {
        acp_stdin = Some(stdin);
    } else if claude_answered(&spec) && spec.ssh.is_none() {
        answers = claude_ask::Input::new(claude_ask::Sink::Pipe(Box::new(stdin)));
        let (input, payload) = (answers.clone(), stdin_payload(&spec));
        thread::spawn(move || input.send(&payload));
    } else {
        if claude_answered(&spec)
            && let (Some(ssh), Some(run)) = (&spec.ssh, &run)
        {
            answers = claude_ask::Input::new(claude_ask::Sink::Remote { ssh: ssh.clone(), run: run.clone() });
        }
        let payload = stdin_payload(&spec);
        thread::spawn(move || {
            let _ = stdin.write_all(payload.as_bytes());
            // Closing standard input tells the harness the prompt is complete.
        });
    }

    let stdout = child.stdout.take().expect("piped stdout");
    let reader_sender = sender.clone();
    let harness = spec.harness.clone();
    let prompt = prompt_blocks(&spec);
    let reader_spec = spec.clone();
    let (reader_answers, reader_tasks) = (answers.clone(), tasks.clone());
    let reader = thread::spawn(move || {
        let lines = BufReader::new(stdout).lines().map_while(Result::ok);
        match (harness, acp_stdin) {
            (_, Some(stdin)) => {
                let error = acp::run_turn(stdin, lines, &reader_spec, &prompt, &reader_sender);
                // The agent would wait for the next prompt: its input is closed now; give it a
                // moment to finish its writes (the title, the last parts), then end it.
                thread::sleep(std::time::Duration::from_millis(700));
                stop_tree(pid);
                error
            }
            (HarnessKind::Claude, _) => read_claude(lines, &prompt, &reader_sender, &reader_answers, &reader_tasks),
            (HarnessKind::DeepSeek, _) => read_dsh(lines, &prompt, &reader_sender),
            (HarnessKind::Codex, _) => read_codex(lines, &prompt, &reader_sender),
            (other, _) => Some(format!("{other} has no live turns")),
        }
    });

    let stderr = child.stderr.take().expect("piped stderr");
    let err_sender = sender.clone();
    let err_reader = thread::spawn(move || {
        let mut tail: Vec<String> = Vec::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            tail.push(line);
            if tail.len() > 20 {
                tail.remove(0);
            }
            let _ = err_sender.unbounded_send(LiveEvent::Stderr(tail.join("\n")));
        }
        tail.join("\n")
    });

    thread::spawn(move || {
        let status = child.wait();
        let error_from_output = reader.join().ok().flatten();
        let stderr_tail = err_reader.join().unwrap_or_default();
        // An ACP agent is stopped once its turn ends: its exit code says nothing.
        let ok = (acp || status.as_ref().is_ok_and(|s| s.success())) && error_from_output.is_none();
        let error = if ok {
            None
        } else {
            error_from_output.or_else(|| {
                let tail = stderr_tail.trim();
                Some(if tail.is_empty() {
                    match status {
                        Ok(status) => format!("the harness exited with {status}"),
                        Err(error) => error.to_string(),
                    }
                } else {
                    tail.to_owned()
                })
            })
        };
        let _ = sender.unbounded_send(LiveEvent::Finished { ok, error });
    });
    let remote = spec.ssh.clone().zip(run);
    let claude = (spec.harness == HarnessKind::Claude).then_some((answers, tasks));
    // The processes of every harness can be found: here under the harness, on a host under
    // the process id its run left there (an ACP agent runs attached, but leaves it too).
    let processes = true;
    Ok(Turn { stop: StopHandle { pid, remote, claude, processes }, events })
}

/// Feeds `stream-json` lines to the history reader. Returns the error a `result` line
/// reported, if any.
/// The answer being written: the text of the open block, token by token, until the whole
/// block arrives as an `assistant` line.
#[derive(Default)]
struct Partial {
    message: String,
    text: String,
    sent: Option<std::time::Instant>,
}

/// How often a growing answer is redrawn.
const PARTIAL_EVERY: std::time::Duration = std::time::Duration::from_millis(60);

/// Takes a `stream_event` of `--include-partial-messages`; true when the answer grew
/// enough to be shown again.
fn take_partial(partial: &mut Partial, value: &Value) -> bool {
    let Some(event) = value.get("event") else { return false };
    match event.get("type").and_then(Value::as_str) {
        Some("message_start") => {
            partial.message = event.pointer("/message/id").and_then(Value::as_str).unwrap_or("").to_owned();
            partial.text.clear();
            false
        }
        Some("content_block_start") => {
            partial.text.clear();
            false
        }
        Some("content_block_delta") => {
            let Some(text) = event.pointer("/delta/text").and_then(Value::as_str) else { return false };
            partial.text.push_str(text);
            let due = partial.sent.is_none_or(|at| at.elapsed() >= PARTIAL_EVERY);
            if due {
                partial.sent = Some(std::time::Instant::now());
            }
            due
        }
        _ => false,
    }
}

/// The turn so far with the text being written at its end, in the message it belongs to.
fn with_partial(mut turn: Transcript, partial: &Partial, now: i64) -> Transcript {
    if partial.text.is_empty() {
        return turn;
    }
    let block = Block::Text { text: partial.text.clone() };
    match turn.entries.last_mut() {
        Some(last) if last.role == Role::Assistant && !partial.message.is_empty() && last.id == partial.message => last.blocks.push(block),
        _ => turn.entries.push(Entry {
            id: partial.message.clone(),
            role: Role::Assistant,
            timestamp_ms: now,
            blocks: vec![block],
            model: None,
            usage: None,
            compact_summary: false,
        }),
    }
    turn
}

fn read_claude(
    lines: impl Iterator<Item = String>,
    prompt: &[Block],
    sender: &UnboundedSender<LiveEvent>,
    answers: &claude_ask::Input,
    tasks: &claude_ask::Tasks,
) -> Option<String> {
    let mut parser = ClaudeParser::new();
    let started = now_ms();
    let mut named = false;
    let mut error = None;
    let mut partial = Partial::default();
    for line in lines {
        let Ok(value) = serde_json::from_str::<Value>(&line) else { continue };
        if !named && let Some(id) = value.get("session_id").and_then(Value::as_str) {
            named = true;
            let _ = sender.unbounded_send(LiveEvent::Session(id.to_owned()));
        }
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "control_request" {
            claude_ask::take_request(&value, answers, sender);
            continue;
        }
        if kind == "system" {
            claude_ask::take_task(&value, tasks);
        }
        if kind == "result" {
            // The turn is over: with its input closed, Claude ends instead of waiting for
            // the next prompt.
            answers.close();
        }
        if kind == "result" && value.get("is_error").and_then(Value::as_bool) == Some(true) {
            error = value
                .get("result")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| Some("the turn failed".into()));
        }
        if kind == "stream_event" {
            // A subagent streams too: its words are not the answer of this turn.
            let subagent = value.get("parent_tool_use_id").is_some_and(|id| !id.is_null());
            if !subagent && take_partial(&mut partial, &value) {
                let mut turn = parser.snapshot();
                turn.entries.insert(0, prompt_entry(prompt, started));
                let _ = sender.unbounded_send(LiveEvent::Update(with_partial(turn, &partial, now_ms())));
            }
            continue;
        }
        if !matches!(kind, "assistant" | "user" | "system" | "result") {
            continue;
        }
        if kind == "assistant" {
            // The whole block is here: what was being written is in it.
            partial.text.clear();
        }
        parser.feed(&value);
        let mut turn = parser.snapshot();
        turn.entries.insert(0, prompt_entry(prompt, started));
        let _ = sender.unbounded_send(LiveEvent::Update(turn));
    }
    error
}

/// One item of `codex exec --json` as a block: the message, the reasoning summary, or a
/// tool call whose result comes when the item completes.
fn codex_block(item: &Value, done: bool) -> Option<Block> {
    let text = |name: &str| item.get(name).and_then(Value::as_str).unwrap_or("").to_owned();
    let status = text("status");
    let failed = matches!(status.as_str(), "failed" | "declined");
    let id = text("id");
    let call = |name: String, input: Value, result: Option<ToolResult>| Block::ToolCall { id: id.clone(), kind: ToolKind::classify(&name), name, input, result };
    Some(match item.get("type").and_then(Value::as_str)? {
        "agent_message" => Block::Text { text: text("text") },
        "reasoning" => Block::Thinking { text: text("text") },
        "command_execution" => {
            let exit = item.get("exit_code").and_then(Value::as_i64);
            let finished = done || exit.is_some() || matches!(status.as_str(), "completed" | "failed" | "declined");
            let result = finished.then(|| ToolResult { text: text("aggregated_output"), is_error: failed || exit.is_some_and(|code| code != 0) });
            call("shell".into(), json!({"command": text("command")}), result)
        }
        "file_change" => {
            let changes = item.get("changes").cloned().unwrap_or(Value::Null);
            let lines: Vec<String> = changes
                .as_array()
                .into_iter()
                .flatten()
                .map(|change| format!("{} {}", change.get("kind").and_then(Value::as_str).unwrap_or("update"), change.get("path").and_then(Value::as_str).unwrap_or("")))
                .collect();
            call("apply_patch".into(), json!({"changes": changes}), done.then(|| ToolResult { text: lines.join("\n"), is_error: failed }))
        }
        "mcp_tool_call" => {
            let name = format!("mcp__{}__{}", text("server"), text("tool"));
            let result = done.then(|| match item.get("error").filter(|e| !e.is_null()) {
                Some(error) => ToolResult { text: error.get("message").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| error.to_string()), is_error: true },
                None => {
                    let content = item.pointer("/result/content").and_then(Value::as_array);
                    let parts: Vec<&str> = content.into_iter().flatten().filter_map(|part| part.get("text").and_then(Value::as_str)).collect();
                    ToolResult { text: parts.join("\n"), is_error: failed }
                }
            });
            call(name, item.get("arguments").cloned().unwrap_or(Value::Null), result)
        }
        "web_search" => call("web_search".into(), json!({"query": text("query")}), done.then(|| ToolResult { text: String::new(), is_error: false })),
        "todo_list" => {
            let steps: Vec<String> = item
                .get("items")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|step| {
                    let mark = if step.get("completed").and_then(Value::as_bool) == Some(true) { "[x]" } else { "[ ]" };
                    format!("- {mark} {}", step.get("text").and_then(Value::as_str).unwrap_or(""))
                })
                .collect();
            Block::Text { text: steps.join("\n") }
        }
        "error" => Block::Text { text: format!("⚠ {}", text("message")) },
        _ => return None,
    })
}

/// Builds the turn from `codex exec --json`: `thread.started` names the session, items
/// start, update and complete, `turn.failed` and `error` carry what went wrong.
/// UNVERIFIED against a real Codex run: written from its `exec_events.rs`.
fn read_codex(lines: impl Iterator<Item = String>, prompt: &[Block], sender: &UnboundedSender<LiveEvent>) -> Option<String> {
    let mut turn = Transcript::default();
    turn.entries.push(prompt_entry(prompt, now_ms()));
    let mut error = None;
    for line in lines {
        let Ok(value) = serde_json::from_str::<Value>(&line) else { continue };
        let block = match value.get("type").and_then(Value::as_str).unwrap_or("") {
            "thread.started" => {
                if let Some(id) = value.get("thread_id").and_then(Value::as_str) {
                    let _ = sender.unbounded_send(LiveEvent::Session(id.to_owned()));
                }
                None
            }
            kind @ ("item.started" | "item.updated" | "item.completed") => value.get("item").and_then(|item| codex_block(item, kind == "item.completed")),
            // The usage of `turn.completed` sums every call of the turn, so it says nothing
            // of the context; the session file read after the turn has the true numbers.
            "turn.failed" => {
                error = Some(value.pointer("/error/message").and_then(Value::as_str).unwrap_or("the turn failed").to_owned());
                None
            }
            "error" => {
                error = Some(value.get("message").and_then(Value::as_str).unwrap_or("the turn failed").to_owned());
                None
            }
            _ => None,
        };
        let Some(block) = block else { continue };
        let id = value.pointer("/item/id").and_then(Value::as_str).unwrap_or("").to_owned();
        match turn.entries.last_mut() {
            Some(last) if last.role == Role::Assistant => {
                // An item repeats as it moves on: the block of the same item is replaced.
                let ids = turn_item_ids(last);
                match ids.iter().position(|known| !id.is_empty() && known == &id) {
                    Some(at) => last.blocks[at] = block,
                    None => last.blocks.push(block),
                }
                last.id = encode_ids(&ids, &id);
            }
            _ => turn.entries.push(Entry {
                id: encode_ids(&[], &id),
                role: Role::Assistant,
                timestamp_ms: now_ms(),
                blocks: vec![block],
                model: None,
                usage: None,
                compact_summary: false,
            }),
        }
        let _ = sender.unbounded_send(LiveEvent::Update(turn.clone()));
    }
    error
}

/// The item ids of the blocks of a live Codex answer, kept in its entry id in order.
fn turn_item_ids(entry: &Entry) -> Vec<String> {
    entry.id.strip_prefix("live-answer:").map(|ids| ids.split(' ').map(str::to_owned).collect()).unwrap_or_default()
}

/// The entry id with `id` added when it is a new item.
fn encode_ids(ids: &[String], id: &str) -> String {
    let mut all = ids.to_vec();
    if !all.iter().any(|known| known == id) {
        all.push(id.to_owned());
    }
    format!("live-answer:{}", all.join(" "))
}

/// Builds the turn from `dsh headless --json`: `session`, `text`, `thinking`, `tool_call`,
/// `tool_result`, `status` (with usage at step ends), `final` and `error` lines.
/// UNVERIFIED against a real dsh run: written from its `json-stream.ts`.
fn read_dsh(lines: impl Iterator<Item = String>, prompt: &[Block], sender: &UnboundedSender<LiveEvent>) -> Option<String> {
    let mut turn = Transcript::default();
    turn.entries.push(prompt_entry(prompt, now_ms()));
    let mut error = None;
    for line in lines {
        let Ok(value) = serde_json::from_str::<Value>(&line) else { continue };
        let at = now_ms();
        let text = |name: &str| value.get(name).and_then(Value::as_str).unwrap_or("").to_owned();
        let block = match value.get("type").and_then(Value::as_str).unwrap_or("") {
            "session" => {
                let _ = sender.unbounded_send(LiveEvent::Session(text("sessionId")));
                None
            }
            "text" => Some(Block::Text { text: text("text") }),
            "thinking" => Some(Block::Thinking { text: text("text") }),
            "tool_call" => {
                let name = text("tool");
                Some(Block::ToolCall {
                    id: text("callId"),
                    kind: ToolKind::classify(&name),
                    name,
                    input: value.get("input").cloned().unwrap_or(Value::Null),
                    result: None,
                })
            }
            "tool_result" => {
                let id = text("callId");
                let result = ToolResult {
                    text: value
                        .get("result")
                        .map(|r| r.as_str().map(str::to_owned).unwrap_or_else(|| r.to_string()))
                        .unwrap_or_default(),
                    is_error: value.get("status").and_then(Value::as_str) == Some("error"),
                };
                for entry in turn.entries.iter_mut().rev() {
                    if let Some(Block::ToolCall { result: slot, .. }) = entry
                        .blocks
                        .iter_mut()
                        .find(|b| matches!(b, Block::ToolCall { id: other, .. } if *other == id))
                    {
                        *slot = Some(result);
                        break;
                    }
                }
                None
            }
            "status" => {
                if let Some(usage) = value.get("usage") {
                    let field = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
                    let usage = Usage {
                        input: field("inputTokens"),
                        output: field("outputTokens"),
                        cache_read: field("cacheReadTokens"),
                        cache_write: field("cacheWriteTokens"),
                        cache_write_long: 0,
                    };
                    turn.add_call(Some("deepseek-flash"), usage, at);
                }
                None
            }
            "error" => {
                error = Some(value.get("message").and_then(Value::as_str).unwrap_or("the turn failed").to_owned());
                None
            }
            _ => None,
        };
        if let Some(block) = block {
            match turn.entries.last_mut() {
                Some(last) if last.role == Role::Assistant => last.blocks.push(block),
                _ => turn.entries.push(Entry {
                    id: "live-answer".into(),
                    role: Role::Assistant,
                    timestamp_ms: at,
                    blocks: vec![block],
                    model: None,
                    usage: None,
                    compact_summary: false,
                }),
            }
        }
        let _ = sender.unbounded_send(LiveEvent::Update(turn.clone()));
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(harness: HarnessKind) -> TurnSpec {
        TurnSpec {
            harness,
            session: Some("abc".into()),
            cwd: ".".into(),
            prompt: "say \"hi\" & exit".into(),
            model: Some("haiku".into()),
            effort: None,
            permission: Permission::AcceptEdits,
            ssh: None,
            attachments: Vec::new(),
            ide: None,
            drop_turn: None,
        }
    }

    /// A real Haiku turn on an SSH host through the detached runner (costs a fraction of a
    /// cent): `BOWYER_SSH_HOST=user@host BOWYER_SSH_KEY=key BOWYER_SSH_CWD=/tmp/x cargo test
    /// -p live -- --ignored over_ssh`.
    #[test]
    #[ignore = "needs an SSH host with Claude Code and spends a Haiku call"]
    fn a_claude_turn_runs_over_ssh() {
        let host = std::env::var("BOWYER_SSH_HOST").expect("BOWYER_SSH_HOST");
        let key = std::env::var_os("BOWYER_SSH_KEY").map(std::path::PathBuf::from);
        let mut spec = spec(HarnessKind::Claude);
        spec.session = None;
        spec.cwd = std::env::var("BOWYER_SSH_CWD").unwrap_or_else(|_| "/tmp".into());
        spec.prompt = "Reply with the single word: detached".into();
        spec.model = Some("haiku".into());
        spec.ssh = Some(envs::SshEnvironment::new(&host).with_identity(key));
        let turn = start(spec).unwrap();
        let (_, mut events) = turn.into_parts();
        let mut answer = String::new();
        let finished = loop {
            match futures_executor_next(&mut events) {
                Some(LiveEvent::Update(transcript)) => answer = transcript.search_text(10_000),
                Some(LiveEvent::Finished { ok, error }) => break (ok, error),
                Some(_) => {}
                None => break (false, Some("no end".into())),
            }
        };
        assert_eq!(finished, (true, None));
        assert!(answer.to_lowercase().contains("detached"), "{answer}");
    }

    /// The next event, waiting on the channel without an executor.
    /// A long answer over SSH arrives in many growing updates, not at once. Spends a Haiku
    /// call: `BOWYER_SSH_HOST=… cargo test -p live -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs an SSH host with Claude Code and spends a Haiku call"]
    fn a_long_answer_streams_over_ssh() {
        let host = std::env::var("BOWYER_SSH_HOST").expect("BOWYER_SSH_HOST");
        let key = std::env::var_os("BOWYER_SSH_KEY").map(std::path::PathBuf::from);
        let mut spec = spec(HarnessKind::Claude);
        spec.session = None;
        spec.cwd = std::env::var("BOWYER_SSH_CWD").unwrap_or_else(|_| "/tmp".into());
        spec.prompt = "Write 30 numbered sentences about rivers, one per line.".into();
        spec.model = Some("haiku".into());
        spec.permission = Permission::Settings;
        spec.ssh = Some(envs::SshEnvironment::new(&host).with_identity(key));
        let turn = start(spec).unwrap();
        let (_, mut events) = turn.into_parts();
        let started = std::time::Instant::now();
        let mut lengths: Vec<(u128, usize)> = Vec::new();
        let finished = loop {
            match futures_executor_next(&mut events) {
                Some(LiveEvent::Update(transcript)) => {
                    let length = transcript.entries.iter().skip(1).map(|e| e.blocks.iter().map(|b| match b {
                        Block::Text { text } => text.len(),
                        _ => 0,
                    }).sum::<usize>()).sum();
                    if lengths.last().is_none_or(|(_, last)| *last != length) {
                        lengths.push((started.elapsed().as_millis(), length));
                    }
                }
                Some(LiveEvent::Finished { ok, error }) => break (ok, error),
                Some(_) => {}
                None => break (false, Some("no end".into())),
            }
        };
        eprintln!("answer length by time (ms, bytes): {lengths:?}");
        assert_eq!(finished, (true, None));
        assert!(lengths.len() >= 5, "the answer grew in steps: {lengths:?}");
    }

    fn futures_executor_next(events: &mut UnboundedReceiver<LiveEvent>) -> Option<LiveEvent> {
        loop {
            match events.try_next() {
                Ok(Some(event)) => return Some(event),
                Ok(None) => return None,
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
            }
        }
    }

    #[test]
    fn an_answer_shows_while_it_is_written_and_once_when_done() {
        let lines = [
            r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"m1"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Rivers "}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"flow."}}}"#,
            r#"{"type":"assistant","message":{"id":"m1","model":"claude-haiku-5-5","content":[{"type":"text","text":"Rivers flow."}],"usage":{"input_tokens":1,"output_tokens":3}}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Rivers flow."}"#,
        ]
        .map(String::from);
        let (sender, mut events) = unbounded();
        let error = read_claude(lines.into_iter(), &[Block::Text { text: "Rivers?".into() }], &sender, &claude_ask::Input::closed(), &claude_ask::Tasks::default());
        assert_eq!(error, None);
        let mut updates = Vec::new();
        while let Ok(Some(event)) = events.try_next() {
            if let LiveEvent::Update(turn) = event {
                updates.push(turn);
            }
        }
        let text_of = |turn: &Transcript| turn.search_text(1000);
        assert!(updates.iter().any(|turn| text_of(turn).contains("Rivers ") && !text_of(turn).contains("flow.")), "the first words show alone");
        let last = updates.last().unwrap();
        assert_eq!(text_of(last).matches("Rivers flow.").count(), 1, "the whole block replaces the partial text");
        assert_eq!(last.entries.len(), 2, "the prompt and one answer");
    }

    #[derive(Clone, Default)]
    struct Written(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for Written {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn claude_asks_the_window_and_its_input_closes_at_the_result() {
        let written = Written::default();
        let answers = claude_ask::Input::new(claude_ask::Sink::Pipe(Box::new(written.clone())));
        let (line, lines) = std::sync::mpsc::channel::<String>();
        let (sender, mut events) = unbounded();
        let reader_answers = answers.clone();
        let reader = thread::spawn(move || read_claude(lines.into_iter(), &[Block::Text { text: "Make a".into() }], &sender, &reader_answers, &claude_ask::Tasks::default()));
        line.send(r#"{"type":"system","subtype":"init","session_id":"s1"}"#.into()).unwrap();
        line.send(r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"mkdir a"},"tool_use_id":"t1"}}"#.into()).unwrap();
        let ask = loop {
            match futures_executor_next(&mut events) {
                Some(LiveEvent::Permission(ask)) => break ask,
                Some(_) => {}
                None => panic!("no question"),
            }
        };
        assert_eq!((ask.tool_call_id.as_str(), ask.title.as_str(), ask.kind.as_str()), ("r1", "mkdir a", "execute"));
        ask.reply.send(Some("allow".into())).unwrap();
        let sent = loop {
            let text = String::from_utf8(written.0.lock().unwrap().clone()).unwrap();
            if !text.is_empty() {
                break text;
            }
            thread::sleep(std::time::Duration::from_millis(5));
        };
        let reply: Value = serde_json::from_str(sent.trim()).unwrap();
        assert_eq!(reply["response"]["request_id"], "r1");
        assert_eq!(reply["response"]["response"]["behavior"], "allow");
        line.send(r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#.into()).unwrap();
        drop(line);
        assert_eq!(reader.join().unwrap(), None);
        assert!(!answers.send("late\n"), "the input closed at the result");
    }

    #[test]
    fn a_claude_command_stops_through_its_task() {
        let written = Written::default();
        let answers = claude_ask::Input::new(claude_ask::Sink::Pipe(Box::new(written.clone())));
        let tasks = claude_ask::Tasks::default();
        let lines = [
            r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
            r#"{"type":"system","subtype":"task_started","task_id":"b58","tool_use_id":"toolu_1","description":"Wait","is_backgrounded":false,"task_type":"local_bash"}"#,
        ]
        .map(String::from);
        let (sender, _events) = unbounded();
        read_claude(lines.into_iter(), &[], &sender, &answers, &tasks);
        let stop = StopHandle { pid: 0, remote: None, claude: Some((answers, tasks)), processes: false };
        assert!(stop.stops_commands());
        stop.stop_command("toolu_1", Some("python -c pass")).unwrap();
        let sent: Value = serde_json::from_str(String::from_utf8(written.0.lock().unwrap().clone()).unwrap().trim()).unwrap();
        assert_eq!(sent["type"], "control_request");
        assert_eq!(sent["request"], json!({"subtype": "stop_task", "task_id": "b58"}));
        assert_eq!(stop.stop_command("toolu_2", None).unwrap_err().kind(), io::ErrorKind::NotFound, "an unknown call has nothing to stop");
    }

    #[test]
    fn claude_asks_everywhere_but_on_a_host_that_allows_everything() {
        let mut claude = spec(HarnessKind::Claude);
        let asks = |spec: &TurnSpec| command_line(spec).unwrap().1.windows(2).any(|w| w == ["--permission-prompt-tool", "stdio"]);
        assert!(asks(&claude));
        claude.permission = Permission::Bypass;
        assert!(asks(&claude), "a question to the user still comes here");
        claude.ssh = Some(envs::SshEnvironment::new("vm"));
        assert!(!asks(&claude));
        claude.permission = Permission::AcceptEdits;
        assert!(asks(&claude));
    }

    #[test]
    fn codex_events_become_a_turn() {
        let lines = [
            r#"{"type":"thread.started","thread_id":"0199a213-81c0-7800-8aa1-bbab2a035a53"}"#,
            r#"{"type":"turn.started"}"#,
            r#"{"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"Look at the files first."}}"#,
            r#"{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"ls","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#,
            r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"ls","aggregated_output":"src\n","exit_code":0,"status":"completed"}}"#,
            r#"{"type":"item.completed","item":{"id":"item_2","type":"file_change","changes":[{"path":"src/a.rs","kind":"add"}],"status":"completed"}}"#,
            r#"{"type":"item.completed","item":{"id":"item_3","type":"agent_message","text":"Added src/a.rs."}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":2400,"cached_input_tokens":1200,"output_tokens":80,"reasoning_output_tokens":40}}"#,
        ]
        .map(String::from);
        let (sender, mut events) = unbounded();
        let error = read_codex(lines.into_iter(), &[Block::Text { text: "Add a.rs".into() }], &sender);
        assert_eq!(error, None);
        let mut session = None;
        let mut last = None;
        let mut running = false;
        while let Ok(Some(event)) = events.try_next() {
            match event {
                LiveEvent::Session(id) => session = Some(id),
                LiveEvent::Update(turn) => {
                    running |= turn.entries[1].blocks.iter().any(|b| matches!(b, Block::ToolCall { result: None, .. }));
                    last = Some(turn);
                }
                _ => {}
            }
        }
        assert_eq!(session.as_deref(), Some("0199a213-81c0-7800-8aa1-bbab2a035a53"));
        assert!(running, "a command shows while it runs");
        let turn = last.unwrap();
        assert_eq!(turn.entries.len(), 2);
        let blocks = &turn.entries[1].blocks;
        assert_eq!(blocks.len(), 4, "the started command is replaced when it completes");
        assert!(matches!(&blocks[1], Block::ToolCall { kind: ToolKind::Shell, result: Some(r), .. } if r.text == "src\n" && !r.is_error));
        assert!(matches!(&blocks[2], Block::ToolCall { kind: ToolKind::Edit, .. }));
        assert!(matches!(&blocks[3], Block::Text { text } if text == "Added src/a.rs."));

        let failed = [r#"{"type":"turn.failed","error":{"message":"usage limit"}}"#.to_owned()];
        let (sender, _events) = unbounded();
        assert_eq!(read_codex(failed.into_iter(), &[], &sender).as_deref(), Some("usage limit"));
    }

    #[test]
    fn codex_options_go_before_resume_and_the_prompt_on_stdin() {
        let mut codex = spec(HarnessKind::Codex);
        codex.session = Some("0199a213".into());
        codex.model = Some("gpt-5-codex".into());
        codex.permission = Permission::AcceptEdits;
        let (program, args) = command_line(&codex).unwrap();
        assert_eq!(program, "codex");
        assert_eq!(args, ["exec", "--json", "--skip-git-repo-check", "--model", "gpt-5-codex", "--sandbox", "workspace-write", "resume", "0199a213", "-"]);
        assert_eq!(stdin_payload(&codex), codex.prompt);

        let picture = Attachment { path: "/w/shot.png".into(), image: Some(("image/png".into(), std::sync::Arc::new(vec![1]))) };
        codex.attachments = vec![picture];
        let (_, args) = command_line(&codex).unwrap();
        assert_eq!(&args[args.len() - 3..], ["--image", "/w/shot.png", "-"], "after resume, one value each");
        codex.session = None;
        let (_, args) = command_line(&codex).unwrap();
        assert_eq!(&args[..4], ["exec", "--image", "/w/shot.png", "--json"], "a new session: before any flag, never next to the -");
        assert_eq!(args.last().unwrap(), "-");
        codex.effort = Some("high".into());
        let (_, args) = command_line(&codex).unwrap();
        assert!(args.windows(2).any(|w| w == ["-c", "model_reasoning_effort=high"]), "{args:?}");
    }

    #[test]
    fn claude_takes_the_effort_and_others_ignore_it() {
        let mut claude = spec(HarnessKind::Claude);
        let (_, args) = command_line(&claude).unwrap();
        assert!(!args.contains(&"--effort".to_owned()), "no effort, the harness default");
        claude.effort = Some("xhigh".into());
        let (_, args) = command_line(&claude).unwrap();
        assert!(args.windows(2).any(|w| w == ["--effort", "xhigh"]), "{args:?}");
        let mut opencode = spec(HarnessKind::OpenCode);
        opencode.effort = Some("high".into());
        assert_eq!(command_line(&opencode).unwrap().1, ["acp"]);
    }

    #[test]
    fn the_prompt_never_reaches_the_command_line() {
        let (program, args) = command_line(&spec(HarnessKind::Claude)).unwrap();
        assert_eq!(program, "claude");
        assert!(args.windows(2).any(|w| w == ["--resume", "abc"]));
        assert!(args.windows(2).any(|w| w == ["--permission-mode", "acceptEdits"]));
        assert!(!args.iter().any(|a| a.contains("hi")));
        let payload: Value = serde_json::from_str(stdin_payload(&spec(HarnessKind::Claude)).trim()).unwrap();
        assert_eq!(payload["message"]["content"], "say \"hi\" & exit");
        let (program, args) = command_line(&spec(HarnessKind::OpenCode)).unwrap();
        assert_eq!((program.as_str(), args), ("opencode", vec!["acp".to_owned()]), "OpenCode takes everything over ACP");
    }

    #[test]
    fn a_dropped_turn_resumes_before_its_prompt() {
        let mut dropping = spec(HarnessKind::Claude);
        dropping.drop_turn = Some(("kept-line".into(), "failed-prompt".into()));
        let (_, args) = command_line(&dropping).unwrap();
        assert!(args.windows(6).any(|w| w == ["--resume", "abc", "--resume-session-at", "kept-line", "--resume-drops-turn", "failed-prompt"]));
        dropping.session = None;
        let (_, args) = command_line(&dropping).unwrap();
        assert!(!args.iter().any(|arg| arg.starts_with("--resume")), "a new session has nothing to drop");
    }

    #[test]
    fn pictures_go_inline_to_claude_and_files_by_path() {
        let picture = Attachment {
            path: "/tmp/shot.png".into(),
            image: Some(("image/png".into(), std::sync::Arc::new(vec![1, 2, 3]))),
        };
        let file = Attachment { path: "/tmp/notes.txt".into(), image: None };
        let mut claude = spec(HarnessKind::Claude);
        claude.attachments = vec![picture.clone(), file.clone()];
        let payload: Value = serde_json::from_str(stdin_payload(&claude).trim()).unwrap();
        let content = &payload["message"]["content"];
        assert!(content[0]["text"].as_str().unwrap().ends_with("Attached files:\n- /tmp/notes.txt"));
        assert_eq!(content[1]["source"]["data"], "AQID");
        assert_eq!(prompt_blocks(&claude).len(), 2);

        assert_eq!(image_type("Shot.JPEG"), Some("image/jpeg"));
        assert_eq!(image_type("notes.txt"), None);
    }

}
