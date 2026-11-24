use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

use model::EnvId;

use crate::{Environment, FileMeta, GitInfo, ReadRange, git};

/// This computer.
#[derive(Clone, Debug, Default)]
pub struct LocalEnvironment {
    /// Another home folder, for tests and for harnesses with a moved config folder.
    home: Option<PathBuf>,
}

impl LocalEnvironment {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_home(home: impl Into<PathBuf>) -> Self {
        Self { home: Some(home.into()) }
    }
}

fn meta_of(path: &Path, meta: &fs::Metadata) -> FileMeta {
    let modified_ms = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_millis() as i64);
    FileMeta {
        path: path.to_string_lossy().into_owned(),
        size: meta.len(),
        modified_ms,
        is_dir: meta.is_dir(),
    }
}

fn walk(dir: &Path, suffix: &str, depth_left: u32, out: &mut Vec<FileMeta>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else { continue };
        let path = entry.path();
        if file_type.is_dir() {
            if depth_left > 1 {
                walk(&path, suffix, depth_left - 1, out);
            }
        } else if entry.file_name().to_string_lossy().ends_with(suffix)
            && let Ok(meta) = entry.metadata()
        {
            out.push(meta_of(&path, &meta));
        }
    }
}

impl Environment for LocalEnvironment {
    fn id(&self) -> EnvId {
        EnvId::local()
    }

    fn label(&self) -> String {
        "This computer".to_owned()
    }

    fn is_windows(&self) -> bool {
        cfg!(windows)
    }

    fn home(&self) -> io::Result<String> {
        if let Some(home) = &self.home {
            return Ok(home.to_string_lossy().into_owned());
        }
        let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home folder"))?;
        Ok(PathBuf::from(home).to_string_lossy().into_owned())
    }

    fn find_files(&self, root: &str, suffix: &str, max_depth: u32) -> io::Result<Vec<FileMeta>> {
        let mut out = Vec::new();
        walk(Path::new(root), suffix, max_depth.max(1), &mut out);
        Ok(out)
    }

    fn read_dir(&self, path: &str) -> io::Result<Vec<FileMeta>> {
        let mut entries: Vec<FileMeta> = fs::read_dir(path)?
            .flatten()
            .filter_map(|entry| entry.metadata().ok().map(|meta| meta_of(&entry.path(), &meta)))
            .collect();
        entries.sort_by(|a, b| {
            b.is_dir.cmp(&a.is_dir).then_with(|| a.path.to_lowercase().cmp(&b.path.to_lowercase()))
        });
        Ok(entries)
    }

    fn read(&self, path: &str, range: ReadRange) -> io::Result<Vec<u8>> {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        let mut buffer = Vec::new();
        match range {
            ReadRange::All => {
                file.read_to_end(&mut buffer)?;
            }
            ReadRange::Head(bytes) => {
                file.take(bytes).read_to_end(&mut buffer)?;
            }
            ReadRange::Tail(bytes) => {
                file.seek(SeekFrom::Start(len.saturating_sub(bytes)))?;
                file.read_to_end(&mut buffer)?;
            }
            ReadRange::From(offset) => {
                file.seek(SeekFrom::Start(offset.min(len)))?;
                file.read_to_end(&mut buffer)?;
            }
        }
        Ok(buffer)
    }

    fn stat(&self, path: &str) -> io::Result<FileMeta> {
        let meta = fs::metadata(path)?;
        Ok(meta_of(Path::new(path), &meta))
    }

    fn local_copy(&self, path: &str, _cache: &Path) -> io::Result<PathBuf> {
        Ok(PathBuf::from(path))
    }

    fn run(&self, program: &str, args: &[&str], cwd: Option<&str>) -> io::Result<String> {
        self.run_bytes(program, args, cwd).map(|out| String::from_utf8_lossy(&out).into_owned())
    }

    fn run_bytes(&self, program: &str, args: &[&str], cwd: Option<&str>) -> io::Result<Vec<u8>> {
        let mut command = Command::new(program);
        command.args(args);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        hide_console(&mut command);
        let output = command.output()?;
        if !output.status.success() {
            return Err(io::Error::other(String::from_utf8_lossy(&output.stderr).into_owned()));
        }
        Ok(output.stdout)
    }

    fn run_with_vars(&self, program: &str, args: &[&str], cwd: Option<&str>, vars: &[(&str, &str)]) -> io::Result<String> {
        let mut command = Command::new(program);
        command.args(args).envs(vars.iter().copied());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        hide_console(&mut command);
        let output = command.output()?;
        if !output.status.success() {
            return Err(io::Error::other(String::from_utf8_lossy(&output.stderr).into_owned()));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn run_with_input(&self, program: &str, args: &[&str], cwd: Option<&str>, input: &[u8]) -> io::Result<String> {
        let mut command = Command::new(program);
        command.args(args);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        hide_console(&mut command);
        crate::output_with_input(command, input)
    }

    fn interactive(&self, program: &str, args: &[&str], cwd: Option<&str>) -> Command {
        let mut command = Command::new(program);
        command.args(args);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        command
    }

    fn git_info(&self, cwd: &str) -> Option<GitInfo> {
        git::git_info_local(Path::new(cwd))
    }
}

/// A command started from a window program must not flash a console window on Windows.
pub(crate) fn hide_console(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_support::TempDir;

    #[test]
    fn finds_files_by_suffix_within_depth() {
        let dir = TempDir::new("finds_files");
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        fs::write(dir.path().join("top.jsonl"), "1").unwrap();
        fs::write(dir.path().join("a/one.jsonl"), "12").unwrap();
        fs::write(dir.path().join("a/b/deep.jsonl"), "123").unwrap();
        fs::write(dir.path().join("a/skip.txt"), "x").unwrap();
        let env = LocalEnvironment::new();
        let root = dir.path().to_string_lossy();
        let mut names: Vec<u64> =
            env.find_files(&root, ".jsonl", 2).unwrap().iter().map(|f| f.size).collect();
        names.sort();
        assert_eq!(names, vec![1, 2]);
        assert!(env.find_files(&format!("{root}/missing"), ".jsonl", 3).unwrap().is_empty());
    }

    #[test]
    fn reads_head_and_tail() {
        let dir = TempDir::new("reads_ranges");
        let path = dir.path().join("f.txt");
        fs::write(&path, "0123456789").unwrap();
        let path = path.to_string_lossy();
        let env = LocalEnvironment::new();
        assert_eq!(env.read(&path, ReadRange::Head(3)).unwrap(), b"012");
        assert_eq!(env.read(&path, ReadRange::Tail(3)).unwrap(), b"789");
        assert_eq!(env.read(&path, ReadRange::Tail(30)).unwrap().len(), 10);
        assert_eq!(env.read(&path, ReadRange::From(7)).unwrap(), b"789");
        assert!(env.read(&path, ReadRange::From(30)).unwrap().is_empty());
    }
}
