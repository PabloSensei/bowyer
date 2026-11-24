//! The shell of a new terminal: the newest PowerShell this machine has, else the one of
//! Windows, else the Git Bash, else the command prompt. The `+` menu lists every shell
//! found, in the order the user is likely to want them.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::shells::{Shell, find_in, push_unique, quote};

/// The command line of the shell found, quoted for the console of Windows.
pub fn default_shell() -> Option<String> {
    let mut found = find_in_path("pwsh.exe");
    if found.is_none() {
        found = find_in_path("powershell.exe");
    }
    if found.is_none() {
        found = git_bash();
    }
    if found.is_none() {
        found = std::env::var_os("ComSpec")
            .map(PathBuf::from)
            .filter(|path| path.is_file());
    }
    found.map(|path| command_line(&path))
}

/// Every shell this machine has, in the order the menu shows them.
pub fn available_shells() -> Vec<Shell> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut shells = shells_in(&path);
    if let Some(bash) = git_bash() {
        push_unique(&mut shells, Shell::new("Git Bash", &bash));
    }
    if let Some(wsl) = find_in(&path, "wsl.exe").or_else(|| system32("wsl.exe")) {
        push_unique(&mut shells, Shell::new("WSL", &wsl));
    }
    // The prompt of Windows is there even when PATH does not name it.
    if let Some(prompt) = std::env::var_os("ComSpec")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
    {
        push_unique(&mut shells, Shell::new("Command Prompt", &prompt));
    }
    shells
}

/// The shells `path` names alone, without the places outside PATH a shell of Windows
/// lives in. A PATH of its own makes this testable.
pub fn shells_in(path: &OsStr) -> Vec<Shell> {
    let candidates = [
        ("pwsh.exe", "PowerShell"),
        ("powershell.exe", "Windows PowerShell"),
        ("cmd.exe", "Command Prompt"),
        ("bash.exe", "Git Bash"),
        ("wsl.exe", "WSL"),
    ];
    let mut shells = Vec::new();
    for (program, name) in candidates {
        // The first `bash.exe` on PATH is often the one of System32, which starts WSL:
        // Git Bash is the one inside a Git folder.
        let found = if program == "bash.exe" { git_bash_in(path) } else { find_in(path, program) };
        if let Some(found) = found {
            push_unique(&mut shells, Shell::new(name, &found));
        }
    }
    shells
}

/// The `bash.exe` of a Git for Windows that `path` names: one under a folder called Git.
fn git_bash_in(path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path).map(|folder| folder.join("bash.exe")).find(|bash| {
        bash.is_file() && bash.components().any(|part| part.as_os_str().eq_ignore_ascii_case("git"))
    })
}

fn command_line(shell: &Path) -> String {
    let quoted = quote(shell);
    let name = shell
        .file_stem()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name == "bash" {
        format!("{quoted} --login -i")
    } else {
        quoted
    }
}

fn find_in_path(program: &str) -> Option<PathBuf> {
    find_in(std::env::var_os("PATH")?.as_os_str(), program)
}

/// A program of the system folder, which PATH does not have to name.
fn system32(program: &str) -> Option<PathBuf> {
    let folder = std::env::var_os("SystemRoot").map(PathBuf::from)?;
    let file = folder.join("System32").join(program);
    file.is_file().then_some(file)
}

/// The `bash.exe` of a Git for Windows installed in its usual places, which is often
/// not on `PATH`.
fn git_bash() -> Option<PathBuf> {
    let folders = [
        std::env::var_os("ProgramFiles").map(PathBuf::from),
        std::env::var_os("ProgramFiles(x86)").map(PathBuf::from),
        std::env::var_os("LOCALAPPDATA").map(|folder| PathBuf::from(folder).join("Programs")),
    ];
    folders.into_iter().flatten().find_map(|folder| {
        let bash = folder.join("Git").join("bin").join("bash.exe");
        bash.is_file().then_some(bash)
    })
}

#[cfg(test)]
mod tests {
    use super::{command_line, shells_in};
    use std::ffi::OsString;
    use std::path::Path;

    #[test]
    fn a_shell_path_is_quoted_and_bash_gets_its_login_flags() {
        assert_eq!(
            command_line(Path::new(r"C:\Program Files\PowerShell\7\pwsh.exe")),
            r#""C:\Program Files\PowerShell\7\pwsh.exe""#
        );
        assert!(
            command_line(Path::new(r"C:\Program Files\Git\bin\bash.exe")).ends_with("--login -i")
        );
    }

    /// A PATH of its own, with the files `names` in the first folder.
    fn path_with(name: &str, names: &[&str]) -> OsString {
        let folder = test_support::scratch(name);
        for name in names {
            std::fs::write(folder.join(name), "").unwrap();
        }
        folder.as_os_str().to_os_string()
    }

    #[test]
    fn the_shells_of_a_path_keep_the_order_of_the_menu() {
        let path = path_with("shells-order", &["pwsh.exe", "cmd.exe", "wsl.exe"]);
        let shells = shells_in(&path);
        let names: Vec<&str> = shells.iter().map(|shell| shell.name.as_str()).collect();
        assert_eq!(names, ["PowerShell", "Command Prompt", "WSL"]);
    }

    #[test]
    fn the_bash_of_wsl_is_no_git_bash() {
        let root = test_support::scratch("shells-bash");
        let (system, git) = (root.join("System32"), root.join("Git").join("bin"));
        for folder in [&system, &git] {
            std::fs::create_dir_all(folder).unwrap();
            std::fs::write(folder.join("bash.exe"), "").unwrap();
        }
        let only_system = std::env::join_paths([&system]).unwrap();
        assert!(shells_in(&only_system).is_empty(), "the bash of System32 starts WSL");
        let both = std::env::join_paths([&system, &git]).unwrap();
        let shells = shells_in(&both);
        assert_eq!(shells.len(), 1);
        assert_eq!(shells[0].name, "Git Bash");
        assert!(shells[0].command_line.contains(&git.join("bash.exe").display().to_string()), "{}", shells[0].command_line);
    }

    #[test]
    fn a_folder_without_shells_names_none() {
        let path = path_with("shells-empty", &["notes.txt"]);
        assert!(shells_in(&path).is_empty());
    }
}
