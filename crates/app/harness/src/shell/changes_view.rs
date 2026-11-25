//! Git changes of the session's folder: the Changes tab lists what is staged and what is
//! not; a file opens as a diff in the centre, where each hunk can be staged, unstaged or
//! discarded. Discarding asks for a second click.

use std::sync::Arc;

use envs::SharedEnv;
use model::EnvId;
use ui_kit::prelude::*;
use ui_kit::*;
use vcs::{Branch, Commit, FileDiff, Side, Status};

use super::{MONO, Shell};
use crate::highlight;
use crate::i18n::t;

/// Diff lines drawn at most; a bigger diff says how many are left out.
pub(super) const MAX_LINES: usize = 4000;

#[derive(Default)]
pub(super) struct ChangesState {
    key: Option<(EnvId, String)>,
    status: Option<Result<Arc<Status>, String>>,
    loading: bool,
    /// The message of the next commit; kept while the folder stays the same.
    message: TextField,
    committing: bool,
    branch: Option<Branch>,
    history: Vec<Commit>,
    history_open: bool,
    /// The git command that talks to a remote or moves the branch, while it runs.
    syncing: Option<SyncOp>,
    /// The local branches, listed under the branch row when it is clicked.
    branches: Option<Vec<String>>,
    branches_open: bool,
}

/// A git command of the branch row.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SyncOp {
    Fetch,
    Pull,
    Push,
    Switch(String),
}

/// How many commits the history shows.
const HISTORY: usize = 30;

/// A commit opened from the history: its changes in the centre, read only.
pub(super) struct CommitView {
    hash: String,
    title: String,
    files: Option<Result<Arc<Vec<(String, FileDiff)>>, String>>,
}

pub(super) struct DiffView {
    pub path: String,
    pub side: Side,
    diff: Option<Result<Arc<FileDiff>, String>>,
    /// The hunk whose Discard was clicked once and waits for the second click.
    confirm: Option<usize>,
    busy: bool,
}

/// What a click on a hunk does.
#[derive(Clone, Copy)]
enum HunkAction {
    Stage,
    Unstage,
    Discard,
}

