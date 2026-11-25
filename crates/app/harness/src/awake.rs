//! Keeps the computer from sleeping while agents work: the system may still turn the screen
//! off, but does not suspend. The hold goes with Bowyer: it ends when the process does.
//!
//! - Windows: `SetThreadExecutionState`, set and cleared on the window's thread (the state
//!   belongs to the thread that set it).
//! - macOS: `caffeinate -i -w <pid>` while held; it also ends with Bowyer.
//! - Linux: `systemd-inhibit --what=idle:sleep` around `cat` on a pipe of ours: closing the
//!   pipe, or Bowyer ending, ends `cat` and the hold with it.

/// The hold, taken and given back as the work starts and ends.
#[derive(Default)]
pub struct Awake {
    held: bool,
    #[cfg(not(windows))]
    child: Option<std::process::Child>,
}

impl Awake {
    /// Holds the computer awake or lets it sleep again; nothing when it already is so.
    pub fn set(&mut self, hold: bool) {
        if hold == self.held {
            return;
        }
        self.held = hold;
        if hold { self.take() } else { self.give_back() }
    }

    #[cfg(windows)]
    fn take(&mut self) {
        use windows_sys::Win32::System::Power::{ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState};
        // SAFETY: a plain call with flags; the state stays with this thread until changed.
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED);
        }
    }

    #[cfg(windows)]
    fn give_back(&mut self) {
        use windows_sys::Win32::System::Power::{ES_CONTINUOUS, SetThreadExecutionState};
        // SAFETY: as above; `ES_CONTINUOUS` alone clears what this thread asked for.
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS);
        }
    }

    #[cfg(not(windows))]
    fn take(&mut self) {
        use std::process::{Command, Stdio};
        let mut command;
        #[cfg(target_os = "macos")]
        {
            command = Command::new("/usr/bin/caffeinate");
            command.args(["-i", "-w", &std::process::id().to_string()]).stdin(Stdio::null());
        }
        #[cfg(not(target_os = "macos"))]
        {
            command = Command::new("systemd-inhibit");
            command
                .args(["--what=idle:sleep", "--who=Bowyer", "--why=Agents are working", "--mode=block", "cat"])
                .stdin(Stdio::piped());
        }
        command.stdout(Stdio::null()).stderr(Stdio::null());
        // No such program: the computer sleeps as it would without Bowyer.
        self.child = command.spawn().ok();
    }

    #[cfg(not(windows))]
    fn give_back(&mut self) {
        if let Some(mut child) = self.child.take() {
            // The pipe closes first: `cat` ends and `systemd-inhibit` lets go by itself.
            drop(child.stdin.take());
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Awake {
    fn drop(&mut self) {
        self.set(false);
    }
}

#[cfg(all(test, windows))]
mod tests {
    use windows_sys::Win32::System::Power::{ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState};

    use super::Awake;

    /// What this thread asks of the system now (setting the same again to read it).
    fn asked() -> u32 {
        // SAFETY: plain calls with flags; the second puts back what the first read.
        unsafe {
            let previous = SetThreadExecutionState(ES_CONTINUOUS);
            SetThreadExecutionState(previous);
            previous
        }
    }

    #[test]
    fn the_hold_is_taken_and_given_back() {
        let mut awake = Awake::default();
        awake.set(true);
        assert_eq!(asked() & ES_SYSTEM_REQUIRED, ES_SYSTEM_REQUIRED, "the system is asked to stay up");
        if let Ok(requests) = std::process::Command::new("powercfg").arg("/requests").output() {
            eprintln!("{}", String::from_utf8_lossy(&requests.stdout));
        }
        awake.set(false);
        assert_eq!(asked() & ES_SYSTEM_REQUIRED, 0, "and let go");
        awake.set(true);
        drop(awake);
        assert_eq!(asked() & ES_SYSTEM_REQUIRED, 0, "a hold dropped lets go");
    }
}
