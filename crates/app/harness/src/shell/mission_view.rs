//! Mission control: every agent at work in one grid, here and on the hosts. A card is a
//! turn the window runs (streamed as it goes) or a session at work in a terminal of this
//! computer (read from the end of its history every second). A card that waits for the
//! user glows and answers a question about an action with its own buttons; a click opens
//! the session.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use live::usage::{Plan, WindowKind};
use model::{EnvId, HarnessKind, SessionKey};
use ui_kit::prelude::*;
use ui_kit::*;

use super::{HarnessColor, MONO, critter, hex, RowStatus, Shell, ago, now_ms, row_status};
use crate::i18n::t;
use crate::mission::{CardLine, ToolState, activity, card_lines, growth, last_turn_usage};

/// Cards shown at most.
const CARDS: usize = 9;
/// Lines a card keeps: it shows its end and scrolls back through the rest.
const LINES: usize = 100;
/// Bars of a card's activity.
const SPARK: usize = 28;
/// How long a finished turn of the window stays as a card once the window forgets it.
const KEEP_FINISHED: Duration = Duration::from_secs(10 * 60);
/// How often the view moves: the spinners, the glow, the clocks.
const FRAME: Duration = Duration::from_millis(150);
/// Seconds the pace is taken over.
const PACE: usize = 8;
/// The end of a session read for its card at first. A line can be large (a picture, a long
/// result), so the read grows fourfold while it gives fewer lines than a card keeps.
const TAIL: u64 = 96 * 1024;
/// The most of a session read for its card.
const MAX_TAIL: u64 = 6 * 1024 * 1024;

/// What a card stands for: a turn of the window (its id names it before the harness names
/// the session), or a session at work elsewhere.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum CardId {
    Run(u64),
    Session(SessionKey),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CardState {
    Working,
    Waiting,
    Background,
    Done,
    Failed,
}

/// A question about an action that the card answers with buttons: the tool call, what the
/// harness wants to do, its options `(id, kind, label)`.
#[derive(Clone)]
struct CardAsk {
    call: String,
    title: String,
    options: Vec<(String, String, String)>,
}

#[derive(Clone)]
struct Card {
    id: CardId,
    key: SessionKey,
    title: String,
    state: CardState,
    lines: Vec<CardLine>,
    asks: Vec<CardAsk>,
    /// A question with choices waits: it is answered in the session.
    question: bool,
    /// Tokens in the context and the model's window.
    context: Option<(u64, Option<u64>)>,
    cost: Option<f64>,
    /// When the turn began, for a turn of the window.
    started: Option<Instant>,
    /// When the session last wrote, for one elsewhere.
    updated_ms: Option<i64>,
    activity: usize,
}

/// The end of a session elsewhere, as last read.
struct Preview {
    modified: Option<std::time::SystemTime>,
    lines: Vec<CardLine>,
    context: Option<(u64, Option<u64>)>,
    /// What the last turn cost.
    cost: Option<f64>,
    /// What the session wrote since the card first read it.
    activity: usize,
    /// What each entry at the end held, by id: the next read counts only what is new.
    seen: HashMap<String, usize>,
    /// How much of the end is read.
    span: u64,
}

#[derive(Default)]
pub(super) struct Mission {
    /// The order of the cards: one keeps its place while it lives.
    slots: Vec<CardId>,
    /// The cards as last drawn, for a turn the window has forgotten.
    last: HashMap<CardId, Card>,
    /// Turns the window forgot, kept as they ended.
    finished: HashMap<CardId, (Card, Instant)>,
    /// Activity per card: the last amount, and the growth of each second.
    spark: HashMap<CardId, (usize, VecDeque<f32>)>,
    previews: HashMap<SessionKey, Preview>,
    reading: bool,
    /// Names the ticker of this opening of the view.
    generation: u64,
    /// Something on a card moves: a turn at work or waiting.
    moving: bool,
    sampled: Option<Instant>,
    /// Where each card's log is scrolled.
    scrolls: HashMap<CardId, ScrollHandle>,
}

