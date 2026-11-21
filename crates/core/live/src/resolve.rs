//! Finds the executable of a harness. On Windows npm installs a `.cmd` shim, and starting
//! a shim runs `cmd.exe`, which would parse arguments with its own rules; the shim names the
//! real `.exe` (or the Node script), so that is what starts.

use std::path::PathBuf;

/// The program and the arguments before the harness's own, found on the PATH.
pub fn resolve_program(name: &str) -> Option<(PathBuf, Vec<String>)> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if cfg!(windows) {
            let exe = dir.join(format!("{name}.exe"));
            if exe.is_file() {
                return Some((exe, Vec::new()));
            }
            let shim = dir.join(format!("{name}.cmd"));
            if shim.is_file()
                && let Ok(text) = std::fs::read_to_string(&shim)
            {
                if let Some(relative) = shim_exe(&text) {
                    return Some((dir.join(relative), Vec::new()));
                }
                if let Some(script) = shim_script(&text) {
                    let node = dir.join("node.exe");
                    let node = if node.is_file() { node } else { PathBuf::from("node") };
                    return Some((node, vec![dir.join(script).to_string_lossy().into_owned()]));
                }
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

/// The script a Node shim runs: the quoted `.js` path after `%dp0%`.
pub(crate) fn shim_script(text: &str) -> Option<String> {
    for marker in ["\"%dp0%\\", "\"%~dp0\\"] {
        for (index, _) in text.match_indices(marker) {
            let rest = &text[index + marker.len()..];
            let end = rest.find('"')?;
            let relative = &rest[..end];
            let lower = relative.to_ascii_lowercase();
            if lower.ends_with(".js") || lower.ends_with(".mjs") || lower.ends_with(".cjs") {
                return Some(relative.replace('\\', std::path::MAIN_SEPARATOR_STR));
            }
        }
    }
    None
}

pub(crate) fn shim_exe(text: &str) -> Option<String> {
    for marker in ["\"%dp0%\\", "\"%~dp0\\"] {
        for (index, _) in text.match_indices(marker) {
            let rest = &text[index + marker.len()..];
            let end = rest.find('"')?;
            let relative = &rest[..end];
            if relative.to_ascii_lowercase().ends_with(".exe") {
                return Some(relative.replace('\\', std::path::MAIN_SEPARATOR_STR));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::shim_exe;

    #[test]
    fn npm_shims_name_their_executable() {
        let shim = "@ECHO off\nGOTO start\n:find_dp0\nSET dp0=%~dp0\nEXIT /b\n:start\nSETLOCAL\nCALL :find_dp0\n\"%dp0%\\node_modules\\@opencode\\cli\\bin\\opencode.exe\"   %*\n";
        let exe = shim_exe(shim).unwrap();
        assert!(exe.ends_with("opencode.exe"));
        assert!(exe.starts_with("node_modules"));
        assert_eq!(shim_exe("\"%dp0%\\node.exe\" \"%dp0%\\cli.js\""), Some("node.exe".into()));
        let node_shim = "\"%_prog%\"  \"%dp0%\\node_modules\\@deepseek-ai\\dsh\\lib\\bin.js\" %*";
        assert!(super::shim_script(node_shim).unwrap().ends_with("bin.js"));
        assert_eq!(shim_exe(node_shim), None);
    }
}
