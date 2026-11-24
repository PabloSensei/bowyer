//! Bowyer's memory for Claude Code on SSH hosts. The window serves the memory over HTTP on
//! `127.0.0.1` of this computer ([`mcp::http`]), and keeps one tunnel per host (`ssh -R`)
//! that brings it to the host's own `127.0.0.1`. A turn the window starts there gets the
//! server in its MCP config; for sessions the user starts on the host, the window shows the
//! command that adds it to one project (the user runs it: the window writes no harness
//! config). The port on the host is open to every user there, so every request carries a
//! token. Each host has its own, and the server knows the host by it: whoever reads a
//! host's token gets that host's sessions and notes, and nothing of this computer's.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::{Arc, Mutex};

use envs::SshEnvironment;
use model::EnvId;
use ui_kit::prelude::*;

use super::{Shell, now_ms};

/// How long a failed tunnel waits before it is tried again.
const RETRY_MS: i64 = 60_000;
/// Ports a tunnel takes on a host: high, and out of the ranges systems hand out.
const PORTS: std::ops::RangeInclusive<u16> = 41_000..=48_999;
/// Tries of other ports when the host's is taken (a tunnel of a crashed window may hold it).
const PORT_TRIES: u32 = 3;

/// Where a host's tunnel stands.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum TunnelState {
    Starting,
    /// The port on the host leads to the window's server.
    Ready(u16),
    /// What went wrong, and when.
    Failed(String, i64),
}

/// A running tunnel: the connection, and its standard input, whose end ends it.
struct TunnelProcess {
    child: Child,
    _stdin: ChildStdin,
}

#[derive(Default)]
pub(super) struct MemoryLink {
    /// The port of the window's server, once it listens.
    server: Option<u16>,
    server_failed: bool,
    /// The hosts' tokens the server takes.
    tokens: mcp::http::Tokens,
    tunnels: HashMap<String, TunnelState>,
    processes: HashMap<String, TunnelProcess>,
}

impl MemoryLink {
    pub(super) fn state(&self, host: &str) -> Option<&TunnelState> {
        self.tunnels.get(host)
    }
}

impl Drop for MemoryLink {
    fn drop(&mut self) {
        for process in self.processes.values_mut() {
            let _ = process.child.kill();
        }
    }
}

/// Why a tunnel did not come up.
enum TunnelError {
    /// The host gave no port: it is taken, or the host forwards none.
    NoForwarding,
    Other(String),
}

/// Opens a tunnel and waits for the host to say it holds the port.
fn open_tunnel(ssh: SshEnvironment, remote: u16, local: u16) -> Result<TunnelProcess, TunnelError> {
    let mut command = ssh.tunnel(remote, local);
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| TunnelError::Other(error.to_string()))?;
    let stdin = child.stdin.take().ok_or_else(|| TunnelError::Other("no input".into()))?;
    // What ssh says goes on in the background, so a full pipe never stops it.
    let said = Arc::new(Mutex::new(String::new()));
    if let Some(mut stderr) = child.stderr.take() {
        let said = said.clone();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            while let Ok(read) = stderr.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                if let Ok(mut said) = said.lock()
                    && said.len() < 16_384
                {
                    said.push_str(&String::from_utf8_lossy(&buffer[..read]));
                }
            }
        });
    }
    let stdout = child.stdout.take().ok_or_else(|| TunnelError::Other("no output".into()))?;
    let mut lines = BufReader::new(stdout).lines();
    let ready = lines.any(|line| line.is_ok_and(|line| line.trim() == envs::TUNNEL_READY));
    if ready {
        // Nothing more comes, but the pipe is read to its end so it never fills.
        std::thread::spawn(move || lines.for_each(drop));
        return Ok(TunnelProcess { child, _stdin: stdin });
    }
    let _ = child.wait();
    let said = said.lock().map(|said| said.trim().to_owned()).unwrap_or_default();
    if said.contains("remote port forwarding failed") {
        return Err(TunnelError::NoForwarding);
    }
    Err(TunnelError::Other(if said.is_empty() { "the connection ended".into() } else { said }))
}

/// A port for a host's tunnel, nobody's in particular.
fn some_port() -> u16 {
    let token = mcp::http::new_token();
    let n = u32::from_str_radix(&token[..8], 16).unwrap_or(0);
    PORTS.start() + (n % u32::from(PORTS.end() - PORTS.start() + 1)) as u16
}

