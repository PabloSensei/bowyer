//! A host over SSH through the `ssh` program of the system, so `~/.ssh/config`, keys and
//! the agent work as they do in a terminal. Connections use `BatchMode`: a host that needs
//! a password typed in shows up as an error, never as a prompt hidden behind the window.
//! A host the user gave a password to signs in without `BatchMode`: `ssh` asks Bowyer for
//! the password (`SSH_ASKPASS`), which reads it from the system's store, and an unknown
//! host key fails instead of being asked about. Host keys the user trusted in the window go
//! to Bowyer's own file, never to `~/.ssh`.
//! Short calls go through a persistent shell session of the host ([`crate::session`]);
//! whole-file reads, uploads, terminals and harness turns get connections of their own.
//! The remote side needs a POSIX shell and GNU `find`; sessions also need `base64`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};

use model::EnvId;

use crate::local::hide_console;
use crate::session::{Sessions, sessions_for};
use crate::{Environment, FileMeta, GitInfo, ReadRange, not_found, parse_head};

/// A `Host` entry of `~/.ssh/config`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshHost {
    pub alias: String,
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
}

/// Concrete hosts of an SSH config, without patterns such as `*`.
pub fn read_ssh_config(text: &str) -> Vec<SshHost> {
    let mut hosts: Vec<SshHost> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = match line.split_once(|c: char| c.is_whitespace() || c == '=') {
            Some((key, value)) => (key.to_ascii_lowercase(), value.trim().trim_start_matches('=').trim()),
            None => continue,
        };
        match key.as_str() {
            "host" => {
                current.clear();
                for alias in value.split_whitespace() {
                    if alias.contains(['*', '?', '!']) {
                        continue;
                    }
                    current.push(hosts.len());
                    hosts.push(SshHost { alias: alias.to_owned(), hostname: None, user: None, port: None });
                }
            }
            "match" => current.clear(),
            "hostname" => current.iter().for_each(|&i| hosts[i].hostname = Some(value.to_owned())),
            "user" => current.iter().for_each(|&i| hosts[i].user = Some(value.to_owned())),
            "port" => {
                let port = value.parse().ok();
                current.iter().for_each(|&i| hosts[i].port = port);
            }
            _ => {}
        }
    }
    hosts
}

/// Quotes a word for a POSIX shell.
pub fn shell_quote(word: &str) -> String {
    if !word.is_empty()
        && word.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c))
    {
        return word.to_owned();
    }
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// A remote path may start with `~/`, which a quoted word would not expand.
fn remote_path(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => format!("\"$HOME\"/{}", shell_quote(rest)),
        None => shell_quote(path),
    }
}

/// The remote script of an interactive start. An empty `program` is the login shell of the
/// user; anything else runs inside a login shell, so the PATH finds harnesses npm installed.
fn interactive_script(program: &str, args: &[&str]) -> String {
    if program.is_empty() {
        return "exec \"${SHELL:-sh}\" -l".to_owned();
    }
    let mut inner = format!("exec {}", shell_quote(program));
    for arg in args {
        inner.push(' ');
        inner.push_str(&shell_quote(arg));
    }
    format!("exec \"${{SHELL:-sh}}\" -lc {}", shell_quote(&inner))
}

/// The program `ssh` asks for a password: Bowyer itself (see [`askpass_answer`]).
static ASKPASS: OnceLock<PathBuf> = OnceLock::new();
/// Bowyer's own file of host keys, which the user trusted in the window.
static KNOWN_HOSTS: OnceLock<PathBuf> = OnceLock::new();

/// The variable that tells Bowyer it was started by `ssh` as an askpass program, and for
/// which host.
pub const ASKPASS_HOST: &str = "BOWYER_ASKPASS_HOST";

/// Sets the askpass program; the running executable when never set.
pub fn set_askpass(program: PathBuf) {
    let _ = ASKPASS.set(program);
}

/// Sets Bowyer's own file of host keys, read next to the user's own.
pub fn set_known_hosts(file: PathBuf) {
    let _ = KNOWN_HOSTS.set(file);
}

fn askpass() -> Option<PathBuf> {
    ASKPASS.get().cloned().or_else(|| std::env::current_exe().ok())
}

/// The answer of the askpass program to `prompt` for `host`: the kept password when `ssh`
/// asks for a password, nothing for any other question. A host key question in particular
/// must never get the password: `ssh` would ask again and again.
pub fn askpass_answer(host: &str, prompt: &str) -> Option<String> {
    let prompt = prompt.to_lowercase();
    // The host key question is "(yes/no/[fingerprint])?" and then "Please type 'yes', 'no' or
    // the fingerprint"; a password prompt names the user and host, which may hold any word.
    let host_key_question = prompt.contains("(yes/no") || prompt.contains("'yes'");
    if !prompt.contains("password") || host_key_question {
        return None;
    }
    crate::secret::load(host)
}

/// Whether `error` says the host's key is not known (rather than changed, which is never
/// offered to trust).
pub fn is_unknown_host_key(error: &str) -> bool {
    error.contains("Host key verification failed") && !error.contains("HAS CHANGED")
}

/// The system's own files of host keys, which an option naming Bowyer's file must keep.
#[cfg(windows)]
const GLOBAL_KNOWN_HOSTS: &str = r"__PROGRAMDATA__\ssh/ssh_known_hosts __PROGRAMDATA__\ssh/ssh_known_hosts2";
#[cfg(not(windows))]
const GLOBAL_KNOWN_HOSTS: &str = "/etc/ssh/ssh_known_hosts /etc/ssh/ssh_known_hosts2";

/// The keys a host shows, read without signing in, for the user to trust.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKeys {
    /// Lines for a `known_hosts` file.
    pub lines: String,
    /// The fingerprints, one line per key: `SHA256:… (ED25519)`.
    pub fingerprints: Vec<String>,
}

#[derive(Clone)]
pub struct SshEnvironment {
    /// The alias of `~/.ssh/config` or `user@host`.
    pub host: String,
    pub ssh: PathBuf,
    /// A private key of this host, kept by the application rather than in `~/.ssh/config`.
    pub identity: Option<PathBuf>,
    /// Whether the host signs in with the password kept in the system's store.
    pub password: bool,
}

