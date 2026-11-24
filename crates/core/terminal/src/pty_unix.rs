//! A pseudoterminal and its process. The pair comes from `posix_openpt`, the child is
//! started in a session of its own with the slave as its terminal, so Ctrl+C reaches the
//! foreground program and a job can signal the whole session.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::session::SpawnSpec;

/// Quotes one argument for a POSIX shell.
fn push_argument(line: &mut String, argument: &str) {
    if !argument.is_empty()
        && argument
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_./:=@%+,".contains(&byte))
    {
        line.push_str(argument);
        return;
    }
    line.push('\'');
    for ch in argument.chars() {
        if ch == '\'' {
            line.push_str("'\\''");
        } else {
            line.push(ch);
        }
    }
    line.push('\'');
}

/// One line for `/bin/sh -c` from a program and its arguments.
pub fn command_line(program: &Path, arguments: &[String]) -> String {
    let mut line = String::new();
    push_argument(&mut line, &program.to_string_lossy());
    for argument in arguments {
        line.push(' ');
        push_argument(&mut line, argument);
    }
    line
}

fn last_error(context: &str) -> io::Error {
    io::Error::other(format!("{context}: {}", io::Error::last_os_error()))
}

fn set_size(master: &File, cols: u16, rows: u16) -> io::Result<()> {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `TIOCSWINSZ` reads the size structure, which lives for the call.
    let done = unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size) };
    if done < 0 {
        return Err(last_error("TIOCSWINSZ"));
    }
    Ok(())
}

pub(crate) struct Pty {
    master: File,
    child: Mutex<Option<Child>>,
    pub(crate) pid: u32,
    /// The terminal as `pkill -t` names it (`pts/3`, `ttys005`): every process that still
    /// has it as its own terminal was started in it.
    tty: String,
    /// The child has exited: its number may belong to another process soon, so it is not
    /// signalled any more. Set before the child is reaped.
    exited: AtomicBool,
}

