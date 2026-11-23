//! Deleting a session the user asked to delete: its files move to `~/.bowyer-trash/<harness>`
//! on the machine that has them, where they can be put back by hand. Only harnesses that
//! keep one file per session lose files; a session inside a shared store (the database of
//! OpenCode) is never touched.

use std::io;
use std::path::Path;

use envs::Environment;
use model::{HarnessKind, SessionSummary};

/// The folder in the home folder that takes deleted sessions.
pub const TRASH_DIR: &str = ".bowyer-trash";

/// The files and folders of a session, when the harness keeps them apart from other
/// sessions; `None` when deleting would mean editing a store other sessions share.
pub fn session_files(summary: &SessionSummary) -> Option<Vec<String>> {
    let path = summary.source.path.as_str();
    if path.is_empty() || path.contains('#') {
        return None;
    }
    match summary.key.harness {
        // `<project>/<id>.jsonl`, and the subagents and large tool results of the session
        // in `<project>/<id>/`.
        HarnessKind::Claude => {
            let mut files = vec![path.to_owned()];
            if let Some(folder) = path.strip_suffix(".jsonl") {
                files.push(folder.to_owned());
            }
            Some(files)
        }
        // One rollout file per session.
        HarnessKind::Codex => Some(vec![path.to_owned()]),
        _ => None,
    }
}

/// Moves `files` into the trash of `harness` on the machine of `env`. A file that is not
/// there any more is skipped.
pub fn move_to_trash(env: &dyn Environment, harness: &HarnessKind, files: &[String]) -> io::Result<()> {
    let home = env.home()?;
    let trash = env.join(&env.join(&home, TRASH_DIR), harness.id());
    if env.id().is_local() {
        std::fs::create_dir_all(&trash)?;
        for file in files {
            let from = Path::new(file);
            if !from.exists() {
                continue;
            }
            let Some(name) = from.file_name() else { continue };
            std::fs::rename(from, Path::new(&trash).join(name))?;
        }
        return Ok(());
    }
    let script = r#"set -e; trash="$1"; shift; mkdir -p "$trash"; for p in "$@"; do if [ -e "$p" ]; then mv -- "$p" "$trash/"; fi; done"#;
    let mut args = vec!["-c", script, "sh", trash.as_str()];
    args.extend(files.iter().map(String::as_str));
    env.run("sh", &args, None).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use envs::LocalEnvironment;
    use model::{EnvId, SessionKey, SourceStamp};
    use test_support::TempDir;

    fn summary(harness: HarnessKind, path: &str) -> SessionSummary {
        SessionSummary {
            key: SessionKey { env: EnvId::local(), harness, id: "s1".into() },
            title: String::new(),
            first_prompt: None,
            cwd: None,
            git_branch: None,
            model: None,
            created_ms: 0,
            updated_ms: 0,
            parent: None,
            source: SourceStamp { path: path.into(), size: 0, modified_ms: 0 },
            turn: None,
        }
    }

    #[test]
    fn only_sessions_with_files_of_their_own_are_deleted() {
        let claude = session_files(&summary(HarnessKind::Claude, "/h/.claude/projects/p/s1.jsonl")).unwrap();
        assert_eq!(claude, ["/h/.claude/projects/p/s1.jsonl", "/h/.claude/projects/p/s1"]);
        assert_eq!(session_files(&summary(HarnessKind::Codex, "/h/.codex/sessions/r.jsonl")).unwrap().len(), 1);
        assert!(session_files(&summary(HarnessKind::OpenCode, "/h/opencode.db#s1")).is_none(), "a shared database");
        assert!(session_files(&summary(HarnessKind::DeepSeek, "/h/.dsh/s1/session.json")).is_none());
        assert!(session_files(&summary(HarnessKind::Claude, "")).is_none());
    }

    #[test]
    fn a_deleted_session_waits_in_the_trash_of_its_home() {
        let home = TempDir::new("trash-home");
        let project = home.path().join(".claude").join("projects").join("p");
        std::fs::create_dir_all(project.join("s1").join("subagents")).unwrap();
        std::fs::write(project.join("s1.jsonl"), "{}\n").unwrap();
        std::fs::write(project.join("s1").join("subagents").join("a.jsonl"), "{}\n").unwrap();
        std::fs::write(project.join("s2.jsonl"), "{}\n").unwrap();
        let path = project.join("s1.jsonl").to_string_lossy().into_owned();
        let files = session_files(&summary(HarnessKind::Claude, &path)).unwrap();

        let env = LocalEnvironment::with_home(home.path());
        move_to_trash(&env, &HarnessKind::Claude, &files).unwrap();

        let trash = home.path().join(TRASH_DIR).join("claude");
        assert!(trash.join("s1.jsonl").is_file());
        assert!(trash.join("s1").join("subagents").join("a.jsonl").is_file());
        assert!(!project.join("s1.jsonl").exists() && !project.join("s1").exists());
        assert!(project.join("s2.jsonl").exists(), "other sessions stay");
        // Deleting again finds nothing to move.
        move_to_trash(&env, &HarnessKind::Claude, &files).unwrap();
    }
}
