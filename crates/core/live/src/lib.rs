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
mod follow;
mod ide;
mod kill;
mod pi_rpc;
mod resolve;
pub mod models;
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
    /// A new Claude Code session from the line `fork_at` of `session`, which stays as it is
    /// (`--resume-session-at` with `--fork-session`).
    pub fork_at: Option<String>,
    /// Claude Code on a host: the MCP config of Bowyer's memory, reached through a tunnel.
    /// It holds a token, so it goes into a file only the user can read, never into the
    /// command line every user of the host can see.
    pub mcp_config: Option<McpConfig>,
}

/// The MCP config of a turn on a host.
#[derive(Clone, Debug, PartialEq)]
pub enum McpConfig {
    /// Its text, put on the host when the turn starts.
    Text(String),
    /// The file on the host it was put in, by [`place_mcp_config`].
    Placed(String),
}

/// Puts the MCP config of a turn on its host ahead of the turn, which then starts without a
/// round trip to the host. Blocks for that round trip: off the window's thread. Without the
/// file the turn still runs, only without the memory.
pub fn place_mcp_config(spec: &mut TurnSpec) {
    if let (Some(ssh), Some(McpConfig::Text(config))) = (&spec.ssh, &spec.mcp_config) {
        let run = format!("{}-{}", now_ms(), std::process::id());
        spec.mcp_config = write_mcp_config(ssh, &run, config).ok().map(McpConfig::Placed);
    }
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
    /// The harness answered (`true`) but goes on running for what it left in the background
    /// (a command, an agent, a wakeup); `false` once that brings it back to work.
    Resting(bool),
    /// What the harness runs in the background now: commands and agents it started and has
    /// not heard the end of (Claude's `background_tasks_changed`).
    Background(Vec<BackgroundTask>),
    /// The harness took a prompt sent while the turn ran ([`StopHandle::say`]); one event per
    /// prompt, in the order they were sent.
    Heard,
    Finished { ok: bool, error: Option<String> },
}

/// A command or an agent a harness runs in the background.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackgroundTask {
    pub id: String,
    pub description: String,
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
    /// For a run detached on a host: the connection that follows it now.
    link: Option<follow::Link>,
}

impl StopHandle {
    pub fn stop(&self) {
        let reached = self.remote.as_ref().is_some_and(|(ssh, run)| ssh.stop_detached(run).is_ok());
        match &self.link {
            Some(link) => {
                link.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
                // A run stopped on the host ends its connection by itself, with the host's
                // word that it ended; one out of reach is left, and the connection with it.
                let current = link.current.load(std::sync::atomic::Ordering::SeqCst);
                if !reached && current != 0 {
                    stop_tree(current);
                }
            }
            None => stop_tree(self.pid),
        }
    }

    /// Sends one more prompt to a turn whose harness still reads its input: Claude past its
    /// answer while a command or an agent of it runs in the background. False when the
    /// input is closed; [`LiveEvent::Heard`] says the harness took it. `spec` names the
    /// prompt and its files as for a turn of its own. Blocks for a round trip to a host.
    pub fn say(&self, spec: &TurnSpec) -> bool {
        self.claude.as_ref().is_some_and(|(input, _)| input.send(&stdin_payload(spec)))
    }

