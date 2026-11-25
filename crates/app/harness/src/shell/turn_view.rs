//! What one turn of a Claude Code session did to the files, opened from its prompt: each
//! file's diff from Claude Code's own file history, and undoing the turn. Undoing asks for a
//! second click, is refused once a file changed after the turn, and keeps what the files
//! were in the trash.

use std::sync::Arc;

use model::SessionKey;
use ui_kit::prelude::*;
use ui_kit::*;
use vcs::FileDiff;

use super::changes_view::{MAX_LINES, hunk_card, hunk_lines};
use super::{MONO, RowStatus, Shell, now_ms, row_status};
use crate::highlight;
use crate::i18n::t;

pub(super) struct TurnView {
    key: SessionKey,
    prompt: String,
    title: String,
    files: Option<Arc<Vec<TurnDiff>>>,
    /// Undo was clicked once and waits for the second click.
    confirm: bool,
    busy: bool,
}

struct TurnDiff {
    /// The path under the session's folder, or the whole path outside it.
    shown: String,
    diff: Result<FileDiff, String>,
    /// The file is as the turn left it.
    untouched: bool,
    /// The file is as it was before the turn.
    as_before: bool,
}

/// `path` under `cwd`, or `path` itself when it is elsewhere.
fn under(path: &str, cwd: Option<&str>) -> String {
    let path_slashes = path.replace('\\', "/");
    if let Some(cwd) = cwd {
        let cwd = cwd.replace('\\', "/");
        let cwd = cwd.trim_end_matches('/');
        let windows = path_slashes.as_bytes().get(1) == Some(&b':');
        if let Some(head) = path_slashes.get(..cwd.len())
            && (if windows { head.eq_ignore_ascii_case(cwd) } else { head == cwd })
            && let Some(rest) = path_slashes[cwd.len()..].strip_prefix('/')
        {
            return rest.to_owned();
        }
    }
    path.to_owned()
}

impl TurnView {
    pub(super) fn key(&self) -> &SessionKey {
        &self.key
    }
}

impl Shell {
    /// The prompt of item `ix` when its turn changed files Claude Code keeps backups of,
    /// with how many.
    pub(super) fn turn_files_of(&self, ix: usize) -> Option<usize> {
        let open = self.open.as_ref()?;
        let (prompt, _) = open.items.get(ix)?.prompt.as_ref()?;
        open.turns.get(prompt).map(Vec::len)
    }

    pub(super) fn open_turn(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(open) = &self.open else { return };
        let Some((prompt, text)) = open.items.get(ix).and_then(|item| item.prompt.clone()) else { return };
        if !open.turns.contains_key(&prompt) {
            return;
        }
        self.commit_view = None;
        self.diff_view = None;
        self.preview = None;
        self.spend_view = None;
        let title = format!("{}: {}", t("turn.title"), model::shorten(text.lines().next().unwrap_or(""), 90));
        self.turn_view = Some(TurnView { key: open.summary.key.clone(), prompt, title, files: None, confirm: false, busy: false });
        self.load_turn(cx);
        cx.notify();
    }

    /// The machine, the session's file history and folder, and the turn's files.
    fn turn_place(&self) -> Option<(envs::SharedEnv, String, Option<String>, Vec<model::TurnFile>)> {
        let view = self.turn_view.as_ref()?;
        let open = self.open.as_ref().filter(|open| open.summary.key == view.key)?;
        let env = self.env_for(&open.summary.key.env)?;
        let dir = history::file_history_dir(&*env, &open.summary.source.path, &open.summary.key.id)?;
        Some((env, dir, open.summary.cwd.clone(), open.turns.get(&view.prompt)?.clone()))
    }

