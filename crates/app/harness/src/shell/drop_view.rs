//! Dropping a prompt that only got API errors (a wrong model, a prompt sent to the wrong
//! session): Bowyer never edits the history. It hides the failed turn and resumes the next
//! turn before it with Claude Code's `--resume-session-at`, `--resume-drops-turn` naming the
//! prompt, so Claude Code refuses if anything else would go. The failed turn stays in the file
//! on a branch Claude Code no longer follows; the reader leaves it out from then on.

use model::{HarnessKind, SessionKey};
use ui_kit::prelude::*;
use ui_kit::*;

use super::Shell;
use crate::i18n::t;

fn drop_setting(key: &SessionKey) -> String {
    format!("drop.{}", key.encode())
}

impl Shell {
    /// The failed turn the user dropped in `key`: the line the session goes on from and the
    /// prompt left out.
    pub(super) fn dropped_turn(&self, key: &SessionKey) -> Option<(String, String)> {
        let value = self.lib().setting(&drop_setting(key))?;
        let (kept, prompt) = value.split_once(' ')?;
        Some((kept.to_owned(), prompt.to_owned()))
    }

    /// The prompt the user dropped in `key`, to leave out of the conversation.
    pub(super) fn dropped_prompt(&self, key: &SessionKey) -> Option<String> {
        self.dropped_turn(key).map(|(_, prompt)| prompt)
    }

    /// What the next turn of the open session leaves out: the dropped turn, while it is still
    /// the last one of the session.
    pub(super) fn turn_drop(&self, key: &SessionKey) -> Option<(String, String)> {
        let open = self.open.as_ref().filter(|open| open.summary.key == *key)?;
        let failed = open.failed.as_ref()?;
        self.dropped_turn(key).filter(|(kept, prompt)| *kept == failed.kept && *prompt == failed.prompt)
    }

    /// The turn left out ran: the history no longer has it on its way.
    pub(super) fn forget_drop(&self, key: &SessionKey) {
        let _ = self.lib().set_setting(&drop_setting(key), "");
    }

    fn set_drop(&mut self, drop: bool, cx: &mut Context<Self>) {
        let Some(open) = &self.open else { return };
        let key = open.summary.key.clone();
        match (&open.failed, drop) {
            (Some(failed), true) => {
                let _ = self.lib().set_setting(&drop_setting(&key), &format!("{} {}", failed.kept, failed.prompt));
            }
            _ => self.forget_drop(&key),
        }
        // Read the session again, with or without the turn.
        self.selected = None;
        self.open_session(key, cx);
    }

    /// The bar over the composer of a Claude Code session whose last prompt never reached
    /// the model.
    pub(super) fn render_failed_turn(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let open = self.open.as_ref()?;
        let key = &open.summary.key;
        if key.harness != HarnessKind::Claude || key.id.is_empty() || self.live().is_some_and(|live| !live.finished) {
            return None;
        }
        let failed = open.failed.as_ref()?;
        let dropped = self.dropped_prompt(key).is_some_and(|prompt| prompt == failed.prompt);
        let (text, action, tip) = if dropped {
            (t("drop.done"), t("drop.undo"), t("drop.undo_tip"))
        } else {
            (t("drop.failed"), t("drop.action"), t("drop.tip"))
        };
        Some(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .mb(px(8.))
                .px(px(12.))
                .py(px(7.))
                .rounded(px(8.))
                .bg(theme.island_bg)
                .border_1()
                .border_color(if dropped { theme.border } else { theme.warning_mark })
                .text_size(px(12.5))
                .child(div().flex_1().min_w_0().text_color(theme.text_secondary).child(text))
                .child(
                    div()
                        .id("drop-turn")
                        .flex_none()
                        .px(px(10.))
                        .py(px(3.))
                        .rounded(px(6.))
                        .border_1()
                        .border_color(theme.border)
                        .hover(|style| style.bg(theme.hover))
                        .child(action)
                        .tooltip(move |_, cx| tooltip(tip, None, cx))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.set_drop(!dropped, cx))),
                )
                .into_any_element(),
        )
    }
}
