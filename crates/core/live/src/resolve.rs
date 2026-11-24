//! Finds the executable of a harness. On Windows npm installs a `.cmd` shim, and starting
//! a shim runs `cmd.exe`, which would parse arguments with its own rules; the shim names the
//! real `.exe` (or the Node script), so that is what starts.

use std::path::{Path, PathBuf};

/// The program and the arguments before the harness's own, found on the PATH.
pub fn resolve_program(name: &str) -> Option<(PathBuf, Vec<String>)> {
    let path = std::env::var_os("PATH")?;
    resolve_in(name, std::env::split_paths(&path))
}

fn resolve_in(name: &str, dirs: impl IntoIterator<Item = PathBuf>) -> Option<(PathBuf, Vec<String>)> {
    for dir in dirs {
        if cfg!(windows) {
            let exe = dir.join(format!("{name}.exe"));
            if exe.is_file() {
                return Some((exe, Vec::new()));
            }
            let shim = dir.join(format!("{name}.cmd"));
            if shim.is_file()
                && let Ok(text) = std::fs::read_to_string(&shim)
                && let Some(found) = from_shim(&dir, &text)
            {
                return Some(found);
            }
        } else {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some((candidate, Vec::new()));
            }
        }
    }
    None
}

/// What a shim in `dir` runs. A Node shim names `node.exe` beside it first (`IF EXIST
/// "%dp0%\node.exe"`), which is seldom there: the script is what it runs, with that
/// `node.exe` if it is there and the one on the PATH if not.
fn from_shim(dir: &Path, text: &str) -> Option<(PathBuf, Vec<String>)> {
    if let Some(script) = shim_script(text) {
        let node = dir.join("node.exe");
        let node = if node.is_file() { node } else { PathBuf::from("node") };
        return Some((node, vec![dir.join(script).to_string_lossy().into_owned()]));
    }
    shim_exe(text).map(|relative| (dir.join(relative), Vec::new())).filter(|(exe, _)| exe.is_file())
}

/// The quoted paths after `%dp0%` in a shim, relative to its folder.
fn shim_paths(text: &str) -> impl Iterator<Item = String> + '_ {
    ["\"%dp0%\\", "\"%~dp0\\"].into_iter().flat_map(move |marker| {
        text.match_indices(marker).filter_map(move |(index, _)| {
            let rest = &text[index + marker.len()..];
            let end = rest.find('"')?;
            Some(rest[..end].replace('\\', std::path::MAIN_SEPARATOR_STR))
        })
    })
}

/// The script a Node shim runs: the quoted `.js` path after `%dp0%`.
pub(crate) fn shim_script(text: &str) -> Option<String> {
    shim_paths(text).find(|path| {
        let lower = path.to_ascii_lowercase();
        lower.ends_with(".js") || lower.ends_with(".mjs") || lower.ends_with(".cjs")
    })
}

/// The executable a shim of a native program runs; never Node, which a Node shim names too.
pub(crate) fn shim_exe(text: &str) -> Option<String> {
    shim_paths(text).find(|path| {
        let lower = path.to_ascii_lowercase();
        lower.ends_with(".exe") && !lower.ends_with("node.exe")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// npm's shim for a Node package, as `npm install -g` writes it.
    const NODE_SHIM: &str = "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\n\r\nIF EXIST \"%dp0%\\node.exe\" (\r\n  SET \"_prog=%dp0%\\node.exe\"\r\n) ELSE (\r\n  SET \"_prog=node\"\r\n  SET PATHEXT=%PATHEXT:;.JS;=;%\r\n)\r\n\r\nendLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"  \"%dp0%\\node_modules\\@openai\\codex\\bin\\codex.js\" %*\r\n";
    const EXE_SHIM: &str = "@ECHO off\nGOTO start\n:find_dp0\nSET dp0=%~dp0\nEXIT /b\n:start\nSETLOCAL\nCALL :find_dp0\n\"%dp0%\\node_modules\\@opencode\\cli\\bin\\opencode.exe\"   %*\n";

    #[test]
    fn npm_shims_name_their_executable_or_script() {
        let exe = shim_exe(EXE_SHIM).unwrap();
        assert!(exe.ends_with("opencode.exe") && exe.starts_with("node_modules"), "{exe}");
        assert_eq!(shim_script(EXE_SHIM), None);
        assert!(shim_script(NODE_SHIM).unwrap().ends_with("codex.js"));
        assert_eq!(shim_exe(NODE_SHIM), None, "node.exe is not the program");
        let short = "\"%_prog%\"  \"%dp0%\\node_modules\\@deepseek-ai\\dsh\\lib\\bin.js\" %*";
        assert!(shim_script(short).unwrap().ends_with("bin.js"));
    }

    #[test]
    fn a_node_shim_starts_its_script_with_node() {
        let dir = test_support::TempDir::new("resolve-node-shim");
        let (program, args) = from_shim(dir.path(), NODE_SHIM).unwrap();
        assert_eq!(program, PathBuf::from("node"), "no node.exe beside the shim");
        assert_eq!(args, vec![dir.path().join("node_modules").join("@openai").join("codex").join("bin").join("codex.js").to_string_lossy().into_owned()]);
        std::fs::write(dir.path().join("node.exe"), "").unwrap();
        assert_eq!(from_shim(dir.path(), NODE_SHIM).unwrap().0, dir.path().join("node.exe"));
    }

    #[test]
    fn an_exe_shim_starts_its_exe_when_it_is_there() {
        let dir = test_support::TempDir::new("resolve-exe-shim");
        assert_eq!(from_shim(dir.path(), EXE_SHIM), None);
        let exe = dir.path().join("node_modules").join("@opencode").join("cli").join("bin");
        std::fs::create_dir_all(&exe).unwrap();
        std::fs::write(exe.join("opencode.exe"), "").unwrap();
        assert_eq!(from_shim(dir.path(), EXE_SHIM), Some((exe.join("opencode.exe"), Vec::new())));
    }

    #[cfg(windows)]
    #[test]
    fn the_shim_on_the_path_is_found() {
        let dir = test_support::TempDir::new("resolve-path");
        std::fs::write(dir.path().join("codex.cmd"), NODE_SHIM).unwrap();
        let (program, args) = resolve_in("codex", [PathBuf::from("/nowhere"), dir.path().to_path_buf()]).unwrap();
        assert_eq!(program, PathBuf::from("node"));
        assert!(args[0].ends_with("codex.js"));
        assert_eq!(resolve_in("claude", [dir.path().to_path_buf()]), None);
    }
}
