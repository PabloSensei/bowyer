//! Git changes of a working folder: what is staged and what is not, the diff of a file in
//! hunks, and staging, unstaging or discarding one hunk or a whole file. Everything runs the
//! `git` of the environment, so a folder over SSH works like a local one; a hunk goes to
//! `git apply` as a patch on standard input.

use std::io;

use envs::Environment;

/// One changed path as `git status` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileStatus {
    /// Relative to the root of the repository, with `/`.
    pub path: String,
    /// The letter of the staged change (`M`, `A`, `D`, `R`), if any.
    pub staged: Option<char>,
    /// The letter of the change in the working tree, if any.
    pub unstaged: Option<char>,
    pub untracked: bool,
}

/// The status of a repository.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub root: String,
    pub files: Vec<FileStatus>,
}

/// A commit of the history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub hash: String,
    pub short: String,
    pub subject: String,
    pub author: String,
    /// Commit time, milliseconds since the Unix epoch.
    pub time_ms: i64,
}

/// The branch and how it stands against its upstream.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Branch {
    /// `None` on a detached head.
    pub name: Option<String>,
    pub upstream: Option<String>,
    /// Commits here that the upstream lacks.
    pub ahead: usize,
    /// Commits of the upstream that are not here.
    pub behind: usize,
}

/// A part of a diff that can be staged or discarded on its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    /// The `@@ -a,b +c,d @@` line.
    pub header: String,
    /// The lines after it, each with its ` `, `+`, `-` or `\` mark.
    pub lines: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileDiff {
    /// The lines before the first hunk (`diff --git`, `index`, `---`, `+++`).
    pub header: Vec<String>,
    pub hunks: Vec<Hunk>,
    /// A binary file: no hunks to show.
    pub binary: bool,
}

impl FileDiff {
    /// A patch with only the hunk `index`, for `git apply`.
    pub fn patch_of(&self, index: usize) -> Option<String> {
        let hunk = self.hunks.get(index)?;
        let mut patch = String::new();
        for line in &self.header {
            patch.push_str(line);
            patch.push('\n');
        }
        patch.push_str(&hunk.header);
        patch.push('\n');
        for line in &hunk.lines {
            patch.push_str(line);
            patch.push('\n');
        }
        Some(patch)
    }
}

/// Which side of the index a diff shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// The working tree against the index: what `git add` would stage.
    Unstaged,
    /// The index against `HEAD`: what the next commit holds.
    Staged,
}

fn git(env: &dyn Environment, root: &str, args: &[&str]) -> io::Result<String> {
    let mut all = vec!["-c", "core.quotepath=off", "--no-optional-locks"];
    all.extend_from_slice(args);
    env.run("git", &all, Some(root))
}

/// The root of the repository that holds `cwd`, or `None` outside one.
pub fn root_of(env: &dyn Environment, cwd: &str) -> Option<String> {
    let out = env.run("git", &["rev-parse", "--show-toplevel"], Some(cwd)).ok()?;
    let root = out.trim();
    (!root.is_empty()).then(|| root.to_owned())
}

