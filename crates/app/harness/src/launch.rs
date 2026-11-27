//! Starting things outside the window: a terminal in the folder of a session, a harness
//! resuming a session, a new session. Until the embedded terminal lands, they open in the
//! terminal of the system.

use std::io;
use std::process::Command;

use envs::Environment;
#[cfg(unix)]
use envs::shell_quote;
use model::{HarnessKind, SessionSummary};

/// The program and arguments that start a harness, or resume one of its sessions.
pub fn harness_command(harness: &HarnessKind, resume: Option<&str>) -> (String, Vec<String>) {
    let program = match harness {
        HarnessKind::Claude => "claude",
        HarnessKind::Codex => "codex",
        HarnessKind::OpenCode => "opencode",
        HarnessKind::DeepSeek => "dsh",
        HarnessKind::Pi => "pi",
        HarnessKind::Antigravity => "agy",
        HarnessKind::Other(id) => id.as_str(),
    }
    .to_owned();
    let args = match (harness, resume) {
        (HarnessKind::Claude, Some(id)) => vec!["--resume".into(), id.into()],
        (HarnessKind::Codex, Some(id)) => vec!["resume".into(), id.into()],
        (HarnessKind::OpenCode, Some(id)) => vec!["--session".into(), id.into()],
        (HarnessKind::Pi, Some(id)) => vec!["--session".into(), id.into()],
        (HarnessKind::Antigravity, Some(id)) => vec!["--conversation".into(), id.into()],
        // DeepSeek Harness has no terminal interface: its web interface opens the session.
        (HarnessKind::DeepSeek, _) => vec!["web".into()],
        _ => Vec::new(),
    };
    (program, args)
}

/// A command line for PowerShell, which also finds the `.cmd` shims npm installs.
#[cfg_attr(not(windows), allow(dead_code))]
fn powershell_line(program: &str, args: &[String]) -> String {
    let quote = |word: &str| {
        if word.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=".contains(c)) {
            word.to_owned()
        } else {
            ps_quoted(word)
        }
    };
    let mut line = format!("& {}", quote(program));
    for arg in args {
        line.push(' ');
        line.push_str(&quote(arg));
    }
    line
}

/// Opens a window of the system terminal running `program` in `cwd` (a shell when
/// `program` is `None`).
pub fn open_terminal(env: &dyn Environment, cwd: Option<&str>, program: Option<(&str, &[String])>) -> io::Result<()> {
    if !env.id().is_local() {
        let (program, args): (&str, Vec<&str>) = match program {
            Some((program, args)) => (program, args.iter().map(String::as_str).collect()),
            None => ("", Vec::new()), // The login shell.
        };
        // The environment knows its host and key.
        let remote = env.interactive(program, &args, cwd);
        return spawn_ssh_terminal(remote);
    }
    spawn_local_terminal(cwd, program)
}

/// Opens `ssh` in a console of its own. On Windows PowerShell starts it with a command
/// line built here (`Start-Process -ArgumentList` passes one string as it is: PowerShell
/// itself would drop the double quotes inside the arguments, which would break the remote
/// script), and when `ssh` fails to connect (exit code 255) the console stays with what it
/// said until Enter.
#[cfg(windows)]
fn spawn_ssh_terminal(remote: Command) -> io::Result<()> {
    let program = remote.get_program().to_string_lossy().into_owned();
    let args: Vec<String> = remote.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
    let literal = ps_quoted;
    let script = format!(
        "$p = Start-Process -FilePath {} -ArgumentList {} -NoNewWindow -Wait -PassThru\n\
         if ($p.ExitCode -eq 255) {{ Write-Host ''; Write-Host {}; [void](Read-Host {}) }}",
        literal(&program),
        literal(&windows_command_line(&args)),
        literal(crate::i18n::t("terminal.ssh_failed")),
        literal(crate::i18n::t("terminal.press_enter")),
    );
    let encoded = powershell_encoded(&script);
    if encoded.len() > MAX_ENCODED {
        // A long block: `ssh` in its console directly, without the pause on an error.
        return spawn_console(&remote);
    }
    let mut wrapper = Command::new(envs::system_program("powershell.exe"));
    wrapper.args(["-NoLogo", "-NoProfile", "-EncodedCommand", &encoded]);
    spawn_console(&wrapper)
}

