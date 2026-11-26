//! A host over SSH through the `ssh` program of the system, so `~/.ssh/config`, keys and
//! the agent work as they do in a terminal. Connections use `BatchMode`: a host that needs
//! a password typed in shows up as an error, never as a prompt hidden behind the window.
//! A host the user gave a password to signs in without `BatchMode`: `ssh` asks Bowyer for
//! the password (`SSH_ASKPASS`), which reads it from the system's store, and an unknown
//! host key fails instead of being asked about. The same secret answers the passphrase of a
//! key, for a host whose server takes keys only. Host keys the user trusted in the window go
//! to Bowyer's own file, never to `~/.ssh`.
//! Short calls go through a persistent shell session of the host ([`crate::session`]);
//! whole-file reads, uploads, terminals and harness turns get connections of their own.
//! The remote side needs a POSIX shell and GNU `find`; sessions also need `base64`.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::net::{IpAddr, ToSocketAddrs};
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

/// The answer of the askpass program to `prompt` for `host`: the kept secret when `ssh`
/// asks for a password, or for the passphrase of the key it signs in with (a host whose
/// server takes keys only, with a key that has a passphrase), nothing for any other
/// question. The passphrase opens the key on this computer and never reaches the host. A
/// host key question in particular must never get the secret: `ssh` would ask again and
/// again.
pub fn askpass_answer(host: &str, prompt: &str) -> Option<String> {
    let prompt = prompt.to_lowercase();
    // The host key question is "(yes/no/[fingerprint])?" and then "Please type 'yes', 'no' or
    // the fingerprint"; a password prompt names the user and host, which may hold any word.
    let host_key_question = prompt.contains("(yes/no") || prompt.contains("'yes'");
    let asks_secret = prompt.contains("password") || prompt.trim_start().starts_with("enter passphrase for");
    if !asks_secret || host_key_question {
        return None;
    }
    if let Some((target, jump)) = resolved(host) {
        // Through a jump host (`ProxyJump`) the jump host asks for its own password first:
        // the kept password is the target's, and goes to the target only.
        if let Some(asker) = prompt_machine(&prompt)
            && !answers_to(asker, &target, jump)
        {
            return None;
        }
        // A name of the local network that also leads to the internet may have reached a
        // stranger's machine: the secret does not go there.
        if !jump && !stray_addresses(&target, 22).is_empty() {
            return None;
        }
    }
    crate::secret::load(host)
}

/// Endings of names that only a local network gives out: the Hyper-V Default Switch
/// (`mshome.net`), mDNS (`local`), RFC 8375 (`home.arpa`) and home routers.
const LOCAL_SUFFIXES: [&str; 7] = ["mshome.net", "local", "lan", "home", "internal", "home.arpa", "localdomain"];

/// Words of the error [`SshEnvironment::misdirected`] gives, which
/// [`misdirected_name`] finds it by.
const MISDIRECTED: &str = " is a name of the local network, yet it resolves on the internet to ";

/// Whether `name` is one that only a local network gives out.
fn is_local_name(name: &str) -> bool {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    LOCAL_SUFFIXES.iter().any(|suffix| name.strip_suffix(suffix).is_some_and(|rest| rest.ends_with('.') && rest.len() > 1))
}

/// Whether `address` is on the internet: not a private, shared (CGNAT, Tailscale),
/// loopback or link-local one.
fn is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            let shared = a == 100 && (64..128).contains(&b);
            !(v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() || v4.is_broadcast() || shared)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            let unique_local = first & 0xfe00 == 0xfc00;
            let link_local = first & 0xffc0 == 0xfe80;
            !(v6.is_loopback() || v6.is_unspecified() || unique_local || link_local)
        }
    }
}

/// The addresses on the internet that `name`, a name of the local network, resolves to: a
/// stray entry of the local DNS (Windows half-rewrites `hosts.ics` of the Hyper-V Default
/// Switch, leaving the tail of an old line) sends ssh to a stranger's machine. None for any
/// other name, or for an address.
fn stray_addresses(name: &str, port: u16) -> Vec<IpAddr> {
    if !is_local_name(name) {
        return Vec::new();
    }
    let Ok(found) = (name, port).to_socket_addrs() else { return Vec::new() };
    let mut addresses: Vec<IpAddr> = found.map(|address| address.ip()).filter(|ip| is_public(*ip)).collect();
    addresses.sort_unstable();
    addresses.dedup();
    addresses
}

fn misdirected_message(name: &str, addresses: &[IpAddr]) -> String {
    let addresses = addresses.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
    format!("{name}{MISDIRECTED}{addresses}; Bowyer does not connect there.")
}

/// The name and the addresses on the internet when `error` is the one of
/// [`SshEnvironment::misdirected`].
pub fn misdirected_name(error: &str) -> Option<(String, String)> {
    error.lines().find_map(|line| {
        let (before, after) = line.split_once(MISDIRECTED)?;
        let name = before.rsplit(' ').next()?.to_owned();
        let (addresses, _) = after.split_once(';')?;
        Some((name, addresses.to_owned()))
    })
}

/// The machine a password prompt names: `user@machine's password:` or `(user@machine)
/// Password:`.
fn prompt_machine(prompt: &str) -> Option<&str> {
    let prompt = prompt.trim_start();
    let named = match prompt.strip_prefix('(') {
        Some(rest) => &rest[..rest.find(')')?],
        None => &prompt[..prompt.find("'s password")?],
    };
    named.rsplit_once('@').map(|(_, machine)| machine.trim()).filter(|machine| !machine.is_empty())
}