impl Shell {
    pub(super) fn mission_open(&self) -> bool {
        self.mission.is_some()
    }

    pub(super) fn toggle_mission(&mut self, cx: &mut Context<Self>) {
        if self.mission.take().is_some() {
            cx.notify();
            return;
        }
        self.mission_generation += 1;
        let generation = self.mission_generation;
        self.mission = Some(Mission { generation, slots: self.mission_order.clone(), ..Mission::default() });
        self.sample_mission(cx);
        // One ticker while the view is open: a frame for what moves on the cards, and once a
        // second the activity and the ends of the sessions elsewhere.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(FRAME).await;
                let going = this
                    .update(cx, |this, cx| {
                        let Some(mission) = &this.mission else { return false };
                        if mission.generation != generation {
                            return false;
                        }
                        let due = mission.sampled.is_none_or(|at| at.elapsed() >= Duration::from_secs(1));
                        if due {
                            this.sample_mission(cx);
                        }
                        if due || this.mission.as_ref().is_some_and(|mission| mission.moving) {
                            cx.notify();
                        }
                        true
                    })
                    .unwrap_or(false);
                if !going {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    /// The cards now, in their places.
    fn mission_cards(&mut self) -> Vec<Card> {
        let now = now_ms();
        let mut cards: Vec<Card> = Vec::new();
        for run in &self.runs {
            let title = (!run.key.id.is_empty())
                .then(|| self.summary(&run.key).map(|summary| self.placements.get(&run.key).and_then(|p| p.title.clone()).unwrap_or_else(|| summary.display_title())))
                .flatten()
                .unwrap_or_else(|| model::shorten(run.spec.prompt.lines().next().unwrap_or(""), 80));
            let asks: Vec<CardAsk> = run
                .asks
                .iter()
                .filter(|ask| ask.questions.is_empty())
                .map(|ask| CardAsk { call: ask.tool_call_id.clone(), title: ask.title.clone(), options: ask.options.clone() })
                .collect();
            let question = run.asks.iter().any(|ask| !ask.questions.is_empty());
            let state = if !run.asks.is_empty() {
                CardState::Waiting
            } else if run.finished && run.error.is_some() {
                CardState::Failed
            } else if run.finished || (run.resting && run.background.is_empty()) {
                CardState::Done
            } else if run.resting {
                CardState::Background
            } else {
                CardState::Working
            };
            let running = matches!(state, CardState::Working | CardState::Waiting);
            let mut lines = run.turn.as_ref().map(|turn| card_lines(turn, LINES, running)).unwrap_or_default();
            if !lines.iter().any(|line| matches!(line, CardLine::Prompt(_))) && !run.spec.prompt.trim().is_empty() && lines.len() < LINES {
                lines.insert(0, CardLine::Prompt(model::shorten(&run.spec.prompt.split_whitespace().collect::<Vec<_>>().join(" "), 320)));
            }
            let model_name = run.turn.as_ref().and_then(|turn| turn.model.clone()).or_else(|| run.model.clone());
            cards.push(Card {
                id: CardId::Run(run.id),
                key: run.key.clone(),
                title,
                state,
                lines,
                asks,
                question,
                context: run.turn.as_ref().and_then(|turn| turn.last_usage).map(|usage| (usage.context(), model_name.as_deref().and_then(model::window_of))),
                cost: run.turn.as_ref().filter(|turn| !turn.usage_by_model.is_empty()).map(|turn| model::pricing::session_cost(&turn.usage_by_model).0),
                started: Some(run.started),
                updated_ms: None,
                activity: run.turn.as_ref().map(activity).unwrap_or(0),
            });
        }
        // Sessions at work elsewhere, the latest first; one the window runs shows as its turn.
        let ours: Vec<&SessionKey> = self.runs.iter().map(|run| &run.key).filter(|key| !key.id.is_empty()).collect();
        let at_work = |key: &SessionKey, id: &str| {
            self.runs.iter().any(|run| run.key.id == id && run.key.env == key.env)
                || self.summaries.iter().any(|other| {
                    other.key.id == id && other.key.env == key.env && matches!(row_status(other.turn, other.updated_ms, now), Some(RowStatus::Working | RowStatus::Waiting))
                })
        };
        let mut others: Vec<(i64, Card)> = self
            .summaries
            .iter()
            .filter(|summary| !ours.contains(&&summary.key))
            .filter(|summary| summary.parent.as_deref().is_none_or(|parent| !at_work(&summary.key, parent)))
            .filter_map(|summary| {
                let state = match row_status(summary.turn, summary.updated_ms, now)? {
                    RowStatus::Working => CardState::Working,
                    RowStatus::Waiting => CardState::Waiting,
                    RowStatus::YourTurn => CardState::Done,
                };
                let preview = self.mission.as_ref().and_then(|mission| mission.previews.get(&summary.key));
                let title = self.placements.get(&summary.key).and_then(|p| p.title.clone()).unwrap_or_else(|| summary.display_title());
                Some((
                    summary.updated_ms,
                    Card {
                        id: CardId::Session(summary.key.clone()),
                        key: summary.key.clone(),
                        title,
                        state,
                        lines: preview.map(|preview| preview.lines.clone()).unwrap_or_default(),
                        asks: Vec::new(),
                        question: false,
                        context: preview.and_then(|preview| preview.context),
                        cost: preview.and_then(|preview| preview.cost),
                        started: None,
                        updated_ms: Some(summary.updated_ms),
                        activity: preview.map_or(0, |preview| preview.activity),
                    },
                ))
            })
            .collect();
        others.sort_by_key(|(updated, _)| std::cmp::Reverse(*updated));
        cards.extend(others.into_iter().map(|(_, card)| card).take(CARDS));

        let Some(mission) = &mut self.mission else { return cards };
        // A place whose card is gone goes to the card of the same session: a turn the window
        // ran becomes its session, a session the window runs again becomes the turn. A turn the
        // window forgot without one stays as it ended.
        for slot in mission.slots.iter_mut() {
            if cards.iter().any(|card| card.id == *slot) || mission.finished.contains_key(slot) {
                continue;
            }
            let key = match &*slot {
                CardId::Session(key) => Some(key.clone()),
                CardId::Run(_) => mission.last.get(slot).map(|card| card.key.clone()),
            };
            if let Some(key) = key.filter(|key| !key.id.is_empty())
                && let Some(card) = cards.iter().find(|card| card.key == key)
            {
                *slot = card.id.clone();
                continue;
            }
            let Some(gone) = mission.last.get(slot).cloned() else { continue };
            if matches!(slot, CardId::Run(_)) {
                let mut ended = gone;
                if matches!(ended.state, CardState::Working | CardState::Waiting | CardState::Background) {
                    ended.state = CardState::Done;
                }
                ended.asks.clear();
                mission.finished.insert(slot.clone(), (ended, Instant::now()));
            }
        }
        mission.finished.retain(|_, (_, at)| at.elapsed() < KEEP_FINISHED);
        for (id, (card, _)) in &mission.finished {
            if !cards.iter().any(|known| known.id == *id || (!card.key.id.is_empty() && known.key == card.key)) {
                cards.push(card.clone());
            }
        }
        // Places: a card keeps its own; a new one goes after the others.
        mission.slots.retain(|slot| cards.iter().any(|card| card.id == *slot));
        for card in &cards {
            if !mission.slots.contains(&card.id) {
                mission.slots.push(card.id.clone());
            }
        }
        let mut placed: Vec<Card> = mission.slots.iter().filter_map(|slot| cards.iter().find(|card| card.id == *slot).cloned()).collect();
        placed.truncate(CARDS);
        mission.last = placed.iter().map(|card| (card.id.clone(), card.clone())).collect();
        // Kept for the next opening by session where it is known: a turn ends meanwhile.
        self.mission_order = mission
            .slots
            .iter()
            .map(|slot| match mission.last.get(slot) {
                Some(card) if matches!(slot, CardId::Run(_)) && !card.key.id.is_empty() => CardId::Session(card.key.clone()),
                _ => slot.clone(),
            })
            .collect();
        placed
    }

    /// Once a second: each card's activity, and the ends of the sessions elsewhere.
    fn sample_mission(&mut self, cx: &mut Context<Self>) {
        let cards = self.mission_cards();
        let Some(mission) = &mut self.mission else { return };
        mission.sampled = Some(Instant::now());
        mission.moving = cards.iter().any(|card| matches!(card.state, CardState::Working | CardState::Waiting));
        for card in &cards {
            // A session elsewhere is measured from its first read on.
            if matches!(card.id, CardId::Session(_)) && !mission.previews.contains_key(&card.key) {
                continue;
            }
            let (last, bars) = mission.spark.entry(card.id.clone()).or_insert_with(|| (card.activity, VecDeque::from(vec![0.; SPARK])));
            let grown = card.activity.saturating_sub(*last) as f32;
            *last = card.activity;
            bars.push_back(grown);
            while bars.len() > SPARK {
                bars.pop_front();
            }
        }
        mission.spark.retain(|id, _| cards.iter().any(|card| card.id == *id));
        if mission.reading {
            return;
        }
        // The ends of Claude Code and Codex sessions at work in a terminal of this computer.
        type Known = (Option<std::time::SystemTime>, usize, HashMap<String, usize>, u64);
        type Wanted = (SessionKey, String, Option<Known>, bool);
        let wanted: Vec<Wanted> = cards
            .iter()
            .filter(|card| {
                matches!(card.id, CardId::Session(_)) && card.key.env.is_local() && matches!(card.key.harness, HarnessKind::Claude | HarnessKind::Codex)
            })
            .filter_map(|card| {
                let summary = self.summaries.iter().find(|summary| summary.key == card.key)?;
                let known = mission.previews.get(&card.key).map(|preview| (preview.modified, preview.activity, preview.seen.clone(), preview.span));
                Some((card.key.clone(), summary.source.path.clone(), known, matches!(card.state, CardState::Working | CardState::Waiting)))
            })
            .collect();
        mission.previews.retain(|key, _| cards.iter().any(|card| card.key == *key));
        if wanted.is_empty() {
            return;
        }
        mission.reading = true;
        let task = cx.background_spawn(async move {
            let mut read = Vec::new();
            for (key, path, known, working) in wanted {
                let Ok(meta) = std::fs::metadata(&path) else { continue };
                let modified = meta.modified().ok();
                if modified.is_some() && known.as_ref().is_some_and(|(at, ..)| *at == modified) {
                    continue;
                }
                let (before, seen, span_before) = known.map(|(_, before, seen, span)| (before, seen, span)).unwrap_or((0, HashMap::new(), TAIL));
                let size = meta.len();
                let mut span = span_before;
                let transcript = loop {
                    let Ok(bytes) = read_tail(&path, size, span) else { break None };
                    let text = String::from_utf8_lossy(&bytes);
                    let transcript = match key.harness {
                        HarnessKind::Codex => history::codex_tail(&text),
                        _ => history::claude_tail(&text, size > span),
                    };
                    if span >= size || span >= MAX_TAIL || card_lines(&transcript, LINES, false).len() >= LINES {
                        break Some(transcript);
                    }
                    span = (span * 4).min(MAX_TAIL);
                };
                let Some(transcript) = transcript else { continue };
                let window = transcript.model.as_deref().and_then(model::window_of);
                let (grown, seen_now) = growth(&transcript, &seen);
                // The first read is where the card starts counting from; a longer read brings
                // older entries, which are not new.
                let activity = if seen.is_empty() || span != span_before { before } else { before + grown };
                let turn = last_turn_usage(&transcript);
                read.push((
                    key,
                    Preview {
                        modified,
                        lines: card_lines(&transcript, LINES, working),
                        context: transcript.last_usage.map(|usage| (usage.context(), window)),
                        cost: (!turn.is_empty()).then(|| model::pricing::session_cost(&turn).0),
                        activity,
                        seen: seen_now,
                        span,
                    },
                ));
            }
            read
        });
        cx.spawn(async move |this, cx| {
            let read = task.await;
            this.update(cx, |this, cx| {
                if let Some(mission) = &mut this.mission {
                    mission.reading = false;
                    for (key, preview) in read {
                        mission.previews.insert(key, preview);
                    }
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Answers a question about an action from its card.
    fn answer_from_card(&mut self, run: u64, call: &str, option: String, cx: &mut Context<Self>) {
        let Some(ix) = self.runs.iter().position(|live| live.id == run) else { return };
        self.answer_permission_of(ix, call, option, cx);
    }

    fn open_card(&mut self, card: &Card, cx: &mut Context<Self>) {
        if card.key.id.is_empty() {
            return;
        }
        self.mission = None;
        self.open_session(card.key.clone(), cx);
        cx.notify();
    }

    pub(super) fn render_mission(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.global::<Theme>();
        let cards = self.mission_cards();
        // A log at its end stays there as lines come; one scrolled back stays where it is.
        let scrolls: Vec<ScrollHandle> = match &mut self.mission {
            Some(mission) => {
                mission.scrolls.retain(|id, _| cards.iter().any(|card| card.id == *id));
                cards.iter().map(|card| mission.scrolls.entry(card.id.clone()).or_default().clone()).collect()
            }
            None => cards.iter().map(|_| ScrollHandle::new()).collect(),
        };
        for scroll in &scrolls {
            let end = scroll.max_offset().y;
            if -scroll.offset().y >= end - px(8.) {
                scroll.scroll_to_bottom();
            }
        }
        let count = |state: CardState| cards.iter().filter(|card| card.state == state).count();
        let pace: f32 = cards.iter().map(|card| self.pace(&card.id)).sum();
        let spent: f64 = cards.iter().filter_map(|card| card.cost).sum();
        let five_hours = match self.plans.get(&(EnvId::local(), HarnessKind::Claude)).and_then(|state| state.plan.as_ref()) {
            Some(Plan::Limits { windows, .. }) => windows.iter().find(|window| window.kind == WindowKind::FiveHours).map(|window| window.percent),
            _ => None,
        };
        let stat = |value: String, label: &str, color: Hsla| {
            div()
                .flex()
                .items_end()
                .gap(px(5.))
                .text_size(px(12.))
                .text_color(theme.text_muted)
                .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).text_color(color).child(value))
                .child(label.to_owned())
        };
        let bar = div()
            .flex()
            .items_center()
            .gap(px(18.))
            .h(px(48.))
            .px(px(16.))
            .border_b_1()
            .border_color(theme.border)
            .child(div().font_weight(FontWeight::SEMIBOLD).text_size(px(15.)).child(t("mission.title")))
            .child(stat(count(CardState::Working).to_string(), t("mission.working"), theme.text))
            .child(stat(count(CardState::Waiting).to_string(), t("mission.waiting"), if count(CardState::Waiting) > 0 { theme.warning_mark } else { theme.text }))
            .child(stat((count(CardState::Done) + count(CardState::Failed)).to_string(), t("mission.done"), theme.text))
            .child(stat(format!("≈{}", pace.round() as u64), t("mission.pace"), theme.text))
            .child(stat(format!("${spent:.2}"), t("mission.spent"), theme.text))
            .child(div().flex_1())
            .children(five_hours.map(|percent| {
                div()
                    .w(px(170.))
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .text_size(px(11.))
                            .text_color(theme.text_muted)
                            .child(format!("Claude · {}", t("plan.five_hours")))
                            .child(format!("{}%", percent.round() as u64)),
                    )
                    .child(
                        div()
                            .h(px(5.))
                            .w_full()
                            .rounded(px(3.))
                            .bg(theme.hover)
                            .child(div().h_full().w(px(170. * (percent / 100.0).clamp(0.0, 1.0) as f32)).rounded(px(3.)).bg(super::plan_view::fill_color(percent, &theme))),
                    )
            }))
            .child(
                div()
                    .id("mission-close")
                    .flex()
                    .items_center()
                    .h(px(26.))
                    .px(px(10.))
                    .rounded(px(6.))
                    .text_size(px(12.))
                    .text_color(theme.text_secondary)
                    .hover(|style| style.bg(theme.hover))
                    .child(t("mission.close"))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_mission(cx))),
            );
        let body = if cards.is_empty() {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.text_muted)
                .child(t("mission.empty"))
                .into_any_element()
        } else {
            // Rows of up to three cards; two rows fill the height, more scroll.
            let per_row = if cards.len() <= 2 { cards.len() } else if cards.len() == 4 { 2 } else { 3 };
            let rows: Vec<AnyElement> = cards
                .chunks(per_row)
                .enumerate()
                .map(|(ri, row)| {
                    div()
                        .flex()
                        .gap(px(12.))
                        .flex_1()
                        .min_h(px(260.))
                        .children(row.iter().enumerate().map(|(ci, card)| self.render_card(ri * per_row + ci, card, &scrolls[ri * per_row + ci], &theme, cx)))
                        .children((row.len()..per_row).map(|_| div().flex_1().into_any_element()))
                        .into_any_element()
                })
                .collect();
            div().id("mission-grid").flex_1().min_h_0().overflow_y_scroll().p(px(14.)).flex().flex_col().gap(px(12.)).children(rows).into_any_element()
        };
        island(&theme).flex_1().min_w_0().h_full().bg(theme.editor_bg).flex().flex_col().child(bar).child(body).into_any_element()
    }

    /// Tokens a second a card's activity stands for, roughly: four characters a token.
    fn pace(&self, id: &CardId) -> f32 {
        self.mission
            .as_ref()
            .and_then(|mission| mission.spark.get(id))
            .map_or(0., |(_, bars)| bars.iter().rev().take(PACE).sum::<f32>() / PACE as f32 / 4.)
    }

    fn render_card(&self, ix: usize, card: &Card, scroll: &ScrollHandle, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let theme = *theme;
        let now = now_ms();
        let (label, color) = match card.state {
            CardState::Working => (t("mission.state.working"), theme.accent),
            CardState::Waiting => (t("mission.state.waiting"), theme.warning_mark),
            CardState::Background => (t("status.background"), theme.text_secondary),
            CardState::Done => (t("mission.state.done"), theme.success),
            CardState::Failed => (t("mission.state.failed"), theme.error),
        };
        // A slow glow while the card waits for the user.
        let glow = (card.state == CardState::Waiting).then(|| ((now as f64 / 400.0).sin() * 0.5 + 0.5) as f32);
        let blink = (now / 500) % 2 == 0;
        let pill = div()
            .flex()
            .items_center()
            .gap(px(5.))
            .px(px(8.))
            .rounded(px(10.))
            .text_size(px(11.))
            .text_color(color)
            .bg(color.opacity(0.13))
            .child(match card.state {
                CardState::Working => if blink { "●" } else { "○" },
                CardState::Waiting => "●",
                CardState::Done => "✓",
                CardState::Failed => "✕",
                CardState::Background => "◔",
            })
            .child(label);
        let machine = match card.key.env.ssh_host() {
            Some(host) => format!("⇄ {host}"),
            None => t("local").to_owned(),
        };
        // The harness, the machine and the state above; the title below, the whole width.
        let head = div()
            .flex()
            .flex_col()
            .gap(px(5.))
            .px(px(12.))
            .pt(px(10.))
            .pb(px(8.))
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .px(px(7.))
                    .py(px(1.))
                    .rounded(px(5.))
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(hex(0xFFFFFF))
                    .bg(card.key.harness_color())
                    .child(critter(&card.key.harness, hex(0xFFFFFF), 12.))
                    .child(card.key.harness.label().to_owned()),
                    )
                    .child(div().flex_1().min_w_0().truncate().text_size(px(11.)).text_color(theme.text_muted).child(machine))
                    .child(pill.flex_none()),
            )
            .child(div().truncate().font_weight(FontWeight::SEMIBOLD).child(card.title.clone()));
        let spinner = ["◐", "◓", "◑", "◒"][((now / 150) % 4) as usize];
        let lines: Vec<AnyElement> = card
            .lines
            .iter()
            .map(|line| match line {
                // The prompt as a bubble to the right; a margin rather than a share of the
                // width, so its text wraps inside it.
                CardLine::Prompt(text) => div()
                    .ml(px(32.))
                    .px(px(10.))
                    .py(px(5.))
                    .rounded(px(10.))
                    .bg(theme.selection_muted)
                    .child(text.clone())
                    .into_any_element(),
                CardLine::Text(text) => div().text_color(theme.text).child(text.clone()).into_any_element(),
                CardLine::Notice(text) => div().text_size(px(11.5)).text_color(theme.text_muted).truncate().child(text.clone()).into_any_element(),
                CardLine::Tool { name, arg, state } => {
                    let (mark, mark_color) = match state {
                        ToolState::Running => (spinner, theme.accent),
                        ToolState::Ok => ("✓", theme.success),
                        ToolState::Failed => ("✕", theme.error),
                    };
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(8.))
                        .py(px(3.))
                        .rounded(px(6.))
                        .bg(theme.hover)
                        .font_family(MONO)
                        .text_size(px(12.))
                        .child(div().flex_none().text_color(mark_color).child(mark))
                        .child(div().flex_none().text_color(theme.text_secondary).child(name.clone()))
                        .child(div().flex_1().min_w_0().truncate().text_color(theme.text_muted).child(arg.clone()))
                        .into_any_element()
                }
            })
            .collect();
        let asks: Vec<AnyElement> = card
            .asks
            .iter()
            .map(|ask| {
                let run = match card.id {
                    CardId::Run(run) => run,
                    CardId::Session(_) => 0,
                };
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .p(px(8.))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(theme.warning_mark)
                    .bg(theme.warning_mark.opacity(0.08))
                    .child(div().text_color(theme.warning_mark).child(format!("⚠ {}", model::shorten(&ask.title, 160))))
                    .child(div().flex().flex_wrap().gap(px(6.)).children(ask.options.iter().enumerate().map(|(oi, (id, kind, text))| {
                        let primary = kind == "allow_once";
                        let reject = kind.starts_with("reject");
                        let (call, option) = (ask.call.clone(), id.clone());
                        div()
                            .id(SharedString::from(format!("card-{ix}-ask-{}-{oi}", ask.call)))
                            .flex_none()
                            .px(px(10.))
                            .py(px(3.))
                            .rounded(px(6.))
                            .text_size(px(12.))
                            .when(primary, |this| this.bg(theme.accent).text_color(theme.over_accent))
                            .when(!primary, |this| {
                                this.border_1()
                                    .border_color(theme.border)
                                    .text_color(if reject { theme.error } else { theme.text_secondary })
                                    .hover(|style| style.bg(theme.hover))
                            })
                            .child(super::live_view::option_label(kind, text, false))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                this.answer_from_card(run, &call, option.clone(), cx);
                            }))
                    })))
                    .into_any_element()
            })
            .collect();
        let question = card.question.then(|| {
            div().p(px(8.)).rounded(px(8.)).border_1().border_color(theme.warning_mark).text_color(theme.warning_mark).child(t("mission.question")).into_any_element()
        });
        let elsewhere = (card.lines.is_empty() && card.updated_ms.is_some()).then(|| {
            div().text_color(theme.text_muted).text_size(px(12.)).child(t("mission.elsewhere")).into_any_element()
        });
        let log = div()
            .id(("mission-log", ix))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(scroll)
            .px(px(12.))
            .py(px(8.))
            .flex()
            .flex_col()
            .gap(px(5.))
            .text_size(px(12.5))
            .line_height(px(17.))
            .children(elsewhere)
            .children(lines)
            .children(asks)
            .children(question);
        // The foot: context, activity, cost, time.
        let bars = self.mission.as_ref().and_then(|mission| mission.spark.get(&card.id)).map(|(_, bars)| bars.clone()).unwrap_or_default();
        let active = matches!(card.state, CardState::Working | CardState::Background);
        let busiest = bars.iter().copied().fold(400., f32::max);
        let spark = div().flex_1().min_w(px(30.)).h(px(20.)).flex().items_end().gap(px(1.)).children(bars.iter().map(move |grown| {
            let high = (grown / busiest * 20.).clamp(1., 20.);
            div().flex_1().h(px(high)).rounded(px(1.)).bg(if active { theme.accent.opacity(0.55) } else { theme.border })
        }));
        let context = card.context.map(|(used, window)| {
            let share = window.map(|window| used as f32 / window.max(1) as f32);
            div()
                .flex_none()
                .w(px(72.))
                .flex()
                .flex_col()
                .gap(px(3.))
                .child(format!("{} {}", t("mission.context"), compact(used)))
                .children(share.map(|share| {
                    let fill = if share > 0.85 { theme.error } else if share > 0.7 { theme.warning_mark } else { theme.accent };
                    div().h(px(4.)).w(px(72.)).rounded(px(2.)).bg(theme.hover).child(div().h_full().w(px(72. * share.clamp(0., 1.))).rounded(px(2.)).bg(fill))
                }))
        });
        let clock = match (card.started, card.updated_ms) {
            (Some(started), _) => {
                let seconds = started.elapsed().as_secs();
                format!("{}:{:02}", seconds / 60, seconds % 60)
            }
            (None, Some(updated)) => ago(updated),
            _ => String::new(),
        };
        let foot = div()
            .flex()
            .items_center()
            .gap(px(12.))
            .px(px(12.))
            .py(px(8.))
            .border_t_1()
            .border_color(theme.border)
            .text_size(px(11.))
            .text_color(theme.text_muted)
            .children(context)
            .child(spark)
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap(px(1.))
                    .child(div().text_color(theme.text).child(card.cost.map(|usd| format!("${usd:.3}")).unwrap_or_default()))
                    .child(format!("≈{} tok/s · {clock}", self.pace(&card.id).round() as u64)),
            );
        let border = match glow {
            Some(level) => theme.warning_mark.opacity(0.55 + 0.45 * level),
            None if card.state == CardState::Done => theme.success.opacity(0.35),
            None => theme.border,
        };
        let open = card.clone();
        div()
            .id(("mission-card", ix))
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .rounded(px(10.))
            .border_1()
            .border_color(border)
            .when(glow.is_some(), |this| this.border_2())
            .bg(theme.island_bg)
            .overflow_hidden()
            .cursor_pointer()
            .hover(|style| style.border_color(theme.text_muted))
            .child(head)
            .child(log)
            .child(foot)
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.open_card(&open, cx)))
            .into_any_element()
    }
}

/// `12.3k`, `1.2M`.
fn compact(tokens: u64) -> String {
    match tokens {
        t if t >= 1_000_000 => format!("{:.1}M", t as f64 / 1e6),
        t if t >= 1_000 => format!("{}k", t / 1000),
        t => t.to_string(),
    }
}

/// The last bytes of a file of this computer.
fn read_tail(path: &str, size: u64, span: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(size.saturating_sub(span)))?;
    let mut bytes = Vec::new();
    file.take(span).read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::compact;

    #[test]
    fn token_counts_read_short() {
        assert_eq!(compact(950), "950");
        assert_eq!(compact(42_500), "42k");
        assert_eq!(compact(1_250_000), "1.2M");
    }
}