impl Shell {
    /// The token requests from `host` carry, made once and kept, and taken by the server
    /// from now on as that host's.
    pub(super) fn memory_token(&self, host: &str) -> String {
        let key = format!("memory.token.{host}");
        let token = match self.lib().setting(&key).filter(|token| token.len() >= 32) {
            Some(token) => token,
            None => {
                let token = mcp::http::new_token();
                let _ = self.lib().set_setting(&key, &token);
                token
            }
        };
        if let Ok(mut tokens) = self.memory_link.tokens.write() {
            tokens.entry(token.clone()).or_insert_with(|| EnvId::ssh(host));
        }
        token
    }

    /// Ends the tunnel to a host the user removed and takes its token back: whatever kept
    /// it there no longer reaches the memory.
    pub(super) fn drop_memory_link(&mut self, host: &str) {
        if let Some(mut process) = self.memory_link.processes.remove(host) {
            let _ = process.child.kill();
        }
        self.memory_link.tunnels.remove(host);
        let env = EnvId::ssh(host);
        if let Ok(mut tokens) = self.memory_link.tokens.write() {
            tokens.retain(|_, machine| *machine != env);
        }
        let _ = self.lib().remove_setting(&format!("memory.token.{host}"));
    }

    /// The window's memory server, started on first need on the port it had before (a
    /// tunnel a crashed window left on a host then still leads to it).
    fn memory_server(&mut self) -> Option<u16> {
        if self.memory_link.server.is_some() || self.memory_link.server_failed {
            return self.memory_link.server;
        }
        let tokens = self.memory_link.tokens.clone();
        let library = self.settings_dir.join("library.db");
        let cache = self.cache_dir.clone();
        let kept = self.lib().setting("memory.http_port").and_then(|port| port.parse::<u16>().ok()).unwrap_or(0);
        let started = mcp::http::start(kept, library.clone(), cache.clone(), tokens.clone())
            .or_else(|_| mcp::http::start(0, library, cache, tokens));
        match started {
            Ok(port) => {
                if port != kept {
                    let _ = self.lib().set_setting("memory.http_port", &port.to_string());
                }
                self.memory_link.server = Some(port);
            }
            Err(error) => {
                self.memory_link.server_failed = true;
                self.status = Some(format!("{}: {error}", crate::i18n::t("memory.server_failed")));
            }
        }
        self.memory_link.server
    }

    /// The port a host's tunnel takes there: the one it had, so a command the user ran on
    /// the host keeps working.
    fn tunnel_port(&self, host: &str) -> u16 {
        let key = format!("memory.tunnel_port.{host}");
        if let Some(port) = self.lib().setting(&key).and_then(|port| port.parse::<u16>().ok()) {
            return port;
        }
        let port = some_port();
        let _ = self.lib().set_setting(&key, &port.to_string());
        port
    }

    /// Opens the tunnel to `env`'s host unless it is up, coming up or failed a short while
    /// ago. Called where a Claude Code session of a host is open, so the tunnel is there by
    /// the time a turn starts.
    pub(super) fn want_tunnel(&mut self, env: &EnvId, cx: &mut Context<Self>) {
        let Some(host) = env.ssh_host().map(str::to_owned) else { return };
        if !self.memory_enabled() {
            return;
        }
        // A tunnel whose connection ended is opened again.
        if let Some(process) = self.memory_link.processes.get_mut(&host)
            && !matches!(process.child.try_wait(), Ok(None))
        {
            self.memory_link.processes.remove(&host);
            self.memory_link.tunnels.insert(host.clone(), TunnelState::Failed("the tunnel ended".into(), 0));
        }
        match self.memory_link.tunnels.get(&host) {
            Some(TunnelState::Ready(_) | TunnelState::Starting) => return,
            Some(TunnelState::Failed(_, at)) if now_ms() - at < RETRY_MS => return,
            _ => {}
        }
        let (Some(local), Some(ssh)) = (self.memory_server(), self.ssh_env(&host)) else { return };
        // The host's token is taken before its tunnel leads anywhere.
        self.memory_token(&host);
        self.memory_link.tunnels.insert(host.clone(), TunnelState::Starting);
        self.open_tunnel_on(host, ssh, local, self.tunnel_port_of(env), 0, cx);
    }

    fn tunnel_port_of(&self, env: &EnvId) -> u16 {
        self.tunnel_port(env.ssh_host().unwrap_or_default())
    }