/// The status of the repository that holds `cwd`.
pub fn status(env: &dyn Environment, cwd: &str) -> io::Result<Status> {
    let root = root_of(env, cwd).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not a git repository"))?;
    let out = git(env, &root, &["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
    Ok(Status { root, files: parse_status(&out) })
}

/// `git status --porcelain=v1 -z`: `XY path\0`, a rename followed by its old path.
pub fn parse_status(out: &str) -> Vec<FileStatus> {
    let mut files = Vec::new();
    let mut parts = out.split('\0');
    while let Some(entry) = parts.next() {
        if entry.len() < 4 {
            continue;
        }
        let mut chars = entry.chars();
        let (x, y) = (chars.next().unwrap_or(' '), chars.next().unwrap_or(' '));
        let path = entry[3..].to_owned();
        if x == 'R' || x == 'C' {
            parts.next(); // The old path.
        }
        let untracked = x == '?';
        let letter = |c: char| (c != ' ' && c != '?' && c != '!').then_some(c);
        files.push(FileStatus {
            path,
            staged: if untracked { None } else { letter(x) },
            unstaged: if untracked { Some('?') } else { letter(y) },
            untracked,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

/// The diff of one file on one side, in hunks.
pub fn diff(env: &dyn Environment, root: &str, path: &str, side: Side) -> io::Result<FileDiff> {
    let mut args = vec!["diff", "--no-color", "--no-ext-diff", "-U3"];
    if side == Side::Staged {
        args.push("--cached");
    }
    args.extend(["--", path]);
    Ok(parse_diff(&git(env, root, &args)?))
}

pub fn parse_diff(text: &str) -> FileDiff {
    let mut diff = FileDiff::default();
    // Not `lines()`: it would drop the `\r` of a file kept with CRLF, and the patch of such a
    // hunk would no longer apply.
    for line in text.split_terminator('\n') {
        if line.starts_with("@@") {
            diff.hunks.push(Hunk { header: line.to_owned(), lines: Vec::new() });
        } else if let Some(hunk) = diff.hunks.last_mut() {
            hunk.lines.push(line.to_owned());
        } else {
            if line.starts_with("Binary files") {
                diff.binary = true;
            }
            diff.header.push(line.to_owned());
        }
    }
    diff
}

/// Stages one hunk of the unstaged diff.
pub fn stage_hunk(env: &dyn Environment, root: &str, diff: &FileDiff, index: usize) -> io::Result<()> {
    apply(env, root, diff, index, &["apply", "--cached", "--whitespace=nowarn", "-"])
}

/// Takes one hunk of the staged diff back out of the index; the working tree keeps it.
pub fn unstage_hunk(env: &dyn Environment, root: &str, diff: &FileDiff, index: usize) -> io::Result<()> {
    apply(env, root, diff, index, &["apply", "--cached", "-R", "--whitespace=nowarn", "-"])
}

/// Undoes one hunk of the unstaged diff in the working tree. The change is lost.
pub fn discard_hunk(env: &dyn Environment, root: &str, diff: &FileDiff, index: usize) -> io::Result<()> {
    apply(env, root, diff, index, &["apply", "-R", "--whitespace=nowarn", "-"])
}

fn apply(env: &dyn Environment, root: &str, diff: &FileDiff, index: usize, args: &[&str]) -> io::Result<()> {
    let patch = diff.patch_of(index).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such hunk"))?;
    let mut all = vec!["-c", "core.quotepath=off"];
    all.extend_from_slice(args);
    env.run_with_input("git", &all, Some(root), patch.as_bytes()).map(|_| ())
}

pub fn stage_file(env: &dyn Environment, root: &str, path: &str) -> io::Result<()> {
    git(env, root, &["add", "--", path]).map(|_| ())
}

pub fn unstage_file(env: &dyn Environment, root: &str, path: &str) -> io::Result<()> {
    git(env, root, &["reset", "-q", "--", path]).map(|_| ())
}

/// The last `limit` commits of the current branch, newest first.
pub fn log(env: &dyn Environment, root: &str, limit: usize) -> io::Result<Vec<Commit>> {
    let count = format!("-n{limit}");
    let out = git(env, root, &["log", &count, "--format=%H%x1f%h%x1f%s%x1f%an%x1f%ct%x1e"]);
    match out {
        Ok(out) => Ok(parse_log(&out)),
        // A repository without a commit yet has no history.
        Err(error) if error.to_string().contains("does not have any commits") => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

/// `git log` with fields split by the unit separator and commits by the record separator.
pub fn parse_log(out: &str) -> Vec<Commit> {
    out.split('\x1e')
        .filter_map(|record| {
            let fields: Vec<&str> = record.trim_start_matches(['\n', '\r']).split('\x1f').collect();
            let [hash, short, subject, author, time] = fields.as_slice() else { return None };
            Some(Commit {
                hash: (*hash).to_owned(),
                short: (*short).to_owned(),
                subject: (*subject).to_owned(),
                author: (*author).to_owned(),
                time_ms: time.trim().parse::<i64>().ok()? * 1000,
            })
        })
        .collect()
}

/// The branch of the working folder and its distance from the upstream.
pub fn branch(env: &dyn Environment, root: &str) -> io::Result<Branch> {
    Ok(parse_branch(&git(env, root, &["status", "--porcelain=v2", "--branch", "--untracked-files=no"])?))
}

/// The `# branch.*` lines of `git status --porcelain=v2 --branch`.
pub fn parse_branch(out: &str) -> Branch {
    let mut branch = Branch::default();
    for line in out.lines() {
        if let Some(head) = line.strip_prefix("# branch.head ") {
            branch.name = (head != "(detached)").then(|| head.to_owned());
        } else if let Some(upstream) = line.strip_prefix("# branch.upstream ") {
            branch.upstream = Some(upstream.to_owned());
        } else if let Some(counts) = line.strip_prefix("# branch.ab ") {
            for part in counts.split_whitespace() {
                if let Some(n) = part.strip_prefix('+') {
                    branch.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    branch.behind = n.parse().unwrap_or(0);
                }
            }
        }
    }
    branch
}

/// Pushes the branch to its upstream. Asks nothing: a remote that wants a password fails
/// with git's message.
pub fn push(env: &dyn Environment, root: &str) -> io::Result<()> {
    git(env, root, &["push"]).map(|_| ())
}

/// Fetches every remote, dropping branches deleted there.
pub fn fetch(env: &dyn Environment, root: &str) -> io::Result<()> {
    git(env, root, &["fetch", "--all", "--prune", "--quiet"]).map(|_| ())
}

/// Brings the upstream's commits in when that needs no merge; a branch that went its own
/// way fails with git's message instead of making a merge commit.
pub fn pull(env: &dyn Environment, root: &str) -> io::Result<()> {
    git(env, root, &["pull", "--ff-only", "--quiet"]).map(|_| ())
}

/// The local branches, the most recently committed first.
pub fn branches(env: &dyn Environment, root: &str) -> io::Result<Vec<String>> {
    let out = git(env, root, &["for-each-ref", "--sort=-committerdate", "--format=%(refname:short)", "refs/heads/"])?;
    Ok(out.lines().map(str::trim).filter(|name| !name.is_empty()).map(str::to_owned).collect())
}

/// Switches the working tree to another local branch. Changes that would be lost stop it
/// with git's message.
pub fn switch(env: &dyn Environment, root: &str, name: &str) -> io::Result<()> {
    if name.starts_with('-') {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a branch name"));
    }
    git(env, root, &["switch", "--quiet", name]).map(|_| ())
}

/// The changes of one commit, file by file, against its first parent.
pub fn show(env: &dyn Environment, root: &str, hash: &str) -> io::Result<Vec<(String, FileDiff)>> {
    Ok(split_patch(&git(env, root, &["show", "--format=", "--patch", "--no-color", hash])?))
}

/// A patch of several files as one diff per file, each with its path (the new one, or
/// the old one for a deleted file).
pub fn split_patch(text: &str) -> Vec<(String, FileDiff)> {
    let mut files: Vec<String> = Vec::new();
    for line in text.split_terminator('\n') {
        if line.starts_with("diff --git ") || files.is_empty() {
            files.push(String::new());
        }
        if let Some(last) = files.last_mut() {
            last.push_str(line);
            last.push('\n');
        }
    }
    files
        .into_iter()
        .filter(|file| file.starts_with("diff --git "))
        .map(|file| {
            let diff = parse_diff(&file);
            let path_of = |prefix: &str| {
                diff.header.iter().find_map(|line| line.strip_prefix(prefix)).map(|p| p.trim_end_matches('\r').to_owned()).filter(|p| p != "/dev/null")
            };
            let path = path_of("+++ b/")
                .or_else(|| path_of("--- a/"))
                .or_else(|| file.lines().next().and_then(|first| first.rsplit(" b/").next()).map(str::to_owned))
                .unwrap_or_default();
            (path, diff)
        })
        .collect()
}

/// Commits what is staged with `message` (given on standard input, so any text goes, over
/// SSH too). Returns the short hash and subject of the new commit.
pub fn commit(env: &dyn Environment, root: &str, message: &str) -> io::Result<String> {
    if message.trim().is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "the commit message is empty"));
    }
    env.run_with_input("git", &["-c", "core.quotepath=off", "commit", "-q", "-F", "-"], Some(root), message.as_bytes())?;
    Ok(git(env, root, &["log", "-1", "--format=%h %s"])?.trim().to_owned())
}

/// The files and folders `.gitignore` leaves out of the repository that holds `cwd`, as
/// paths under its root with forward slashes. An ignored folder is listed once, not file
/// by file.
pub fn ignored(env: &dyn Environment, cwd: &str) -> io::Result<Vec<String>> {
    let root = root_of(env, cwd).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not a git repository"))?;
    let out = git(env, &root, &["ls-files", "--others", "--ignored", "--exclude-standard", "--directory", "-z"])?;
    let root = root.trim_end_matches('/');
    Ok(out.split('\0').map(|path| path.trim_end_matches('/')).filter(|path| !path.is_empty()).map(|path| format!("{root}/{path}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use envs::LocalEnvironment;
    use test_support::TempDir;

    #[test]
    fn status_lines_parse() {
        let out = "M  src/a.rs\0 M b.txt\0?? new file.md\0R  c2.rs\0c.rs\0MM both.rs\0";
        let files = parse_status(out);
        let find = |p: &str| files.iter().find(|f| f.path == p).unwrap().clone();
        assert_eq!(find("src/a.rs").staged, Some('M'));
        assert_eq!(find("src/a.rs").unstaged, None);
        assert_eq!(find("b.txt").unstaged, Some('M'));
        assert!(find("new file.md").untracked);
        assert_eq!(find("c2.rs").staged, Some('R'));
        assert!(!files.iter().any(|f| f.path == "c.rs"), "the old path of a rename is not a file");
        assert_eq!((find("both.rs").staged, find("both.rs").unstaged), (Some('M'), Some('M')));
    }

    #[test]
    fn a_diff_splits_into_hunks_and_one_makes_a_patch() {
        let text = "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n-a\n+A\n b\n@@ -9 +9 @@\n-y\n+Y\n";
        let diff = parse_diff(text);
        assert_eq!(diff.header.len(), 4);
        assert_eq!(diff.hunks.len(), 2);
        assert_eq!(diff.patch_of(1).unwrap(), "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n@@ -9 +9 @@\n-y\n+Y\n");
    }

    fn git_ok(dir: &std::path::Path, args: &[&str]) {
        let ok = std::process::Command::new("git").args(args).current_dir(dir).output().unwrap().status.success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn log_branch_and_patches_parse() {
        let out = "abc123\x1fabc\x1fFix the parser\x1fAda\x1f1795000000\x1e\ndef456\x1fdef\x1fFirst\x1fBo\x1f1794000000\x1e\n";
        let commits = parse_log(out);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].subject, "Fix the parser");
        assert_eq!(commits[1].time_ms, 1_794_000_000_000);

        let branch = parse_branch("# branch.oid abc\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -1\n1 .M N... x\n");
        assert_eq!(branch, Branch { name: Some("main".into()), upstream: Some("origin/main".into()), ahead: 2, behind: 1 });
        assert_eq!(parse_branch("# branch.head (detached)\n").name, None);

        let patch = "diff --git a/a.txt b/a.txt\nindex 1..2 100644\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/gone.rs b/gone.rs\ndeleted file mode 100644\n--- a/gone.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\n";
        let files = split_patch(patch);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].0, "a.txt");
        assert_eq!(files[1].0, "gone.rs", "a deleted file keeps its old path");
        assert_eq!(files[1].1.hunks.len(), 1);
    }

    #[test]
    fn a_commit_takes_the_staged_files_and_ignored_files_are_listed() {
        if std::process::Command::new("git").arg("--version").output().is_err() {
            return; // No git on this machine.
        }
        let dir = TempDir::new("vcs_commit");
        let root = dir.path();
        git_ok(root, &["init", "-q"]);
        git_ok(root, &["config", "user.email", "test@example.com"]);
        git_ok(root, &["config", "user.name", "Test"]);
        std::fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::write(root.join("target/debug/app"), "bin").unwrap();
        std::fs::write(root.join("run.log"), "log").unwrap();
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        let env = LocalEnvironment::new();
        let cwd = root.to_string_lossy().into_owned();
        let top = root_of(&env, &cwd).unwrap();
        let mut list = ignored(&env, &cwd).unwrap();
        list.sort();
        assert_eq!(list, vec![format!("{top}/run.log"), format!("{top}/target")], "a folder once, not its files");

        assert!(commit(&env, &top, "  ").is_err(), "no empty message");
        stage_file(&env, &top, "a.txt").unwrap();
        let made = commit(&env, &top, "Add a\n\nWith a body: \"quotes\" and $dollars").unwrap();
        assert!(made.ends_with(" Add a"), "{made}");
        let body = git(&env, &top, &["log", "-1", "--format=%b"]).unwrap();
        assert_eq!(body.trim(), "With a body: \"quotes\" and $dollars");
        let after = status(&env, &cwd).unwrap();
        assert!(after.files.iter().all(|f| f.path != "a.txt"), "committed");
        let history = log(&env, &top, 10).unwrap();
        assert_eq!(history[0].subject, "Add a");
        let shown = show(&env, &top, &history[0].hash).unwrap();
        assert_eq!(shown[0].0, "a.txt");
        assert_eq!(branch(&env, &top).unwrap().upstream, None, "no remote");
    }

    /// Two clones of one bare repository: what one pushes, the other fetches and pulls;
    /// branches list and switch.
    #[test]
    fn fetch_pull_and_switch_on_real_repositories() {
        if std::process::Command::new("git").arg("--version").output().is_err() {
            return; // No git on this machine.
        }
        let dir = TempDir::new("vcs_sync");
        let base = dir.path();
        let (bare, one, two) = (base.join("origin.git"), base.join("one"), base.join("two"));
        git_ok(base, &["init", "-q", "--bare", "-b", "main", &bare.to_string_lossy()]);
        for clone in [&one, &two] {
            git_ok(base, &["clone", "-q", &bare.to_string_lossy(), &clone.to_string_lossy()]);
            git_ok(clone, &["config", "user.email", "test@example.com"]);
            git_ok(clone, &["config", "user.name", "Test"]);
            git_ok(clone, &["checkout", "-q", "-B", "main"]);
        }
        std::fs::write(one.join("a.txt"), "a\n").unwrap();
        git_ok(&one, &["add", "a.txt"]);
        git_ok(&one, &["commit", "-q", "-m", "First"]);
        git_ok(&one, &["push", "-q", "-u", "origin", "main"]);
        git_ok(&two, &["pull", "-q", "origin", "main"]);
        git_ok(&two, &["branch", "-q", "--set-upstream-to=origin/main"]);
        std::fs::write(one.join("b.txt"), "b\n").unwrap();
        git_ok(&one, &["add", "b.txt"]);
        git_ok(&one, &["commit", "-q", "-m", "Second"]);
        git_ok(&one, &["push", "-q"]);

        let env = LocalEnvironment::new();
        let top = root_of(&env, &two.to_string_lossy()).unwrap();
        assert_eq!(branch(&env, &top).unwrap().behind, 0, "not fetched yet");
        fetch(&env, &top).unwrap();
        assert_eq!(branch(&env, &top).unwrap().behind, 1);
        pull(&env, &top).unwrap();
        assert_eq!(branch(&env, &top).unwrap().behind, 0);
        assert!(two.join("b.txt").exists());

        git_ok(&two, &["branch", "-q", "topic"]);
        let mut names = branches(&env, &top).unwrap();
        names.sort();
        assert_eq!(names, vec!["main", "topic"]);
        switch(&env, &top, "topic").unwrap();
        assert_eq!(branch(&env, &top).unwrap().name.as_deref(), Some("topic"));
        assert!(switch(&env, &top, "--orphan").is_err(), "no options through the name");
        assert!(switch(&env, &top, "missing").is_err());
    }

    /// The same over SSH, in a temporary folder of a real host:
    /// `BOWYER_SSH_HOST=user@host BOWYER_SSH_KEY=~/.ssh/key cargo test -p vcs -- --ignored`.
    #[test]
    #[ignore = "needs an SSH host"]
    fn fetch_pull_and_switch_over_ssh() {
        let host = std::env::var("BOWYER_SSH_HOST").expect("BOWYER_SSH_HOST");
        let key = std::env::var_os("BOWYER_SSH_KEY").map(std::path::PathBuf::from);
        let env = envs::SshEnvironment::new(&host).with_identity(key);
        let base = env.run("mktemp", &["-d", "-t", "bowyer-vcs.XXXXXX"], None).unwrap().trim().to_owned();
        let setup = r#"set -e; cd "$1"
git init -q --bare -b main origin.git
for c in one two; do git clone -q origin.git $c 2>/dev/null; git -C $c config user.email test@example.com; git -C $c config user.name Test; git -C $c checkout -q -B main; done
echo a > one/a.txt; git -C one add a.txt; git -C one commit -q -m First; git -C one push -q -u origin main
git -C two pull -q origin main; git -C two branch -q --set-upstream-to=origin/main; git -C two branch -q topic
echo b > one/b.txt; git -C one add b.txt; git -C one commit -q -m Second; git -C one push -q"#;
        env.run("sh", &["-c", setup, "sh", &base], None).unwrap();
        let two = format!("{base}/two");
        let result = std::panic::catch_unwind(|| {
            fetch(&env, &two).unwrap();
            assert_eq!(branch(&env, &two).unwrap().behind, 1);
            pull(&env, &two).unwrap();
            assert_eq!(branch(&env, &two).unwrap().behind, 0);
            assert!(branches(&env, &two).unwrap().contains(&"topic".to_owned()));
            switch(&env, &two, "topic").unwrap();
            assert_eq!(branch(&env, &two).unwrap().name.as_deref(), Some("topic"));
        });
        env.run("rm", &["-rf", "--", &base], None).unwrap();
        result.unwrap();
    }

    /// A real repository: stage the second hunk only, then discard the first.
    #[test]
    fn hunks_stage_and_discard_on_a_real_repository() {
        if std::process::Command::new("git").arg("--version").output().is_err() {
            return; // No git on this machine.
        }
        let dir = TempDir::new("vcs");
        let root = dir.path();
        git_ok(root, &["init", "-q"]);
        git_ok(root, &["config", "user.email", "test@example.com"]);
        git_ok(root, &["config", "user.name", "Test"]);
        git_ok(root, &["config", "core.autocrlf", "false"]);
        let lines: Vec<String> = (1..=30).map(|i| format!("line {i}")).collect();
        std::fs::write(root.join("f.txt"), lines.join("\n") + "\n").unwrap();
        git_ok(root, &["add", "."]);
        git_ok(root, &["commit", "-qm", "first"]);
        let mut changed = lines.clone();
        changed[1] = "LINE 2".into();
        changed[27] = "LINE 28".into();
        std::fs::write(root.join("f.txt"), changed.join("\n") + "\n").unwrap();

        let env = LocalEnvironment::new();
        let cwd = root.to_string_lossy().into_owned();
        let status = status(&env, &cwd).unwrap();
        assert_eq!(status.files.len(), 1);
        assert_eq!(status.files[0].unstaged, Some('M'));
        let repo = status.root.clone();

        let unstaged = diff(&env, &repo, "f.txt", Side::Unstaged).unwrap();
        assert_eq!(unstaged.hunks.len(), 2);
        stage_hunk(&env, &repo, &unstaged, 1).unwrap();
        let staged = diff(&env, &repo, "f.txt", Side::Staged).unwrap();
        assert_eq!(staged.hunks.len(), 1);
        assert!(staged.hunks[0].lines.iter().any(|l| l == "+LINE 28"));

        let left = diff(&env, &repo, "f.txt", Side::Unstaged).unwrap();
        assert_eq!(left.hunks.len(), 1);
        discard_hunk(&env, &repo, &left, 0).unwrap();
        let text = std::fs::read_to_string(root.join("f.txt")).unwrap();
        assert!(text.contains("line 2\n") && text.contains("LINE 28"), "the first change is gone, the staged one stays");

        unstage_hunk(&env, &repo, &staged, 0).unwrap();
        assert!(diff(&env, &repo, "f.txt", Side::Staged).unwrap().hunks.is_empty());
    }
}
