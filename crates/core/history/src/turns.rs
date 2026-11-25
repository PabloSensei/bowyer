//! What a turn of a Claude Code session did to the files, from Claude Code's own file history
//! (`<config>/file-history/<session>/`), and putting them back as they were before it.

use std::io;
use std::path::Path;

use envs::{Environment, ReadRange};
use model::{TurnAfter, TurnBefore, TurnFile};
use sha2::{Digest, Sha256};

use crate::trash::TRASH_DIR;

/// A file of a turn with its three texts: before the turn, after it, and now. `None` is a
/// file that is not there.
#[derive(Clone, Debug, PartialEq)]
pub struct TurnFileState {
    pub path: String,
    pub before: Option<Vec<u8>>,
    pub after: Option<Vec<u8>>,
    pub current: Option<Vec<u8>>,
}

impl TurnFileState {
    /// Nothing changed the file since the turn: undoing the turn loses nothing done later.
    pub fn untouched(&self) -> bool {
        self.current == self.after
    }

    /// The file is as it was before the turn: undone already, or never changed.
    pub fn as_before(&self) -> bool {
        self.current == self.before
    }
}

/// The folder of a session's file history, beside the `projects` folder its transcript is
/// in (so a `CLAUDE_CONFIG_DIR` of the machine is followed too).
pub fn file_history_dir(env: &dyn Environment, transcript: &str, session: &str) -> Option<String> {
    let mut parts = transcript.rsplitn(4, ['/', '\\']);
    let (_file, _project, projects, root) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    (projects == "projects" && !session.is_empty()).then(|| env.join(&env.join(root, "file-history"), session))
}

fn read_if_there(env: &dyn Environment, path: &str) -> io::Result<Option<Vec<u8>>> {
    match env.read(path, ReadRange::All) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// The name Claude Code gives the first backup of a file it names `path`: the start of the
/// path's SHA-256 and the version. Checked on 33 000 backups of real sessions.
pub fn first_backup_name(path: &str) -> String {
    let hash = Sha256::digest(path.as_bytes());
    let hex: String = hash.iter().take(8).map(|byte| format!("{byte:02x}")).collect();
    format!("{hex}@v1")
}

/// The backup a file had before the turn, `None` when the file did not exist. A first
/// backup that is not there fails, unless the turn made the file.
pub fn backup_before(env: &dyn Environment, dir: &str, before: &TurnBefore) -> io::Result<Option<String>> {
    match before {
        TurnBefore::Backup(name) => Ok(name.clone()),
        TurnBefore::First { named, created } => {
            let name = first_backup_name(named);
            match env.stat(&env.join(dir, &name)) {
                Ok(_) => Ok(Some(name)),
                Err(error) if error.kind() == io::ErrorKind::NotFound && *created => Ok(None),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Err(io::Error::new(io::ErrorKind::NotFound, "Claude Code's backup is gone")),
                Err(error) => Err(error),
            }
        }
    }
}

fn backup(env: &dyn Environment, dir: &str, name: &Option<String>) -> io::Result<Option<Vec<u8>>> {
    match name {
        Some(name) => env.read(&env.join(dir, name), ReadRange::All).map(Some),
        None => Ok(None),
    }
}

/// The texts of a turn's files. A backup that is gone fails the file.
pub fn read_turn(env: &dyn Environment, dir: &str, files: &[TurnFile]) -> Vec<(String, io::Result<TurnFileState>)> {
    files
        .iter()
        .map(|file| {
            let state = (|| {
                let current = read_if_there(env, &file.path)?;
                let after = match &file.after {
                    TurnAfter::Backup(name) => backup(env, dir, name)?,
                    TurnAfter::Current => current.clone(),
                    TurnAfter::Unknown => return Err(io::Error::new(io::ErrorKind::NotFound, "no copy of the file after the turn")),
                };
                let before = backup(env, dir, &backup_before(env, dir, &file.before)?)?;
                Ok(TurnFileState { path: file.path.clone(), before, after, current })
            })();
            (file.path.clone(), state)
        })
        .collect()
}

/// Puts a turn's files back as they were before it: a file gets its backup back, a file the
/// turn made goes. What each file is now is first copied into a folder of its own under
/// `~/.bowyer-trash/revert/`, with `paths.txt` naming where each came from; the folder is
/// returned. Nothing is touched when a file changed after the turn or a backup is gone.
pub fn undo_turn(env: &dyn Environment, dir: &str, files: &[TurnFile]) -> io::Result<String> {
    for (path, state) in read_turn(env, dir, files) {
        let state = state?;
        if !state.untouched() {
            return Err(io::Error::other(format!("{path} changed after the turn")));
        }
    }
    let befores = files.iter().map(|file| backup_before(env, dir, &file.before)).collect::<io::Result<Vec<_>>>()?;
    let home = env.home()?;
    let trash = env.join(&env.join(&env.join(&home, TRASH_DIR), "revert"), &stamp());
    if env.id().is_local() {
        return undo_here(dir, &trash, files, &befores);
    }
    let keep = r#"set -e; d="$1"; n=0; while [ -e "$d" ]; do n=$((n+1)); d="$1-$n"; done; shift; mkdir -p "$d"; i=0; for p in "$@"; do if [ -e "$p" ]; then c="$i-${p##*/}"; cp -p -- "$p" "$d/$c"; printf '%s\t%s\n' "$c" "$p" >> "$d/paths.txt"; fi; i=$((i+1)); done; printf '%s' "$d""#;
    let mut args = vec!["-c", keep, "sh", trash.as_str()];
    args.extend(files.iter().map(|file| file.path.as_str()));
    let kept = env.run("sh", &args, None)?;
    let restore = r#"set -e; h="$1"; shift; while [ $# -gt 1 ]; do p="$1"; b="$2"; shift 2; if [ -n "$b" ]; then mkdir -p -- "$(dirname -- "$p")"; cp -- "$h/$b" "$p"; else rm -f -- "$p"; fi; done"#;
    let mut args = vec!["-c", restore, "sh", dir];
    for (file, before) in files.iter().zip(&befores) {
        args.push(file.path.as_str());
        args.push(before.as_deref().unwrap_or(""));
    }
    env.run("sh", &args, None)?;
    Ok(kept.trim().to_owned())
}

fn stamp() -> String {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_millis()).to_string()
}

