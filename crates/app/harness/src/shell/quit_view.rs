//! Quitting while sessions run: the window asks first, then stops every turn it runs and
//! every terminal it holds, waits for them to end, and only then quits. It asks only when
//! something goes on: a turn at work, or a terminal running a program. A harness that has
//! answered and waits for the user, in a turn or in a terminal, and a shell at its prompt
//! are stopped without a question.

use std::time::{Duration, Instant};

use ui_kit::prelude::*;
use ui_kit::*;

use super::Shell;
use super::update_view::PendingUpdate;
use crate::i18n::t;

/// How long the window waits for the stopped turns before it quits anyway.
const STOP_WAIT: Duration = Duration::from_secs(10);

/// The programs of the harnesses, as a terminal names what its shell runs.
const HARNESSES: &[&str] = &["claude", "codex", "opencode", "dsh"];

/// How long a terminal writes nothing before a shell or a harness in it counts as waiting
/// for the user: a harness at work redraws its spinner many times a second, and a command
/// the shell runs itself (a cmdlet, a builtin) shows no program of its own.
const QUIET: Duration = Duration::from_secs(5);

/// Whether something goes on in a terminal, from what its shell runs (`programs`, `None`
/// when that is not known) and how long it has written nothing. A shell at its prompt or
/// a harness that has been quiet a while waits for the user; any other program is at work,
/// silent or not. `harness` says a harness was resumed here, which an npm install runs as
/// `node`.
pub(super) fn terminal_busy(programs: Option<&[String]>, quiet: Duration, harness: bool) -> bool {
    let Some(programs) = programs else { return true };
    let waits = programs.iter().all(|program| HARNESSES.contains(&program.as_str()) || (harness && program == "node"));
    !waits || quiet < QUIET
}

/// Whether a turn of the window is at work: not ended, and not resting after its answer
/// unless something it left in the background still runs.
pub(super) fn run_at_work(finished: bool, resting: bool, background: bool) -> bool {
    !finished && (!resting || background)
}

/// The question shown before quitting; `stopping` once the user chose to quit.
#[derive(Default)]
pub(super) struct QuitAsk {
    stopping: bool,
    /// Nothing went on: everything stops without a question.
    silent: bool,
    /// The terminals where something went on when the question came, by title.
    terminals: Vec<String>,
    /// The update that goes in once everything has stopped, when installing it asked.
    update: Option<PendingUpdate>,
}

impl Shell {
    /// The titles of the sessions whose turns are at work: those that answered too while a
    /// command or an agent of theirs runs in the background (named after the title), not
    /// those whose harness only waits (for a wakeup, or for nothing).
    fn running_turns(&self) -> Vec<String> {
        self.runs
            .iter()
            .filter(|run| run_at_work(run.finished, run.resting, !run.background.is_empty()))
            .map(|run| {
                let title = self.summary(&run.key).map_or_else(|| t("new_session").to_owned(), |summary| summary.title.clone());
                match run.background.first().filter(|_| run.resting) {
                    Some((task, _)) => format!("{title} · {}", task.description),
                    None => title,
                }
            })
            .collect()
    }

    /// The titles of the terminals where something goes on. Reads the process table.
    fn busy_terminals(&self, cx: &App) -> Vec<String> {
        self.terminals.iter().map(|term| term.read(cx)).filter(|term| term.busy()).map(|term| term.title().to_owned()).collect()
    }

    /// Whether a turn's harness or a terminal's program still runs, at work or not.
    fn anything_alive(&self, cx: &App) -> bool {
        self.runs.iter().any(|run| !run.finished) || self.terminals.iter().any(|term| !term.read(cx).is_closed())
    }

    /// Whether the app may quit now. With turns at work or busy terminals it asks the user
    /// instead and says no; their answer quits later. With harnesses and shells that only
    /// wait it says no too, stops them without asking, and quits once they have ended.
    pub(crate) fn may_quit(&mut self, cx: &mut Context<Self>) -> bool {
        if self.quitting() {
            return false;
        }
        // A question already up keeps the update it waits with.
        if self.quit_ask.is_some() {
            cx.notify();
            return false;
        }
        let terminals = self.busy_terminals(cx);
        if self.running_turns().is_empty() && terminals.is_empty() {
            if !self.anything_alive(cx) {
                return true;
            }
            self.quit_ask = Some(QuitAsk { silent: true, ..QuitAsk::default() });
            // The caller may put an update behind it first: the stop comes a moment later.
            cx.spawn(async move |this, cx| {
                this.update(cx, |this, cx| this.stop_all_and_quit(cx)).ok();
            })
            .detach();
            return false;
        }
        self.quit_ask = Some(QuitAsk { terminals, ..QuitAsk::default() });
        cx.notify();
        false
    }