impl Pty {
    /// Returns the host plus the writer to the child's input and the reader of its output.
    pub(crate) fn spawn(spec: &SpawnSpec) -> io::Result<(Pty, File, File)> {
        // SAFETY: plain calls; every descriptor is checked.
        let master = unsafe {
            let descriptor = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
            if descriptor < 0 {
                return Err(last_error("posix_openpt"));
            }
            let master = File::from_raw_fd(descriptor);
            if libc::grantpt(descriptor) != 0 {
                return Err(last_error("grantpt"));
            }
            if libc::unlockpt(descriptor) != 0 {
                return Err(last_error("unlockpt"));
            }
            master
        };
        let name = slave_name(&master)?;
        // `O_NOCTTY`: a window started without a terminal must not take this one as its
        // own (a session leader opening a terminal would); only the child's session does.
        let slave = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC)
            .open(&name)?;
        // macOS refuses the size of a terminal whose slave side nobody has opened yet.
        set_size(&master, spec.cols, spec.rows)?;

        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(&spec.command_line)
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave))
            .env_clear();
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        // A session of its own is what makes the slave the controlling terminal, so the
        // foreground program receives the signals of this terminal and nobody else.
        // SAFETY: both calls are async-signal-safe and the child is between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        let pid = child.id();
        let writer = master.try_clone()?;
        let reader = master.try_clone()?;
        Ok((
            Pty {
                master,
                child: Mutex::new(Some(child)),
                pid,
                tty: name.trim_start_matches("/dev/").to_owned(),
                exited: AtomicBool::new(false),
            },
            writer,
            reader,
        ))
    }

    pub(crate) fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        set_size(&self.master, cols, rows)
    }

    /// Blocks until the child exits.
    pub(crate) fn wait(&self) -> u32 {
        use std::os::unix::process::ExitStatusExt;

        let mut child = self
            .child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(child) = child.as_mut() else {
            return 1;
        };
        // Wait without reaping first: until the flag is up the number is still the child's,
        // so a `terminate` meanwhile cannot hit a process that took it over.
        // SAFETY: `info` is written by the call and read by nobody.
        unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            while libc::waitid(libc::P_PID, self.pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOWAIT) != 0
                && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted
            {}
        }
        self.exited.store(true, Ordering::SeqCst);
        match child.wait() {
            Ok(status) => status.code().map_or_else(
                || 128 + status.signal().unwrap_or(0) as u32,
                |code| code as u32,
            ),
            Err(_) => 1,
        }
    }

    /// The output stream ends when the child closes the slave; nothing else is needed.
    pub(crate) fn close_console(&self) {}

    /// Kills the child and everything it started. The shell puts each job in a process
    /// group of its own, so the group of the child is not enough: every process of the
    /// child's session goes, background jobs too, and with them the last holders of the
    /// terminal, so the tab ends.
    pub(crate) fn terminate(&self) {
        // A session lives as long as one of its processes, and its number is not given to a
        // new process meanwhile: `pkill -s` reaches only what the terminal started. The
        // `pkill` of macOS does not find a session's processes that way, so the processes
        // whose terminal this is go too (a job of the shell keeps it; a daemon that left it,
        // as daemons do, stays). The name is ours while `master` is open, which it is here
        // and in `drop`: no other terminal can have it yet.
        let pkill = |args: &[&str]| {
            let _ = Command::new("pkill").args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
        };
        // By the terminal first: once the session's leader is gone, macOS takes the terminal
        // away from the rest of the session and `-t` finds nothing more.
        if !self.tty.is_empty() {
            pkill(&["-KILL", "-t", &self.tty]);
        }
        pkill(&["-KILL", "-s", &self.pid.to_string()]);
        if !self.exited.load(Ordering::SeqCst) {
            // SAFETY: the child has not exited, so the number is still its own.
            unsafe {
                libc::kill(-(self.pid as i32), libc::SIGKILL);
                libc::kill(self.pid as i32, libc::SIGKILL);
            }
        }
    }

    /// The session of the child is the unit here, so there is nothing to pull in.
    pub(crate) fn adopt(&self) -> usize {
        0
    }

    /// `terminate` already signals the whole session of the child.
    pub(crate) fn kill_tree(&self) {}
}

impl Drop for Pty {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(not(target_os = "macos"))]
use libc::ptsname_r;

// The libc crate does not declare ptsname_r for macOS; the system has it since 10.13.4.
#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn ptsname_r(fd: libc::c_int, buffer: *mut libc::c_char, length: libc::size_t) -> libc::c_int;
}

/// The path of the slave side of a pseudoterminal.
fn slave_name(master: &File) -> io::Result<String> {
    let mut buffer = [0u8; 128];
    // SAFETY: the buffer is writable for its whole length and the descriptor is ours.
    let done = unsafe { ptsname_r(master.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
    if done != 0 {
        // Linux returns the error number, macOS returns -1 and sets errno.
        return Err(if done > 0 {
            io::Error::from_raw_os_error(done)
        } else {
            io::Error::last_os_error()
        });
    }
    // SAFETY: a successful call wrote a zero-terminated name into the buffer.
    let name = unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr().cast()) };
    Ok(name.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(arguments: &[&str]) -> String {
        let arguments: Vec<String> = arguments.iter().map(|text| text.to_string()).collect();
        command_line(Path::new("/usr/bin/tool"), &arguments)
    }

    #[test]
    fn arguments_are_quoted_for_a_shell() {
        assert_eq!(line(&["plain"]), "/usr/bin/tool plain");
        assert_eq!(line(&[""]), "/usr/bin/tool ''");
        assert_eq!(line(&["two words"]), "/usr/bin/tool 'two words'");
        assert_eq!(line(&["say 'hi'"]), "/usr/bin/tool 'say '\\''hi'\\'''");
        assert_eq!(line(&["/a:b"]), "/usr/bin/tool /a:b");
    }
}