/// The old and new numbers of the first line of a hunk, from `@@ -a,b +c,d @@`.
fn hunk_start(header: &str) -> (usize, usize) {
    let number = |mark: char| {
        header
            .split_whitespace()
            .find(|part| part.starts_with(mark))
            .and_then(|part| part[1..].split(',').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(1)
    };
    (number('-'), number('+'))
}

impl Shell {
    fn changes_key(&self) -> Option<(EnvId, String)> {
        let open = self.open.as_ref()?;
        Some((open.summary.key.env.clone(), open.summary.cwd.clone()?))
    }

    /// Reads `git status` again for the open session's folder.
    pub(super) fn refresh_changes(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.changes_key() else { return };
        let Some(env) = self.env_for(&key.0) else { return };
        if self.changes.key.as_ref() != Some(&key) {
            self.changes = ChangesState { key: Some(key.clone()), ..Default::default() };
        }
        self.changes.loading = true;
        let cwd = key.1.clone();
        let task = cx.background_spawn(async move {
            let status = vcs::status(&*env, &cwd).map(Arc::new).map_err(|e| e.to_string());
            let (branch, history) = match &status {
                Ok(status) => (vcs::branch(&*env, &status.root).ok(), vcs::log(&*env, &status.root, HISTORY).unwrap_or_default()),
                Err(_) => (None, Vec::new()),
            };
            (status, branch, history)
        });
        cx.spawn(async move |this, cx| {
            let (status, branch, history) = task.await;
            this.update(cx, |this, cx| {
                if this.changes.key.as_ref() == Some(&key) {
                    this.changes.status = Some(status);
                    this.changes.branch = branch;
                    this.changes.history = history;
                    this.changes.loading = false;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn repo(&self) -> Option<(SharedEnv, String)> {
        let (env, _) = self.changes.key.clone()?;
        let root = self.changes.status.as_ref()?.as_ref().ok()?.root.clone();
        Some((self.env_for(&env)?, root))
    }

    pub(super) fn open_diff(&mut self, path: String, side: Side, cx: &mut Context<Self>) {
        self.preview = None;
        self.mission = None;
        self.diff_view = Some(DiffView { path, side, diff: None, confirm: None, busy: false });
        self.turn_view = None;
        self.spend_view = None;
        self.load_diff(cx);
    }

    fn load_diff(&mut self, cx: &mut Context<Self>) {
        let Some((env, root)) = self.repo() else { return };
        let Some(view) = &self.diff_view else { return };
        let (path, side) = (view.path.clone(), view.side);
        let task = cx.background_spawn(async move {
            vcs::diff(&*env, &root, &path, side).map(Arc::new).map_err(|e| e.to_string())
        });
        let (path, side) = (self.diff_view.as_ref().map(|v| v.path.clone()), side);
        cx.spawn(async move |this, cx| {
            let diff = task.await;
            this.update(cx, |this, cx| {
                if let Some(view) = &mut this.diff_view
                    && Some(&view.path) == path.as_ref()
                    && view.side == side
                {
                    view.diff = Some(diff);
                    view.busy = false;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Runs a git change off the window thread, then reads the status and the diff again.
    fn vcs_job(
        &mut self,
        job: impl FnOnce(&dyn envs::Environment, &str) -> std::io::Result<()> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some((env, root)) = self.repo() else { return };
        if let Some(view) = &mut self.diff_view {
            view.busy = true;
            view.confirm = None;
        }
        let task = cx.background_spawn(async move { job(&*env, &root) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.status = Some(format!("git: {}", model::shorten(error.to_string().trim(), 300)));
                }
                this.refresh_changes(cx);
                this.load_diff(cx);
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn hunk_action(&mut self, index: usize, action: HunkAction, cx: &mut Context<Self>) {
        let Some(view) = &mut self.diff_view else { return };
        let Some(Ok(diff)) = view.diff.clone() else { return };
        if view.busy {
            return;
        }
        if matches!(action, HunkAction::Discard) && view.confirm != Some(index) {
            view.confirm = Some(index);
            cx.notify();
            return;
        }
        self.vcs_job(
            move |env, root| match action {
                HunkAction::Stage => vcs::stage_hunk(env, root, &diff, index),
                HunkAction::Unstage => vcs::unstage_hunk(env, root, &diff, index),
                HunkAction::Discard => vcs::discard_hunk(env, root, &diff, index),
            },
            cx,
        );
    }

    /// Runs a fetch, pull, push or branch switch in the background, then reads the folder
    /// again. One at a time.
    fn sync(&mut self, op: SyncOp, cx: &mut Context<Self>) {
        let Some((env, root)) = self.repo() else { return };
        if self.changes.syncing.is_some() {
            return;
        }
        self.changes.syncing = Some(op.clone());
        self.changes.branches_open = false;
        let task = cx.background_spawn({
            let op = op.clone();
            async move {
                match &op {
                    SyncOp::Fetch => vcs::fetch(&*env, &root),
                    SyncOp::Pull => vcs::pull(&*env, &root),
                    SyncOp::Push => vcs::push(&*env, &root),
                    SyncOp::Switch(name) => vcs::switch(&*env, &root, name),
                }
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.changes.syncing = None;
                this.changes.branches = None;
                let (command, done) = match &op {
                    SyncOp::Fetch => ("git fetch", t("changes.fetched").to_owned()),
                    SyncOp::Pull => ("git pull", t("changes.pulled").to_owned()),
                    SyncOp::Push => ("git push", t("changes.pushed").to_owned()),
                    SyncOp::Switch(name) => ("git switch", format!("{} {name}", t("changes.switched"))),
                };
                this.status = Some(match result {
                    Ok(()) => done,
                    Err(error) => format!("{command}: {}", model::shorten(error.to_string().trim(), 300)),
                });
                if matches!(op, SyncOp::Pull | SyncOp::Switch(_)) {
                    // The files changed under the open diff and file tree.
                    this.diff_view = None;
                    this.commit_view = None;
                }
                this.refresh_changes(cx);
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Opens or closes the list of local branches, reading it when it opens.
    fn toggle_branches(&mut self, cx: &mut Context<Self>) {
        self.changes.branches_open = !self.changes.branches_open;
        cx.notify();
        if !self.changes.branches_open {
            return;
        }
        let Some((env, root)) = self.repo() else { return };
        let key = self.changes.key.clone();
        let task = cx.background_spawn(async move { vcs::branches(&*env, &root).unwrap_or_default() });
        cx.spawn(async move |this, cx| {
            let names = task.await;
            this.update(cx, |this, cx| {
                if this.changes.key == key {
                    this.changes.branches = Some(names);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn open_commit(&mut self, commit: &Commit, cx: &mut Context<Self>) {
        let Some((env, root)) = self.repo() else { return };
        let hash = commit.hash.clone();
        self.diff_view = None;
        self.preview = None;
        self.mission = None;
        self.commit_view = Some(CommitView { hash: hash.clone(), title: format!("{} {}", commit.short, commit.subject), files: None });
        self.turn_view = None;
        self.spend_view = None;
        let task = cx.background_spawn({
            let hash = hash.clone();
            async move { vcs::show(&*env, &root, &hash).map(Arc::new).map_err(|e| e.to_string()) }
        });
        cx.spawn(async move |this, cx| {
            let files = task.await;
            this.update(cx, |this, cx| {
                if let Some(view) = &mut this.commit_view
                    && view.hash == hash
                {
                    view.files = Some(files);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// The branch against its upstream: Fetch always, Pull when the upstream is ahead, Push
    /// when this branch is. A click on the name lists the local branches to switch to.
    fn render_branch(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = *cx.global::<Theme>();
        let branch = self.changes.branch.clone()?;
        let name = branch.name.clone().unwrap_or_else(|| t("changes.detached").to_owned());
        let syncing = self.changes.syncing.clone();
        let busy = syncing.is_some();
        let has_upstream = branch.upstream.is_some();
        let action = |id: &'static str, label: &'static str, running: &'static str, op: SyncOp, primary: bool, cx: &mut Context<Self>| {
            let active = syncing.as_ref() == Some(&op);
            div()
                .id(id)
                .flex()
                .items_center()
                .h(px(22.))
                .px(px(9.))
                .rounded(px(5.))
                .text_size(px(11.5))
                .when(primary, |this| this.bg(theme.accent).text_color(theme.over_accent))
                .when(!primary, |this| this.border_1().border_color(theme.border).text_color(theme.text_secondary).hover(|s| s.bg(theme.hover)))
                .when(busy, |this| this.opacity(0.6))
                .child(if active { running } else { label })
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.sync(op.clone(), cx)))
        };
        let open = self.changes.branches_open;
        let row = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(14.))
            .pb(px(6.))
            .text_size(px(12.))
            .child(
                div()
                    .id("branch-name")
                    .flex()
                    .items_center()
                    .gap(px(3.))
                    .px(px(4.))
                    .ml(px(-4.))
                    .rounded(px(4.))
                    .font_family(MONO)
                    .hover(|s| s.bg(theme.hover))
                    .child(name)
                    .child(icon_sized(if open { "general/chevronDown" } else { "general/chevronRight" }, 12., &theme))
                    .tooltip(|_, cx| tooltip(t("changes.branches"), None, cx))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_branches(cx))),
            )
            .children(branch.upstream.clone().map(|upstream| div().text_color(theme.text_muted).text_size(px(11.)).child(format!("→ {upstream}"))))
            .child(div().flex_1())
            .when(has_upstream, |this| {
                this.child(div().font_family(MONO).text_size(px(11.)).text_color(theme.text_secondary).child(format!("↑{} ↓{}", branch.ahead, branch.behind)))
            })
            .child(action("fetch", t("changes.fetch"), t("changes.fetching"), SyncOp::Fetch, false, cx))
            .when(has_upstream && branch.behind > 0, |this| {
                this.child(action("pull", t("changes.pull"), t("changes.pulling"), SyncOp::Pull, branch.ahead == 0, cx))
            })
            .when(has_upstream && branch.ahead > 0, |this| this.child(action("push", t("changes.push"), t("changes.pushing"), SyncOp::Push, true, cx)));
        let list = open.then(|| {
            let current = branch.name.clone();
            let names = self.changes.branches.clone();
            div()
                .flex()
                .flex_col()
                .mx(px(10.))
                .mb(px(6.))
                .py(px(3.))
                .rounded(px(6.))
                .border_1()
                .border_color(theme.border)
                .text_size(px(12.))
                .font_family(MONO)
                .map(|this| match names {
                    None => this.child(div().px(px(10.)).py(px(3.)).text_color(theme.text_muted).child(t("loading"))),
                    Some(names) => this.children(names.into_iter().enumerate().map(|(ix, name)| {
                        let here = current.as_deref() == Some(name.as_str());
                        div()
                            .id(("branch", ix))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .px(px(10.))
                            .py(px(3.))
                            .when(here, |this| this.text_color(theme.accent))
                            .when(!here, |this| this.hover(|s| s.bg(theme.hover)))
                            .child(div().w(px(10.)).child(if here { "●" } else { "" }))
                            .child(name.clone())
                            .when(!here, |this| {
                                this.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.sync(SyncOp::Switch(name.clone()), cx)))
                            })
                    })),
                })
        });
        Some(div().flex().flex_col().child(row).children(list).into_any_element())
    }

    /// The last commits, folded under their title.
    fn render_history(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = *cx.global::<Theme>();
        if self.changes.history.is_empty() {
            return None;
        }
        let open = self.changes.history_open;
        let active = self.commit_view.as_ref().map(|v| v.hash.clone());
        let title = div()
            .id("history-title")
            .flex()
            .items_center()
            .gap(px(4.))
            .px(px(8.))
            .pt(px(8.))
            .pb(px(4.))
            .text_size(px(11.5))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.text_secondary)
            .child(icon_sized(if open { "general/chevronDown" } else { "general/chevronRight" }, 14., &theme))
            .child(format!("{}  {}", t("changes.history"), self.changes.history.len()))
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.changes.history_open = !this.changes.history_open;
                cx.notify();
            }));
        let rows = open.then(|| {
            self.changes.history.clone().into_iter().enumerate().map(|(ix, commit)| {
                let selected = active.as_deref() == Some(commit.hash.as_str());
                let tip = format!("{} · {}", commit.author, model::format_ms_utc(commit.time_ms));
                div()
                    .id(("commit", ix))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(24.))
                    .px(px(8.))
                    .mx(px(4.))
                    .rounded(px(5.))
                    .text_size(px(12.))
                    .when(selected, |this| this.bg(theme.selection_inactive))
                    .when(!selected, |this| this.hover(|style| style.bg(theme.hover)))
                    .child(div().flex_none().font_family(MONO).text_size(px(11.)).text_color(theme.text_muted).child(commit.short.clone()))
                    .child(div().flex_1().min_w_0().truncate().child(commit.subject.clone()))
                    .child(div().flex_none().text_size(px(11.)).text_color(theme.text_muted).child(super::ago(commit.time_ms)))
                    .tooltip(move |_, cx| tooltip(tip.clone(), None, cx))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.open_commit(&commit, cx)))
            })
        });
        Some(div().flex_none().flex().flex_col().child(title).children(rows.into_iter().flatten()).into_any_element())
    }

    /// The changes of a commit in the centre, file by file, read only.
    pub(super) fn render_commit_view(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = *cx.global::<Theme>();
        let view = self.commit_view.as_ref()?;
        let header = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(48.))
            .px(px(16.))
            .border_b_1()
            .border_color(theme.border)
            .child(div().flex_1().min_w_0().truncate().font_weight(FontWeight::SEMIBOLD).child(view.title.clone()))
            .child(
                div()
                    .id("close-commit")
                    .flex()
                    .items_center()
                    .h(px(26.))
                    .px(px(10.))
                    .rounded(px(6.))
                    .text_size(px(12.))
                    .text_color(theme.text_secondary)
                    .hover(|style| style.bg(theme.hover))
                    .child(t("files.back"))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.commit_view = None;
                        cx.notify();
                    })),
            );
        let note = |text: String| div().p(px(20.)).text_color(theme.text_muted).child(text).into_any_element();
        let body = match &view.files {
            None => note(t("loading").to_owned()),
            Some(Err(error)) => note(format!("git: {error}")),
            Some(Ok(files)) => {
                let mut shown = 0usize;
                let mut cards: Vec<AnyElement> = Vec::new();
                for (fi, (path, diff)) in files.iter().enumerate() {
                    cards.push(
                        div()
                            .pt(px(4.))
                            .font_family("Inter")
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(px(12.5))
                            .child(path.clone())
                            .into_any_element(),
                    );
                    if diff.binary {
                        cards.push(note(t("files.binary").to_owned()));
                        continue;
                    }
                    let lang = highlight::lang_for(path);
                    for (hi, hunk) in diff.hunks.iter().enumerate() {
                        if shown > MAX_LINES {
                            break;
                        }
                        let lines = hunk_lines(hunk, lang, &theme, &mut shown);
                        cards.push(hunk_card(SharedString::from(format!("commit-hunk-{fi}-{hi}")).into(), &hunk.header, None, lines, &theme));
                    }
                }
                div()
                    .id("commit-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p(px(14.))
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .font_family(MONO)
                    .text_size(px(12.5))
                    .line_height(px(19.))
                    .children(cards)
                    .when(shown > MAX_LINES, |this| {
                        this.child(div().font_family("Inter").text_color(theme.text_muted).child(t("changes.more_lines")))
                    })
                    .into_any_element()
            }
        };
        Some(div().flex_1().min_h_0().flex().flex_col().child(header).child(body).into_any_element())
    }

    /// Commits the staged files with the typed message.
    fn commit(&mut self, cx: &mut Context<Self>) {
        let message = self.changes.message.text.trim().to_owned();
        if message.is_empty() || self.changes.committing {
            return;
        }
        let Some((env, root)) = self.repo() else { return };
        self.changes.committing = true;
        let task = cx.background_spawn(async move { vcs::commit(&*env, &root, &message) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.changes.committing = false;
                match result {
                    Ok(made) => {
                        this.changes.message = TextField::default();
                        this.status = Some(format!("{}: {made}", t("changes.committed")));
                    }
                    Err(error) => this.status = Some(format!("git: {}", model::shorten(error.to_string().trim(), 300))),
                }
                this.refresh_changes(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn commit_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        // Enter commits; Shift+Enter starts a new line of the body.
        if keystroke.key == "enter" && !keystroke.modifiers.shift {
            self.commit(cx);
        } else if !self.changes.message.key_with_clipboard(event, true, cx) {
            return;
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// The message field and the button under the staged files.
    fn render_commit_box(&mut self, staged: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.global::<Theme>();
        let focused = self.commit_focus.is_focused(window);
        let field = &self.changes.message;
        let empty = field.text.trim().is_empty();
        let busy = self.changes.committing;
        let input = div()
            .id("commit-message")
            .track_focus(&self.commit_focus)
            .on_key_down(cx.listener(Self::commit_key))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.commit_focus.focus(window, cx);
                cx.notify();
            }))
            .min_h(px(30.))
            .px(px(8.))
            .py(px(6.))
            .rounded(px(7.))
            .bg(theme.editor_bg)
            .text_size(px(12.5))
            .border_1()
            .border_color(if focused { theme.accent } else { theme.border })
            .when(field.text.is_empty() && !focused, |this| this.child(div().text_color(theme.text_muted).child(t("changes.message"))))
            .when(!field.text.is_empty() || focused, |this| {
                this.children(text_field_lines(field, theme.text, theme.over_accent, theme.selection, theme.caret, 16., focused))
            });
        let button = div()
            .id("commit-button")
            .flex()
            .items_center()
            .justify_center()
            .h(px(28.))
            .rounded(px(7.))
            .text_size(px(12.5))
            .font_weight(FontWeight::SEMIBOLD)
            .when(empty || busy, |this| this.bg(theme.hover).text_color(theme.text_muted))
            .when(!empty && !busy, |this| this.bg(theme.accent).text_color(theme.over_accent).hover(|style| style.opacity(0.9)))
            .child(if busy { t("changes.committing").to_owned() } else { format!("{} ({staged})", t("changes.commit")) })
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.commit(cx)));
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(6.))
            .p(px(8.))
            .border_t_1()
            .border_color(theme.border)
            .child(input)
            .child(button)
            .into_any_element()
    }

    fn file_action(&mut self, path: String, stage: bool, cx: &mut Context<Self>) {
        self.vcs_job(
            move |env, root| if stage { vcs::stage_file(env, root, &path) } else { vcs::unstage_file(env, root, &path) },
            cx,
        );
    }

    pub(super) fn render_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.global::<Theme>();
        if self.changes.key != self.changes_key() && self.changes_key().is_some() {
            self.refresh_changes(cx);
        }
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .px(px(14.))
            .pt(px(10.))
            .pb(px(6.))
            .text_size(px(11.))
            .text_color(theme.text_muted)
            .child(if self.changes.loading { t("loading") } else { "git status" })
            .child(icon_button("refresh-changes", "general/refresh", t("refresh"), None, &theme).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| this.refresh_changes(cx),
            )));
        let body: AnyElement = match &self.changes.status {
            None => div().into_any_element(),
            Some(Err(error)) => {
                let text = if error.contains("not a git repository") { t("changes.no_repo").to_owned() } else { error.clone() };
                div().px(px(14.)).text_size(px(12.)).text_color(theme.text_muted).child(text).into_any_element()
            }
            Some(Ok(status)) if status.files.is_empty() => {
                div().px(px(14.)).text_size(px(12.)).text_color(theme.text_muted).child(t("changes.clean")).into_any_element()
            }
            Some(Ok(status)) => {
                let status = status.clone();
                let staged: Vec<_> = status.files.iter().filter(|f| f.staged.is_some()).cloned().collect();
                let unstaged: Vec<_> = status.files.iter().filter(|f| f.unstaged.is_some()).cloned().collect();
                let open = self.diff_view.as_ref().map(|v| (v.path.clone(), v.side));
                let mut rows: Vec<AnyElement> = Vec::new();
                for (side, files) in [(Side::Staged, staged), (Side::Unstaged, unstaged)] {
                    if files.is_empty() {
                        continue;
                    }
                    let title = if side == Side::Staged { t("changes.staged") } else { t("changes.unstaged") };
                    rows.push(
                        div()
                            .px(px(8.))
                            .pt(px(8.))
                            .pb(px(4.))
                            .text_size(px(11.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.text_secondary)
                            .child(format!("{title}  {}", files.len()))
                            .into_any_element(),
                    );
                    for file in files {
                        let letter = if side == Side::Staged { file.staged } else { file.unstaged }.unwrap_or(' ');
                        let color = match letter {
                            'A' | '?' => theme.success,
                            'D' => theme.error,
                            _ => theme.info,
                        };
                        let (dir, name) = match file.path.rsplit_once('/') {
                            Some((dir, name)) => (dir.to_owned(), name.to_owned()),
                            None => (String::new(), file.path.clone()),
                        };
                        let active = open.as_ref() == Some(&(file.path.clone(), side));
                        let path = file.path.clone();
                        let action_path = file.path.clone();
                        let untracked = file.untracked;
                        let full = format!("{}/{}", status.root, file.path);
                        let row_id = SharedString::from(format!("chg-{}-{}", side == Side::Staged, file.path));
                        let action_id = SharedString::from(format!("chg-act-{}-{}", side == Side::Staged, file.path));
                        rows.push(
                            div()
                                .id(row_id)
                                .group("change-row")
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .h(px(24.))
                                .px(px(8.))
                                .mx(px(4.))
                                .rounded(px(5.))
                                .text_size(px(12.5))
                                .when(active, |this| this.bg(theme.selection_inactive))
                                .when(!active, |this| this.hover(|style| style.bg(theme.hover)))
                                .child(div().w(px(12.)).flex_none().font_family(MONO).text_size(px(11.5)).text_color(color).child(letter.to_string()))
                                .child(div().flex_none().max_w(px(160.)).truncate().child(name))
                                .child(div().flex_1().min_w_0().truncate().text_size(px(11.)).text_color(theme.text_muted).child(dir))
                                .child(
                                    div()
                                        .id(action_id)
                                        .flex_none()
                                        .px(px(6.))
                                        .rounded(px(4.))
                                        .text_size(px(11.))
                                        .opacity(0.)
                                        .group_hover("change-row", |style| style.opacity(1.))
                                        .hover(|style| style.bg(theme.selection_hover))
                                        .child(if side == Side::Staged { t("changes.unstage") } else { t("changes.stage") })
                                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                            cx.stop_propagation();
                                            this.file_action(action_path.clone(), side == Side::Unstaged, cx);
                                        })),
                                )
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    if untracked {
                                        this.diff_view = None;
                                        this.open_preview(full.clone(), cx);
                                    } else {
                                        this.open_diff(path.clone(), side, cx);
                                    }
                                }))
                                .into_any_element(),
                        );
                    }
                }
                div().id("changes-list").flex_1().min_h_0().overflow_y_scroll().pb(px(8.)).children(rows).into_any_element()
            }
        };
        let staged = match &self.changes.status {
            Some(Ok(status)) => status.files.iter().filter(|f| f.staged.is_some()).count(),
            _ => 0,
        };
        let commit_box = (staged > 0).then(|| self.render_commit_box(staged, window, cx));
        let branch = self.render_branch(cx);
        let history = self.render_history(cx);
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(header)
            .children(branch)
            .child(body)
            .children(history)
            .children(commit_box)
            .into_any_element()
    }

    /// The diff of one file in the centre, hunk by hunk with its buttons.
    pub(super) fn render_diff(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = *cx.global::<Theme>();
        let view = self.diff_view.as_ref()?;
        let side = view.side;
        let path = view.path.clone();
        let name = path.rsplit('/').next().unwrap_or(&path).to_owned();
        let side_label = if side == Side::Staged { t("changes.staged") } else { t("changes.unstaged") };
        let file_button = div()
            .id("diff-file-action")
            .flex()
            .items_center()
            .h(px(26.))
            .px(px(10.))
            .rounded(px(6.))
            .text_size(px(12.))
            .bg(theme.hover)
            .hover(|style| style.bg(theme.selection_hover))
            .child(if side == Side::Staged { t("changes.unstage_file") } else { t("changes.stage_file") })
            .on_click(cx.listener({
                let path = path.clone();
                move |this, _: &ClickEvent, _, cx| this.file_action(path.clone(), side == Side::Unstaged, cx)
            }));
        let header = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(48.))
            .px(px(16.))
            .border_b_1()
            .border_color(theme.border)
            .child(div().font_weight(FontWeight::SEMIBOLD).child(name))
            .child(badge_text(side_label, theme.text_secondary))
            .child(div().flex_1().min_w_0().truncate().text_size(px(11.5)).text_color(theme.text_muted).child(path.clone()))
            .when(view.busy, |this| this.child(div().text_size(px(11.5)).text_color(theme.text_muted).child(t("loading"))))
            .child(file_button)
            .child(
                div()
                    .id("close-diff")
                    .flex()
                    .items_center()
                    .h(px(26.))
                    .px(px(10.))
                    .rounded(px(6.))
                    .text_size(px(12.))
                    .text_color(theme.text_secondary)
                    .hover(|style| style.bg(theme.hover))
                    .child(t("files.back"))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.diff_view = None;
                        cx.notify();
                    })),
            );
        let note = |text: String| div().p(px(20.)).text_color(theme.text_muted).child(text).into_any_element();
        let body = match &view.diff {
            None => note(t("loading").to_owned()),
            Some(Err(error)) => note(format!("git: {error}")),
            Some(Ok(diff)) if diff.binary => note(t("files.binary").to_owned()),
            Some(Ok(diff)) if diff.hunks.is_empty() => note(t("changes.no_diff").to_owned()),
            Some(Ok(diff)) => {
                let mut shown = 0usize;
                let confirm = view.confirm;
                let busy = view.busy;
                let lang = highlight::lang_for(&path);
                let cards = diff.hunks.iter().enumerate().map(|(index, hunk)| {
                    let button = |id: &str, label: &'static str, danger: bool| {
                        div()
                            .id(SharedString::from(format!("{id}-{index}")))
                            .flex()
                            .items_center()
                            .h(px(22.))
                            .px(px(8.))
                            .rounded(px(5.))
                            .text_size(px(11.5))
                            .font_family("Inter")
                            .when(danger, |this| this.bg(theme.error).text_color(theme.over_accent))
                            .when(!danger, |this| this.bg(theme.hover).hover(|style| style.bg(theme.selection_hover)))
                            .when(busy, |this| this.opacity(0.5))
                            .child(label)
                    };
                    let actions = div().flex().gap(px(6.)).map(|this| match side {
                        Side::Unstaged => this
                            .child(button("stage", t("changes.stage"), false).on_click(cx.listener(
                                move |this, _: &ClickEvent, _, cx| this.hunk_action(index, HunkAction::Stage, cx),
                            )))
                            .child(
                                button(
                                    "discard",
                                    if confirm == Some(index) { t("changes.confirm_discard") } else { t("changes.discard") },
                                    confirm == Some(index),
                                )
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.hunk_action(index, HunkAction::Discard, cx)
                                })),
                            ),
                        Side::Staged => this.child(button("unstage", t("changes.unstage"), false).on_click(cx.listener(
                            move |this, _: &ClickEvent, _, cx| this.hunk_action(index, HunkAction::Unstage, cx),
                        ))),
                    });
                    let lines = hunk_lines(hunk, lang, &theme, &mut shown);
                    hunk_card(("hunk-lines", index).into(), &hunk.header, Some(actions.into_any_element()), lines, &theme)
                });
                let cards: Vec<_> = cards.collect();
                div()
                    .id("diff-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p(px(14.))
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .font_family(MONO)
                    .text_size(px(12.5))
                    .line_height(px(19.))
                    .children(cards)
                    .when(shown > MAX_LINES, |this| {
                        this.child(
                            div()
                                .font_family("Inter")
                                .text_color(theme.text_muted)
                                .child(format!("{} {}", shown - MAX_LINES, t("changes.more_lines"))),
                        )
                    })
                    .into_any_element()
            }
        };
        Some(div().flex_1().min_h_0().flex().flex_col().child(header).child(body).into_any_element())
    }
}