impl std::fmt::Debug for SshEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshEnvironment").field("host", &self.host).field("identity", &self.identity).field("password", &self.password).finish()
    }
}

impl SshEnvironment {
    pub fn new(host: &str) -> Self {
        Self { host: host.to_owned(), ssh: PathBuf::from("ssh"), identity: None, password: false }
    }

    pub fn with_identity(mut self, identity: Option<PathBuf>) -> Self {
        self.identity = identity;
        self
    }

    /// A host that signs in with the password kept for it (see [`crate::secret`]).
    pub fn with_password(mut self, password: bool) -> Self {
        self.password = password;
        self
    }

    /// Options every connection of this host takes.
    fn options(&self) -> Vec<String> {
        let mut options = vec!["-o".into(), "ConnectTimeout=8".into()];
        if let Some(identity) = &self.identity {
            options.extend(["-i".into(), identity.to_string_lossy().into_owned(), "-o".into(), "IdentitiesOnly=yes".into()]);
        }
        if let Some(file) = KNOWN_HOSTS.get().filter(|file| file.is_file()) {
            options.extend(["-o".into(), format!("GlobalKnownHostsFile={GLOBAL_KNOWN_HOSTS} \"{}\"", file.display())]);
        }
        if self.password {
            // One try of the password, and no question about an unknown host key: askpass
            // would get it, and only the window may answer it.
            options.extend(
                ["-o", "PreferredAuthentications=keyboard-interactive,password", "-o", "NumberOfPasswordPrompts=1", "-o", "StrictHostKeyChecking=yes"]
                    .map(str::to_owned),
            );
        }
        options
    }

