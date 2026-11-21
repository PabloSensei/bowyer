//! Stopping one command of a running turn by its processes, for harnesses that cannot stop a
//! single command themselves: among the processes under the harness, the one whose command
//! line holds the command is ended with everything it started. The harness sees the command
//! fail and goes on with its turn.

use std::io;
use std::process::{Command, Stdio};

/// One process of a listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Proc {
    pub pid: u32,
    pub parent: u32,
    pub args: String,
}

/// Parses `pid ppid args` lines (`ps -eo pid=,ppid=,args=`, or tab separated).
pub(crate) fn parse_listing(text: &str) -> Vec<Proc> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let (pid, rest) = line.split_once(|c: char| c.is_whitespace())?;
            let rest = rest.trim_start();
            let (parent, args) = rest.split_once(|c: char| c.is_whitespace()).unwrap_or((rest, ""));
            Some(Proc { pid: pid.parse().ok()?, parent: parent.parse().ok()?, args: args.trim().to_owned() })
        })
        .collect()
}

/// What to look for in a command line: the longest piece of the command's first line
/// without quotes or backslashes, which shells and harnesses write out differently.
pub(crate) fn needle(command: &str) -> Option<String> {
    let line = command.lines().map(str::trim).find(|line| !line.is_empty())?;
    let piece = line.split(['"', '\'', '\\', '`']).map(str::trim).max_by_key(|piece| piece.len())?;
    let piece: String = piece.chars().take(80).collect();
    (piece.chars().count() >= 4).then_some(piece)
}

/// Every process under `root`, not `root` itself.
fn descendants(processes: &[Proc], root: u32) -> Vec<u32> {
    let mut found = Vec::new();
    let mut queue = vec![root];
    while let Some(parent) = queue.pop() {
        for process in processes.iter().filter(|p| p.parent == parent && p.pid != parent) {
            if !found.contains(&process.pid) {
                found.push(process.pid);
                queue.push(process.pid);
            }
        }
    }
    found
}

/// The processes that run `command` under `root`: the topmost ones whose command line holds
/// it (a shell and the program it runs both do; the shell goes, and its program with it).
pub(crate) fn targets(processes: &[Proc], root: u32, command: &str) -> Vec<u32> {
    let Some(needle) = needle(command) else { return Vec::new() };
    let under = descendants(processes, root);
    let matching: Vec<&Proc> = processes.iter().filter(|p| under.contains(&p.pid) && p.args.contains(&needle)).collect();
    matching.iter().filter(|p| !matching.iter().any(|other| other.pid == p.parent)).map(|p| p.pid).collect()
}

/// The targets with everything under them: what a plain `kill` has to reach.
pub(crate) fn with_descendants(processes: &[Proc], targets: &[u32]) -> Vec<u32> {
    let mut all = Vec::new();
    for &target in targets {
        all.push(target);
        all.extend(descendants(processes, target));
    }
    all.sort_unstable();
    all.dedup();
    all
}

#[cfg(windows)]
fn listing() -> io::Result<Vec<Proc>> {
    use std::os::windows::process::CommandExt;
    // UTF-8 out: in a pipe PowerShell would write the console code page, and a command
    // with Cyrillic in it would never match.
    let script = "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; Get-CimInstance Win32_Process | ForEach-Object { \"$($_.ProcessId)`t$($_.ParentProcessId)`t$($_.CommandLine)\" }";
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(0x0800_0000)
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("the process list did not come"));
    }
    Ok(parse_listing(&String::from_utf8_lossy(&output.stdout)))
}

#[cfg(not(windows))]
fn listing() -> io::Result<Vec<Proc>> {
    let output = Command::new("ps").args(["-eo", "pid=,ppid=,args="]).stdin(Stdio::null()).output()?;
    if !output.status.success() {
        return Err(io::Error::other("ps failed"));
    }
    Ok(parse_listing(&String::from_utf8_lossy(&output.stdout)))
}