/// The lines of a hunk with their old and new numbers and the colours of the code, as many
/// as are left of `MAX_LINES` (`shown` counts them over the whole view).
pub(super) fn hunk_lines(hunk: &vcs::Hunk, lang: Option<&'static highlight::Lang>, theme: &Theme, shown: &mut usize) -> Vec<AnyElement> {
    let (mut old, mut new) = hunk_start(&hunk.header);
    let mut highlighter = lang.map(highlight::Highlighter::new);
    hunk
        .lines
        .iter()
        .filter(|_| {
            *shown += 1;
            *shown <= MAX_LINES
        })
        .map(|line| {
            let line = line.trim_end_matches('\r');
            let mark = line.chars().next().unwrap_or(' ');
            let code = super::files_view::shown_line(line.get(mark.len_utf8()..).unwrap_or(""));
            let spans = match (&mut highlighter, mark) {
                (Some(h), '+' | '-' | ' ') => h.line(&code),
                _ => Vec::new(),
            };
            let (bg, numbers) = match mark {
                '+' => {
                    new += 1;
                    (Some(theme.success.opacity(0.14)), (String::new(), (new - 1).to_string()))
                }
                '-' => {
                    old += 1;
                    (Some(theme.error.opacity(0.14)), ((old - 1).to_string(), String::new()))
                }
                '\\' => (None, (String::new(), String::new())),
                _ => {
                    old += 1;
                    new += 1;
                    (None, ((old - 1).to_string(), (new - 1).to_string()))
                }
            };
            div()
                .flex()
                .min_h(px(19.))
                .when_some(bg, |this, bg| this.bg(bg))
                .child(div().flex_none().w(px(44.)).pr(px(6.)).flex().justify_end().text_color(theme.text_disabled).child(numbers.0))
                .child(div().flex_none().w(px(44.)).pr(px(10.)).flex().justify_end().text_color(theme.text_disabled).child(numbers.1))
                .child(div().flex_none().w(px(14.)).text_color(theme.text_muted).child(mark.to_string()))
                .child(
                    div()
                        .flex_none()
                        .pr(px(16.))
                        .whitespace_nowrap()
                        .text_color(theme.code)
                        .child(super::code_text(SharedString::from(code), &spans, theme)),
                )
                .into_any_element()
        })
        .collect()
}