    /// `BatchMode` turns password sign-in off, so a password host goes without it; its
    /// password comes from the askpass program, never from a prompt behind the window.
    fn batch_mode(&self) -> &'static str {
        if self.password { "BatchMode=no" } else { "BatchMode=yes" }
    }

    /// The variables that make `ssh` ask Bowyer for the password of this host.
    pub fn askpass_env(&self) -> Vec<(String, String)> {
        let Some(program) = askpass().filter(|_| self.password) else { return Vec::new() };
        vec![
            ("SSH_ASKPASS".into(), program.to_string_lossy().into_owned()),
            ("SSH_ASKPASS_REQUIRE".into(), "force".into()),
            (ASKPASS_HOST.into(), self.host.clone()),
        ]
    }

    fn base_command(&self) -> Command {
        let mut command = Command::new(&self.ssh);
        command.args(["-o", self.batch_mode(), "-T"]);
        command.args(self.options());
        command.arg(&self.host);
        command.envs(self.askpass_env());
        command
    }

    /// The sessions of this host and key, shared by every copy of the environment.
    fn sessions(&self) -> Arc<Sessions> {
        sessions_for(&format!("{}|{}|{:?}|{}", self.ssh.display(), self.host, self.identity, self.password))
    }

    /// The connection a session runs over: a dead one is noticed within a minute.
    fn session_command(&self) -> Command {
        let mut command = Command::new(&self.ssh);
        command.args(["-o", self.batch_mode(), "-T", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3"]);
        command.args(self.options());
        command.args([self.host.as_str(), "--"]);
        command.envs(self.askpass_env());
        hide_console(&mut command);
        command
    }

    /// Reads the keys of the host without signing in (`ssh-keyscan` at the address and
    /// port `ssh -G` resolves), for the user to compare and trust.
    pub fn scan_host_keys(&self) -> io::Result<HostKeys> {
        let mut resolve = Command::new(&self.ssh);
        resolve.arg("-G").args(self.options()).arg(&self.host);
        hide_console(&mut resolve);
        let config = String::from_utf8_lossy(&resolve.output()?.stdout).into_owned();
        let value = |key: &str| config.lines().find_map(|line| line.strip_prefix(key).map(|rest| rest.trim().to_owned()));
        let hostname = value("hostname ").ok_or_else(|| io::Error::other("ssh -G named no host"))?;
        let port = value("port ").unwrap_or_else(|| "22".into());
        let mut scan = Command::new(self.ssh.with_file_name("ssh-keyscan"));
        scan.args(["-T", "8", "-p", &port, &hostname]);
        hide_console(&mut scan);
        let scanned = scan.output()?;
        let mut lines = String::from_utf8_lossy(&scanned.stdout).lines().filter(|line| !line.starts_with('#') && !line.trim().is_empty()).map(str::to_owned).collect::<Vec<_>>();
        if lines.is_empty() {
            let message = String::from_utf8_lossy(&scanned.stderr).trim().to_owned();
            return Err(io::Error::other(if message.is_empty() { format!("{hostname}:{port}: no keys") } else { message }));
        }
        // A `HostKeyAlias` is the name ssh looks the key up by.
        if let Some(alias) = value("hostkeyalias ").filter(|alias| alias != "none") {
            for line in &mut lines {
                if let Some((_, rest)) = line.split_once(' ') {
                    *line = format!("{alias} {rest}");
                }
            }
        }
        let lines = lines.join("\n") + "\n";
        let mut keygen = Command::new(self.ssh.with_file_name("ssh-keygen"));
        keygen.args(["-l", "-f", "-"]);
        hide_console(&mut keygen);
        let listed = crate::output_with_input(keygen, lines.as_bytes())?;
        // `256 SHA256:… [host]:port (ED25519)` → `SHA256:… (ED25519)`.
        let fingerprints = listed
            .lines()
            .filter_map(|line| {
                let mut words = line.split_whitespace();
                let fingerprint = words.nth(1)?;
                let kind = line.rsplit_once(' ').map(|(_, kind)| kind).unwrap_or_default();
                Some(format!("{fingerprint} {kind}"))
            })
            .collect();
        Ok(HostKeys { lines, fingerprints })
    }

    /// Adds keys the user trusted to Bowyer's own file of host keys.
    pub fn trust_host_keys(keys: &HostKeys) -> io::Result<()> {
        let file = KNOWN_HOSTS.get().ok_or_else(|| io::Error::other("no file of host keys"))?;
        if let Some(dir) = file.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut text = fs::read_to_string(file).unwrap_or_default();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        for line in keys.lines.lines() {
            if !text.lines().any(|known| known == line) {
                text.push_str(line);
                text.push('\n');
            }
        }
        fs::write(file, text)
    }

    /// Closes the idle sessions of this host.
    pub fn close_sessions(&self) {
        self.sessions().close_idle();
    }

    /// The outcome of a script: its output, or an error with what it wrote to stderr.
    fn outcome(&self, success: bool, stdout: Vec<u8>, stderr: &[u8]) -> io::Result<Vec<u8>> {
        if success {
            return Ok(stdout);
        }
        let message = String::from_utf8_lossy(stderr).trim().to_owned();
        if message.contains("No such file") {
            return Err(io::Error::new(io::ErrorKind::NotFound, message));
        }
        Err(io::Error::other(format!("{}: {message}", self.host)))
    }

    /// A program run on the host without a terminal, its standard streams over the
    /// connection: for headless harness turns. The caller sets up the pipes.
    pub fn headless(&self, program: &str, args: &[&str], cwd: Option<&str>) -> Command {
        let mut script = String::new();
        if let Some(cwd) = cwd {
            script.push_str(&format!("cd {} && ", remote_path(cwd)));
        }
        script.push_str(&interactive_script(program, args));
        let mut command = self.base_command();
        command.arg("--").arg(script);
        hide_console(&mut command);
        command
    }

    /// [`SshEnvironment::headless`] that leaves the id of its process in
    /// `~/.cache/bowyer/runs/<run>.pid` before the harness takes its place (`exec` keeps
    /// the id): the commands of the turn can then be found under it and stopped.
    pub fn headless_run(&self, program: &str, args: &[&str], cwd: Option<&str>, run: &str) -> Command {
        let run: String = run.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')).collect();
        let mut script = String::new();
        if let Some(cwd) = cwd {
            script.push_str(&format!("cd {} && ", remote_path(cwd)));
        }
        script.push_str(&format!("d=\"$HOME/.cache/bowyer/runs\"; mkdir -p \"$d\" && echo $$ > \"$d/{run}.pid\" && "));
        script.push_str(&interactive_script(program, args));
        let mut command = self.base_command();
        command.arg("--").arg(script);
        hide_console(&mut command);
        command
    }

    /// A harness turn that outlives the connection: the prompt (standard input) and the
    /// output go to files under `~/.cache/bowyer/runs/<run>` on the host, the harness runs
    /// in a session of its own (`setsid nohup`), and the connection only follows its
    /// output (`tail -F --pid`). If the connection drops, the turn goes on and writes its
    /// history as usual. The exit code and the error output come back at the end.
    pub fn detached(&self, program: &str, args: &[&str], cwd: Option<&str>, run: &str) -> Command {
        self.detached_run(program, args, cwd, run, false)
    }

    /// [`Self::detached`] for a harness that is answered while it runs (Claude Code's
    /// permission questions): its standard input follows the input file, where
    /// [`Self::append_input`] adds lines from later connections. The host itself ends that
    /// input at the first `result` line of the output, so the harness ends after its turn
    /// even when the connection is gone.
    pub fn detached_answered(&self, program: &str, args: &[&str], cwd: Option<&str>, run: &str) -> Command {
        self.detached_run(program, args, cwd, run, true)
    }

    /// Adds `input` to the standard input of a run of [`Self::detached_answered`].
    pub fn append_input(&self, run: &str, input: &[u8]) -> io::Result<()> {
        let script = format!("cat >> \"$HOME/.cache/bowyer/runs/\"{}.in", shell_quote(run));
        let mut command = self.base_command();
        command.arg("--").arg(script);
        hide_console(&mut command);
        crate::output_with_input(command, input).map(|_| ())
    }

    fn detached_run(&self, program: &str, args: &[&str], cwd: Option<&str>, run: &str, answered: bool) -> Command {
        let mut harness = format!("exec {}", shell_quote(program));
        for arg in args {
            harness.push(' ');
            harness.push_str(&shell_quote(arg));
        }
        let login = format!("\"${{SHELL:-sh}}\" -lc {}", shell_quote(&harness));
        let job = if answered {
            // A pipe fed by `tail -f` of the input file; a second `tail` watches the output
            // and ends the feed at the turn's result. Both end with the job (`--pid`). The
            // key order of a line changes between Claude versions, so `"type":"result"` is
            // looked for anywhere in it, with `"total_cost_usd"`, a key only the result line
            // has: a tool's input or output is JSON too and may hold the first one. One
            // `grep` only: the pipeline ends when `tail` sees its reader gone, and a second
            // `grep` between them would keep reading.
            format!(
                "rm -f \"$1.fifo\"; mkfifo \"$1.fifo\" || exit 1; \
                 tail -n +1 -f --pid=$$ \"$1.in\" > \"$1.fifo\" & feed=$!; \
                 (tail -n +1 -F --pid=$$ \"$1.out\" 2>/dev/null | grep -q -E '\"type\": *\"result\".*\"total_cost_usd\"|\"total_cost_usd\".*\"type\": *\"result\"' && kill $feed 2>/dev/null) & \
                 {login} < \"$1.fifo\" > \"$1.out\" 2> \"$1.err\"; code=$?; kill $feed 2>/dev/null; rm -f \"$1.fifo\"; echo $code > \"$1.code\""
            )
        } else {
            format!("{login} < \"$1.in\" > \"$1.out\" 2> \"$1.err\"; echo $? > \"$1.code\"")
        };
        let cd = cwd.map(|cwd| format!("cd {} || exit 1; ", remote_path(cwd))).unwrap_or_default();
        let script = format!(
            "d=\"$HOME/.cache/bowyer/runs\"; mkdir -p \"$d\" || exit 1; find \"$d\" \\( -type f -o -type p \\) -mtime +2 -delete 2>/dev/null; \
             f=\"$d/{run}\"; cat > \"$f.in\" || exit 1; {cd}\
             setsid nohup sh -c {job} sh \"$f\" < /dev/null > /dev/null 2>&1 & \
             echo $! > \"$f.pid\"; tail -n +1 -F --pid=$! \"$f.out\" 2>/dev/null; \
             cat \"$f.err\" >&2 2>/dev/null; exit \"$(cat \"$f.code\" 2>/dev/null || echo 1)\"",
            run = shell_quote(run).trim_matches('\''),
            job = shell_quote(&job),
        );
        let mut command = self.base_command();
        command.arg("--").arg(script);
        hide_console(&mut command);
        command
    }

    /// Stops a detached turn and everything it started on the host.
    pub fn stop_detached(&self, run: &str) -> io::Result<()> {
        let run = shell_quote(run);
        let script = format!(
            "rm -f \"$HOME/.cache/bowyer/runs/\"{run}.fifo; p=$(cat \"$HOME/.cache/bowyer/runs/\"{run}.pid 2>/dev/null) || exit 0; kill -TERM -- \"-$p\" 2>/dev/null || kill -TERM \"$p\" 2>/dev/null; exit 0"
        );
        self.script(&script).map(|_| ())
    }

    /// Checks that the host answers without a password prompt.
    pub fn probe(&self) -> io::Result<String> {
        let text = self.script_text("printf '%s|%s' \"$(uname -sr)\" \"$HOME\"")?;
        Ok(text.trim().to_owned())
    }

    /// Runs a shell script on the host and returns its standard output as bytes: in a
    /// session of the host when one is free, else over a connection of its own.
    pub fn script(&self, script: &str) -> io::Result<Vec<u8>> {
        if let Some(reply) = self.sessions().call(&|| self.session_command(), script, b"") {
            let reply = reply?;
            return self.outcome(reply.code == 0, reply.stdout, &reply.stderr);
        }
        self.script_alone(script)
    }

    /// [`SshEnvironment::script`] over a connection of its own: for answers that may be
    /// large, such as a whole file.
    fn script_alone(&self, script: &str) -> io::Result<Vec<u8>> {
        let mut command = self.base_command();
        command.arg("--").arg(script);
        command.stdin(Stdio::null());
        hide_console(&mut command);
        let output = command.output()?;
        self.outcome(output.status.success(), output.stdout, &output.stderr)
    }

    /// Copies a file of this computer into `~/.cache/bowyer/attachments` on the host and
    /// returns its absolute path there, for harnesses that read attachments from disk.
    pub fn upload(&self, local: &std::path::Path, name: &str) -> io::Result<String> {
        let name: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
            .collect();
        let script = format!(
            "d=\"$HOME/.cache/bowyer/attachments\"; mkdir -p \"$d\" && cat > \"$d\"/{name} && printf '%s' \"$d\"/{name}",
            name = shell_quote(&name)
        );
        let mut command = self.base_command();
        command.arg("--").arg(script);
        command.stdin(Stdio::from(std::fs::File::open(local)?));
        hide_console(&mut command);
        let output = command.output()?;
        if !output.status.success() {
            let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(io::Error::other(format!("{}: {message}", self.host)));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn script_text(&self, script: &str) -> io::Result<String> {
        Ok(String::from_utf8_lossy(&self.script(script)?).into_owned())
    }
}

/// The most standard input a call sends through a session; more goes over a connection
/// of its own.
const SESSION_INPUT: usize = 256 * 1024;

/// Lines of `path<TAB>size<TAB>mtime-seconds<TAB>type` as `find -printf` writes them.
fn parse_listing(text: &str) -> Vec<FileMeta> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.rsplitn(4, '\t');
            let kind = fields.next()?;
            let mtime: f64 = fields.next()?.parse().ok()?;
            let size = fields.next()?.parse().ok()?;
            let path = fields.next()?.to_owned();
            Some(FileMeta { path, size, modified_ms: (mtime * 1000.0) as i64, is_dir: kind == "d" })
        })
        .collect()
}