    /// Whether the window shows the question: it does not when nothing went on.
    pub(crate) fn asking(&self) -> bool {
        self.quit_ask.as_ref().is_some_and(|ask| !ask.silent)
    }

    /// Whether the user chose to stop everything and quit: no turn starts any more.
    pub(super) fn quitting(&self) -> bool {
        self.quit_ask.as_ref().is_some_and(|ask| ask.stopping)
    }

    /// Puts an update behind the open question: its answer installs it.
    pub(super) fn set_quit_update(&mut self, update: PendingUpdate) {
        if let Some(ask) = &mut self.quit_ask {
            ask.update = Some(update);
        }
    }

    /// Closes the question without quitting; an update waiting behind it is given up.
    fn cancel_quit(&mut self, cx: &mut Context<Self>) {
        if self.quit_ask.take().is_some_and(|ask| ask.update.is_some()) {
            self.update_given_up();
        }
        cx.notify();
    }

    /// Stops every turn and terminal, waits until the turns have ended, then quits, or
    /// installs the update the question was for (which quits).
    fn stop_all_and_quit(&mut self, cx: &mut Context<Self>) {
        let (update, silent) = self.quit_ask.take().map_or((None, false), |ask| (ask.update, ask.silent));
        self.quit_ask = Some(QuitAsk { stopping: true, silent, ..QuitAsk::default() });
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
            match update {
                None => cx.update(|cx| cx.quit()),
                Some(update) => {
                    this.update(cx, |this, cx| {
                        this.quit_ask = None;
                        this.apply_update(update, cx);
                    })
                    .ok();
                }
            }
        })
        .detach();
    }

    /// The question over the window, while there is one.
    pub(super) fn render_quit_ask(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ask = self.quit_ask.as_ref().filter(|ask| !ask.silent)?;
        let theme = *cx.global::<Theme>();
        let turns = self.running_turns();
        let terminals = ask.terminals.clone();
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
                .child(button("quit-cancel", t("quit.cancel"), false).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.cancel_quit(cx))))
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
                        .when(ask.update.is_some(), |this| {
                            this.child(div().text_size(px(12.5)).text_color(theme.text_secondary).child(t("quit.update")))
                        })
                        .children(list(t("quit.turns"), turns))
                        .children(list(t("quit.terminals"), terminals))
                        .child(actions),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{run_at_work, terminal_busy};

    #[test]
    fn a_turn_is_at_work_until_it_rests_with_nothing_in_the_background() {
        assert!(run_at_work(false, false, false), "answering");
        assert!(run_at_work(false, true, true), "answered, a command of it runs on");
        assert!(!run_at_work(false, true, false), "answered, it only waits");
        assert!(!run_at_work(true, false, true), "ended");
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn a_terminal_is_busy_only_while_something_goes_on() {
        let (quiet, writing) = (Duration::from_secs(60), Duration::from_millis(200));
        assert!(!terminal_busy(Some(&[]), quiet, false), "a shell at its prompt");
        assert!(terminal_busy(Some(&[]), writing, false), "a shell that prints runs a command of its own");
        assert!(terminal_busy(Some(&names(&["cargo"])), quiet, false), "a silent command is still at work");
        assert!(!terminal_busy(Some(&names(&["claude"])), quiet, false), "a quiet harness waits for the user");
        assert!(terminal_busy(Some(&names(&["claude"])), writing, false), "a harness that draws its spinner works");
        assert!(terminal_busy(Some(&names(&["claude", "cargo"])), quiet, false), "a harness beside a command");
        assert!(terminal_busy(Some(&names(&["node"])), quiet, false), "node alone may be anything");
        assert!(!terminal_busy(Some(&names(&["node"])), quiet, true), "but a resumed npm harness runs as node");
        assert!(terminal_busy(Some(&names(&["ssh"])), quiet, false), "what runs on a host is not seen");
        assert!(terminal_busy(None, quiet, false), "not known: ask");
    }
}
