//! The changed files of a session as a tree of folders. Chains of folders with one folder
//! inside and no files show as one row (`.claude/worktrees`), as file trees of editors do.
//! Folders stay closed to one line with their totals until the user opens them.

use std::collections::HashSet;

use model::FileChange;
use ui_kit::prelude::*;
use ui_kit::*;

use super::{MONO, Shell};

/// A folder of changed files.
#[derive(Debug, Default, PartialEq)]
pub(super) struct ChangeDir<'a> {
    /// The name shown: one folder or a chain of them, `a/b`.
    pub name: String,
    /// The path from the session's folder, the key of its open state.
    pub path: String,
    pub dirs: Vec<ChangeDir<'a>>,
    pub files: Vec<(String, &'a FileChange)>,
}

impl ChangeDir<'_> {
    pub fn file_count(&self) -> usize {
        self.files.len() + self.dirs.iter().map(ChangeDir::file_count).sum::<usize>()
    }

    pub fn totals(&self) -> (u64, u64) {
        let own = self.files.iter().fold((0, 0), |(a, r), (_, change)| (a + u64::from(change.added), r + u64::from(change.removed)));
        self.dirs.iter().map(ChangeDir::totals).fold(own, |(a, r), (da, dr)| (a + da, r + dr))
    }
}

/// `path` from the session's folder when it lies inside it, with `/` for separators.
pub(super) fn relative_path(path: &str, cwd: &str) -> String {
    let normalized = path.replace('\\', "/");
    let base = cwd.replace('\\', "/");
    let base = base.trim_end_matches('/');
    if !base.is_empty() && normalized.to_lowercase().starts_with(&format!("{}/", base.to_lowercase())) {
        normalized[base.len() + 1..].to_owned()
    } else {
        normalized
    }
}

/// The tree of `changes`, folders and files by name.
pub(super) fn build<'a>(changes: &'a [FileChange], cwd: &str) -> ChangeDir<'a> {
    let mut root = ChangeDir::default();
    for change in changes {
        let path = relative_path(&change.path, cwd);
        let mut parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        let Some(name) = parts.pop() else { continue };
        let mut dir = &mut root;
        for part in parts {
            let at = match dir.dirs.iter().position(|d| d.name == part) {
                Some(at) => at,
                None => {
                    let path = if dir.path.is_empty() { part.to_owned() } else { format!("{}/{part}", dir.path) };
                    dir.dirs.push(ChangeDir { name: part.to_owned(), path, ..Default::default() });
                    dir.dirs.len() - 1
                }
            };
            dir = &mut dir.dirs[at];
        }
        dir.files.push((name.to_owned(), change));
    }
    sort_and_join(&mut root);
    root
}

fn sort_and_join(dir: &mut ChangeDir<'_>) {
    for child in &mut dir.dirs {
        sort_and_join(child);
        while child.files.is_empty() && child.dirs.len() == 1 {
            let inner = child.dirs.pop().unwrap_or_default();
            child.name = format!("{}/{}", child.name, inner.name);
            child.path = inner.path;
            child.dirs = inner.dirs;
            child.files = inner.files;
        }
    }
    dir.dirs.sort_by_key(|d| d.name.to_lowercase());
    dir.files.sort_by_key(|f| f.0.to_lowercase());
}

/// The rows of the tree; files of the session's own folder first.
pub(super) fn render(changes: &[FileChange], cwd: &str, opened: &HashSet<String>, theme: &Theme, cx: &mut Context<Shell>) -> impl IntoElement {
    let root = build(changes, cwd);
    let mut rows = Vec::new();
    push_files(&root, 0, &mut rows, theme, cx);
    push_dirs(&root, 0, opened, &mut rows, theme, cx);
    div().flex().flex_col().gap(px(1.)).children(rows)
}

const INDENT: f32 = 14.;

fn counts(added: u64, removed: u64, theme: &Theme) -> [Div; 2] {
    [
        div().flex_none().text_color(theme.success).font_family(MONO).text_size(px(11.)).child(format!("+{added}")),
        div().flex_none().text_color(theme.error).font_family(MONO).text_size(px(11.)).child(format!("−{removed}")),
    ]
}