impl Environment for SshEnvironment {
    fn id(&self) -> EnvId {
        EnvId::ssh(&self.host)
    }

    fn label(&self) -> String {
        self.host.clone()
    }

    fn is_windows(&self) -> bool {
        false
    }

    fn home(&self) -> io::Result<String> {
        Ok(self.script_text("printf %s \"$HOME\"")?.trim().to_owned())
    }

    fn find_files(&self, root: &str, suffix: &str, max_depth: u32) -> io::Result<Vec<FileMeta>> {
        let script = format!(
            "[ -d {root} ] || exit 0; find {root} -maxdepth {max_depth} -type f -name {pattern} -printf '%p\\t%s\\t%T@\\t%y\\n'",
            root = remote_path(root),
            pattern = shell_quote(&format!("*{suffix}")),
        );
        Ok(parse_listing(&self.script_text(&script)?))
    }

    fn read_dir(&self, path: &str) -> io::Result<Vec<FileMeta>> {
        let script = format!(
            "[ -d {p} ] || {{ echo 'No such file' >&2; exit 2; }}; find {p} -mindepth 1 -maxdepth 1 -printf '%p\\t%s\\t%T@\\t%y\\n'",
            p = remote_path(path),
        );
        let mut entries = parse_listing(&self.script_text(&script)?);
        entries.sort_by(|a, b| {
            b.is_dir.cmp(&a.is_dir).then_with(|| a.path.to_lowercase().cmp(&b.path.to_lowercase()))
        });
        Ok(entries)
    }

    fn read(&self, path: &str, range: ReadRange) -> io::Result<Vec<u8>> {
        let p = remote_path(path);
        let script = match range {
            // A whole file may be large: it goes over a connection of its own.
            ReadRange::All => return self.script_alone(&format!("cat {p}")),
            ReadRange::Head(bytes) => format!("head -c {bytes} {p}"),
            ReadRange::Tail(bytes) => format!("tail -c {bytes} {p}"),
            ReadRange::From(offset) => format!("tail -c +{} {p}", offset + 1),
        };
        self.script(&script)
    }

