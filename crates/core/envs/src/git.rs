//! The branch of a working folder without starting `git`: the window asks for it on every
//! selection, and reading `HEAD` takes microseconds.

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitInfo {
    /// The folder that holds `.git`.
    pub root: String,
    /// The branch, or the short commit of a detached head.
    pub branch: String,
    pub detached: bool,
    /// A linked worktree rather than the main checkout.
    pub worktree: bool,
}

/// The branch named by the text of a `HEAD` file.
pub fn parse_head(head: &str) -> Option<(String, bool)> {
    let head = head.trim();
    if let Some(reference) = head.strip_prefix("ref:") {
        let reference = reference.trim();
        let branch = reference.strip_prefix("refs/heads/").unwrap_or(reference);
        return Some((branch.to_owned(), false));
    }
    if head.len() >= 7 && head.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some((head[..7].to_owned(), true));
    }
    None
}

/// Walks up from `cwd` to the first `.git`, a folder or the file of a linked worktree.
pub fn git_info_local(cwd: &Path) -> Option<GitInfo> {
    let mut dir: Option<&Path> = Some(cwd);
    while let Some(current) = dir {
        let dot_git = current.join(".git");
        if let Ok(meta) = fs::metadata(&dot_git) {
            let (git_dir, worktree) = if meta.is_dir() {
                (dot_git, false)
            } else {
                let text = fs::read_to_string(&dot_git).ok()?;
                let target = text.trim().strip_prefix("gitdir:")?.trim();
                let target = PathBuf::from(target);
                let target = if target.is_absolute() { target } else { current.join(target) };
                (target, true)
            };
            let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
            let (branch, detached) = parse_head(&head)?;
            return Some(GitInfo {
                root: current.to_string_lossy().into_owned(),
                branch,
                detached,
                worktree,
            });
        }
        dir = current.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_support::TempDir;

    #[test]
    fn heads_name_branches_or_commits() {
        assert_eq!(parse_head("ref: refs/heads/main\n"), Some(("main".into(), false)));
        assert_eq!(parse_head("0123456789abcdef"), Some(("0123456".into(), true)));
        assert_eq!(parse_head("garbage"), None);
    }

    #[test]
    fn finds_the_branch_from_a_subfolder_and_a_worktree() {
        let dir = TempDir::new("git_info");
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join(".git/worktrees/wt")).unwrap();
        fs::create_dir_all(repo.join("src/deep")).unwrap();
        fs::write(repo.join(".git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        let info = git_info_local(&repo.join("src/deep")).unwrap();
        assert_eq!(info.branch, "feature/x");
        assert!(!info.worktree);

        let wt = dir.path().join("wt");
        fs::create_dir_all(&wt).unwrap();
        fs::write(repo.join(".git/worktrees/wt/HEAD"), "ref: refs/heads/side\n").unwrap();
        let gitdir = repo.join(".git/worktrees/wt");
        fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        let info = git_info_local(&wt).unwrap();
        assert_eq!(info.branch, "side");
        assert!(info.worktree);
    }
}