/// A PowerShell string in single quotes. PowerShell takes the typographic quotes
/// `‘ ’ ‚ ‛` for single quotes too, so each is doubled like `'`: a title or a block with
/// `don’t` stays one string.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn ps_quoted(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('\'');
    for c in text.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            quoted.push(c);
        }
        quoted.push(c);
    }
    quoted.push('\'');
    quoted
}

/// The encoded script grows about 2.7 times; a command line holds 32767 characters.
#[cfg(windows)]
const MAX_ENCODED: usize = 30_000;

/// A PowerShell script for `-EncodedCommand` (base64 of UTF-16LE): it reaches PowerShell
/// whole, where `-Command` would lose the double quotes inside it.
#[cfg(windows)]
pub(crate) fn powershell_encoded(script: &str) -> String {
    use base64::Engine as _;
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(utf16)
}

/// Starts a program in a console window of its own, with that console as its input and
/// output. `Command` would hand the child the standard handles of this process (a pipe
/// under a test, the console of the terminal that started a debug build), and the new
/// window would show nothing and read nothing.
#[cfg(windows)]
fn spawn_console(command: &Command) -> io::Result<()> {
    let mut words = vec![command.get_program().to_string_lossy().into_owned()];
    words.extend(command.get_args().map(|a| a.to_string_lossy().into_owned()));
    spawn_console_line(&windows_command_line(&words), command.get_current_dir())
}

/// [`spawn_console`] with a command line as it is, for programs that split their own
/// (`cmd.exe`).
#[cfg(windows)]
fn spawn_console_line(line: &str, cwd: Option<&std::path::Path>) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_CONSOLE, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW,
    };
    let mut line: Vec<u16> = line.encode_utf16().chain([0]).collect();
    let cwd: Option<Vec<u16>> = cwd.map(|dir| dir.as_os_str().encode_wide().chain([0]).collect());
    let startup = STARTUPINFOW { cb: std::mem::size_of::<STARTUPINFOW>() as u32, ..unsafe { std::mem::zeroed() } };
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the command line is a writable, NUL-terminated buffer as CreateProcessW asks;
    // the folder is NUL-terminated or null; the structures live across the call.
    let created = unsafe {
        CreateProcessW(
            std::ptr::null(),
            line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_NEW_CONSOLE | CREATE_UNICODE_ENVIRONMENT,
            std::ptr::null(),
            cwd.as_ref().map_or(std::ptr::null(), |dir| dir.as_ptr()),
            &startup,
            &mut process,
        )
    };
    if created == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both handles came from the call above and are closed once.
    unsafe {
        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
    }
    Ok(())
}

/// A command line of arguments as Windows programs split it (the rules of the C runtime):
/// an argument with a space, a tab or a quote goes in quotes, its quotes and the
/// backslashes before them escaped.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_command_line(args: &[String]) -> String {
    let mut line = String::new();
    for arg in args {
        if !line.is_empty() {
            line.push(' ');
        }
        if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
            line.push_str(arg);
            continue;
        }
        line.push('"');
        let mut backslashes = 0;
        for c in arg.chars() {
            match c {
                '\\' => backslashes += 1,
                '"' => {
                    line.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                    line.push('"');
                    backslashes = 0;
                }
                _ => {
                    line.extend(std::iter::repeat_n('\\', backslashes));
                    line.push(c);
                    backslashes = 0;
                }
            }
        }
        line.extend(std::iter::repeat_n('\\', backslashes * 2));
        line.push('"');
    }
    line
}

#[cfg(not(windows))]
fn spawn_ssh_terminal(remote: Command) -> io::Result<()> {
    let program = remote.get_program().to_string_lossy().into_owned();
    let args: Vec<String> = remote.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
    spawn_local_terminal(None, Some((&program, &args)))
}

#[cfg(windows)]
fn spawn_local_terminal(cwd: Option<&str>, program: Option<(&str, &[String])>) -> io::Result<()> {
    let mut command = Command::new(envs::system_program("powershell.exe"));
    command.arg("-NoLogo").arg("-NoExit");
    if let Some((program, args)) = program {
        command.arg("-Command").arg(powershell_line(program, args));
    }
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    spawn_console(&command)
}