/// A hunk as a card: its header with the buttons, if any, and its lines, which scroll
/// sideways on their own (Shift+wheel).
pub(super) fn hunk_card(id: ElementId, header: &str, actions: Option<AnyElement>, lines: Vec<AnyElement>, theme: &Theme) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .rounded(px(8.))
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(10.))
                .px(px(10.))
                .py(px(5.))
                .bg(theme.hover)
                .child(div().flex_1().min_w_0().truncate().text_color(theme.text_muted).child(header.to_owned()))
                .children(actions.map(|actions| div().flex_none().child(actions))),
        )
        .child(
            div()
                .id(id)
                .flex()
                .overflow_x_scroll()
                .py(px(4.))
                .child(div().flex().flex_col().flex_none().min_w_full().children(lines)),
        )
        .into_any_element()
}

fn badge_text(label: &str, color: Hsla) -> impl IntoElement {
    div()
        .flex_none()
        .px(px(7.))
        .py(px(1.))
        .rounded(px(5.))
        .border_1()
        .border_color(color.opacity(0.4))
        .text_color(color)
        .text_size(px(11.))
        .child(label.to_owned())
}

#[cfg(test)]
mod tests {
    use super::hunk_start;

    #[test]
    fn hunk_headers_give_line_numbers() {
        assert_eq!(hunk_start("@@ -12,7 +14,8 @@ fn main()"), (12, 14));
        assert_eq!(hunk_start("@@ -1 +1 @@"), (1, 1));
        assert_eq!(hunk_start("@@ -0,0 +1,3 @@"), (0, 1));
    }
}