fn undo_here(dir: &str, trash: &str, files: &[TurnFile], befores: &[Option<String>]) -> io::Result<String> {
    let mut folder = Path::new(trash).to_path_buf();
    let mut n = 0;
    while folder.exists() {
        n += 1;
        folder = Path::new(&format!("{trash}-{n}")).to_path_buf();
    }
    std::fs::create_dir_all(&folder)?;
    let mut paths = String::new();
    for (i, file) in files.iter().enumerate() {
        let path = Path::new(&file.path);
        if path.exists() {
            let name = format!("{i}-{}", path.file_name().map(|name| name.to_string_lossy()).unwrap_or_default());
            std::fs::copy(path, folder.join(&name))?;
            paths.push_str(&format!("{name}\t{}\n", file.path));
        }
    }
    std::fs::write(folder.join("paths.txt"), paths)?;
    for (file, before) in files.iter().zip(befores) {
        let path = Path::new(&file.path);
        match before {
            Some(name) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(Path::new(dir).join(name), path)?;
            }
            None if path.exists() => std::fs::remove_file(path)?,
            None => {}
        }
    }
    Ok(folder.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use envs::LocalEnvironment;
    use test_support::TempDir;

    /// A file first changed during a turn has its first backup by Claude Code's naming;
    /// without it, a file the turn made did not exist, and any other has lost its backup.
    #[test]
    fn a_first_backup_is_found_by_its_name() {
        assert_eq!(first_backup_name("/home/u/p/a.rs"), format!("{}@v1", &format!("{:x}", Sha256::digest(b"/home/u/p/a.rs"))[..16]));
        let home = TempDir::new("turn-first");
        let env = LocalEnvironment::with_home(home.path());
        let dir = home.path().to_string_lossy().into_owned();
        std::fs::write(home.path().join(first_backup_name("/p/kept.rs")), "old").unwrap();
        let first = |named: &str, created: bool| TurnBefore::First { named: named.into(), created };
        assert_eq!(backup_before(&env, &dir, &first("/p/kept.rs", false)).unwrap(), Some(first_backup_name("/p/kept.rs")));
        assert_eq!(backup_before(&env, &dir, &first("/p/made.rs", true)).unwrap(), None);
        assert!(backup_before(&env, &dir, &first("/p/lost.rs", false)).is_err(), "never taken for a file that was gone");
        let unknown = TurnFile { path: "/p/kept.rs".into(), before: first("/p/kept.rs", false), after: TurnAfter::Unknown };
        assert!(read_turn(&env, &dir, &[unknown])[0].1.is_err());
    }

    /// The same undoing on a real host, with its own shell: an edited file gets its backup
    /// back, a file the turn made goes, and what both were is kept in the host's trash.
    /// `BOWYER_SSH_HOST=user@host BOWYER_SSH_KEY=key cargo test -p history -- --ignored undone_on`.
    #[test]
    #[ignore = "needs an SSH host"]
    fn a_turn_is_undone_on_a_real_host() {
        let host = std::env::var("BOWYER_SSH_HOST").expect("BOWYER_SSH_HOST");
        let key = std::env::var_os("BOWYER_SSH_KEY").map(std::path::PathBuf::from);
        let ssh = envs::SshEnvironment::new(&host).with_identity(key);
        let dir = format!("/tmp/bowyer-undo-{}", std::process::id());
        let setup = format!("mkdir -p {dir}/h '{dir}/my p' && printf 'old\\n' > {dir}/h/h@v1 && printf 'new\\n' > '{dir}/my p/a.rs' && printf 'hi\\n' > '{dir}/my p/made.md'");
        ssh.run("sh", &["-c", &setup], None).unwrap();
        let missing = ssh.read(&format!("{dir}/none"), ReadRange::All).unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound, "{missing}");
        let (edited, made) = (format!("{dir}/my p/a.rs"), format!("{dir}/my p/made.md"));
        let files = vec![
            TurnFile { path: edited.clone(), before: TurnBefore::Backup(Some("h@v1".into())), after: TurnAfter::Current },
            TurnFile { path: made.clone(), before: TurnBefore::First { named: made.clone(), created: true }, after: TurnAfter::Current },
        ];
        let history = format!("{dir}/h");
        assert!(read_turn(&ssh, &history, &files).iter().all(|(_, state)| state.as_ref().unwrap().untouched()));
        let kept = undo_turn(&ssh, &history, &files).unwrap();
        let outcome = (
            ssh.read(&edited, ReadRange::All).map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
            ssh.stat(&made).map_err(|error| error.kind()),
            ssh.read(&format!("{kept}/0-a.rs"), ReadRange::All).map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
            ssh.read(&format!("{kept}/paths.txt"), ReadRange::All).map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
        );
        ssh.run("sh", &["-c", &format!("rm -rf {dir} '{kept}'")], None).unwrap();
        assert!(kept.contains("/.bowyer-trash/revert/"), "{kept}");
        assert_eq!(outcome.0.unwrap(), "old\n");
        assert_eq!(outcome.1.unwrap_err(), io::ErrorKind::NotFound);
        assert_eq!(outcome.2.unwrap(), "new\n");
        assert!(outcome.3.unwrap().contains(&format!("1-made.md\t{made}")));
    }

    #[test]
    fn the_history_sits_beside_the_projects() {
        let env = LocalEnvironment::new();
        let dir = file_history_dir(&env, "/home/u/.claude/projects/-home-u-p/abc.jsonl", "abc").unwrap();
        assert_eq!(dir.replace('\\', "/"), "/home/u/.claude/file-history/abc");
        assert_eq!(file_history_dir(&env, "/tmp/abc.jsonl", "abc"), None);
    }

    /// Undoing a turn gives an edited file its text from before and takes away a file it
    /// made; what both were is kept in the trash. A file changed since stops it all.
    #[test]
    fn a_turn_is_undone_from_its_backups() {
        let home = TempDir::new("turn-undo");
        let env = LocalEnvironment::with_home(home.path());
        let history = home.path().join("history");
        std::fs::create_dir_all(&history).unwrap();
        std::fs::write(history.join("h@v1"), "old\n").unwrap();
        let project = home.path().join("p");
        std::fs::create_dir_all(&project).unwrap();
        let edited = project.join("a.rs");
        let made = project.join("new.md");
        std::fs::write(&edited, "new\n").unwrap();
        std::fs::write(&made, "hello\n").unwrap();
        let files = vec![
            TurnFile { path: edited.to_string_lossy().into_owned(), before: TurnBefore::Backup(Some("h@v1".into())), after: TurnAfter::Current },
            TurnFile { path: made.to_string_lossy().into_owned(), before: TurnBefore::Backup(None), after: TurnAfter::Current },
        ];
        let dir = history.to_string_lossy().into_owned();
        let states = read_turn(&env, &dir, &files);
        assert!(states.iter().all(|(_, state)| state.as_ref().unwrap().untouched()));

        let mut changed = files.clone();
        std::fs::write(history.join("h@v2"), "new\n").unwrap();
        changed[0].after = TurnAfter::Backup(Some("h@v2".into()));
        std::fs::write(&edited, "newer\n").unwrap();
        assert!(undo_turn(&env, &dir, &changed).is_err());
        assert_eq!(std::fs::read_to_string(&edited).unwrap(), "newer\n", "nothing touched");
        std::fs::write(&edited, "new\n").unwrap();

        let kept = undo_turn(&env, &dir, &changed).unwrap();
        assert_eq!(std::fs::read_to_string(&edited).unwrap(), "old\n");
        assert!(!made.exists());
        let kept = Path::new(&kept);
        assert!(kept.starts_with(home.path().join(TRASH_DIR).join("revert")));
        assert_eq!(std::fs::read_to_string(kept.join("0-a.rs")).unwrap(), "new\n");
        assert_eq!(std::fs::read_to_string(kept.join("1-new.md")).unwrap(), "hello\n");
        assert!(std::fs::read_to_string(kept.join("paths.txt")).unwrap().contains("1-new.md\t"));
    }
}