    fn read_many(&self, requests: &[(String, ReadRange)]) -> Vec<io::Result<Vec<u8>>> {
        if requests.len() < 2 {
            return requests.iter().map(|(path, range)| self.read(path, *range)).collect();
        }
        // One script: for each file its byte count on a line, then the bytes; -1 when it fails.
        let mut script = String::from("t=$(mktemp) || exit 1; ");
        for (path, range) in requests {
            let p = remote_path(path);
            let read = match range {
                ReadRange::All => format!("cat {p}"),
                ReadRange::Head(bytes) => format!("head -c {bytes} {p}"),
                ReadRange::Tail(bytes) => format!("tail -c {bytes} {p}"),
            ReadRange::From(offset) => format!("tail -c +{} {p}", offset + 1),
            };
            script.push_str(&format!(
                "if {read} >\"$t\" 2>/dev/null; then wc -c <\"$t\"; cat \"$t\"; else echo -1; fi; "
            ));
        }
        script.push_str("rm -f \"$t\"");
        let whole = requests.iter().any(|(_, range)| matches!(range, ReadRange::All | ReadRange::From(_)));
        let answer = if whole { self.script_alone(&script) } else { self.script(&script) };
        match answer {
            Ok(bytes) => split_batch(&bytes, requests.len()),
            Err(error) => requests.iter().map(|_| Err(io::Error::new(error.kind(), error.to_string()))).collect(),
        }
    }

    fn stat(&self, path: &str) -> io::Result<FileMeta> {
        let script = format!(
            "find {p} -maxdepth 0 -printf '%p\\t%s\\t%T@\\t%y\\n'",
            p = remote_path(path)
        );
        parse_listing(&self.script_text(&script)?).into_iter().next().ok_or_else(|| not_found(path))
    }

    fn local_copy(&self, path: &str, cache: &Path) -> io::Result<PathBuf> {
        fs::create_dir_all(cache)?;
        let name: String = format!("{}-{}", self.host, path)
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '.' { c } else { '_' })
            .collect();
        let target = cache.join(name);
        fs::write(&target, self.read(path, ReadRange::All)?)?;
        // A database in WAL mode keeps recent writes in the journal next to it.
        let wal = PathBuf::from(format!("{}-wal", target.display()));
        match self.read(&format!("{path}-wal"), ReadRange::All) {
            Ok(bytes) if !bytes.is_empty() => fs::write(&wal, bytes)?,
            _ => {
                let _ = fs::remove_file(&wal);
            }
        }
        Ok(target)
    }

    fn run(&self, program: &str, args: &[&str], cwd: Option<&str>) -> io::Result<String> {
        let mut script = String::new();
        if let Some(cwd) = cwd {
            script.push_str(&format!("cd {} && ", remote_path(cwd)));
        }
        script.push_str(&shell_quote(program));
        for arg in args {
            script.push(' ');
            script.push_str(&shell_quote(arg));
        }
        self.script_text(&script)
    }

    fn run_with_input(&self, program: &str, args: &[&str], cwd: Option<&str>, input: &[u8]) -> io::Result<String> {
        let mut script = String::new();
        if let Some(cwd) = cwd {
            script.push_str(&format!("cd {} && ", remote_path(cwd)));
        }
        script.push_str(&shell_quote(program));
        for arg in args {
            script.push(' ');
            script.push_str(&shell_quote(arg));
        }
        if input.len() <= SESSION_INPUT
            && let Some(reply) = self.sessions().call(&|| self.session_command(), &script, input)
        {
            let reply = reply?;
            if reply.code != 0 {
                let message = String::from_utf8_lossy(&reply.stderr).trim().to_owned();
                return Err(io::Error::other(if message.is_empty() { format!("exit status: {}", reply.code) } else { message }));
            }
            return Ok(String::from_utf8_lossy(&reply.stdout).into_owned());
        }
        let mut command = self.base_command();
        command.arg("--").arg(script);
        hide_console(&mut command);
        crate::output_with_input(command, input)
    }

    fn interactive(&self, program: &str, args: &[&str], cwd: Option<&str>) -> Command {
        let mut script = String::new();
        if let Some(cwd) = cwd {
            script.push_str(&format!("cd {} && ", remote_path(cwd)));
        }
        script.push_str(&interactive_script(program, args));
        let mut command = Command::new(&self.ssh);
        command.arg("-t");
        command.args(self.options());
        command.args([&self.host, "--", &script]);
        // A terminal that drops these variables gets the password prompt of ssh itself.
        command.envs(self.askpass_env());
        command
    }

    fn git_info(&self, cwd: &str) -> Option<GitInfo> {
        let script = format!(
            "cd {} 2>/dev/null && git rev-parse --show-toplevel --git-dir 2>/dev/null && cat \"$(git rev-parse --git-dir)/HEAD\"",
            remote_path(cwd)
        );
        let text = self.script_text(&script).ok()?;
        let mut lines = text.lines();
        let root = lines.next()?.to_owned();
        let git_dir = lines.next()?;
        let (branch, detached) = parse_head(lines.next()?)?;
        Some(GitInfo { root, branch, detached, worktree: git_dir.contains("/worktrees/") })
    }
}