/// The machine `ssh` reaches for `host`, and whether it goes through a jump host there.
fn resolved(host: &str) -> Option<(String, bool)> {
    SshEnvironment::new(host).destination().map(|(name, _, jump)| (name, jump))
}

/// Whether a prompt from `asker` gets the password of `target`: always without a jump host,
/// and through one only when the target itself asks.
fn answers_to(asker: &str, target: &str, jump: bool) -> bool {
    !jump || asker.eq_ignore_ascii_case(target)
}

/// Whether the private key at `key` has a passphrase: `ssh-keygen` (next to `ssh`) cannot
/// open it with an empty one. Such a key signs in only when someone answers its passphrase,
/// which `BatchMode` never asks. Nothing is written; a key that cannot be read at all is
/// not called locked.
pub fn key_has_passphrase(ssh: &Path, key: &Path) -> bool {
    let mut keygen = Command::new(ssh.with_file_name("ssh-keygen"));
    keygen.args(["-y", "-P", "", "-f"]).arg(key).stdin(Stdio::null());
    hide_console(&mut keygen);
    match keygen.output() {
        Ok(output) => !output.status.success() && String::from_utf8_lossy(&output.stderr).to_lowercase().contains("passphrase"),
        Err(_) => false,
    }
}

/// Whether `error` says the host's key is not known (rather than changed, which is never
/// offered to trust).
pub fn is_unknown_host_key(error: &str) -> bool {
    error.contains("Host key verification failed") && !error.contains("HAS CHANGED")
}

/// The ways of signing in the host still offered when it let the user in by none:
/// `Permission denied (publickey,password).` → `["publickey", "password"]`.
pub fn refused_sign_in(error: &str) -> Option<Vec<String>> {
    let (_, rest) = error.rsplit_once("Permission denied (")?;
    let (methods, _) = rest.split_once(')')?;
    Some(methods.split(',').map(|method| method.trim().to_owned()).filter(|method| !method.is_empty()).collect())
}

/// Whether `error` says the host takes no password at all, whatever was typed.
pub fn host_takes_no_password(error: &str) -> bool {
    refused_sign_in(error).is_some_and(|methods| !methods.iter().any(|method| method == "password" || method == "keyboard-interactive"))
}

/// The files of host keys that `error` names as holding a key the host no longer shows:
/// `Offending ED25519 key in <file>:<line>` after `REMOTE HOST IDENTIFICATION HAS CHANGED`.
pub fn changed_host_key_files(error: &str) -> Vec<String> {
    if !error.contains("HAS CHANGED") {
        return Vec::new();
    }
    error
        .lines()
        .filter_map(|line| {
            // `Offending ED25519 key in …` or `Offending key for IP in …`.
            let (_, file) = line.trim().strip_prefix("Offending ")?.split_once(" in ")?;
            let file = file.trim();
            let file = match file.rsplit_once(':') {
                Some((path, number)) if !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()) => path,
                _ => file,
            };
            Some(file.to_owned())
        })
        .collect()
}

/// Whether the key that changed is only in Bowyer's own file of host keys, the one file the
/// window may replace it in; one in the user's own files stays theirs to remove.
pub fn changed_key_is_own(error: &str) -> bool {
    KNOWN_HOSTS.get().is_some_and(|own| changed_key_is_in(error, own))
}

fn changed_key_is_in(error: &str, own: &Path) -> bool {
    // ssh prints the path with either slash, and Windows OpenSSH doubles its backslashes
    // (`C:\\Users\\…`); Windows paths are compared without case.
    let normal = |path: &str| {
        let mut slashed = String::new();
        for c in path.trim_matches('"').chars() {
            let c = if c == '\\' { '/' } else { c };
            if !(c == '/' && slashed.ends_with('/')) {
                slashed.push(c);
            }
        }
        if cfg!(windows) { slashed.to_lowercase() } else { slashed }
    };
    let own = normal(&own.to_string_lossy());
    let files = changed_host_key_files(error);
    !files.is_empty() && files.iter().all(|file| normal(file) == own)
}

/// The key lines of `known_hosts` text, without comments and empty lines.
fn known_host_lines(text: &str) -> Vec<String> {
    text.lines().map(str::trim).filter(|line| !line.starts_with('#') && !line.is_empty()).map(str::to_owned).collect()
}

/// `known_hosts` text with `new` lines in place of the lines kept for the same host names.
/// Comments and marker lines (`@cert-authority`, `@revoked`) stay.
fn merged_known_hosts(text: &str, new: &str) -> String {
    let names: HashSet<&str> = new.lines().filter_map(|line| line.split_whitespace().next()).flat_map(|names| names.split(',')).collect();
    let mut merged = String::new();
    for line in text.lines() {
        let first = line.split_whitespace().next().unwrap_or_default();
        let replaced = !line.starts_with(['#', '@']) && first.split(',').any(|name| names.contains(name));
        if !replaced {
            merged.push_str(line);
            merged.push('\n');
        }
    }
    for line in new.lines().filter(|line| !line.trim().is_empty()) {
        merged.push_str(line);
        merged.push('\n');
    }
    merged
}

/// What a [`SshEnvironment::tunnel`] prints on the host once it holds the port.
pub const TUNNEL_READY: &str = "bowyer-tunnel-ready";
/// What a detached run's connection writes to its error output once the run is going on
/// the host, so a connection that drops after it is followed again ([`SshEnvironment::follow_detached`]).
pub const RUN_STARTED: &str = "bowyer-run-started";
/// What the connection writes to its error output last, when the run has ended on the host:
/// without it, the connection dropped while the run went on.
pub const RUN_END: &str = "bowyer-run-end";