    fn open_tunnel_on(&mut self, host: String, ssh: SshEnvironment, local: u16, remote: u16, tries: u32, cx: &mut Context<Self>) {
        let task = cx.background_spawn({
            let ssh = ssh.clone();
            async move { open_tunnel(ssh, remote, local) }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(process) => {
                        // Another port is kept for next time only once the host gave it: a
                        // command run on the host with the old one has to be run again,
                        // which the window says.
                        let key = format!("memory.tunnel_port.{host}");
                        if this.lib().setting(&key).and_then(|port| port.parse::<u16>().ok()) != Some(remote) {
                            let _ = this.lib().set_setting(&key, &remote.to_string());
                        }
                        this.memory_link.processes.insert(host.clone(), process);
                        this.memory_link.tunnels.insert(host, TunnelState::Ready(remote));
                    }
                    // ssh says the same when the port is taken and when the host allows no
                    // forwarding at all: another port tells the two apart, and the kept one
                    // stays until another works.
                    Err(TunnelError::NoForwarding) if tries + 1 < PORT_TRIES => {
                        this.open_tunnel_on(host, ssh, local, some_port(), tries + 1, cx);
                    }
                    Err(TunnelError::NoForwarding) => {
                        this.memory_link.tunnels.insert(host, TunnelState::Failed(crate::i18n::t("memory.port_taken").to_owned(), now_ms()));
                    }
                    Err(TunnelError::Other(error)) => {
                        this.memory_link.tunnels.insert(host, TunnelState::Failed(error, now_ms()));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The MCP config of a Claude Code turn on a host, when the memory is on for its project
    /// and the tunnel is up; otherwise the turn goes without, and the tunnel is asked for.
    pub(super) fn turn_mcp_config(&mut self, env: &EnvId, project: &str, cx: &mut Context<Self>) -> Option<String> {
        env.ssh_host()?;
        if !self.memory_enabled() || !mcp::memory_on(&self.lib(), project) {
            return None;
        }
        self.want_tunnel(env, cx);
        let host = env.ssh_host()?;
        let Some(TunnelState::Ready(port)) = self.memory_link.state(host) else { return None };
        Some(mcp::http::claude_config(*port, &self.memory_token(host), project))
    }

    /// The command that adds the memory to Claude Code for one project on a host, run in
    /// that project's folder there. It names the tunnel's port.
    pub(super) fn host_connect_command(&self, env: &EnvId, project: &str) -> Option<String> {
        let host = env.ssh_host()?;
        let port = match self.memory_link.state(host) {
            Some(TunnelState::Ready(port)) => *port,
            _ => self.tunnel_port(host),
        };
        let url = mcp::http::url(port, project);
        let header = format!("Authorization: Bearer {}", self.memory_token(host));
        Some(format!(
            "claude mcp add --scope local --transport http bowyer {} --header {}",
            envs::shell_quote(&url),
            envs::shell_quote(&header)
        ))
    }
}

/// A command with the token of its `Authorization` header masked, for the screen.
pub(super) fn masked_token(command: &str) -> String {
    const MARK: &str = "Bearer ";
    let Some(at) = command.find(MARK) else { return command.to_owned() };
    let start = at + MARK.len();
    let end = command[start..].find(|c: char| !c.is_ascii_alphanumeric()).map_or(command.len(), |length| start + length);
    if end - start < 8 {
        return command.to_owned();
    }
    format!("{}{}…{}", &command[..start], &command[start..start + 4], &command[end..])
}

#[cfg(test)]
mod tests {
    use super::{PORTS, masked_token, some_port};

    #[test]
    fn the_token_is_masked_on_screen() {
        let command = "claude mcp add --scope local --transport http bowyer 'http://127.0.0.1:41000/mcp?project=/w' --header 'Authorization: Bearer 0123456789abcdef0123'";
        let shown = masked_token(command);
        assert_eq!(shown, "claude mcp add --scope local --transport http bowyer 'http://127.0.0.1:41000/mcp?project=/w' --header 'Authorization: Bearer 0123…'");
        assert_eq!(masked_token("claude mcp add bowyer -- bowyer mcp"), "claude mcp add bowyer -- bowyer mcp");
    }

    #[test]
    fn a_tunnel_port_is_a_high_one() {
        for _ in 0..50 {
            assert!(PORTS.contains(&some_port()));
        }
    }
}
