//! What runs in a terminal besides its shell: the programs the shell started, by name. A
//! shell at its prompt has none; one that runs a command or a harness has that program.

/// Shells, and the launchers that only start one: a chain of them is the shell of the
/// terminal (`cmd /c … & pwsh`, Git Bash's `bash.exe` starting `usr/bin/bash.exe`).
const SHELLS: &[&str] = &["sh", "bash", "zsh", "fish", "dash", "ksh", "nu", "cmd", "pwsh", "powershell"];

/// Console hosts the system starts beside a program: not a program of the user.
const HOSTS: &[&str] = &["conhost", "openconsole"];

/// The name of a program as `ps` or the process table gives it: no folder, no `.exe`, in
/// lower case.
pub(crate) fn plain_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name).trim();
    let base = base.strip_prefix('-').unwrap_or(base);
    let lower = base.to_ascii_lowercase();
    lower.strip_suffix(".exe").map(str::to_owned).unwrap_or(lower)
}

/// The programs below the shell of a terminal whose first process is `root`, from a
/// process table of `(pid, parent, name)`. A chain of single shells is followed down to
/// the last one; what that one runs is the answer, by plain name. A terminal that started
/// a program in place of a shell (`ssh`) runs that program.
pub(crate) fn programs_below(table: &[(u32, u32, String)], root: u32) -> Vec<String> {
    if let Some((_, _, name)) = table.iter().find(|(pid, _, _)| *pid == root)
        && !SHELLS.contains(&plain_name(name).as_str())
    {
        return vec![plain_name(name)];
    }
    let children = |parent: u32| -> Vec<&(u32, u32, String)> {
        table.iter().filter(|(pid, ppid, name)| *ppid == parent && *pid != parent && !HOSTS.contains(&plain_name(name).as_str())).collect()
    };
    let mut shell = root;
    // A shell is never its own descendant, but a table read in two moments might say so.
    for _ in 0..8 {
        let below = children(shell);
        match below.as_slice() {
            [(pid, _, name)] if SHELLS.contains(&plain_name(name).as_str()) => shell = *pid,
            _ => break,
        }
    }
    children(shell).into_iter().map(|(_, _, name)| plain_name(name)).collect()
}

/// Every process of the machine with its parent and name; `None` when it cannot be read.
#[cfg(windows)]
pub(crate) fn process_table() -> Option<Vec<(u32, u32, String)>> {
    let table = crate::sys::process_table();
    (!table.is_empty()).then(|| table.into_iter().map(|entry| (entry.pid, entry.parent.unwrap_or(0), entry.name)).collect())
}

#[cfg(unix)]
pub(crate) fn process_table() -> Option<Vec<(u32, u32, String)>> {
    let output = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=", "-o", "ppid=", "-o", "comm="])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Some(
        text.lines()
            .filter_map(|line| {
                let mut words = line.split_whitespace();
                let pid = words.next()?.parse().ok()?;
                let parent = words.next()?.parse().ok()?;
                let name = words.collect::<Vec<_>>().join(" ");
                Some((pid, parent, name))
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::{plain_name, programs_below};

    fn table(rows: &[(u32, u32, &str)]) -> Vec<(u32, u32, String)> {
        rows.iter().map(|(pid, parent, name)| (*pid, *parent, (*name).to_owned())).collect()
    }

    #[test]
    fn names_are_plain() {
        assert_eq!(plain_name(r"C:\Program Files\Git\bin\bash.exe"), "bash");
        assert_eq!(plain_name("/usr/local/bin/claude"), "claude");
        assert_eq!(plain_name("-zsh"), "zsh");
        assert_eq!(plain_name("Claude.EXE"), "claude");
    }

    #[test]
    fn a_shell_at_its_prompt_runs_nothing() {
        let rows = table(&[(10, 1, "pwsh.exe"), (11, 1, "conhost.exe"), (50, 2, "other.exe")]);
        assert!(programs_below(&rows, 10).is_empty());
    }

    #[test]
    fn a_chain_of_shells_is_one_shell() {
        // `cmd /c chcp & pwsh` and Git Bash's launcher.
        let rows = table(&[(10, 1, "cmd.exe"), (11, 10, "pwsh.exe"), (20, 1, "bash.exe"), (21, 20, "bash.exe")]);
        assert!(programs_below(&rows, 10).is_empty());
        assert!(programs_below(&rows, 20).is_empty());
    }

    #[test]
    fn what_the_shell_runs_is_named() {
        let rows = table(&[
            (10, 1, "powershell.exe"),
            (12, 10, "claude.exe"),
            // What the harness started is its own: not a program of the shell.
            (13, 12, "node.exe"),
            (20, 1, "sh"),
            (21, 20, "cargo"),
            (22, 20, "sleep"),
        ]);
        assert_eq!(programs_below(&rows, 10), ["claude"]);
        assert_eq!(programs_below(&rows, 20), ["cargo", "sleep"]);
    }

    #[test]
    fn a_program_in_place_of_a_shell_is_what_runs() {
        let rows = table(&[(10, 1, "ssh.exe"), (11, 10, "conhost.exe")]);
        assert_eq!(programs_below(&rows, 10), ["ssh"]);
    }
}