    fn load_turn(&mut self, cx: &mut Context<Self>) {
        let Some(view) = &self.turn_view else { return };
        let (key, prompt) = (view.key.clone(), view.prompt.clone());
        let Some((env, dir, cwd, files)) = self.turn_place() else { return };
        let task = cx.background_spawn(async move {
            history::read_turn(&*env, &dir, &files)
                .into_iter()
                .map(|(path, state)| {
                    let shown = under(&path, cwd.as_deref());
                    match state {
                        Ok(state) => TurnDiff {
                            diff: Ok(vcs::text_diff(&shown, state.before.as_deref(), state.after.as_deref())),
                            untouched: state.untouched(),
                            as_before: state.as_before(),
                            shown,
                        },
                        Err(error) => TurnDiff { shown, diff: Err(error.to_string()), untouched: false, as_before: false },
                    }
                })
                .collect::<Vec<_>>()
        });
        cx.spawn(async move |this, cx| {
            let files = task.await;
            this.update(cx, |this, cx| {
                if let Some(view) = &mut this.turn_view
                    && view.key == key
                    && view.prompt == prompt
                {
                    view.files = Some(Arc::new(files));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Why the turn cannot be undone now, or `None` when it can.
    fn turn_blocked(&self) -> Option<&'static str> {
        let view = self.turn_view.as_ref()?;
        let files = view.files.as_ref()?;
        // At work in a terminal, or waiting there on a tool: its files may change any moment.
        let busy = self.summary(&view.key).is_some_and(|summary| {
            matches!(row_status(summary.turn, summary.updated_ms, now_ms()), Some(RowStatus::Working | RowStatus::Waiting))
        });
        if self.live_running() || busy {
            Some(t("turn.running"))
        } else if files.iter().any(|file| file.diff.is_err()) {
            Some(t("turn.no_backup"))
        } else if files.iter().all(|file| file.as_before) {
            Some(t("turn.as_before"))
        } else if files.iter().any(|file| !file.untouched) {
            Some(t("turn.changed_since"))
        } else {
            None
        }
    }

    fn undo_turn(&mut self, cx: &mut Context<Self>) {
        let loaded = self.turn_view.as_ref().is_some_and(|view| view.files.is_some() && !view.busy);
        if !loaded || self.turn_blocked().is_some() {
            return;
        }
        let Some((env, dir, _, files)) = self.turn_place() else { return };
        let Some(view) = &mut self.turn_view else { return };
        if !view.confirm {
            view.confirm = true;
            cx.notify();
            return;
        }
        view.confirm = false;
        view.busy = true;
        let task = cx.background_spawn(async move { history::undo_turn(&*env, &dir, &files) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if let Some(view) = &mut this.turn_view {
                    view.busy = false;
                }
                this.status = Some(match result {
                    Ok(kept) => t("turn.undone").replace("{dir}", &kept),
                    Err(error) => format!("{}: {error}", t("turn.undo_failed")),
                });
                this.load_turn(cx);
                this.refresh_changes(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The turn view is open for the session shown.
    pub(super) fn turn_view_shown(&self) -> bool {
        self.turn_view.as_ref().is_some_and(|view| self.open.as_ref().is_some_and(|open| open.summary.key == view.key))
    }

    /// The files a turn changed in the centre, with Undo in the header.
    pub(super) fn render_turn_view(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.turn_view_shown() {
            return None;
        }
        let theme = *cx.global::<Theme>();
        let blocked = self.turn_blocked();
        let view = self.turn_view.as_ref()?;
        let can_undo = view.files.is_some() && blocked.is_none() && !view.busy;
        let undo = div()
            .id("undo-turn")
            .flex()
            .items_center()
            .h(px(26.))
            .px(px(10.))
            .rounded(px(6.))
            .text_size(px(12.))
            .when(view.confirm, |this| this.bg(theme.error).text_color(theme.over_accent))
            .when(!view.confirm, |this| this.border_1().border_color(theme.border).hover(|style| style.bg(theme.hover)))
            .when(!can_undo, |this| this.opacity(0.45))
            .child(if view.confirm { t("turn.confirm_undo") } else { t("turn.undo") })
            .tooltip(move |_, cx| tooltip(blocked.unwrap_or(t("turn.undo_tip")), None, cx))
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.undo_turn(cx)));
        let header = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(48.))
            .px(px(16.))
            .border_b_1()
            .border_color(theme.border)
            .child(div().flex_1().min_w_0().truncate().font_weight(FontWeight::SEMIBOLD).child(view.title.clone()))
            .child(undo)
            .child(
                div()
                    .id("close-turn")
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
                        this.turn_view = None;
                        cx.notify();
                    })),
            );
        let note = |text: String| div().p(px(20.)).font_family("Inter").text_color(theme.text_muted).child(text).into_any_element();
        let body = match &view.files {
            None => note(t("loading").to_owned()),
            Some(files) => {
                let mut shown = 0usize;
                let mut cards: Vec<AnyElement> = Vec::new();
                if let Some(reason) = blocked {
                    cards.push(div().font_family("Inter").text_color(theme.text_secondary).child(reason).into_any_element());
                }
                for (fi, file) in files.iter().enumerate() {
                    cards.push(
                        div()
                            .flex()
                            .gap(px(8.))
                            .pt(px(4.))
                            .font_family("Inter")
                            .text_size(px(12.5))
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(file.shown.clone()))
                            .when(!file.untouched && !file.as_before && file.diff.is_ok(), |this| {
                                this.child(div().text_color(theme.text_muted).child(t("turn.file_changed")))
                            })
                            .into_any_element(),
                    );
                    let diff = match &file.diff {
                        Ok(diff) => diff,
                        Err(error) => {
                            cards.push(note(format!("{}: {error}", t("turn.no_backup"))));
                            continue;
                        }
                    };
                    if diff.binary {
                        cards.push(note(t("files.binary").to_owned()));
                        continue;
                    }
                    if diff.hunks.is_empty() {
                        cards.push(note(t("turn.same").to_owned()));
                        continue;
                    }
                    let lang = highlight::lang_for(&file.shown);
                    for (hi, hunk) in diff.hunks.iter().enumerate() {
                        if shown > MAX_LINES {
                            break;
                        }
                        let lines = hunk_lines(hunk, lang, &theme, &mut shown);
                        cards.push(hunk_card(SharedString::from(format!("turn-hunk-{fi}-{hi}")).into(), &hunk.header, None, lines, &theme));
                    }
                }
                div()
                    .id("turn-body")
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
}

#[cfg(test)]
mod tests {
    use super::under;

    #[test]
    fn a_file_shows_under_the_session_folder() {
        assert_eq!(under("C:\\Work\\App\\src\\a.rs", Some("c:\\work\\app")), "src/a.rs");
        assert_eq!(under("/home/u/app/src/a.rs", Some("/home/u/app/")), "src/a.rs");
        assert_eq!(under("/home/u/apps/a.rs", Some("/home/u/app")), "/home/u/apps/a.rs");
        assert_eq!(under("/home/u/App/a.rs", Some("/home/u/app")), "/home/u/App/a.rs", "case counts off Windows");
    }
}