    /// Stops a command or an agent the turn runs in the background, by its task id.
    pub fn stop_background(&self, task: &str) -> bool {
        self.claude.as_ref().is_some_and(|(input, _)| claude_ask::stop_task_id(input, task))
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
    // Ids come from files and transcripts: one that starts with `-` would be read as an
    // option of the harness.
    let option_like = |value: &str| value.starts_with('-');
    if spec.session.as_deref().is_some_and(option_like)
        || spec.drop_turn.as_ref().is_some_and(|(kept, prompt)| option_like(kept) || option_like(prompt))
        || spec.fork_at.as_deref().is_some_and(option_like)
    {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "a session id that reads as an option"));
    }
    let mut args: Vec<String> = Vec::new();
    let program = match spec.harness {
        HarnessKind::Claude => {
            args.extend(
                // Prompts sent while it runs come back as it takes them: the window shows
                // them where they went in, and knows they arrived.
                ["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--include-partial-messages", "--replay-user-messages"]
                    .map(String::from),
            );
            if let Some(session) = &spec.session {
                args.extend(["--resume".into(), session.clone()]);
                // Claude Code checks that only the dropped turn goes, and refuses otherwise.
                if let Some(at) = &spec.fork_at {
                    args.extend(["--resume-session-at".into(), at.clone(), "--fork-session".into()]);
                } else if let Some((kept, prompt)) = &spec.drop_turn {
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
        HarnessKind::Pi => {
            // The prompt goes over the protocol; Pi asks no permissions.
            args.extend(["--mode", "rpc"].map(String::from));
            if let Some(session) = &spec.session {
                args.extend(["--session".into(), session.clone()]);
            }
            if let Some(model) = &spec.model {
                args.extend(["--model".into(), model.clone()]);
            }
            if let Some(effort) = &spec.effort {
                args.extend(["--thinking".into(), effort.clone()]);
            }
            "pi"
        }
        HarnessKind::Antigravity => {
            // Session, prompt and files go over the protocol, as with OpenCode.
            args.push("acp".into());
            "agy"
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

/// Whether a harness talks with the window both ways for the whole turn: an ACP agent
/// (OpenCode, Antigravity) or Pi in RPC mode.
fn talks_both_ways(harness: &HarnessKind) -> bool {
    matches!(harness, HarnessKind::OpenCode | HarnessKind::Antigravity | HarnessKind::Pi)
}

/// Whether a Claude turn keeps its input open while it runs: for the questions it asks
/// before an action (and `AskUserQuestion`, which comes even when everything is allowed),
/// and for prompts sent while a command of it runs in the background.
fn claude_answered(spec: &TurnSpec) -> bool {
    spec.harness == HarnessKind::Claude
}

/// The prompt with the paths of the files the harness reads from disk: every attachment
/// but the pictures Claude takes inline, and nothing for OpenCode, which gets `--file`.
fn prompt_text(spec: &TurnSpec) -> String {
    let listed: Vec<&str> = spec
        .attachments
        .iter()
        .filter(|a| match spec.harness {
            HarnessKind::Claude | HarnessKind::Codex | HarnessKind::Pi => a.image.is_none(),
            HarnessKind::OpenCode | HarnessKind::Antigravity => false,
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

/// Puts a turn's MCP config on the host, readable by its owner only, next to the run's other
/// files (they go after two days), and returns its path.
fn write_mcp_config(ssh: &envs::SshEnvironment, run: &str, config: &str) -> io::Result<String> {
    use envs::Environment;
    let run: String = run.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    let script = format!("umask 077; d=\"$HOME/.cache/bowyer/runs\"; mkdir -p \"$d\" && f=\"$d/{run}.mcp.json\" && cat > \"$f\" && printf %s \"$f\"");
    let path = ssh.run_with_input("sh", &["-c", &script], None, config.as_bytes())?;
    let path = path.trim();
    if path.is_empty() {
        return Err(io::Error::other("no path for the MCP config"));
    }
    Ok(path.to_owned())
}

/// Starts a turn on this computer.
pub fn start(spec: TurnSpec) -> io::Result<Turn> {
    let (program, mut args) = command_line(&spec)?;
    // A turn on an SSH host runs detached from the connection, so a dropped link does not
    // end it; the remote login shell finds the harness, the prompt still goes on stdin.
    // An ACP agent, and Pi in RPC mode, talk both ways for the whole turn, so they cannot
    // run detached.
    let acp = talks_both_ways(&spec.harness);
    // Either way the run has a name on the host, under which its process id is kept.
    let run = spec.ssh.as_ref().map(|_| format!("{}-{}", now_ms(), std::process::id()));
    if spec.harness == HarnessKind::Claude
        && let (Some(ssh), Some(config), Some(run)) = (&spec.ssh, &spec.mcp_config, &run)
    {
        // Without the file the turn still runs, only without the memory.
        let path = match config {
            McpConfig::Placed(path) => Some(path.clone()),
            McpConfig::Text(config) => write_mcp_config(ssh, run, config).ok(),
        };
        if let Some(path) = path {
            args.extend(["--mcp-config".into(), path]);
        }
    }
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

    // A run detached on a host is followed over connections that may drop and be made
    // again; anything else is read from the process the window started.
    let link = (spec.ssh.is_some() && !acp).then(follow::Link::default);
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (lines, ending): (Lines, Box<dyn FnOnce() -> Ending + Send>) =
        match (&link, &spec.ssh, &run) {
            (Some(link), Some(ssh), Some(run)) => {
                let first = follow::Connection {
                    stdout: Box::new(stdout),
                    stderr: Box::new(stderr),
                    pid,
                    wait: Box::new(move || child.wait().ok().and_then(|status| status.code())),
                };
                let (line_sender, line_receiver) = std::sync::mpsc::channel();
                let (ssh, run, link, follow_sender) = (ssh.clone(), run.clone(), link.clone(), sender.clone());
                let follower = thread::spawn(move || {
                    let reopen = |from: u64| {
                        let mut command = ssh.follow_detached(&run, from);
                        command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
                        let child = command.spawn()?;
                        follow::Connection::of(child).ok_or_else(|| io::Error::other("no output"))
                    };
                    follow::follow(first, reopen, &link, &line_sender, &follow_sender, follow::FIRST_PAUSE)
                });
                let ending = move || {
                    let outcome = follower.join().unwrap_or(follow::Outcome { code: None, stderr: String::new(), lost: true });
                    if outcome.lost {
                        return Ending { success: false, stderr: follow::LOST.into(), failure: String::new() };
                    }
                    let failure = outcome.code.map_or_else(|| "the connection ended".to_owned(), |code| format!("the harness exited with code {code}"));
                    Ending { success: outcome.code == Some(0), stderr: outcome.stderr, failure }
                };
                (Box::new(line_receiver.into_iter()), Box::new(ending))
            }
            _ => {
                let lines = lossy_lines(stdout);
                let err_sender = sender.clone();
                let err_reader = thread::spawn(move || {
                    let mut tail: Vec<String> = Vec::new();
                    for line in lossy_lines(stderr) {
                        tail.push(line);
                        if tail.len() > 20 {
                            tail.remove(0);
                        }
                        let _ = err_sender.unbounded_send(LiveEvent::Stderr(tail.join("\n")));
                    }
                    tail.join("\n")
                });
                let ending = move || {
                    let status = child.wait();
                    let stderr = err_reader.join().unwrap_or_default();
                    let failure = match &status {
                        Ok(status) => format!("the harness exited with {status}"),
                        Err(error) => error.to_string(),
                    };
                    Ending { success: status.is_ok_and(|status| status.success()), stderr, failure }
                };
                (Box::new(lines), Box::new(ending))
            }
        };

    let reader_sender = sender.clone();
    let harness = spec.harness.clone();
    let prompt = prompt_blocks(&spec);
    let reader_spec = spec.clone();
    let (reader_answers, reader_tasks) = (answers.clone(), tasks.clone());
    let reader = thread::spawn(move || {
        match (harness, acp_stdin) {
            (harness, Some(stdin)) => {
                let error = if harness == HarnessKind::Pi {
                    pi_rpc::run_turn(stdin, lines, &reader_spec, &prompt, &reader_sender)
                } else {
                    acp::run_turn(stdin, lines, &reader_spec, &prompt, &reader_sender)
                };
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

    thread::spawn(move || {
        let ending = ending();
        let error_from_output = reader.join().ok().flatten();
        // An ACP agent is stopped once its turn ends: its exit code says nothing.
        let ok = (acp || ending.success) && error_from_output.is_none();
        let error = if ok {
            None
        } else {
            error_from_output.or_else(|| {
                let tail = ending.stderr.trim();
                Some(if tail.is_empty() { ending.failure } else { tail.to_owned() })
            })
        };
        let _ = sender.unbounded_send(LiveEvent::Finished { ok, error });
    });
    let remote = spec.ssh.clone().zip(run);
    let claude = (spec.harness == HarnessKind::Claude).then_some((answers, tasks));
    // The processes of every harness can be found: here under the harness, on a host under
    // the process id its run left there (an ACP agent runs attached, but leaves it too).
    let processes = true;
    Ok(Turn { stop: StopHandle { pid, remote, claude, processes, link }, events })
}

/// The output of a turn, line by line.
type Lines = Box<dyn Iterator<Item = String> + Send>;

/// The lines of `stream` without their line ends, to its end. A line that is not UTF-8 (a
/// tool's output in another code page) comes with replacement characters, and does not end
/// the reading: the harness would then wait on a full pipe for ever.
fn lossy_lines(stream: impl io::Read) -> impl Iterator<Item = String> {
    let mut reader = BufReader::new(stream);
    std::iter::from_fn(move || {
        let mut line = Vec::new();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(String::from_utf8_lossy(&line).trim_end_matches(['\n', '\r']).to_owned()),
        }
    })
}

/// How the process or connections of a turn ended.
struct Ending {
    success: bool,
    /// The end of the error output.
    stderr: String,
    /// What to say when the error output says nothing.
    failure: String,
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
    // Past a result Claude still runs while a command, an agent or a wakeup it left in the
    // background is pending; what it finishes comes back as a turn of its own.
    let mut resting = false;
    // Its input stays open past a result while something runs in the background, so a
    // prompt can still go in; it closes once nothing does (Claude ends after the turn the
    // last one's notice starts). The host of a detached run follows the same rule.
    let mut answered = false;
    let mut background = 0usize;
    // The prompt the turn began with comes back too: it shows already. A slash command may
    // not come back, so it is known by its text, not by coming first.
    let own_prompt = prompt.iter().find_map(|block| match block {
        Block::Text { text } => Some(text.trim().to_owned()),
        _ => None,
    });
    let mut own_seen = false;
    for line in lines {
        let Ok(value) = serde_json::from_str::<Value>(&line) else { continue };
        if !named && let Some(id) = value.get("session_id").and_then(Value::as_str) {
            named = true;
            let _ = sender.unbounded_send(LiveEvent::Session(id.to_owned()));
        }
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        if resting && matches!(kind, "user" | "assistant" | "stream_event") {
            resting = false;
            let _ = sender.unbounded_send(LiveEvent::Resting(false));
        }
        if kind == "result" && !resting {
            resting = true;
            let _ = sender.unbounded_send(LiveEvent::Resting(true));
        }
        if kind == "control_request" {
            claude_ask::take_request(&value, answers, sender);
            continue;
        }
        if kind == "system" {
            claude_ask::take_task(&value, tasks);
            if value.get("subtype").and_then(Value::as_str) == Some("background_tasks_changed") {
                let field = |task: &Value, name: &str| task.get(name).and_then(Value::as_str).unwrap_or_default().to_owned();
                let running: Vec<BackgroundTask> = value
                    .get("tasks")
                    .and_then(Value::as_array)
                    .map(|tasks| tasks.iter().map(|task| BackgroundTask { id: field(task, "task_id"), description: field(task, "description") }).collect())
                    .unwrap_or_default();
                background = running.len();
                if answered && background == 0 {
                    answers.close();
                }
                let _ = sender.unbounded_send(LiveEvent::Background(running));
            }
        }
        if kind == "result" {
            // The turn is over: with its input closed, Claude ends instead of waiting for
            // the next prompt, unless something of it still runs in the background.
            answered = true;
            if background == 0 {
                answers.close();
            }
        }
        if kind == "user"
            && let Some(text) = replay_text(&value)
        {
            if !own_seen && own_prompt.as_deref().is_none_or(|own| own == text.trim()) {
                own_seen = true;
                continue;
            }
            let _ = sender.unbounded_send(LiveEvent::Heard);
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
        parser.feed(&without_subagent_words(value));
        let mut turn = parser.snapshot();
        turn.entries.insert(0, prompt_entry(prompt, started));
        let _ = sender.unbounded_send(LiveEvent::Update(turn));
    }
    error
}

/// A line of the turn with only what the main conversation says. A subagent's lines come
/// in the same stream: its calls stay, to show that it works, but the task it was given and
/// its own words and thinking go, or they would read as a prompt of the user and the answer.
/// What it found comes back as the result of its call. The history drops these lines too.
fn without_subagent_words(mut value: Value) -> Value {
    let subagent = value.get("parent_tool_use_id").is_some_and(|id| !id.is_null()) || value.get("isSidechain").and_then(Value::as_bool) == Some(true);
    if !subagent {
        return value;
    }
    if let Some(content) = value.pointer_mut("/message/content") {
        let calls = match content.take() {
            Value::Array(blocks) => blocks.into_iter().filter(|block| matches!(block.get("type").and_then(Value::as_str), Some("tool_use" | "tool_result"))).collect(),
            _ => Vec::new(),
        };
        *content = Value::Array(calls);
    }
    value
}

/// The text of a prompt Claude took, when a `user` line of its output is one
/// (`--replay-user-messages`): Claude Code 2.0.19 and later mark it `isReplay`, an older one
/// only by its shape (text and pictures, not a tool's result, not a subagent's prompt).
fn replay_text(line: &Value) -> Option<String> {
    // What a hook said to the model comes back too: it is not a prompt the user sent.
    if history::hook_feedback(line).is_some() {
        return None;
    }
    match line.get("isReplay") {
        Some(Value::Bool(true)) => {}
        Some(_) => return None,
        None if line.get("parent_tool_use_id").is_some_and(|id| !id.is_null()) => return None,
        None => {}
    }
    match line.pointer("/message/content")? {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) if !blocks.is_empty() && blocks.iter().all(|block| matches!(block.get("type").and_then(Value::as_str), Some("text" | "image"))) => {
            Some(blocks.iter().filter_map(|block| block.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"))
        }
        _ => None,
    }
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

    /// The MCP config of a turn lands on the host readable by its owner only, and is
    /// removed after. `BOWYER_SSH_HOST=user@host cargo test -p live -- --ignored mcp_config`.
    #[test]
    #[ignore = "needs an SSH host"]
    fn the_mcp_config_is_for_its_owner_only() {
        use envs::Environment;
        let host = std::env::var("BOWYER_SSH_HOST").expect("BOWYER_SSH_HOST");
        let mut ssh = envs::SshEnvironment::new(&host);
        let system = std::path::Path::new(r"C:\Windows\System32\OpenSSH\ssh.exe");
        if cfg!(windows) && system.is_file() {
            ssh.ssh = system.to_path_buf();
        }
        let run = format!("test-{}", std::process::id());
        let path = write_mcp_config(&ssh, &run, r#"{"mcpServers":{}}"#).unwrap();
        assert!(path.ends_with(&format!(".cache/bowyer/runs/{run}.mcp.json")), "{path}");
        let check = ssh.run("sh", &["-c", &format!("stat -c %a \"{path}\"; cat \"{path}\"; rm -f \"{path}\"")], None).unwrap();
        assert_eq!(check.lines().collect::<Vec<_>>(), ["600", r#"{"mcpServers":{}}"#]);

        // Put ahead of the turn, the config is a file by the time the turn starts.
        let mut turn = spec(HarnessKind::Claude);
        turn.ssh = Some(ssh.clone());
        turn.mcp_config = Some(McpConfig::Text(r#"{"mcpServers":{"x":{}}}"#.into()));
        place_mcp_config(&mut turn);
        let Some(McpConfig::Placed(path)) = turn.mcp_config else { panic!("not placed: {:?}", turn.mcp_config) };
        let check = ssh.run("sh", &["-c", &format!("cat \"{path}\"; rm -f \"{path}\"")], None).unwrap();
        assert_eq!(check.trim(), r#"{"mcpServers":{"x":{}}}"#);
    }

    /// Without a host there is nothing to put anywhere.
    #[test]
    fn a_local_turn_places_no_mcp_config() {
        let mut turn = spec(HarnessKind::Claude);
        turn.mcp_config = Some(McpConfig::Text("{}".into()));
        place_mcp_config(&mut turn);
        assert_eq!(turn.mcp_config, Some(McpConfig::Text("{}".into())));
    }

    /// An id that reads as an option never reaches the harness's command line.
    #[test]
    fn an_id_that_reads_as_an_option_is_refused() {
        for harness in [HarnessKind::Claude, HarnessKind::Codex, HarnessKind::DeepSeek] {
            let mut turn = spec(harness.clone());
            turn.session = Some("--dangerously-skip-permissions".into());
            assert_eq!(command_line(&turn).unwrap_err().kind(), io::ErrorKind::InvalidInput, "{harness}");
        }
        let mut turn = spec(HarnessKind::Claude);
        turn.drop_turn = Some(("kept".into(), "-p".into()));
        assert!(command_line(&turn).is_err());
        assert!(command_line(&spec(HarnessKind::Claude)).is_ok());
    }

    /// A line that is not UTF-8 is read with replacement characters, and the lines after it
    /// still come.
    #[test]
    fn a_line_that_is_not_utf8_does_not_end_the_output() {
        let bytes: &[u8] = b"first\ncp1251 \xcf\xf0\xe8\xe2\xe5\xf2\r\nlast";
        let lines: Vec<String> = lossy_lines(bytes).collect();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[0], "first");
        assert!(lines[1].starts_with("cp1251 ") && lines[1].contains(char::REPLACEMENT_CHARACTER), "{:?}", lines[1]);
        assert_eq!(lines[2], "last");
    }

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
            fork_at: None,
            mcp_config: None,
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

    /// A subagent's task and words are not shown as a prompt and an answer of the turn; its
    /// calls are, and what it found comes in the result of the call that started it.
    #[test]
    fn a_subagent_shows_by_its_calls_not_its_words() {
        let lines = [
            r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"Agent","input":{"description":"Find prices","prompt":"Find the price of the kettle."}}]},"parent_tool_use_id":null}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Find the price of the kettle."}]},"parent_tool_use_id":"t1"}"#,
            r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"thinking","thinking":"Where to look."},{"type":"text","text":"Let me look it up."},{"type":"tool_use","id":"w1","name":"WebFetch","input":{"url":"https://example.com/kettle"}}]},"parent_tool_use_id":"t1"}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"w1","content":"Kettle: 99"}]},"parent_tool_use_id":"t1"}"#,
            r#"{"type":"assistant","message":{"id":"m3","content":[{"type":"text","text":"The kettle costs 99."}]},"parent_tool_use_id":"t1"}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"The kettle costs 99."}]},"parent_tool_use_id":null}"#,
            r#"{"type":"assistant","message":{"id":"m4","content":[{"type":"text","text":"It costs 99."}]},"parent_tool_use_id":null}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"It costs 99."}"#,
        ]
        .map(String::from);
        let (sender, mut events) = unbounded();
        read_claude(lines.into_iter(), &[Block::Text { text: "How much is the kettle?".into() }], &sender, &claude_ask::Input::closed(), &claude_ask::Tasks::default());
        let mut last = None;
        while let Ok(Some(event)) = events.try_next() {
            if let LiveEvent::Update(turn) = event {
                last = Some(turn);
            }
        }
        let turn = last.unwrap();
        let prompts: Vec<&Entry> = turn.entries.iter().filter(|entry| entry.role == Role::User && entry.blocks.iter().any(|block| matches!(block, Block::Text { .. }))).collect();
        assert_eq!(prompts.len(), 1, "only the user's prompt: {prompts:?}");
        let words: Vec<&str> = turn
            .entries
            .iter()
            .flat_map(|entry| &entry.blocks)
            .filter_map(|block| match block {
                Block::Text { text } | Block::Thinking { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(words, ["How much is the kettle?", "It costs 99."]);
        let calls: Vec<(&str, bool)> = turn
            .entries
            .iter()
            .flat_map(|entry| &entry.blocks)
            .filter_map(|block| match block {
                Block::ToolCall { name, result, .. } => Some((name.as_str(), result.is_some())),
                _ => None,
            })
            .collect();
        assert_eq!(calls, [("Agent", true), ("WebFetch", true)]);
    }

    /// Claude answers, waits for a command it left in the background, and takes up the
    /// notice of its end as a turn of its own: the window sees it rest and work again.
    #[test]
    fn claude_rests_between_its_answer_and_what_it_left_in_the_background() {
        // As Claude Code 2.1.332 wrote them in a real turn: the command goes to the background,
        // the answer ends the turn, and the notice of the end starts one of its own.
        let lines = [
            r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo build","run_in_background":true}}]}}"#,
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"b1","task_type":"local_bash","description":"Build the app"}]}"#,
            r#"{"type":"system","subtype":"task_started","task_id":"b1","tool_use_id":"t1","description":"Build the app","is_backgrounded":true}"#,
            r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"Building in the background."}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Building in the background."}"#,
            r#"{"type":"system","subtype":"task_updated","task_id":"b1","patch":{"status":"completed"}}"#,
            r#"{"type":"system","subtype":"task_notification","task_id":"b1","tool_use_id":"t1","status":"completed"}"#,
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#,
            r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
            r#"{"type":"assistant","message":{"id":"m3","content":[{"type":"text","text":"The build passed."}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"The build passed."}"#,
        ]
        .map(String::from);
        let (sender, mut events) = unbounded();
        let error = read_claude(lines.into_iter(), &[Block::Text { text: "Build it".into() }], &sender, &claude_ask::Input::closed(), &claude_ask::Tasks::default());
        assert_eq!(error, None);
        let mut seen = Vec::new();
        while let Ok(Some(event)) = events.try_next() {
            match event {
                LiveEvent::Resting(now) => seen.push(format!("resting {now}")),
                LiveEvent::Background(tasks) => seen.push(format!("background {:?}", tasks.iter().map(|task| (task.id.as_str(), task.description.as_str())).collect::<Vec<_>>())),
                _ => {}
            }
        }
        assert_eq!(
            seen,
            [r#"background [("b1", "Build the app")]"#, "resting true", "background []", "resting false", "resting true"],
            "system lines alone do not wake it"
        );
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
        let stop = StopHandle { pid: 0, remote: None, claude: Some((answers, tasks)), processes: false, link: None };
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
        assert!(asks(&claude), "on a host too: prompts may go in while a command runs in the background");
        claude.permission = Permission::AcceptEdits;
        assert!(asks(&claude));
        assert!(command_line(&claude).unwrap().1.iter().any(|arg| arg == "--replay-user-messages"));
    }

    /// A prompt sent while the turn runs is known as taken by its replay, not by coming
    /// second: a turn whose own prompt (a slash command) does not come back still hears it,
    /// and a text line that is no replay hears nothing.
    #[test]
    fn a_prompt_taken_is_known_by_its_replay() {
        let lines = [
            r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
            r#"{"type":"user","message":{"role":"user","content":"This session is being continued from a previous conversation."},"isReplay":false}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#,
            r#"{"type":"user","message":{"role":"user","content":"Later prompt"},"parent_tool_use_id":null,"isReplay":true}"#,
        ]
        .map(String::from);
        let (sender, mut events) = unbounded();
        read_claude(lines.into_iter(), &[Block::Text { text: "/compact".into() }], &sender, &claude_ask::Input::closed(), &claude_ask::Tasks::default());
        let mut heard = 0;
        while let Ok(Some(event)) = events.try_next() {
            heard += matches!(event, LiveEvent::Heard) as usize;
        }
        assert_eq!(heard, 1, "the later prompt only");
    }

    /// As Claude Code 2.1.332 answered a real turn (Haiku, 2026-11-25): past its answer the
    /// input stays open while a command runs in the background, a prompt sent then comes back
    /// as it is taken, and the input closes once nothing runs in the background.
    #[test]
    fn claude_takes_a_prompt_while_a_command_runs_in_the_background() {
        let written = Written::default();
        let answers = claude_ask::Input::new(claude_ask::Sink::Pipe(Box::new(written.clone())));
        let (line, lines) = std::sync::mpsc::channel::<String>();
        let (sender, mut events) = unbounded();
        let reader_answers = answers.clone();
        let reader = thread::spawn(move || read_claude(lines.into_iter(), &[Block::Text { text: "Build it".into() }], &sender, &reader_answers, &claude_ask::Tasks::default()));
        let wait_for = |events: &mut UnboundedReceiver<LiveEvent>, wanted: &dyn Fn(&LiveEvent) -> bool| loop {
            match futures_executor_next(events) {
                Some(event) if wanted(&event) => break event,
                Some(_) => {}
                None => panic!("the reader ended"),
            }
        };
        for text in [
            r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Build it"}]},"parent_tool_use_id":null,"isReplay":true}"#,
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"b1","description":"Build the app"}]}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t1","type":"tool_result","content":"Command running in background"}]},"parent_tool_use_id":null}"#,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"STARTED"}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"STARTED"}"#,
        ] {
            line.send(text.into()).unwrap();
        }
        wait_for(&mut events, &|event| matches!(event, LiveEvent::Resting(true)));
        assert!(answers.send("{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"PONG?\"}}\n"), "the input is open past the answer");
        for text in [
            r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"PONG?"}]},"parent_tool_use_id":null,"isReplay":true}"#,
            r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"PONG"}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"PONG"}"#,
        ] {
            line.send(text.into()).unwrap();
        }
        wait_for(&mut events, &|event| matches!(event, LiveEvent::Heard));
        let shown = loop {
            if let LiveEvent::Update(turn) = wait_for(&mut events, &|event| matches!(event, LiveEvent::Update(_))) {
                if turn.search_text(1000).contains("PONG?") && turn.search_text(1000).contains("PONG\n") || turn.entries.len() >= 4 {
                    break turn;
                }
            }
        };
        let users: Vec<String> = shown.entries.iter().filter(|entry| entry.role == Role::User).map(|entry| format!("{:?}", entry.blocks)).collect();
        assert_eq!(users.iter().filter(|blocks| blocks.contains("Build it")).count(), 1, "the first prompt shows once: {users:?}");
        assert!(users.iter().any(|blocks| blocks.contains("PONG?")), "the prompt sent meanwhile shows where it went in: {users:?}");
        assert!(answers.send("\n"), "still open: the command runs on");
        line.send(r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#.into()).unwrap();
        wait_for(&mut events, &|event| matches!(event, LiveEvent::Background(tasks) if tasks.is_empty()));
        assert!(!answers.send("\n"), "closed once nothing runs in the background");
        drop(line);
        assert_eq!(reader.join().unwrap(), None);
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

    /// A branch is a new session from a line of another, which stays as it is: as Claude
    /// Code 2.1.332 did it with Haiku (a new id, the conversation up to the line only, the
    /// file it came from unchanged).
    #[test]
    fn a_branch_resumes_at_its_line_into_a_new_session() {
        let mut branching = spec(HarnessKind::Claude);
        branching.fork_at = Some("line-before".into());
        branching.drop_turn = Some(("kept-line".into(), "failed-prompt".into()));
        let (_, args) = command_line(&branching).unwrap();
        assert!(args.windows(5).any(|w| w == ["--resume", "abc", "--resume-session-at", "line-before", "--fork-session"]), "{args:?}");
        assert!(!args.iter().any(|arg| arg == "--resume-drops-turn"), "a branch drops nothing of the session it leaves");
        branching.fork_at = Some("-x".into());
        assert!(command_line(&branching).is_err(), "a line that reads as an option");
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