/// The answers of a batched read: a decimal length on its own line, then that many bytes.
fn split_batch(bytes: &[u8], count: usize) -> Vec<io::Result<Vec<u8>>> {
    let mut out = Vec::with_capacity(count);
    let mut at = 0;
    while out.len() < count {
        let Some(end) = bytes[at.min(bytes.len())..].iter().position(|b| *b == b'\n').map(|i| at + i) else { break };
        let header = String::from_utf8_lossy(&bytes[at..end]).trim().to_owned();
        at = end + 1;
        match header.parse::<i64>() {
            Ok(length) if length >= 0 && at + length as usize <= bytes.len() => {
                out.push(Ok(bytes[at..at + length as usize].to_vec()));
                at += length as usize;
            }
            _ => out.push(Err(io::Error::new(io::ErrorKind::NotFound, "cannot read the file"))),
        }
    }
    while out.len() < count {
        out.push(Err(io::Error::other("the batch ended early")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batched_reads_split_on_their_lengths() {
        let parts = split_batch(b"3\nabc-1\n0\n5\nx\ny\nz", 4);
        assert_eq!(parts[0].as_ref().unwrap(), b"abc");
        assert!(parts[1].is_err());
        assert_eq!(parts[2].as_ref().unwrap(), b"");
        assert_eq!(parts[3].as_ref().unwrap(), b"x\ny\nz");
    }

    #[test]
    fn config_hosts_skip_patterns() {
        let config = "Host *\n  ServerAliveInterval 30\n\nHost dev-vm vm2\n  HostName 10.0.0.10\n  User dev\n  Port 2222\nHost git\n  HostName=git.example.com\n";
        let hosts = read_ssh_config(config);
        assert_eq!(hosts.len(), 3);
        assert_eq!(hosts[0].alias, "dev-vm");
        assert_eq!(hosts[1].hostname.as_deref(), Some("10.0.0.10"));
        assert_eq!(hosts[1].port, Some(2222));
        assert_eq!(hosts[2].hostname.as_deref(), Some("git.example.com"));
    }

    #[test]
    fn quoting_survives_spaces_and_quotes() {
        assert_eq!(shell_quote("plain/path.jsonl"), "plain/path.jsonl");
        assert_eq!(shell_quote("it's here"), r"'it'\''s here'");
        assert_eq!(remote_path("~/.claude/projects"), "\"$HOME\"/.claude/projects");
    }

    #[test]
    fn interactive_scripts_expand_the_shell_on_the_remote_side() {
        assert_eq!(interactive_script("", &[]), "exec \"${SHELL:-sh}\" -l");
        assert_eq!(
            interactive_script("claude", &["--resume", "a b"]),
            r#"exec "${SHELL:-sh}" -lc 'exec claude --resume '\''a b'\'''"#
        );
        let command = SshEnvironment::new("vm").interactive("", &[], Some("~/work"));
        let args: Vec<String> = command.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args.last().unwrap(), "cd \"$HOME\"/work && exec \"${SHELL:-sh}\" -l");
        let keyed = SshEnvironment::new("vm").with_identity(Some("k".into())).headless("claude", &["-p"], None);
        let args: Vec<String> = keyed.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(args.windows(2).any(|w| w == ["-i", "k"]));
        assert!(args.contains(&"-T".to_owned()));
        assert_eq!(args.last().unwrap(), r#"exec "${SHELL:-sh}" -lc 'exec claude -p'"#);
    }

    fn words(command: &Command) -> Vec<String> {
        command.get_args().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    fn envs(command: &Command) -> Vec<(String, String)> {
        command
            .get_envs()
            .filter_map(|(key, value)| Some((key.to_string_lossy().into_owned(), value?.to_string_lossy().into_owned())))
            .collect()
    }

    #[test]
    fn a_password_host_asks_bowyer_and_never_about_host_keys() {
        let host = SshEnvironment::new("me@vm").with_password(true);
        for command in [host.headless("claude", &[], None), host.session_command(), host.interactive("", &[], None)] {
            let args = words(&command);
            assert!(!args.contains(&"BatchMode=yes".to_owned()), "{args:?}");
            for option in ["StrictHostKeyChecking=yes", "NumberOfPasswordPrompts=1", "PreferredAuthentications=keyboard-interactive,password"] {
                assert!(args.windows(2).any(|w| w == ["-o", option]), "{option} in {args:?}");
            }
            let envs = envs(&command);
            assert!(envs.contains(&("SSH_ASKPASS_REQUIRE".into(), "force".into())), "{envs:?}");
            assert!(envs.contains(&(ASKPASS_HOST.into(), "me@vm".into())), "{envs:?}");
            assert!(envs.iter().any(|(key, value)| key == "SSH_ASKPASS" && !value.is_empty()), "{envs:?}");
        }
        assert!(!format!("{host:?}").contains("askpass"), "the debug form names no secret");
    }

    #[test]
    fn a_key_host_stays_in_batch_mode_without_askpass() {
        let host = SshEnvironment::new("vm");
        let command = host.headless("claude", &[], None);
        assert!(words(&command).windows(2).any(|w| w == ["-o", "BatchMode=yes"]));
        assert!(!words(&command).contains(&"StrictHostKeyChecking=yes".to_owned()), "the user's own policy holds");
        assert!(envs(&command).is_empty());
        assert!(envs(&host.interactive("", &[], None)).is_empty());
    }

    #[test]
    fn askpass_answers_only_a_password_question() {
        let nobody = "nobody-bowyer-test@example.invalid";
        for prompt in [
            "Are you sure you want to continue connecting (yes/no/[fingerprint])? ",
            "Please type 'yes', 'no' or the fingerprint: ",
            "Enter passphrase for key '/home/me/.ssh/id_ed25519': ",
            "Verification code: ",
        ] {
            assert_eq!(askpass_answer(nobody, prompt), None, "{prompt}");
        }
        assert_eq!(askpass_answer(nobody, "(me@vm) Password: "), None, "no password kept, no answer");
    }

    #[cfg(windows)]
    #[test]
    fn askpass_answers_a_password_question_with_the_kept_password() {
        if !crate::secret::usable_in_tests() {
            return;
        }
        let host = format!("hayes-{}@bayes.example.invalid", std::process::id());
        crate::secret::store(&host, "s3cret").unwrap();
        let answers = [
            askpass_answer(&host, "(me@vm) Password: "),
            askpass_answer(&host, "me@vm's password: "),
            // A user or host may hold "yes" or "fingerprint" in its name.
            askpass_answer(&host, "(hayes@fingerprint-yes.example) Password: "),
        ];
        let refused = [
            askpass_answer(&host, "Please type 'yes', 'no' or the fingerprint: "),
            askpass_answer(&host, "Are you sure you want to continue connecting (yes/no/[fingerprint])? password"),
        ];
        crate::secret::remove(&host).unwrap();
        assert_eq!(answers, [Some("s3cret".into()), Some("s3cret".into()), Some("s3cret".into())]);
        assert_eq!(refused, [None, None]);
    }

    #[test]
    fn an_unknown_host_key_is_told_from_a_changed_one() {
        assert!(is_unknown_host_key("No ED25519 host key is known for [127.0.0.1]:2222 and you have requested strict checking.\r\nHost key verification failed."));
        assert!(!is_unknown_host_key("@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @\nHost key verification failed."));
        assert!(!is_unknown_host_key("Permission denied (password)."));
    }

    #[test]
    fn a_detached_turn_keeps_its_prompt_and_output_on_the_host() {
        let command = SshEnvironment::new("vm").detached("claude", &["-p", "it's"], Some("~/work"), "17-42");
        let script = command.get_args().last().unwrap().to_string_lossy().into_owned();
        assert!(script.contains(r#"f="$d/17-42"; cat > "$f.in""#), "{script}");
        assert!(script.contains(r#"cd "$HOME"/work || exit 1; setsid nohup sh -c"#), "{script}");
        assert!(script.contains("tail -n +1 -F --pid=$!"), "{script}");
        assert!(script.contains(r#"exec claude -p '\''it'\''\'\'''\''s'\''"#) || script.contains("exec claude -p"), "{script}");
        assert!(script.ends_with(r#"exit "$(cat "$f.code" 2>/dev/null || echo 1)""#), "{script}");
    }

    fn real_host() -> SshEnvironment {
        let host = std::env::var("BOWYER_SSH_HOST").expect("BOWYER_SSH_HOST");
        let key = std::env::var_os("BOWYER_SSH_KEY").map(PathBuf::from);
        SshEnvironment::new(&host).with_identity(key)
    }

    /// Calls through the session of a real host answer as a connection of their own
    /// would: exact bytes, errors, a script that reads its input does not eat the next
    /// request. `BOWYER_SSH_HOST=user@host BOWYER_SSH_KEY=key cargo test -p envs --
    /// --ignored session`.
    #[test]
    #[ignore = "needs an SSH host"]
    fn a_session_answers_like_a_connection_of_its_own() {
        let ssh = real_host();
        let dir = format!("/tmp/bowyer-session-{}", std::process::id());
        let all: Vec<u8> = (0..=255u8).collect();
        ssh.run_with_input("sh", &["-c", &format!("mkdir -p {dir} && cat > {dir}/bytes && : > {dir}/empty && printf 'no newline' > {dir}/plain")], None, &all).unwrap();
        assert_eq!(ssh.read(&format!("{dir}/bytes"), ReadRange::Head(1000)).unwrap(), all, "every byte value");
        assert_eq!(ssh.read(&format!("{dir}/bytes"), ReadRange::Tail(3)).unwrap(), [253, 254, 255]);
        assert_eq!(ssh.read(&format!("{dir}/empty"), ReadRange::Head(10)).unwrap(), b"");
        assert_eq!(ssh.read(&format!("{dir}/plain"), ReadRange::Tail(100)).unwrap(), b"no newline");
        assert_eq!(ssh.read(&format!("{dir}/bytes"), ReadRange::All).unwrap(), all, "a whole file over its own connection");
        assert_eq!(ssh.stat(&format!("{dir}/missing")).unwrap_err().kind(), io::ErrorKind::NotFound);
        assert_eq!(ssh.read(&format!("{dir}/missing"), ReadRange::Head(10)).unwrap_err().kind(), io::ErrorKind::NotFound);
        // A script that reads its input gets its own, empty or given, and the session goes on.
        assert_eq!(ssh.script("cat; echo after").unwrap(), b"after\n");
        assert_eq!(ssh.run_with_input("cat", &[], None, b"given").unwrap(), "given");
        assert!(ssh.script("echo out; echo err >&2; exit 3").unwrap_err().to_string().contains("err"));
        assert_eq!(ssh.home().unwrap(), ssh.script("printf %s \"$HOME\"").map(|b| String::from_utf8(b).unwrap()).unwrap());
        let _ = ssh.run("rm", &["-rf", &dir], None);
    }

    /// Many threads share the two sessions and the connections beside them; each gets its
    /// own answer. A session whose connection is cut is replaced at the next call.
    #[test]
    #[ignore = "needs an SSH host"]
    fn sessions_serve_many_threads_and_outlive_a_cut() {
        let ssh = real_host();
        let threads: Vec<_> = (0..6)
            .map(|n| {
                let ssh = ssh.clone();
                std::thread::spawn(move || {
                    for k in 0..10 {
                        let out = ssh.script(&format!("printf '%s-%s' {n} {k}")).unwrap();
                        assert_eq!(out, format!("{n}-{k}").into_bytes());
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        // Cut the idle sessions' connections the hard way.
        for pid in ssh.sessions().idle_pids() {
            let _ = std::process::Command::new(if cfg!(windows) { "taskkill" } else { "kill" })
                .args(if cfg!(windows) { vec!["/F".to_owned(), "/PID".to_owned(), pid.to_string()] } else { vec!["-9".to_owned(), pid.to_string()] })
                .output();
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(ssh.script("printf ok").unwrap(), b"ok", "a cut session is replaced");

        // And a call through a session takes a round trip, not a connection.
        ssh.script("true").unwrap();
        let started = std::time::Instant::now();
        for _ in 0..10 {
            ssh.script("true").unwrap();
        }
        let each = started.elapsed() / 10;
        assert!(each < std::time::Duration::from_millis(150), "{each:?} per call");
        ssh.close_sessions();
    }

    /// The command of a terminal on a real host keeps its quotes: a folder with a space,
    /// a quoted argument, a second line. `BOWYER_SSH_HOST=user@host BOWYER_SSH_KEY=key
    /// cargo test -p envs -- --ignored terminal`.
    #[test]
    #[ignore = "needs an SSH host"]
    fn a_terminal_command_keeps_its_quotes_on_a_real_host() {
        let host = std::env::var("BOWYER_SSH_HOST").expect("BOWYER_SSH_HOST");
        let key = std::env::var_os("BOWYER_SSH_KEY").map(PathBuf::from);
        let ssh = SshEnvironment::new(&host).with_identity(key);
        let dir = format!("/tmp/bowyer term {}", std::process::id());
        ssh.script_text(&format!("mkdir -p {}", shell_quote(&dir))).unwrap();
        let script = "echo \"a  b\" 'it'\\''s'\npwd";
        let mut command = ssh.interactive("sh", &["-c", script], Some(&dir));
        let output = command.stdin(Stdio::null()).output().unwrap();
        let _ = ssh.script_text(&format!("rmdir {}", shell_quote(&dir)));
        let text = String::from_utf8_lossy(&output.stdout).replace('\r', "");
        assert_eq!(text, format!("a  b it's\n{dir}\n"));
    }

    /// A detached turn on a real host: the output follows, a dropped connection does not
    /// end it. `BOWYER_SSH_HOST=user@host BOWYER_SSH_KEY=key cargo test -p envs -- --ignored detached`.
    #[test]
    #[ignore = "needs an SSH host"]
    fn a_detached_turn_outlives_its_connection() {
        let host = std::env::var("BOWYER_SSH_HOST").expect("BOWYER_SSH_HOST");
        let key = std::env::var_os("BOWYER_SSH_KEY").map(PathBuf::from);
        let ssh = SshEnvironment::new(&host).with_identity(key);
        // The "harness" counts slowly; the first connection is cut after a second.
        let run = format!("test-{}", std::process::id());
        let mut first = ssh.detached("sh", &["-c", "for i in 1 2 3 4; do echo $i; sleep 1; done"], None, &run);
        first.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = first.spawn().unwrap();
        drop(child.stdin.take());
        std::thread::sleep(std::time::Duration::from_millis(1500));
        child.kill().unwrap();
        let _ = child.wait();
        std::thread::sleep(std::time::Duration::from_secs(5));
        let out = ssh.script_text(&format!("cat \"$HOME/.cache/bowyer/runs/{run}.out\" \"$HOME/.cache/bowyer/runs/{run}.code\"")).unwrap();
        assert_eq!(out.split_whitespace().collect::<Vec<_>>(), ["1", "2", "3", "4", "0"], "it ran to the end");

        // A stop ends the whole group on the host.
        let run = format!("stop-{}", std::process::id());
        let mut second = ssh.detached("sh", &["-c", "sleep 30; echo late"], None, &run);
        second.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
        let mut child = second.spawn().unwrap();
        drop(child.stdin.take());
        std::thread::sleep(std::time::Duration::from_secs(2));
        ssh.stop_detached(&run).unwrap();
        let status = child.wait().unwrap();
        assert!(!status.success(), "the follower ends with the turn");
        let late = ssh.script_text(&format!("cat \"$HOME/.cache/bowyer/runs/{run}.out\"")).unwrap();
        assert!(!late.contains("late"));
        let _ = ssh.script_text(&format!("rm -f \"$HOME/.cache/bowyer/runs/\"test-* \"$HOME/.cache/bowyer/runs/\"stop-*"));
    }

    #[test]
    fn an_answered_turn_reads_its_input_file_until_the_result() {
        let command = SshEnvironment::new("vm").detached_answered("claude", &["-p"], None, "17-43");
        let script = command.get_args().last().unwrap().to_string_lossy().into_owned();
        assert!(script.contains("mkfifo"), "{script}");
        assert!(script.contains(r#"grep -q -E '\''"type": *"result".*"total_cost_usd"|"total_cost_usd".*"type": *"result"'\''"#), "{script}");
        assert!(script.contains(r#"cat > "$f.in""#), "{script}");
    }

    /// An answer added after the start reaches the harness, and the host ends its input at
    /// the result even with the connection gone. Puts a stub harness into the host's run
    /// folder; `BOWYER_SSH_HOST=user@host BOWYER_SSH_KEY=key cargo test -p envs -- --ignored answered`.
    #[test]
    #[ignore = "needs an SSH host with python3"]
    fn an_answered_turn_takes_appended_input_and_ends_without_its_connection() {
        use std::io::{BufRead, Write};
        let host = std::env::var("BOWYER_SSH_HOST").expect("BOWYER_SSH_HOST");
        let key = std::env::var_os("BOWYER_SSH_KEY").map(PathBuf::from);
        let ssh = SshEnvironment::new(&host).with_identity(key);
        let run = format!("answered-{}", std::process::id());
        // A tool's JSON with `"type": "result"` in it comes first: the input stays open.
        let stub = "import json, sys\n\
            print(json.dumps({'type': 'assistant', 'input': {'type': 'result'}}), flush=True)\n\
            print(json.dumps({'type': 'ask'}), flush=True)\n\
            prompt = sys.stdin.readline().strip()\n\
            answer = sys.stdin.readline().strip()\n\
            print(json.dumps({'type': 'result', 'prompt': prompt, 'answer': answer, 'total_cost_usd': 0}), flush=True)\n\
            print(json.dumps({'type': 'eof', 'rest': sys.stdin.read()}), flush=True)\n";
        let mut command = ssh.detached_answered("python3", &["-u", "-c", stub], None, &run);
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = command.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"the prompt\n").unwrap();
        drop(stdin);
        let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
        assert!(lines.nth(1).unwrap().unwrap().contains("ask"));
        // Time for the watcher to see the tool's line, were it to close the input.
        std::thread::sleep(std::time::Duration::from_millis(1500));
        ssh.append_input(&run, b"yes\n").unwrap();
        // The connection goes before the harness answers.
        child.kill().unwrap();
        let _ = child.wait();
        std::thread::sleep(std::time::Duration::from_secs(4));
        let out = ssh.script_text(&format!("cat \"$HOME/.cache/bowyer/runs/{run}.out\"; echo; cat \"$HOME/.cache/bowyer/runs/{run}.code\"")).unwrap();
        let _ = ssh.script_text(&format!("rm -f \"$HOME/.cache/bowyer/runs/{run}\".*"));
        assert!(out.contains(r#""prompt": "the prompt", "answer": "yes""#), "{out}");
        assert!(out.contains(r#"{"type": "eof", "rest": ""}"#), "its input ended at the result: {out}");
        assert_eq!(out.lines().last(), Some("0"), "{out}");
    }

    #[test]
    fn listings_parse_tabs_in_order() {
        let listing = "/home/u/a b.jsonl\t120\t1700000000.5\tf\n/home/u/dir\t4096\t1700000001.0\td\nbroken line\n";
        let files = parse_listing(listing);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "/home/u/a b.jsonl");
        assert_eq!(files[0].modified_ms, 1_700_000_000_500);
        assert!(files[1].is_dir);
    }
}