/// The shell loop that watches the output of an answered run and ends (and with it the
/// run's input) at a result while nothing runs in the background, or when the last
/// background command ends after a result. Lines of tools hold their JSON escaped, so a
/// `"subtype"` there never matches; a result line is the one with `"total_cost_usd"`.
/// Not `awk`: `mawk` (Debian's and Ubuntu's) fills its buffer before it reads a line, and
/// run interactive it cuts lines longer than its buffer, so a result after a long line
/// (Claude's partial messages reach 12 kB) went unseen and the turn never ended.
const RESULT_WATCH: &str = r#"while IFS= read -r l; do case $l in *'"subtype":"background_tasks_changed"'*|*'"subtype": "background_tasks_changed"'*) case $l in *'"tasks":[]'*|*'"tasks": []'*) busy= ;; *) busy=1 ;; esac ;; *'"total_cost_usd"'*) case $l in *'"type":"result"'*|*'"type": "result"'*) done=1 ;; esac ;; esac; [ -n "$done" ] && [ -z "$busy" ] && break; done"#;

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
            // would get it, and only the window may answer it. Keys come after the password,
            // for a server that takes keys only: the kept secret then opens a key that has a
            // passphrase, one try of it too.
            options.extend(
                [
                    "-o",
                    "PreferredAuthentications=keyboard-interactive,password,publickey",
                    "-o",
                    "NumberOfPasswordPrompts=1",
                    "-o",
                    "StrictHostKeyChecking=yes",
                ]
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

    /// [`Self::base_command`] for a connection that lasts: a dead one is noticed within a
    /// minute, not after the TCP timeout of hours.
    fn lasting_command(&self) -> Command {
        let mut command = Command::new(&self.ssh);
        command.args(["-o", self.batch_mode(), "-T", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3"]);
        command.args(self.options());
        command.arg(&self.host);
        command.envs(self.askpass_env());
        command
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

    /// The configuration `ssh -G` resolves for this host.
    fn config(&self) -> io::Result<String> {
        let mut resolve = Command::new(&self.ssh);
        resolve.arg("-G").args(self.options()).arg(&self.host).stdin(Stdio::null());
        hide_console(&mut resolve);
        Ok(String::from_utf8_lossy(&resolve.output()?.stdout).into_owned())
    }

    /// The machine `ssh` reaches for this host, its port, and whether it goes there through
    /// a jump host or a proxy command, which resolve the name themselves.
    fn destination(&self) -> Option<(String, u16, bool)> {
        let config = self.config().ok()?;
        let value = |key: &str| config.lines().find_map(|line| line.strip_prefix(key).map(|rest| rest.trim().to_owned()));
        let set = |key: &str| value(key).is_some_and(|value| !value.eq_ignore_ascii_case("none"));
        let port = value("port ").and_then(|port| port.parse().ok()).unwrap_or(22);
        Some((value("hostname ")?, port, set("proxyjump ") || set("proxycommand ")))
    }

    /// An error when the machine `ssh` reaches for this host has a name of the local
    /// network (`dev-vm.mshome.net`) that resolves to an address on the internet: a stray
    /// entry of the local DNS, which leads to a stranger's machine. Nothing is sent there.
    pub fn misdirected(&self) -> Option<io::Error> {
        let (name, port, through) = self.destination()?;
        if through {
            return None;
        }
        let stray = stray_addresses(&name, port);
        (!stray.is_empty()).then(|| io::Error::other(misdirected_message(&name, &stray)))
    }

    /// Reads the keys of the host without signing in (`ssh-keyscan` at the address and
    /// port `ssh -G` resolves), for the user to compare and trust.
    pub fn scan_host_keys(&self) -> io::Result<HostKeys> {
        if let Some(error) = self.misdirected() {
            return Err(error);
        }
        let config = self.config()?;
        let value = |key: &str| config.lines().find_map(|line| line.strip_prefix(key).map(|rest| rest.trim().to_owned()));
        let hostname = value("hostname ").ok_or_else(|| io::Error::other("ssh -G named no host"))?;
        let port = value("port ").unwrap_or_else(|| "22".into());
        let mut scan = Command::new(self.ssh.with_file_name("ssh-keyscan"));
        scan.args(["-T", "8", "-p", &port, &hostname]);
        hide_console(&mut scan);
        let scanned = scan.output()?;
        let mut lines = known_host_lines(&String::from_utf8_lossy(&scanned.stdout));
        if lines.is_empty() {
            // ssh-keyscan offers every key exchange it knows by name, built in or not: the one
            // of Windows (OpenSSH 9.5) offers sntrup761 without having it and fails on a server
            // that picks it (OpenSSH 10). ssh itself offers only what it has.
            match self.host_key_lines_by_ssh() {
                Ok(found) => lines = found,
                Err(by_ssh) => {
                    let message = String::from_utf8_lossy(&scanned.stderr).trim().to_owned();
                    let message = if message.is_empty() { format!("{hostname}:{port}: no keys") } else { message };
                    return Err(io::Error::other(format!("{message}\n{by_ssh}")));
                }
            }
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

    /// The key the host shows to `ssh` itself, read without signing in: ssh connects with
    /// an empty `known_hosts` of its own, keeps the key it is shown there, and stops before
    /// any way of signing in. The user's files are not touched.
    fn host_key_lines_by_ssh(&self) -> io::Result<Vec<String>> {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_nanos());
        let file = std::env::temp_dir().join(format!("bowyer-host-key-{}-{stamp}", std::process::id()));
        let path = file.to_string_lossy().replace('\\', "/");
        let mut ssh = Command::new(&self.ssh);
        // The first value of an option wins: these come before the host's own.
        for option in [
            "BatchMode=yes".to_owned(),
            "StrictHostKeyChecking=accept-new".to_owned(),
            format!("UserKnownHostsFile=\"{path}\""),
            "GlobalKnownHostsFile=none".to_owned(),
            "HashKnownHosts=no".to_owned(),
            "UpdateHostKeys=no".to_owned(),
            "PubkeyAuthentication=no".to_owned(),
            "PasswordAuthentication=no".to_owned(),
            "KbdInteractiveAuthentication=no".to_owned(),
            "GSSAPIAuthentication=no".to_owned(),
            "HostbasedAuthentication=no".to_owned(),
            "LogLevel=ERROR".to_owned(),
        ] {
            ssh.arg("-o").arg(option);
        }
        ssh.args(self.options()).arg(&self.host).arg("exit").stdin(Stdio::null());
        hide_console(&mut ssh);
        let output = ssh.output()?;
        let text = fs::read_to_string(&file).unwrap_or_default();
        let _ = fs::remove_file(&file);
        let lines = known_host_lines(&text);
        if lines.is_empty() {
            let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(io::Error::other(if message.is_empty() { "ssh read no host key".to_owned() } else { message }));
        }
        Ok(lines)
    }

    /// Puts keys the user trusted in Bowyer's own file of host keys, in place of the keys
    /// kept there for the same host before (a host whose key changed).
    pub fn trust_host_keys(keys: &HostKeys) -> io::Result<()> {
        let file = KNOWN_HOSTS.get().ok_or_else(|| io::Error::other("no file of host keys"))?;
        if let Some(dir) = file.parent() {
            fs::create_dir_all(dir)?;
        }
        let text = fs::read_to_string(file).unwrap_or_default();
        fs::write(file, merged_known_hosts(&text, &keys.lines))
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
        // A changed key or a refused sign-in may come from a stranger's machine that a stray
        // entry of the local DNS leads to: that is what to tell, not how to forget the key.
        let stranger = message.contains("HAS CHANGED") || message.contains("Host key verification failed") || message.contains("Permission denied (");
        if stranger && let Some(misdirected) = self.misdirected() {
            return Err(io::Error::other(format!("{}: {misdirected}\n{message}", self.host)));
        }
        Err(io::Error::other(format!("{}: {message}", self.host)))
    }

    /// A connection that brings `127.0.0.1:local` of this computer to `127.0.0.1:remote` of
    /// the host (`ssh -R`), for as long as its standard input stays open: the host reads it
    /// until it ends, so when Bowyer goes the tunnel goes with it and frees the port. Once the
    /// port is taken on the host it prints [`TUNNEL_READY`]; a port already taken there ends
    /// the connection (`ExitOnForwardFailure`). The caller sets up the pipes.
    pub fn tunnel(&self, remote: u16, local: u16) -> Command {
        let mut command = Command::new(&self.ssh);
        command.args(["-o", self.batch_mode(), "-T", "-o", "ExitOnForwardFailure=yes", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3"]);
        command.args(self.options());
        command.args(["-R", &format!("127.0.0.1:{remote}:127.0.0.1:{local}")]);
        command.args([self.host.as_str(), "--", &format!("echo {TUNNEL_READY}; cat >/dev/null")]);
        command.envs(self.askpass_env());
        hide_console(&mut command);
        command
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
        script.push_str(&format!("d=\"$HOME/.cache/bowyer/runs\"; mkdir -p \"$d\" && chmod 700 \"$d\" && echo $$ > \"$d/{run}.pid\" && "));
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
    /// permission questions, and prompts sent while a command of it runs in the
    /// background): its standard input follows the input file, where [`Self::append_input`]
    /// adds lines from later connections. The host itself ends that input at a `result`
    /// line of the output when nothing runs in the background, or later when the last such
    /// command ends, so the harness ends after its turn even when the connection is gone.
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
            // and ends the feed at the turn's result, or, while something runs in the
            // background (Claude's `background_tasks_changed` with tasks in it), once nothing
            // does after a result. Both end with the job (`--pid`). The key order of a line
            // changes between Claude versions, so `"type":"result"` is looked for anywhere
            // in it, with `"total_cost_usd"`, a key only the result line has: a tool's input
            // or output is JSON too and may hold the first one. One reader only: the pipeline
            // ends when `tail` sees its reader gone, and a second reader between them would
            // keep reading.
            format!(
                "rm -f \"$1.fifo\"; mkfifo \"$1.fifo\" || exit 1; \
                 tail -n +1 -f --pid=$$ \"$1.in\" > \"$1.fifo\" & feed=$!; \
                 (tail -n +1 -F --pid=$$ \"$1.out\" 2>/dev/null | {{ {RESULT_WATCH}; }} && kill $feed 2>/dev/null) & \
                 {login} < \"$1.fifo\" > \"$1.out\" 2> \"$1.err\"; code=$?; kill $feed 2>/dev/null; rm -f \"$1.fifo\"; echo $code > \"$1.code\""
            )
        } else {
            format!("{login} < \"$1.in\" > \"$1.out\" 2> \"$1.err\"; echo $? > \"$1.code\"")
        };
        let cd = cwd.map(|cwd| format!("cd {} || exit 1; ", remote_path(cwd))).unwrap_or_default();
        let script = format!(
            "d=\"$HOME/.cache/bowyer/runs\"; mkdir -p \"$d\" && chmod 700 \"$d\" || exit 1; find \"$d\" \\( -type f -o -type p \\) -mtime +2 -delete 2>/dev/null; \
             f=\"$d/{run}\"; cat > \"$f.in\" || exit 1; {cd}\
             setsid nohup sh -c {job} sh \"$f\" < /dev/null > /dev/null 2>&1 & \
             echo $! > \"$f.pid\"; echo {RUN_STARTED} >&2; tail -c +1 -F --pid=$! \"$f.out\" 2>/dev/null; \
             cat \"$f.err\" >&2 2>/dev/null; echo >&2; echo {RUN_END} >&2; exit \"$(cat \"$f.code\" 2>/dev/null || echo 1)\"",
            run = shell_quote(run).trim_matches('\''),
            job = shell_quote(&job),
        );
        let mut command = self.lasting_command();
        command.arg("--").arg(script);
        hide_console(&mut command);
        command
    }

    /// Follows a run of [`Self::detached`] again after its connection dropped: its output
    /// from byte `from` on (counted from 0), then its error output and exit code once it
    /// has ended.
    pub fn follow_detached(&self, run: &str, from: u64) -> Command {
        let script = format!(
            "f=\"$HOME/.cache/bowyer/runs/\"{run}; p=$(cat \"$f.pid\" 2>/dev/null); \
             if [ -n \"$p\" ]; then tail -c +{start} -F --pid=\"$p\" \"$f.out\" 2>/dev/null; else echo 'bowyer: the run is gone from the host' >&2; fi; \
             cat \"$f.err\" >&2 2>/dev/null; echo >&2; echo {RUN_END} >&2; exit \"$(cat \"$f.code\" 2>/dev/null || echo 1)\"",
            run = shell_quote(run),
            start = from + 1,
        );
        let mut command = self.lasting_command();
        command.arg("--").arg(script);
        hide_console(&mut command);
        command
    }

    /// Stops a detached turn and everything it started on the host.
    pub fn stop_detached(&self, run: &str) -> io::Result<()> {
        self.script(&stop_script(run)).map(|_| ())
    }

    /// Checks that the host answers without a password prompt.
    /// A host whose local name leads to the internet is not tried at all (see
    /// [`Self::misdirected`]).
    pub fn probe(&self) -> io::Result<String> {
        if let Some(error) = self.misdirected() {
            return Err(error);
        }
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
            "d=\"$HOME/.cache/bowyer/attachments\"; mkdir -p \"$d\" && chmod 700 \"$d\" && cat > \"$d\"/{name} && printf '%s' \"$d\"/{name}",
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

/// The script that stops a detached run and its process group. `kill -TERM -PID` names the
/// group in every shell: `dash`, the `sh` of Debian and Ubuntu, takes no `--` there.
fn stop_script(run: &str) -> String {
    let run = shell_quote(run);
    format!(
        "rm -f \"$HOME/.cache/bowyer/runs/\"{run}.fifo; p=$(cat \"$HOME/.cache/bowyer/runs/\"{run}.pid 2>/dev/null) || exit 0; kill -TERM -\"$p\" 2>/dev/null || kill -TERM \"$p\" 2>/dev/null; exit 0"
    )
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
        self.run_bytes(program, args, cwd).map(|out| String::from_utf8_lossy(&out).into_owned())
    }

    fn run_bytes(&self, program: &str, args: &[&str], cwd: Option<&str>) -> io::Result<Vec<u8>> {
        let mut script = String::new();
        if let Some(cwd) = cwd {
            script.push_str(&format!("cd {} && ", remote_path(cwd)));
        }
        script.push_str(&shell_quote(program));
        for arg in args {
            script.push(' ');
            script.push_str(&shell_quote(arg));
        }
        self.script(&script)
    }

    fn run_with_vars(&self, program: &str, args: &[&str], cwd: Option<&str>, vars: &[(&str, &str)]) -> io::Result<String> {
        let mut script = String::new();
        if let Some(cwd) = cwd {
            script.push_str(&format!("cd {} && ", remote_path(cwd)));
        }
        for (name, value) in vars {
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                script.push_str(&format!("{name}={} ", shell_quote(value)));
            }
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
            // `symbolic-ref` rather than the `HEAD` file: a reftable's says `.invalid`.
            "cd {} 2>/dev/null && git rev-parse --show-toplevel --git-dir 2>/dev/null && {{ r=$(git symbolic-ref -q HEAD) && echo \"ref: $r\" || git rev-parse HEAD; }}",
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
            for option in ["StrictHostKeyChecking=yes", "NumberOfPasswordPrompts=1", "PreferredAuthentications=keyboard-interactive,password,publickey"] {
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
            "Verification code: ",
        ] {
            assert_eq!(askpass_answer(nobody, prompt), None, "{prompt}");
        }
        assert_eq!(askpass_answer(nobody, "(me@vm) Password: "), None, "no password kept, no answer");
        assert_eq!(askpass_answer(nobody, "Enter passphrase for key '/home/me/.ssh/id_ed25519': "), None, "no secret kept, no answer");
    }

    /// Through a jump host, the target's password goes to the target only.
    #[test]
    fn a_jump_host_never_gets_the_targets_password() {
        assert_eq!(prompt_machine("me@10.0.0.5's password: "), Some("10.0.0.5"));
        assert_eq!(prompt_machine("(me@jump.example) password: "), Some("jump.example"));
        assert_eq!(prompt_machine("password: "), None);
        assert!(answers_to("jump.example", "10.0.0.5", false), "no jump host: the prompt is the target's");
        assert!(answers_to("10.0.0.5", "10.0.0.5", true));
        assert!(!answers_to("jump.example", "10.0.0.5", true));
    }

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
            // A server that takes keys only, and a key with a passphrase (Windows OpenSSH
            // doubles the backslashes of the path).
            askpass_answer(&host, "Enter passphrase for key '/home/me/.ssh/work_ed25519': "),
            askpass_answer(&host, r"Enter passphrase for key 'C:\\Users\\me/.ssh/work': "),
        ];
        let refused = [
            askpass_answer(&host, "Please type 'yes', 'no' or the fingerprint: "),
            askpass_answer(&host, "Are you sure you want to continue connecting (yes/no/[fingerprint])? password"),
            askpass_answer(&host, "Enter PIN for ED25519-SK key /home/me/.ssh/id_ed25519_sk: "),
            askpass_answer(&host, "Confirm user presence for key ED25519-SK SHA256:AAAA"),
        ];
        crate::secret::remove(&host).unwrap();
        assert_eq!(answers, [Some("s3cret".into()), Some("s3cret".into()), Some("s3cret".into()), Some("s3cret".into()), Some("s3cret".into())]);
        assert_eq!(refused, [None, None, None, None]);
    }

    #[test]
    fn an_unknown_host_key_is_told_from_a_changed_one() {
        assert!(is_unknown_host_key("No ED25519 host key is known for [127.0.0.1]:2222 and you have requested strict checking.\r\nHost key verification failed."));
        assert!(!is_unknown_host_key("@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @\nHost key verification failed."));
        assert!(!is_unknown_host_key("Permission denied (password)."));
    }

    #[test]
    fn a_key_with_a_passphrase_is_told_from_an_open_one() {
        let dir = std::env::temp_dir().join(format!("bowyer-keys-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let make = |name: &str, passphrase: &str| {
            let key = dir.join(name);
            let made = Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", passphrase, "-C", "bowyer-test", "-f"]).arg(&key).output();
            made.is_ok_and(|output| output.status.success()).then_some(key)
        };
        let (Some(open), Some(locked)) = (make("open", ""), make("locked", "test phrase")) else {
            let _ = fs::remove_dir_all(&dir);
            eprintln!("skipped: no ssh-keygen here");
            return;
        };
        let ssh = Path::new("ssh");
        let answers = [key_has_passphrase(ssh, &open), key_has_passphrase(ssh, &locked), key_has_passphrase(ssh, &dir.join("missing"))];
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(answers, [false, true, false]);
    }

    #[test]
    fn a_host_without_passwords_is_told_from_a_wrong_password() {
        let keys_only = "user@vm.example: Permission denied (publickey).\r\n";
        assert_eq!(refused_sign_in(keys_only), Some(vec!["publickey".to_owned()]));
        assert!(host_takes_no_password(keys_only));
        assert!(!host_takes_no_password("user@vm.example: Permission denied (publickey,password)."), "a wrong password");
        assert!(!host_takes_no_password("Permission denied (publickey,keyboard-interactive)."));
        assert!(!host_takes_no_password("ssh: connect to host vm.example port 22: Connection timed out"));
        assert_eq!(refused_sign_in("Connection closed"), None);
    }

    #[test]
    fn names_of_the_local_network_are_told_apart() {
        for name in ["dev-vm.mshome.net", "vm.local", "nas.lan", "files.home", "box.home.arpa", "pi.localdomain.", "db.internal", "VM.MSHOME.NET"] {
            assert!(is_local_name(name), "{name}");
        }
        for name in ["example.com", "mshome.net", "local", "home", "edge", "notmshome.net", "192.168.1.10", "x.homelab.dev"] {
            assert!(!is_local_name(name), "{name}");
        }
    }

    #[test]
    fn addresses_on_the_internet_are_told_from_local_ones() {
        let public = |text: &str| is_public(text.parse().unwrap());
        for address in ["203.0.113.7", "8.8.8.8", "100.128.0.1", "2001:4860:4860::8888", "::ffff:203.0.113.7"] {
            assert!(public(address), "{address}");
        }
        for address in ["172.20.1.5", "172.31.2.9", "10.0.0.1", "192.168.1.20", "127.0.0.1", "169.254.1.1", "100.100.1.1", "fd7a:115c:a1e0::1", "fe80::1", "::1", "::ffff:10.0.0.1"] {
            assert!(!public(address), "{address}");
        }
    }

    #[test]
    fn a_misdirected_name_is_found_in_the_error_it_makes() {
        let addresses: Vec<IpAddr> = vec!["203.0.113.7".parse().unwrap(), "2001:db8::1".parse().unwrap()];
        let error = format!("dev-vm: {}\nHost key verification failed.", misdirected_message("dev-vm.mshome.net", &addresses));
        assert_eq!(misdirected_name(&error), Some(("dev-vm.mshome.net".to_owned(), "203.0.113.7, 2001:db8::1".to_owned())));
        assert_eq!(misdirected_name("dev-vm: Permission denied (publickey)."), None);
    }

    #[test]
    fn an_address_or_a_name_on_the_internet_is_never_called_stray() {
        assert!(stray_addresses("203.0.113.7", 22).is_empty());
        assert!(stray_addresses("localhost", 22).is_empty());
    }

    /// ssh itself reads the key of a host it does not know, without signing in and without
    /// writing to the user's files. Run by hand against a host of one's own:
    /// `BOWYER_TEST_HOST=<host> cargo test -p envs host_key_by_ssh -- --ignored`.
    #[test]
    #[ignore]
    fn host_key_by_ssh_reads_an_unknown_host() {
        let Ok(host) = std::env::var("BOWYER_TEST_HOST") else { return };
        let mut env = SshEnvironment::new(&host);
        if let Ok(ssh) = std::env::var("BOWYER_TEST_SSH") {
            env.ssh = PathBuf::from(ssh);
        }
        let lines = env.host_key_lines_by_ssh().unwrap();
        assert!(!lines.is_empty());
        assert!(lines.iter().all(|line| line.split_whitespace().count() >= 3), "{lines:?}");
        eprintln!("{lines:?}");
    }

    #[test]
    fn a_changed_key_is_replaced_only_in_bowyers_own_file() {
        let own = Path::new(r"C:\data dir\Bowyer\known_hosts");
        let changed = |file: &str| {
            format!(
                "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\n\
                 @    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\r\n\
                 The fingerprint for the ED25519 key sent by the remote host is\r\n\
                 SHA256:AAAA.\r\n\
                 Add correct host key in C:\\Users\\someone/.ssh/known_hosts to get rid of this message.\r\n\
                 Offending ED25519 key in {file}:3\r\n\
                 Host key for [127.0.0.1]:2222 has changed and you have requested strict checking.\r\n\
                 Host key verification failed."
            )
        };
        assert_eq!(changed_host_key_files(&changed(r"C:\data dir\Bowyer\known_hosts")), [r"C:\data dir\Bowyer\known_hosts"]);
        assert!(changed_key_is_in(&changed(r"C:\data dir\Bowyer\known_hosts"), own));
        assert!(changed_key_is_in(&changed("C:/data dir/Bowyer/known_hosts"), own), "either slash");
        // As Windows OpenSSH prints it.
        assert!(changed_key_is_in(&changed(r"C:\\data dir\\Bowyer\\known_hosts"), own), "doubled backslashes");
        assert!(!changed_key_is_in(&changed(r"C:\Users\someone/.ssh/known_hosts"), own), "the user's own file stays theirs");
        let both = changed(r"C:\data dir\Bowyer\known_hosts") + "\r\nOffending key for IP in C:\\Users\\someone/.ssh/known_hosts:7";
        assert!(!changed_key_is_in(&both, own), "a key in the user's file too");
        assert!(!changed_key_is_in("No ED25519 host key is known for vm\r\nHost key verification failed.", own));
        assert!(changed_host_key_files("Offending ED25519 key in /a:1").is_empty(), "only after a change");
    }

    #[test]
    fn trusted_keys_take_the_place_of_the_old_keys_of_their_host() {
        let kept = "# Bowyer\n[127.0.0.1]:2222 ssh-ed25519 OLD\nvm ssh-rsa VM\n[127.0.0.1]:2222 ssh-rsa OLDRSA\n@revoked [127.0.0.1]:2222 ssh-rsa BAD\n";
        let new = "[127.0.0.1]:2222 ssh-ed25519 NEW\n";
        assert_eq!(merged_known_hosts(kept, new), "# Bowyer\nvm ssh-rsa VM\n@revoked [127.0.0.1]:2222 ssh-rsa BAD\n[127.0.0.1]:2222 ssh-ed25519 NEW\n");
        assert_eq!(merged_known_hosts("", new), new);
        assert_eq!(merged_known_hosts("other,10.0.0.1 ssh-ed25519 X", "10.0.0.1 ssh-ed25519 Y\n"), "10.0.0.1 ssh-ed25519 Y\n", "a line with several names");
        assert_eq!(merged_known_hosts(new, new), new, "trusting twice keeps one line");
    }

    #[test]
    fn a_detached_turn_keeps_its_prompt_and_output_on_the_host() {
        let command = SshEnvironment::new("vm").detached("claude", &["-p", "it's"], Some("~/work"), "17-42");
        let script = command.get_args().last().unwrap().to_string_lossy().into_owned();
        assert!(script.contains(r#"f="$d/17-42"; cat > "$f.in""#), "{script}");
        assert!(script.contains(r#"cd "$HOME"/work || exit 1; setsid nohup sh -c"#), "{script}");
        assert!(script.contains("echo bowyer-run-started >&2; tail -c +1 -F --pid=$!"), "{script}");
        assert!(script.contains("echo bowyer-run-end >&2; exit"), "{script}");
        assert!(script.contains(r#"mkdir -p "$d" && chmod 700 "$d""#), "the runs are the user's alone: {script}");
        assert!(command.get_args().any(|arg| arg == "ServerAliveInterval=15"), "a dead connection is noticed");
        let follow = SshEnvironment::new("vm").follow_detached("17-42", 300);
        let again = follow.get_args().last().unwrap().to_string_lossy().into_owned();
        assert!(again.contains("tail -c +301 -F") && again.contains("echo bowyer-run-end >&2; exit"), "{again}");
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
        // The same stop where the shell is `dash`.
        if ssh.script_text("command -v dash").is_ok_and(|found| !found.trim().is_empty()) {
            let run = format!("dash-{}", std::process::id());
            let mut third = ssh.detached("sh", &["-c", "sleep 30; echo late"], None, &run);
            third.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
            let mut child = third.spawn().unwrap();
            drop(child.stdin.take());
            std::thread::sleep(std::time::Duration::from_secs(2));
            ssh.run("dash", &["-c", &stop_script(&run)], None).unwrap();
            assert!(!child.wait().unwrap().success(), "dash stops the group too");
            let _ = ssh.script_text(&format!("rm -f \"$HOME/.cache/bowyer/runs/{run}\".*"));
        }
        let _ = ssh.script_text("rm -f \"$HOME/.cache/bowyer/runs/\"test-* \"$HOME/.cache/bowyer/runs/\"stop-*");
    }

    /// A password host whose server takes keys only, signed in by a key with a passphrase:
    /// the kept secret opens the key through Bowyer as the askpass program.
    /// `BOWYER_SSH_HOST=ssh://user@host:port BOWYER_SSH_KEY=key BOWYER_SSH_PASSPHRASE=…
    /// BOWYER_ASKPASS=path/to/bowyer BOWYER_KNOWN_HOSTS=file [BOWYER_SSH=ssh] cargo test -p envs
    /// -- --ignored passphrase`.
    #[test]
    #[ignore = "needs an SSH host"]
    fn a_kept_secret_opens_a_key_with_a_passphrase() {
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name}"));
        set_askpass(PathBuf::from(var("BOWYER_ASKPASS")));
        set_known_hosts(PathBuf::from(var("BOWYER_KNOWN_HOSTS")));
        let host = var("BOWYER_SSH_HOST");
        let mut ssh = SshEnvironment::new(&host).with_identity(Some(PathBuf::from(var("BOWYER_SSH_KEY")))).with_password(true);
        if let Ok(program) = std::env::var("BOWYER_SSH") {
            ssh.ssh = PathBuf::from(program);
        }
        let before = crate::secret::load(&host);
        crate::secret::store(&host, "not the passphrase").unwrap();
        let wrong = ssh.script_alone("echo in");
        crate::secret::store(&host, &var("BOWYER_SSH_PASSPHRASE")).unwrap();
        let right = ssh.script_alone("echo in");
        match before {
            Some(old) => crate::secret::store(&host, &old).unwrap(),
            None => crate::secret::remove(&host).unwrap(),
        }
        let wrong = wrong.expect_err("a wrong passphrase opens nothing").to_string();
        assert!(host_takes_no_password(&wrong), "{wrong}");
        assert_eq!(String::from_utf8_lossy(&right.unwrap()).trim(), "in");
    }

    #[test]
    fn an_answered_turn_reads_its_input_file_until_the_result() {
        let command = SshEnvironment::new("vm").detached_answered("claude", &["-p"], None, "17-43");
        let script = command.get_args().last().unwrap().to_string_lossy().into_owned();
        assert!(script.contains("mkfifo"), "{script}");
        assert!(script.contains("| { while IFS= read -r l; do"), "a shell loop reads line by line, however long: {script}");
        assert!(!script.contains("awk"), "mawk cuts long lines: {script}");
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

    /// While something runs in the background, the input stays open past a result: a
    /// prompt added then reaches the harness, and the input ends once nothing runs in the
    /// background, with the connection gone. Same host as above.
    #[test]
    #[ignore = "needs an SSH host with python3"]
    fn an_answered_turn_keeps_its_input_while_a_command_runs_in_the_background() {
        use std::io::{BufRead, Write};
        let host = std::env::var("BOWYER_SSH_HOST").expect("BOWYER_SSH_HOST");
        let key = std::env::var_os("BOWYER_SSH_KEY").map(PathBuf::from);
        let ssh = SshEnvironment::new(&host).with_identity(key);
        let run = format!("background-{}", std::process::id());
        let stub = "import json, sys\n\
            prompt = sys.stdin.readline().strip()\n\
            print(json.dumps({'type': 'stream_event', 'text': 'x' * 20000}), flush=True)\n\
            print(json.dumps({'type': 'system', 'subtype': 'background_tasks_changed', 'tasks': [{'task_id': 'b1'}]}), flush=True)\n\
            print(json.dumps({'type': 'result', 'total_cost_usd': 0, 'prompt': prompt}), flush=True)\n\
            later = sys.stdin.readline().strip()\n\
            print(json.dumps({'type': 'result', 'total_cost_usd': 0, 'later': later}), flush=True)\n\
            print(json.dumps({'type': 'system', 'subtype': 'background_tasks_changed', 'tasks': []}), flush=True)\n\
            print(json.dumps({'type': 'eof', 'rest': sys.stdin.read()}), flush=True)\n";
        let mut command = ssh.detached_answered("python3", &["-u", "-c", stub], None, &run);
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = command.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"the prompt\n").unwrap();
        drop(stdin);
        let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
        assert!(lines.nth(2).unwrap().unwrap().contains("the prompt"));
        // Time for the watcher to see the result, were it to close the input there.
        std::thread::sleep(std::time::Duration::from_millis(1500));
        ssh.append_input(&run, b"sent later\n").unwrap();
        child.kill().unwrap();
        let _ = child.wait();
        std::thread::sleep(std::time::Duration::from_secs(4));
        let out = ssh.script_text(&format!("cat \"$HOME/.cache/bowyer/runs/{run}.out\"; echo; cat \"$HOME/.cache/bowyer/runs/{run}.code\"")).unwrap();
        let _ = ssh.script_text(&format!("rm -f \"$HOME/.cache/bowyer/runs/{run}\".*"));
        assert!(out.contains(r#""later": "sent later""#), "a prompt past the result went in: {out}");
        assert!(out.contains(r#"{"type": "eof", "rest": ""}"#), "its input ended once nothing ran in the background: {out}");
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
