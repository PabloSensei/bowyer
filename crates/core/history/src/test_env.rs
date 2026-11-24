//! Environments for tests that fail where real ones do.

use std::io;
use std::path::Path;

use envs::{Environment, LocalEnvironment, ReadRange};

/// This computer, with reads that fail as over a connection that dropped: every read, or
/// the reads of paths that hold a part.
pub(crate) struct Unreadable {
    inner: LocalEnvironment,
    only: Option<&'static str>,
}

impl Unreadable {
    pub(crate) fn all(inner: LocalEnvironment) -> Self {
        Self { inner, only: None }
    }

    pub(crate) fn paths_with(inner: LocalEnvironment, part: &'static str) -> Self {
        Self { inner, only: Some(part) }
    }
}

impl Environment for Unreadable {
    fn id(&self) -> model::EnvId {
        self.inner.id()
    }
    fn label(&self) -> String {
        self.inner.label()
    }
    fn is_windows(&self) -> bool {
        self.inner.is_windows()
    }
    fn home(&self) -> io::Result<String> {
        self.inner.home()
    }
    fn find_files(&self, root: &str, suffix: &str, max_depth: u32) -> io::Result<Vec<envs::FileMeta>> {
        self.inner.find_files(root, suffix, max_depth)
    }
    fn read_dir(&self, path: &str) -> io::Result<Vec<envs::FileMeta>> {
        self.inner.read_dir(path)
    }
    fn read(&self, path: &str, range: ReadRange) -> io::Result<Vec<u8>> {
        if self.only.is_none_or(|part| path.contains(part)) {
            return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "the connection dropped"));
        }
        self.inner.read(path, range)
    }
    fn read_many(&self, requests: &[(String, ReadRange)]) -> Vec<io::Result<Vec<u8>>> {
        requests.iter().map(|(path, range)| self.read(path, *range)).collect()
    }
    fn stat(&self, path: &str) -> io::Result<envs::FileMeta> {
        self.inner.stat(path)
    }
    fn local_copy(&self, path: &str, cache: &Path) -> io::Result<std::path::PathBuf> {
        self.inner.local_copy(path, cache)
    }
    fn run(&self, program: &str, args: &[&str], cwd: Option<&str>) -> io::Result<String> {
        self.inner.run(program, args, cwd)
    }
    fn run_with_vars(&self, program: &str, args: &[&str], cwd: Option<&str>, vars: &[(&str, &str)]) -> io::Result<String> {
        self.inner.run_with_vars(program, args, cwd, vars)
    }
    fn run_with_input(&self, program: &str, args: &[&str], cwd: Option<&str>, input: &[u8]) -> io::Result<String> {
        self.inner.run_with_input(program, args, cwd, input)
    }
    fn interactive(&self, program: &str, args: &[&str], cwd: Option<&str>) -> std::process::Command {
        self.inner.interactive(program, args, cwd)
    }
    fn git_info(&self, cwd: &str) -> Option<envs::GitInfo> {
        self.inner.git_info(cwd)
    }
}
