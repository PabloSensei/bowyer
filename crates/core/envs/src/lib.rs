//! Where harnesses run and keep their history: this computer, or a host over SSH.
//!
//! Every reader of history and every launcher goes through [`Environment`], never through
//! `std::fs` directly, so a remote machine works like the local one. The calls block; the
//! window runs them off its thread. A remote call is one round trip, so the interface asks
//! for whole listings and byte ranges instead of single stats.

use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use model::EnvId;

mod git;
mod local;
pub mod secret;
mod session;
mod ssh;

pub use git::{GitInfo, git_info_local, parse_head};
pub use local::LocalEnvironment;
pub use ssh::{
    ASKPASS_HOST, HostKeys, SshEnvironment, SshHost, askpass_answer, is_unknown_host_key, read_ssh_config, set_askpass, set_known_hosts,
    shell_quote,
};

/// A file as a listing returns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMeta {
    pub path: String,
    pub size: u64,
    /// Milliseconds since the Unix epoch.
    pub modified_ms: i64,
    pub is_dir: bool,
}

/// Which bytes of a file to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadRange {
    All,
    /// The first bytes.
    Head(u64),
    /// The last bytes.
    Tail(u64),
    /// Everything from this byte on, to read on in a file that grows.
    From(u64),
}

pub trait Environment: Send + Sync {
    fn id(&self) -> EnvId;

    /// A name for people: "This computer", "dev-vm".
    fn label(&self) -> String;

    fn is_windows(&self) -> bool;

    /// The home folder of the user the harnesses run as.
    fn home(&self) -> io::Result<String>;

    /// Joins paths with the separator of the environment.
    fn join(&self, base: &str, child: &str) -> String {
        let sep = if self.is_windows() { '\\' } else { '/' };
        if base.ends_with(['/', '\\']) {
            format!("{base}{child}")
        } else {
            format!("{base}{sep}{child}")
        }
    }

    /// Files under `root` whose name ends with `suffix`, at most `max_depth` folders down.
    /// A missing root is an empty listing.
    fn find_files(&self, root: &str, suffix: &str, max_depth: u32) -> io::Result<Vec<FileMeta>>;

    /// The entries of a folder, folders first, then by name.
    fn read_dir(&self, path: &str) -> io::Result<Vec<FileMeta>>;

    fn read(&self, path: &str, range: ReadRange) -> io::Result<Vec<u8>>;

    /// Many reads at once. A remote environment answers them in one round trip; a file
    /// that cannot be read comes back as an error in its place.
    fn read_many(&self, requests: &[(String, ReadRange)]) -> Vec<io::Result<Vec<u8>>> {
        requests.iter().map(|(path, range)| self.read(path, *range)).collect()
    }

    fn stat(&self, path: &str) -> io::Result<FileMeta>;

    /// A path on this computer that holds the file, for formats that need a real file such as
    /// SQLite. The local environment returns the path itself; a remote one copies the file
    /// (and its `-wal` journal) into `cache` first.
    fn local_copy(&self, path: &str, cache: &std::path::Path) -> io::Result<PathBuf>;

    /// Runs a command to the end and returns its standard output.
    fn run(&self, program: &str, args: &[&str], cwd: Option<&str>) -> io::Result<String>;

    /// Like [`Environment::run`], with `input` on the command's standard input: a patch
    /// for `git apply`.
    fn run_with_input(&self, program: &str, args: &[&str], cwd: Option<&str>, input: &[u8]) -> io::Result<String>;

    /// A command that starts `program` in `cwd` interactively, for a terminal or a harness.
    /// Over SSH it asks for a terminal on the remote side.
    fn interactive(&self, program: &str, args: &[&str], cwd: Option<&str>) -> Command;

    /// The branch and state of the repository that holds `cwd`.
    fn git_info(&self, cwd: &str) -> Option<GitInfo>;
}

pub type SharedEnv = Arc<dyn Environment>;

/// Text of bytes that may end inside a character, as a range read leaves them.
pub fn lossy_lines(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Runs a prepared command with `input` on its standard input and returns its standard
/// output; a failure carries the command's error output.
pub(crate) fn output_with_input(mut command: Command, input: &[u8]) -> io::Result<String> {
    use std::io::Write;
    use std::process::Stdio;
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let bytes = input.to_vec();
    // A writer thread, so a command that answers before reading everything cannot block us.
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&bytes);
    });
    let output = child.wait_with_output()?;
    let _ = writer.join();
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(io::Error::other(if message.is_empty() { format!("exit {}", output.status) } else { message }));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub(crate) fn not_found(path: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("{path}: not found"))
}
