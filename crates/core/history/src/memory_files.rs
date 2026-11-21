//! The instruction files a harness puts into the context by itself at the start of a
//! session: `CLAUDE.md` and `AGENTS.md` of the folder and the folders above it, the user's
//! global one, and Claude Code's automatic memory. Their size takes a measured part out of
//! what the transcript does not show.

use envs::{Environment, ReadRange};
use model::{HarnessKind, SessionSummary, estimate_tokens};

/// Claude Code reads only the first lines of its automatic memory index.
const AUTO_MEMORY_LINES: usize = 200;

/// The folders from `cwd` up to the one under the root of the file system, nearest first.
fn folders_up(cwd: &str) -> Vec<String> {
    let path = cwd.replace('\\', "/");
    let path = path.trim_end_matches('/');
    let mut folders = Vec::new();
    let mut current = path.to_owned();
    loop {
        let Some(cut) = current.rfind('/') else { break };
        // `/home` and `C:/Users` are the last ones: `/` and `C:` are the root.
        if cut == 0 || current[..cut].ends_with(':') && !current[..cut].contains('/') {
            folders.push(current.clone());
            break;
        }
        folders.push(current.clone());
        current.truncate(cut);
    }
    folders
}

/// The files `harness` loads for a session in `cwd`, as paths of the machine the session
/// is on (`home` is its home folder). Files that do not exist are fine: they are skipped.
pub fn memory_file_paths(harness: &HarnessKind, cwd: &str, home: &str, session_path: &str) -> Vec<String> {
    let home = home.replace('\\', "/").trim_end_matches('/').to_owned();
    let (names, global): (&[&str], Vec<String>) = match harness {
        HarnessKind::Claude => (&["CLAUDE.md", "CLAUDE.local.md", ".claude/CLAUDE.md"], vec![format!("{home}/.claude/CLAUDE.md")]),
        HarnessKind::Codex => (&["AGENTS.md"], vec![format!("{home}/.codex/AGENTS.md")]),
        HarnessKind::OpenCode => (&["AGENTS.md"], vec![format!("{home}/.config/opencode/AGENTS.md")]),
        _ => return Vec::new(),
    };
    let mut paths = global;
    for folder in folders_up(cwd) {
        for name in names {
            paths.push(format!("{folder}/{name}"));
        }
    }
    if *harness == HarnessKind::Claude
        && let Some(cut) = session_path.replace('\\', "/").rfind('/')
    {
        paths.push(format!("{}/memory/MEMORY.md", &session_path.replace('\\', "/")[..cut]));
    }
    let mut seen = std::collections::HashSet::new();
    paths.retain(|path| seen.insert(if cfg!(windows) { path.to_lowercase() } else { path.clone() }));
    paths
}

/// Tokens of the memory files of a session, estimated from their text; read in one batch.
pub fn memory_tokens(env: &dyn Environment, summary: &SessionSummary) -> u64 {
    let Some(cwd) = summary.cwd.as_deref() else { return 0 };
    let Ok(home) = env.home() else { return 0 };
    let paths = memory_file_paths(&summary.key.harness, cwd, &home, &summary.source.path);
    if paths.is_empty() {
        return 0;
    }
    let requests: Vec<(String, ReadRange)> = paths.iter().map(|path| (path.clone(), ReadRange::Head(256 * 1024))).collect();
    paths
        .iter()
        .zip(env.read_many(&requests))
        .filter_map(|(path, bytes)| {
            let text = String::from_utf8_lossy(&bytes.ok()?).into_owned();
            let text = if path.ends_with("/memory/MEMORY.md") { text.lines().take(AUTO_MEMORY_LINES).collect::<Vec<_>>().join("\n") } else { text };
            Some(estimate_tokens(&text))
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_go_up_to_the_root() {
        assert_eq!(folders_up("/home/u/app/"), vec!["/home/u/app", "/home/u", "/home"]);
        assert_eq!(folders_up("C:\\Users\\u\\app"), vec!["C:/Users/u/app", "C:/Users/u", "C:/Users"]);
    }

    #[test]
    fn claude_reads_its_files_up_the_tree_and_its_automatic_memory() {
        let paths = memory_file_paths(&HarnessKind::Claude, "/home/u/app", "/home/u", "/home/u/.claude/projects/-home-u-app/s1.jsonl");
        assert_eq!(paths[0], "/home/u/.claude/CLAUDE.md");
        assert!(paths.contains(&"/home/u/app/CLAUDE.md".to_owned()));
        assert!(paths.contains(&"/home/u/app/.claude/CLAUDE.md".to_owned()));
        assert!(paths.contains(&"/home/CLAUDE.md".to_owned()));
        assert_eq!(paths.last().unwrap(), "/home/u/.claude/projects/-home-u-app/memory/MEMORY.md");
        let codex = memory_file_paths(&HarnessKind::Codex, "/home/u/app", "/home/u", "");
        assert_eq!(codex, vec!["/home/u/.codex/AGENTS.md", "/home/u/app/AGENTS.md", "/home/u/AGENTS.md", "/home/AGENTS.md"]);
        assert!(memory_file_paths(&HarnessKind::DeepSeek, "/home/u/app", "/home/u", "").is_empty());
    }

    #[test]
    fn the_files_on_disk_count_and_missing_ones_do_not() {
        let dir = test_support::TempDir::new("memory_files");
        let root = dir.path().to_string_lossy().replace('\\', "/");
        std::fs::create_dir_all(dir.path().join("app/.claude")).unwrap();
        std::fs::write(dir.path().join("app/CLAUDE.md"), "a".repeat(400)).unwrap();
        let env = envs::LocalEnvironment::new();
        let mut summary = model::SessionSummary {
            key: model::SessionKey { env: env.id(), harness: HarnessKind::Claude, id: "s".into() },
            title: String::new(),
            first_prompt: None,
            cwd: Some(format!("{root}/app")),
            git_branch: None,
            model: None,
            created_ms: 0,
            updated_ms: 0,
            parent: None,
            source: model::SourceStamp { path: format!("{root}/nowhere/s.jsonl"), size: 0, modified_ms: 0 },
            turn: None,
        };
        let tokens = memory_tokens(&env, &summary);
        assert!(tokens >= estimate_tokens(&"a".repeat(400)), "{tokens}");
        summary.cwd = None;
        assert_eq!(memory_tokens(&env, &summary), 0);
    }
}