#[cfg(target_os = "macos")]
fn spawn_local_terminal(cwd: Option<&str>, program: Option<(&str, &[String])>) -> io::Result<()> {
    let mut script = String::new();
    if let Some(cwd) = cwd {
        script.push_str(&format!("cd {}; ", shell_quote(cwd)));
    }
    if let Some((program, args)) = program {
        script.push_str(&shell_quote(program));
        for arg in args {
            script.push(' ');
            script.push_str(&shell_quote(arg));
        }
    }
    let apple = format!("tell application \"Terminal\" to do script \"{}\"", script.replace('\\', "\\\\").replace('"', "\\\""));
    Command::new("osascript").arg("-e").arg(apple).spawn().map(drop)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn spawn_local_terminal(cwd: Option<&str>, program: Option<(&str, &[String])>) -> io::Result<()> {
    let mut inner = String::new();
    if let Some((program, args)) = program {
        inner.push_str(&shell_quote(program));
        for arg in args {
            inner.push(' ');
            inner.push_str(&shell_quote(arg));
        }
        inner.push_str("; exec \"${SHELL:-sh}\"");
    }
    let candidates: Vec<String> = std::env::var("TERMINAL").into_iter().chain(
        ["x-terminal-emulator", "gnome-terminal", "konsole", "xfce4-terminal", "alacritty", "kitty", "xterm"].map(String::from),
    ).collect();
    let mut last_error = io::Error::new(io::ErrorKind::NotFound, "no terminal found");
    for terminal in candidates {
        let mut command = Command::new(&terminal);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        if !inner.is_empty() {
            if terminal == "gnome-terminal" {
                command.args(["--", "sh", "-c", &inner]);
            } else {
                command.args(["-e", "sh", "-c", &inner]);
            }
        }
        match command.spawn() {
            Ok(_) => return Ok(()),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

/// The shell a fenced code block is written for, if it is one a terminal can run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shell {
    PowerShell,
    Posix,
    Cmd,
}

impl Shell {
    pub fn of_tag(tag: &str) -> Option<Shell> {
        match tag.trim().to_ascii_lowercase().as_str() {
            "powershell" | "pwsh" | "ps1" | "ps" => Some(Shell::PowerShell),
            "bash" | "sh" | "shell" | "zsh" | "console" | "terminal" | "shellscript" => Some(Shell::Posix),
            "cmd" | "bat" | "batch" => Some(Shell::Cmd),
            _ => None,
        }
    }
}

/// Runs a block of shell code in a new terminal in `cwd`; the terminal stays open with a
/// shell after it. On a remote host it runs in the user's shell there.
pub fn run_in_terminal(env: &dyn Environment, cwd: Option<&str>, shell: Shell, script: &str) -> io::Result<()> {
    let script = script.trim();
    if !env.id().is_local() {
        let keep = format!("{script}\nexec \"${{SHELL:-sh}}\" -l");
        return open_terminal(env, cwd, Some(("sh", &["-c".to_owned(), keep])));
    }
    #[cfg(windows)]
    {
        let mut command = match shell {
            // The script goes encoded: `-Command` would drop its double quotes.
            Shell::PowerShell => {
                let mut command = Command::new(envs::system_program("powershell.exe"));
                let encoded = powershell_encoded(script);
                if encoded.len() <= MAX_ENCODED {
                    command.args(["-NoLogo", "-NoExit", "-EncodedCommand", &encoded]);
                } else {
                    command.args(["-NoLogo", "-NoExit", "-Command", script]);
                }
                command
            }
            // cmd reads its command line as it is: `/S /K "…"` drops the outer quotes and
            // keeps every other character of the block.
            Shell::Cmd => {
                return spawn_console_line(&format!("{} /D /S /K \"{script}\"", envs::system_program_line("cmd.exe")), cwd.map(std::path::Path::new));
            }
            // Bash code goes to the bash on the PATH (Git Bash, WSL), started directly: Windows
            // PowerShell would drop the double quotes inside it. A shell stays after it.
            Shell::Posix => {
                let mut command = Command::new(windows_bash());
                command.args(["-c", &format!("{script}\nexec bash -i")]);
                command
            }
        };
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        spawn_console(&command)
    }
    #[cfg(not(windows))]
    {
        let _ = shell;
        spawn_local_terminal(cwd, Some(("sh", &["-c".to_owned(), script.to_owned()])))
    }
}

/// The bash for a bash block on Windows: the first on the PATH that is not the WSL
/// launcher (System32, WindowsApps), then Git's own, then WSL. Windows itself would find
/// WSL's `System32\bash.exe` first, which runs the block in Linux.
#[cfg(windows)]
fn windows_bash() -> std::path::PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let on_path = std::env::split_paths(&path)
        .filter(|dir| {
            let dir = dir.to_string_lossy().to_lowercase();
            !dir.contains("\\system32") && !dir.contains("windowsapps")
        })
        .map(|dir| dir.join("bash.exe"))
        .find(|bash| bash.is_file());
    let git = || {
        ["ProgramFiles", "ProgramW6432", "LocalAppData"]
            .into_iter()
            .filter_map(std::env::var_os)
            .flat_map(|root| {
                let root = std::path::PathBuf::from(root);
                [root.join("Git\\bin\\bash.exe"), root.join("Programs\\Git\\bin\\bash.exe")]
            })
            .find(|bash| bash.is_file())
    };
    on_path.or_else(git).unwrap_or_else(|| "bash".into())
}

/// Resumes a session in its harness, in its folder, in a terminal of the system.
pub fn resume_in_terminal(env: &dyn Environment, summary: &SessionSummary) -> io::Result<()> {
    let (program, args) = harness_command(&summary.key.harness, Some(&summary.key.id));
    open_terminal(env, summary.cwd.as_deref(), Some((&program, &args)))
}

// The terminal of the window ---------------------------------------------------------------
//
// The same commands as the external terminals above, as one command line for the
// pseudoterminal of the `terminal` crate: `CreateProcessW` on Windows, `/bin/sh -c` on Unix.
// `None` when there is no line to give (no shell found, a block too long to encode): the
// caller opens an external terminal then.

/// What runs in a terminal of the window: its command line and the local folder it starts in.
pub struct TerminalLine {
    pub line: String,
    /// The folder on this computer; a remote host gets its folder inside the line.
    pub cwd: Option<std::path::PathBuf>,
    /// Variables the program needs besides the window's own: the askpass of a password host.
    pub env: Vec<(String, String)>,
}

fn line_of(command: &Command) -> String {
    let args: Vec<String> = command.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
    terminal::command_line(std::path::Path::new(command.get_program()), &args)
}

/// [`open_terminal`] for the terminal of the window.
/// The shell the user chose for the terminal of the window, as a command line; `None` lets
/// the terminal crate pick one.
static LOCAL_SHELL: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

pub fn set_local_shell(command_line: Option<String>) {
    if let Ok(mut shell) = LOCAL_SHELL.lock() {
        *shell = command_line;
    }
}

fn local_shell() -> Option<String> {
    LOCAL_SHELL.lock().ok().and_then(|shell| shell.clone()).or_else(terminal::default_shell)
}

pub fn terminal_line(env: &dyn Environment, cwd: Option<&str>, program: Option<(&str, &[String])>) -> Option<TerminalLine> {
    if !env.id().is_local() {
        let (program, args): (&str, Vec<&str>) = match program {
            Some((program, args)) => (program, args.iter().map(String::as_str).collect()),
            None => ("", Vec::new()),
        };
        let command = env.interactive(program, &args, cwd);
        let vars = command.get_envs().filter_map(|(key, value)| Some((key.to_string_lossy().into_owned(), value?.to_string_lossy().into_owned()))).collect();
        return Some(TerminalLine { line: line_of(&command), cwd: None, env: vars });
    }
    let cwd = cwd.map(std::path::PathBuf::from);
    let line = match program {
        None => local_shell()?,
        // A harness on Windows is often an npm `.cmd` shim: PowerShell finds and runs it, and
        // stays after it.
        #[cfg(windows)]
        Some((program, args)) => {
            let encoded = powershell_encoded(&powershell_line(program, args));
            (encoded.len() <= MAX_ENCODED).then(|| format!("{} -NoLogo -NoExit -EncodedCommand {encoded}", envs::system_program_line("powershell.exe")))?
        }
        #[cfg(not(windows))]
        Some((program, args)) => {
            let mut inner = shell_quote(program);
            for arg in args {
                inner.push(' ');
                inner.push_str(&shell_quote(arg));
            }
            inner.push_str("; exec \"${SHELL:-sh}\"");
            terminal::command_line(std::path::Path::new("sh"), &["-c".to_owned(), inner])
        }
    };
    Some(TerminalLine { line, cwd, env: Vec::new() })
}

/// A harness's own command (its update) for the terminal of the window: the program alone,
/// so the tab ends when it does and the window knows the command is over. On this computer
/// the program found on PATH (an npm shim runs its script with node); on a host the login
/// shell, whose PATH has the harness. The words hold no quote.
pub fn harness_tool_line(env: &dyn Environment, program: &str, args: &[&str]) -> Option<TerminalLine> {
    if env.id().is_local() {
        let (path, prefix) = live::resolve_program(program)?;
        let args: Vec<String> = prefix.into_iter().chain(args.iter().map(|arg| (*arg).to_owned())).collect();
        return Some(TerminalLine { line: terminal::command_line(&path, &args), cwd: None, env: Vec::new() });
    }
    let words = std::iter::once(program).chain(args.iter().copied()).collect::<Vec<_>>().join(" ");
    terminal_line(env, None, Some(("sh", &["-c".to_owned(), format!("exec \"${{SHELL:-sh}}\" -lc '{words}'")])))
}

/// [`run_in_terminal`] for the terminal of the window.
pub fn block_line(env: &dyn Environment, cwd: Option<&str>, shell: Shell, script: &str) -> Option<TerminalLine> {
    let script = script.trim();
    if !env.id().is_local() {
        let keep = format!("{script}\nexec \"${{SHELL:-sh}}\" -l");
        return terminal_line(env, cwd, Some(("sh", &["-c".to_owned(), keep])));
    }
    let cwd_path = cwd.map(std::path::PathBuf::from);
    #[cfg(windows)]
    let line = match shell {
        Shell::PowerShell => {
            let encoded = powershell_encoded(script);
            (encoded.len() <= MAX_ENCODED).then(|| format!("{} -NoLogo -NoExit -EncodedCommand {encoded}", envs::system_program_line("powershell.exe")))?
        }
        Shell::Cmd => format!("{} /D /S /K \"{script}\"", envs::system_program_line("cmd.exe")),
        Shell::Posix => terminal::command_line(&windows_bash(), &["-c".to_owned(), format!("{script}\nexec bash -i")]),
    };
    #[cfg(not(windows))]
    let line = {
        let _ = shell;
        terminal::command_line(std::path::Path::new("sh"), &["-c".to_owned(), format!("{script}\nexec \"${{SHELL:-sh}}\"")])
    };
    Some(TerminalLine { line, cwd: cwd_path, env: Vec::new() })
}

/// [`resume_in_terminal`] for the terminal of the window.
pub fn resume_line(env: &dyn Environment, summary: &SessionSummary) -> Option<TerminalLine> {
    let (program, args) = harness_command(&summary.key.harness, Some(&summary.key.id));
    terminal_line(env, summary.cwd.as_deref(), Some((&program, &args)))
}

/// Asks the harness to compact the session: Claude Code takes `/compact` as a prompt of a
/// resumed session. Other harnesses do it from their own interface.
pub fn compact_in_terminal(env: &dyn Environment, summary: &SessionSummary) -> io::Result<()> {
    match summary.key.harness {
        HarnessKind::Claude => {
            let args = vec!["--resume".to_owned(), summary.key.id.clone(), "/compact".to_owned()];
            open_terminal(env, summary.cwd.as_deref(), Some(("claude", &args)))
        }
        _ => resume_in_terminal(env, summary),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_flags_per_harness() {
        assert_eq!(harness_command(&HarnessKind::Claude, Some("abc")).1, vec!["--resume", "abc"]);
        assert_eq!(harness_command(&HarnessKind::Codex, Some("abc")).1, vec!["resume", "abc"]);
        assert_eq!(harness_command(&HarnessKind::OpenCode, Some("ses_1")).1, vec!["--session", "ses_1"]);
        assert_eq!(harness_command(&HarnessKind::DeepSeek, None), ("dsh".to_owned(), vec!["web".to_owned()]));
    }

    #[test]
    fn powershell_quotes_odd_words() {
        assert_eq!(powershell_line("claude", &["--resume".into(), "it's".into()]), "& claude --resume 'it''s'");
        assert_eq!(ps_quoted("don\u{2019}t ‘x’ ‚y‛"), "'don\u{2019}\u{2019}t ‘‘x’’ ‚‚y‛‛'");
    }

    /// Opens a real terminal window: run by hand with `--ignored`, then close the window.
    #[test]
    #[ignore]
    fn a_block_runs_in_a_terminal() {
        let env = envs::LocalEnvironment::new();
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        run_in_terminal(&env, Some(&cwd), Shell::PowerShell, "Write-Output 'it''s the Bowyer run test'; Get-Location").unwrap();
    }

    /// Opens a terminal to a host that does not exist: the window stays with what `ssh`
    /// said until Enter. Run by hand with `--ignored`.
    #[test]
    #[ignore]
    fn a_terminal_to_a_missing_host_keeps_the_error() {
        let env = envs::SshEnvironment::new("nobody@bowyer-missing.invalid");
        open_terminal(&env, Some("/tmp"), None).unwrap();
    }

    /// Blocks of each local shell run in their own console in a folder with a space and
    /// keep their quotes: each writes `a  b` (two spaces) to a file there. Opens windows
    /// that close themselves; run by hand with `--ignored`.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn local_blocks_keep_their_folder_and_quotes() {
        let env = envs::LocalEnvironment::new();
        let dir = std::env::temp_dir().join(format!("bowyer run {}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cwd = dir.to_string_lossy().into_owned();
        let blocks = [
            (Shell::PowerShell, "Set-Content -Path ps.txt -Value \"a  b\" -NoNewline; exit"),
            (Shell::Cmd, "echo \"a  b\"> cmd.txt & exit"),
            (Shell::Posix, "printf '%s' \"a  b\" > bash.txt; exit"),
        ];
        for (shell, script) in blocks {
            run_in_terminal(&env, Some(&cwd), shell, script).unwrap();
        }
        let wait = |name: &str| {
            for _ in 0..100 {
                if let Ok(text) = std::fs::read_to_string(dir.join(name))
                    && !text.is_empty()
                {
                    return text;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            panic!("{name} was not written");
        };
        assert_eq!(wait("ps.txt"), "a  b");
        assert_eq!(wait("cmd.txt").trim_end(), "\"a  b\"");
        assert_eq!(wait("bash.txt"), "a  b");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Opens a shell on a real host in `/tmp`: the window stays with it. Close it by hand.
    #[test]
    #[ignore]
    fn a_terminal_opens_a_shell_on_a_real_host() {
        let host = std::env::var("BOWYER_SSH_HOST").expect("BOWYER_SSH_HOST");
        let key = std::env::var_os("BOWYER_SSH_KEY").map(std::path::PathBuf::from);
        let env = envs::SshEnvironment::new(&host).with_identity(key);
        open_terminal(&env, Some("/tmp"), None).unwrap();
    }

    /// A block run on a real host keeps its quotes: it writes `a  b don’t` (two spaces) to
    /// `/tmp/bowyer-run-test`. `BOWYER_SSH_HOST=user@host BOWYER_SSH_KEY=key`.
    #[test]
    #[ignore]
    fn a_block_runs_in_a_terminal_on_a_real_host() {
        let host = std::env::var("BOWYER_SSH_HOST").expect("BOWYER_SSH_HOST");
        let key = std::env::var_os("BOWYER_SSH_KEY").map(std::path::PathBuf::from);
        let env = envs::SshEnvironment::new(&host).with_identity(key);
        run_in_terminal(&env, Some("/tmp"), Shell::Posix, "echo \"a  b don\u{2019}t\" > /tmp/bowyer-run-test; exit").unwrap();
    }

    #[test]
    fn a_windows_command_line_keeps_quotes_and_backslashes() {
        let args = ["-o".to_owned(), "BatchMode=yes".to_owned(), r#"cd "$HOME"/a\ b && echo "x  y""#.to_owned(), String::new(), r"C:\dir\".to_owned()];
        assert_eq!(
            windows_command_line(&args),
            r#"-o BatchMode=yes "cd \"$HOME\"/a\ b && echo \"x  y\"" "" C:\dir\"#
        );
        assert_eq!(windows_command_line(&[r#"a\"b"#.to_owned()]), r#""a\\\"b""#);
    }

    #[test]
    fn shell_blocks_are_known_by_their_tag() {
        assert_eq!(Shell::of_tag("powershell"), Some(Shell::PowerShell));
        assert_eq!(Shell::of_tag("Bash"), Some(Shell::Posix));
        assert_eq!(Shell::of_tag("cmd"), Some(Shell::Cmd));
        assert_eq!(Shell::of_tag("rust"), None);
        assert_eq!(Shell::of_tag(""), None);
    }
}