/// Ends the processes of `command` under the local process `root`; how many were found.
pub(crate) fn stop_local(root: u32, command: &str) -> io::Result<usize> {
    let processes = listing()?;
    let found = targets(&processes, root, command);
    #[cfg(windows)]
    for pid in &found {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .creation_flags(0x0800_0000)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        let all = with_descendants(&processes, &found);
        if !all.is_empty() {
            let _ = Command::new("kill").arg("-TERM").args(all.iter().map(u32::to_string)).status();
        }
    }
    Ok(found.len())
}

/// Ends the processes of `command` in the detached run `run` on an SSH host.
pub(crate) fn stop_remote(ssh: &envs::SshEnvironment, run: &str, command: &str) -> io::Result<usize> {
    let run: String = run.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')).collect();
    let text = String::from_utf8_lossy(&ssh.script(&format!("cat \"$HOME/.cache/bowyer/runs/{run}.pid\" && ps -eo pid=,ppid=,args="))?).into_owned();
    let (root, listing) = text.split_once('\n').unwrap_or((&text, ""));
    let root: u32 = root.trim().parse().map_err(|_| io::Error::other("the run has no process"))?;
    let processes = parse_listing(listing);
    let found = targets(&processes, root, command);
    let all = with_descendants(&processes, &found);
    if !all.is_empty() {
        let pids: Vec<String> = all.iter().map(u32::to_string).collect();
        ssh.script(&format!("kill -TERM {} 2>/dev/null; exit 0", pids.join(" ")))?;
    }
    Ok(found.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing() -> Vec<Proc> {
        parse_listing(
            "  1     0 /sbin/init\n\
             100     1 sh -c harness\n\
             101   100 codex exec --json -\n\
             102   101 /bin/bash -lc python -c \"import time; time.sleep(60); print(42)\"\n\
             103   102 python -c import time; time.sleep(60); print(42)\n\
             104   101 /bin/bash -lc ls\n\
             200     1 python -c import time; time.sleep(60); print(42)\n",
        )
    }

    #[test]
    fn the_shell_of_the_command_goes_with_what_it_runs_and_nothing_outside_the_turn() {
        let processes = listing();
        assert_eq!(processes.len(), 7);
        assert_eq!(processes[3].args, r#"/bin/bash -lc python -c "import time; time.sleep(60); print(42)""#);
        let command = r#"python -c "import time; time.sleep(60); print(42)""#;
        assert_eq!(needle(command).as_deref(), Some("import time; time.sleep(60); print(42)"));
        assert_eq!(targets(&processes, 100, command), [102], "the topmost match, not the same command elsewhere");
        assert_eq!(with_descendants(&processes, &[102]), [102, 103]);
        assert!(targets(&processes, 100, "make").is_empty());
        assert_eq!(needle("ls"), None, "too short to tell apart");
    }

    /// A "harness" runs a long command in a shell of its own and says when it goes on; the
    /// command stops, the harness does not. `cargo test -p live -- --ignored commands`.
    #[test]
    #[ignore = "needs python on PATH"]
    fn a_local_command_stops_and_its_harness_goes_on() {
        stops_locally(r#"python -c "import time; time.sleep(31); print('late-31')""#);
    }

    /// The longest piece of this command is Cyrillic: the listing has to keep it.
    #[test]
    #[ignore = "needs python on PATH"]
    fn a_local_command_with_cyrillic_stops_too() {
        stops_locally(r#"python -c "import time; time.sleep(31); print('late-31')" остановить-эту-долгую-команду"#);
    }

    fn stops_locally(command: &str) {
        use std::io::Read;
        let harness = "import os, subprocess; subprocess.run(os.environ['BOWYER_CMD'], shell=True); print('after-stop', flush=True)";
        let mut child = Command::new("python").args(["-c", harness]).env("BOWYER_CMD", command).stdout(Stdio::piped()).spawn().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(3));
        assert_eq!(stop_local(child.id(), command).unwrap(), 1);
        let started = std::time::Instant::now();
        let mut out = String::new();
        child.stdout.take().unwrap().read_to_string(&mut out).unwrap();
        assert!(child.wait().unwrap().success());
        assert!(started.elapsed().as_secs() < 15, "the command ended early");
        assert!(out.contains("after-stop") && !out.contains("late-31"), "{out}");
    }

    /// The same in a detached run on an SSH host:
    /// `BOWYER_SSH_HOST=user@host BOWYER_SSH_KEY=key cargo test -p live -- --ignored commands`.
    #[test]
    #[ignore = "needs an SSH host"]
    fn a_remote_command_stops_and_its_harness_goes_on() {
        use std::io::Write;
        let host = std::env::var("BOWYER_SSH_HOST").expect("BOWYER_SSH_HOST");
        let key = std::env::var_os("BOWYER_SSH_KEY").map(std::path::PathBuf::from);
        let ssh = envs::SshEnvironment::new(&host).with_identity(key);
        let run = format!("stopcmd-{}", std::process::id());
        let command = "sleep 31; echo late-31";
        // The harness reads the command from its input, so only the shell it starts holds it.
        let mut follower = ssh.detached("sh", &["-c", "read -r c; sh -c \"$c\"; echo after-stop"], None, &run);
        follower.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
        let mut child = follower.spawn().unwrap();
        child.stdin.take().unwrap().write_all(format!("{command}\n").as_bytes()).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(3));
        assert_eq!(stop_remote(&ssh, &run, command).unwrap(), 1);
        let status = child.wait().unwrap();
        let out = String::from_utf8_lossy(&ssh.script(&format!("cat \"$HOME/.cache/bowyer/runs/{run}.out\"; rm -f \"$HOME/.cache/bowyer/runs/{run}\".*")).unwrap()).into_owned();
        assert!(status.success(), "the harness ended by itself");
        assert!(out.contains("after-stop") && !out.contains("late-31"), "{out}");
    }

    /// The same for an agent that runs attached over SSH, as OpenCode does: its output
    /// comes over the connection, and the process id it left on the host roots the search.
    #[test]
    #[ignore = "needs an SSH host"]
    fn an_attached_remote_command_stops_and_its_agent_goes_on() {
        use std::io::{Read, Write};
        let host = std::env::var("BOWYER_SSH_HOST").expect("BOWYER_SSH_HOST");
        let key = std::env::var_os("BOWYER_SSH_KEY").map(std::path::PathBuf::from);
        let ssh = envs::SshEnvironment::new(&host).with_identity(key);
        let run = format!("attached-{}", std::process::id());
        let command = "sleep 32; echo late-32";
        let mut agent = ssh.headless_run("sh", &["-c", "read -r c; sh -c \"$c\"; echo after-stop"], None, &run);
        agent.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = agent.spawn().unwrap();
        child.stdin.take().unwrap().write_all(format!("{command}
").as_bytes()).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(3));
        let started = std::time::Instant::now();
        assert_eq!(stop_remote(&ssh, &run, command).unwrap(), 1);
        let mut out = String::new();
        child.stdout.take().unwrap().read_to_string(&mut out).unwrap();
        let status = child.wait().unwrap();
        let _ = ssh.script(&format!("rm -f \"$HOME/.cache/bowyer/runs/{run}.pid\""));
        assert!(status.success(), "the agent ended by itself");
        assert!(out.contains("after-stop") && !out.contains("late-32"), "{out}");
        assert!(started.elapsed() < std::time::Duration::from_secs(20), "stopped, not waited out");
    }

    #[test]
    fn tab_separated_windows_lines_parse_too() {
        let processes = parse_listing("4\t0\t\n812\t4\tC:\\Windows\\System32\\cmd.exe /c \"npm test\"\n");
        assert_eq!(processes[0], Proc { pid: 4, parent: 0, args: String::new() });
        assert_eq!(processes[1].args, "C:\\Windows\\System32\\cmd.exe /c \"npm test\"");
    }
}
