//! Keeping the computer awake while agents work (`crate::awake` holds it): a turn of the
//! window at work, or answered while a command of it runs in the background; a session that
//! works in a terminal or on a host; a terminal that runs a program. A question waiting for
//! the user does not hold it, nor a harness that answered and only waits for a wakeup, nor
//! a session whose last tool call never got its result (a permission prompt in a terminal
//! looks the same as a long command there).

use std::time::Duration;

use ui_kit::prelude::*;

use super::quit_view::{run_at_work, terminal_busy};
use super::{RowStatus, Shell, now_ms, row_status};

/// A terminal that wrote within this time is at work; past it the programs of its shell
/// say (that reads the process table, so it is asked only of quiet terminals).
const TERMINAL_RECENT: Duration = Duration::from_secs(30);

/// What a turn of the window does now, as the hold sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TurnNow {
    /// At work: see [`run_at_work`].
    pub at_work: bool,
    /// It waits for the user's answer to a question.
    pub asking: bool,
}

/// Whether the computer is kept awake, from the turns of the window, the state of the
/// sessions the tree knows that no turn of the window runs, and whether a terminal is at
/// work.
pub(super) fn wants_awake(turns: &[TurnNow], sessions: &[Option<RowStatus>], terminal_at_work: bool) -> bool {
    turns.iter().any(|turn| turn.at_work && !turn.asking) || sessions.contains(&Some(RowStatus::Working)) || terminal_at_work
}

impl Shell {
    /// Whether the computer is kept awake while agents work (on unless turned off).
    pub(super) fn keep_awake_enabled(&self) -> bool {
        self.lib().setting("power.keep_awake").is_none_or(|value| value != "false")
    }

    /// Takes or gives back the hold, at every poll and when the setting changes. The hold
    /// is set on the window's thread (Windows keeps it per thread); what the terminals run
    /// is looked at in the background, and only when nothing else holds.
    pub(super) fn keep_awake(&mut self, cx: &mut Context<Self>) {
        if !self.keep_awake_enabled() {
            self.awake.set(false);
            return;
        }
        let turns: Vec<TurnNow> = self
            .runs
            .iter()
            .map(|run| TurnNow { at_work: run_at_work(run.finished, run.resting, !run.background.is_empty()), asking: !run.asks.is_empty() })
            .collect();
        let now = now_ms();
        // A session a turn of the window runs is what its turn says, not what its file does.
        let ours = |key: &model::SessionKey| self.runs.iter().any(|run| !run.finished && &run.key == key);
        let sessions: Vec<Option<RowStatus>> =
            self.summaries.iter().filter(|summary| !ours(&summary.key)).map(|summary| row_status(summary.turn, summary.updated_ms, now)).collect();
        if wants_awake(&turns, &sessions, false) {
            self.awake.set(true);
            return;
        }
        let terminals: Vec<_> = self.terminals.iter().filter_map(|term| term.read(cx).watch()).collect();
        if terminals.iter().any(|(session, _)| session.quiet_for() < TERMINAL_RECENT) {
            self.awake.set(true);
            return;
        }
        if terminals.is_empty() {
            self.awake.set(false);
            return;
        }
        let task = cx.background_spawn(async move {
            terminals.iter().any(|(session, harness)| terminal_busy(session.programs().as_deref(), session.quiet_for(), *harness))
        });
        cx.spawn(async move |this, cx| {
            let busy = task.await;
            this.update(cx, |this, _| this.awake.set(busy && this.keep_awake_enabled())).ok();
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::{RowStatus, TurnNow, wants_awake};

    const AT_WORK: TurnNow = TurnNow { at_work: true, asking: false };

    #[test]
    fn work_keeps_the_computer_awake_and_waiting_for_the_user_does_not() {
        assert!(!wants_awake(&[], &[], false), "nothing at all");
        assert!(wants_awake(&[AT_WORK], &[], false), "a turn at work");
        assert!(!wants_awake(&[TurnNow { at_work: true, asking: true }], &[], false), "a question waits for the user");
        assert!(!wants_awake(&[TurnNow { at_work: false, asking: false }], &[], false), "a turn that ended or only waits");
        assert!(wants_awake(&[], &[None, Some(RowStatus::Working)], false), "a session at work in a terminal");
        assert!(!wants_awake(&[], &[Some(RowStatus::Waiting)], false), "a tool call with no result may be a permission prompt");
        assert!(!wants_awake(&[], &[Some(RowStatus::YourTurn), None], false), "sessions that wait for the user");
        assert!(wants_awake(&[], &[], true), "a terminal at work");
        // A turn waits for the user's answer while its session's file shows the tool call
        // without a result: the session of a turn of the window is left out of `sessions`.
        assert!(!wants_awake(&[TurnNow { at_work: true, asking: true }], &[], false));
    }
}
