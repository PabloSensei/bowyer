//! Quitting while sessions run: the window asks first, then stops every turn it runs and
//! every terminal it holds, waits for them to end, and only then quits.

use std::time::{Duration, Instant};

use ui_kit::prelude::*;
use ui_kit::*;

use super::Shell;
use crate::i18n::t;

/// How long the window waits for the stopped turns before it quits anyway.
const STOP_WAIT: Duration = Duration::from_secs(10);

/// The question shown before quitting; `stopping` once the user chose to quit.
#[derive(Default)]
pub(super) struct QuitAsk {
    stopping: bool,
}

impl Shell {
    /// The titles of the sessions whose turns the window runs.
    fn running_turns(&self) -> Vec<String> {
        self.runs
            .iter()
            .filter(|run| !run.finished)
            .map(|run| self.summary(&run.key).map_or_else(|| t("new_session").to_owned(), |summary| summary.title.clone()))
            .collect()
    }

    /// The titles of the terminals whose programs still run.
    fn open_terminals(&self, cx: &App) -> Vec<String> {
        self.terminals.iter().map(|term| term.read(cx)).filter(|term| !term.is_closed()).map(|term| term.title().to_owned()).collect()
    }

    /// Whether the app may quit now. With turns or terminals running it asks the user
    /// instead and says no; their answer quits later.
    pub(crate) fn may_quit(&mut self, cx: &mut Context<Self>) -> bool {
        if self.quit_ask.as_ref().is_some_and(|ask| ask.stopping) {
            return false;
        }
        if self.running_turns().is_empty() && self.open_terminals(cx).is_empty() {
            return true;
        }
        self.quit_ask = Some(QuitAsk::default());
        cx.notify();
        false
    }

    /// Stops every turn and terminal, waits until the turns have ended, then quits.
    fn stop_all_and_quit(&mut self, cx: &mut Context<Self>) {
        self.quit_ask = Some(QuitAsk { stopping: true });
        for run in self.runs.iter_mut().filter(|run| !run.finished) {
            run.asks.clear();
            let stop = run.stop.clone();
            // Over SSH the stop is a round trip to the host: off the window thread.
            cx.background_spawn(async move { stop.stop() }).detach();
        }
        for term in &self.terminals {
            term.read(cx).terminate();
        }
        cx.notify();
        let started = Instant::now();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(150)).await;
                let ended = this.update(cx, |this, _| this.runs.iter().all(|run| run.finished)).unwrap_or(true);
                if ended || started.elapsed() >= STOP_WAIT {
                    break;
                }
            }
            cx.update(|cx| cx.quit());
        })
        .detach();
    }

    /// The question over the window, while there is one.
    pub(super) fn render_quit_ask(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ask = self.quit_ask.as_ref()?;
        let theme = *cx.global::<Theme>();
        let turns = self.running_turns();
        let terminals = self.open_terminals(cx);
        let list = |title: &str, names: Vec<String>| {
            (!names.is_empty()).then(|| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(div().text_size(px(12.)).text_color(theme.text_muted).child(title.replace("{n}", &names.len().to_string())))
                    .children(names.into_iter().take(8).map(|name| div().text_size(px(12.5)).truncate().child(format!("•  {name}"))))
            })
        };
        let button = |id: &'static str, label: &str, danger: bool| {
            div()
                .id(id)
                .h(px(30.))
                .px(px(14.))
                .flex()
                .items_center()
                .rounded(px(7.))
                .text_size(px(13.))
                .when(danger, |this| this.bg(theme.error).text_color(white()).hover(|style| style.opacity(0.9)))
                .when(!danger, |this| this.border_1().border_color(theme.border).hover(|style| style.bg(theme.hover)))
                .child(label.to_owned())
        };
        let actions = if ask.stopping {
            div().text_size(px(12.5)).text_color(theme.text_muted).child(t("quit.stopping")).into_any_element()
        } else {
            div()
                .flex()
                .justify_end()
                .gap(px(8.))
                .child(button("quit-cancel", t("quit.cancel"), false).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.quit_ask = None;
                    cx.notify();
                })))
                .child(button("quit-stop", t("quit.stop"), true).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.stop_all_and_quit(cx))))
                .into_any_element()
        };
        Some(
            div()
                .id("quit-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(hsla(0., 0., 0., 0.45))
                .occlude()
                .child(
                    island(&theme)
                        .id("quit-dialog")
                        .w(px(440.))
                        .p(px(20.))
                        .flex()
                        .flex_col()
                        .gap(px(14.))
                        .bg(theme.editor_bg)
                        .shadow(popup_shadow(&theme))
                        .child(div().font_weight(FontWeight::SEMIBOLD).text_size(px(15.)).child(t("quit.title")))
                        .child(div().text_size(px(12.5)).text_color(theme.text_secondary).child(t("quit.text")))
                        .children(list(t("quit.turns"), turns))
                        .children(list(t("quit.terminals"), terminals))
                        .child(actions),
                )
                .into_any_element(),
        )
    }
}