fn push_dirs(dir: &ChangeDir<'_>, depth: usize, opened: &HashSet<String>, rows: &mut Vec<AnyElement>, theme: &Theme, cx: &mut Context<Shell>) {
    for child in &dir.dirs {
        let open = opened.contains(&child.path);
        let (added, removed) = child.totals();
        let toggle = child.path.clone();
        rows.push(
            div()
                .id(SharedString::from(format!("change-dir-{}", child.path)))
                .flex()
                .items_center()
                .gap(px(4.))
                .h(px(22.))
                .pl(px(depth as f32 * INDENT))
                .rounded(px(5.))
                .hover(|style| style.bg(theme.hover))
                .text_size(px(12.))
                .text_color(theme.text_secondary)
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    if !this.opened_change_folders.remove(&toggle) {
                        this.opened_change_folders.insert(toggle.clone());
                    }
                    cx.notify();
                }))
                .child(icon_sized(if open { "general/chevronDown" } else { "general/chevronRight" }, 12., theme))
                .child(icon_sized("general/folder", 14., theme))
                .child(div().flex_1().min_w_0().truncate().child(child.name.clone()))
                .child(div().flex_none().text_color(theme.text_muted).text_size(px(11.)).child(child.file_count().to_string()))
                .when(!open, |this| this.children(counts(added, removed, theme)))
                .into_any_element(),
        );
        if open {
            push_dirs(child, depth + 1, opened, rows, theme, cx);
            push_files(child, depth + 1, rows, theme, cx);
        }
    }
}

fn push_files(dir: &ChangeDir<'_>, depth: usize, rows: &mut Vec<AnyElement>, theme: &Theme, cx: &mut Context<Shell>) {
    for (name, change) in &dir.files {
        let path = change.path.clone();
        let kind_color = match change.kind {
            model::ChangeKind::Created => theme.success,
            model::ChangeKind::Modified => theme.info,
            model::ChangeKind::Deleted => theme.error,
        };
        rows.push(
            div()
                .id(SharedString::from(format!("change-{path}")))
                .flex()
                .items_center()
                .gap(px(6.))
                .h(px(22.))
                .pl(px(depth as f32 * INDENT + if depth > 0 { 16. } else { 0. }))
                .rounded(px(5.))
                .hover(|style| style.bg(theme.hover))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.open_preview(path.clone(), cx)))
                .text_size(px(12.))
                .child(div().size(px(6.)).flex_none().rounded_full().bg(kind_color))
                .child(div().flex_1().min_w_0().truncate().child(name.clone()))
                .children(counts(u64::from(change.added), u64::from(change.removed), theme))
                .into_any_element(),
        );
    }
}

#[cfg(test)]
mod tests {
    use model::FileChange;

    use super::build;

    fn change(path: &str, added: u32) -> FileChange {
        FileChange { path: path.into(), kind: model::ChangeKind::Modified, added, removed: 1, edits: 1 }
    }

    #[test]
    fn single_folder_chains_join_and_totals_add_up() {
        let changes = vec![
            change("C:\\w\\ide\\NOTES.md", 10),
            change("C:/w/ide/.claude/worktrees/t1/src/a.rs", 2),
            change("C:/w/ide/.claude/worktrees/t2/src/b.rs", 3),
            change("C:/w/ide/.claude/worktrees/t2/README.md", 4),
            change("/elsewhere/x.txt", 1),
        ];
        let root = build(&changes, "C:\\w\\ide");
        assert_eq!(root.files.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(), vec!["NOTES.md"]);
        let names: Vec<&str> = root.dirs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec![".claude/worktrees", "elsewhere"]);
        let worktrees = &root.dirs[0];
        assert_eq!(worktrees.path, ".claude/worktrees");
        assert_eq!(worktrees.file_count(), 3);
        assert_eq!(worktrees.totals(), (9, 3));
        assert_eq!(worktrees.dirs[0].name, "t1/src");
        assert_eq!(worktrees.dirs[0].path, ".claude/worktrees/t1/src");
        assert_eq!(worktrees.dirs[1].name, "t2", "a folder with a file of its own stays");
        assert_eq!(worktrees.dirs[1].dirs[0].name, "src");
    }
}
