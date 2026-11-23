//! The IDE a Claude Code session can work with. An IDE that speaks the IDE protocol of
//! Claude Code (Arro, VS Code, the JetBrains plugin) writes `<config>/ide/<port>.lock`
//! with the folders it has open and listens on that port of the loopback. A session whose
//! folder is one of them gets the port in `CLAUDE_CODE_SSE_PORT`: Claude Code then sees
//! the file open there and its selection, and shows its edits in the IDE.
//!
//! Only sessions on this computer: the port is on its loopback.

use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

/// The variable that points Claude Code at the port of an IDE.
pub const PORT_VARIABLE: &str = "CLAUDE_CODE_SSE_PORT";
/// The variable an IDE sets in its own terminals so that Claude Code connects on start.
pub const ENABLE_VARIABLE: &str = "ENABLE_IDE_INTEGRATION";

/// How long a lock file's port may take to answer before the IDE counts as gone. A port
/// nobody listens on answers at once on Linux, but Windows tries again for seconds.
const ALIVE_WAIT: Duration = Duration::from_millis(200);

/// An IDE with the folder of a session open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdeLink {
    pub port: u16,
    /// What the IDE calls itself: `Arro`, `Visual Studio Code`.
    pub name: String,
}

impl IdeLink {
    /// The variables of a Claude Code process that should work with this IDE.
    pub fn environment(&self) -> [(&'static str, String); 2] {
        [(PORT_VARIABLE, self.port.to_string()), (ENABLE_VARIABLE, "true".to_owned())]
    }
}

/// The configuration folder of Claude Code on this computer: `CLAUDE_CONFIG_DIR` when it
/// names an absolute folder, as Claude Code reads it, and `~/.claude` otherwise.
pub fn claude_config_home() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from)
        && dir.is_absolute()
    {
        return Some(dir);
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })?;
    Some(PathBuf::from(home).join(".claude"))
}

/// The IDE that has `cwd` open, in the configuration folder of this computer.
pub fn ide_for(cwd: &str) -> Option<IdeLink> {
    find_ide(&claude_config_home()?, cwd)
}

/// The IDE whose lock file in `config` names `cwd` or a folder above it, and whose port
/// answers. Of several, the one with the deepest folder: the project itself before a
/// workspace that holds it.
pub fn find_ide(config: &Path, cwd: &str) -> Option<IdeLink> {
    let entries = std::fs::read_dir(config.join("ide")).ok()?;
    let mut found: Vec<(usize, IdeLink)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "lock") {
            continue;
        }
        let Some(port) = path.file_stem().and_then(|stem| stem.to_str()).and_then(|stem| stem.parse::<u16>().ok()) else {
            continue;
        };
        let Some(lock) = std::fs::read(&path).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()) else {
            continue;
        };
        let depth = lock["workspaceFolders"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|folder| holds(folder, cwd))
            .map(|folder| normalize(folder).len())
            .max();
        let Some(depth) = depth else { continue };
        let name = lock["ideName"].as_str().unwrap_or("IDE").to_owned();
        found.push((depth, IdeLink { port, name }));
    }
    found.sort_by_key(|(depth, _)| std::cmp::Reverse(*depth));
    found.into_iter().map(|(_, link)| link).find(|link| answers(link.port))
}

/// Whether `folder` is `cwd` or a folder above it.
fn holds(folder: &str, cwd: &str) -> bool {
    let (folder, cwd) = (normalize(folder), normalize(cwd));
    if folder.is_empty() {
        return false;
    }
    cwd == folder || cwd.strip_prefix(&folder).is_some_and(|rest| rest.starts_with('/'))
}

/// One spelling for a path: forward slashes, no slash at the end, and on Windows one case.
fn normalize(path: &str) -> String {
    let path = path.replace('\\', "/");
    let path = path.trim_end_matches('/');
    if cfg!(windows) { path.to_lowercase() } else { path.to_owned() }
}

fn answers(port: u16) -> bool {
    TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, port)), ALIVE_WAIT).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use test_support::TempDir;

    fn lock(config: &Path, port: u16, folders: &[&str], name: &str) {
        let body = serde_json::json!({
            "pid": 1,
            "workspaceFolders": folders,
            "ideName": name,
            "transport": "ws",
            "authToken": "t",
        });
        std::fs::create_dir_all(config.join("ide")).unwrap();
        std::fs::write(config.join("ide").join(format!("{port}.lock")), body.to_string()).unwrap();
    }

    #[test]
    fn a_folder_is_held_by_itself_and_the_folders_above_it() {
        assert!(holds("/w/shop", "/w/shop"));
        assert!(holds("/w/shop/", "/w/shop/api"));
        assert!(!holds("/w/shop", "/w/shopping"));
        assert!(!holds("/w/shop/api", "/w/shop"));
        assert!(!holds("", "/w/shop"));
        if cfg!(windows) {
            assert!(holds(r"C:\W\Shop", r"c:\w\shop\api"));
        }
    }

    #[test]
    fn a_session_finds_the_ide_that_has_its_folder_open() {
        let config = TempDir::new("ide-locks");
        let project = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let workspace = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let other = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let gone = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();
        let port = |listener: &TcpListener| listener.local_addr().unwrap().port();
        lock(config.path(), port(&workspace), &["/w"], "Visual Studio Code");
        lock(config.path(), port(&project), &["/w/shop"], "Arro");
        lock(config.path(), port(&other), &["/w/blog"], "Arro");
        // An IDE that closed without removing its lock file.
        lock(config.path(), gone, &["/w/shop/api"], "Arro");
        std::fs::write(config.path().join("ide").join("notes.txt"), "x").unwrap();
        std::fs::write(config.path().join("ide").join("7.lock"), "not json").unwrap();

        let found = find_ide(config.path(), "/w/shop/api").unwrap();
        assert_eq!(found, IdeLink { port: port(&project), name: "Arro".into() }, "the project before the workspace, a closed IDE skipped");
        assert_eq!(find_ide(config.path(), "/w/docs").unwrap().port, port(&workspace));
        assert!(find_ide(config.path(), "/elsewhere").is_none());
        assert!(find_ide(&config.path().join("missing"), "/w/shop").is_none());
        assert_eq!(found.environment()[0], (PORT_VARIABLE, port(&project).to_string()));
    }
}
