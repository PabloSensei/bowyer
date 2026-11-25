//! What an account spent over the last weeks, in the centre: each day and week at API
//! prices, the projects and sessions that spent the most, and how full the windows of a
//! subscription's limits were each hour, from what the window saw while it ran.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use live::usage::{Plan, WindowKind};
use model::{EnvId, HarnessKind, SessionKey, SpendReport};
use ui_kit::prelude::*;
use ui_kit::*;

use super::{Shell, now_ms};
use crate::i18n::t;

/// Days the history covers.
const DAYS: i64 = 30;
/// Hours the chart of the limits covers.
const LIMIT_HOURS: i64 = 7 * 24;
const DAY_MS: i64 = 86_400_000;
const HOUR_MS: i64 = 3_600_000;
/// Heights of the charts.
const DAY_CHART: f32 = 110.;
const LIMIT_CHART: f32 = 44.;

/// What each session spent per day, with the stamp of the history it came from: another
/// look reads only the sessions that changed.
pub(super) type SpendCache = Arc<Mutex<HashMap<SessionKey, (i64, Vec<(i64, model::SpentSince)>)>>>;

pub(super) struct SpendView {
    account: (EnvId, HarnessKind),
    report: Option<SpendReport>,
    /// `(at, kind, percent)` of the last week, oldest first.
    samples: Vec<(i64, String, f64)>,
}

/// The key of an account in the library's samples of limits.
pub(super) fn account_key(env: &EnvId, harness: &HarnessKind) -> String {
    format!("{}|{}", env.0, harness.id())
}

/// The name a window of the limits is kept under.
pub(super) fn window_kind(kind: WindowKind, model: Option<&str>) -> String {
    match (kind, model) {
        (WindowKind::FiveHours, _) => "five_hours".to_owned(),
        (WindowKind::Week, None) => "week".to_owned(),
        (WindowKind::Week, Some(model)) => format!("week:{model}"),
    }
}

fn money(usd: f64) -> String {
    match usd {
        u if u >= 100.0 => format!("${u:.0}"),
        u if u >= 0.995 => format!("${u:.2}"),
        u if u >= 0.01 => format!("${u:.3}"),
        u if u > 0.0 => format!("${u:.4}"),
        _ => "$0".to_owned(),
    }
}

/// `2026-11-25` of a UTC day number.
fn date(day: i64) -> String {
    model::format_ms_utc(day * DAY_MS).split(' ').next().unwrap_or_default().to_owned()
}

/// The highest share of each hour of the last week: `None` for an hour nobody looked.
fn hourly(samples: &[(i64, String, f64)], kind: &str, now: i64) -> Vec<Option<f64>> {
    let first = now.div_euclid(HOUR_MS) - LIMIT_HOURS + 1;
    let mut hours = vec![None; LIMIT_HOURS as usize];
    for (at, _, percent) in samples.iter().filter(|(_, sample_kind, _)| sample_kind == kind) {
        let hour = at.div_euclid(HOUR_MS) - first;
        if let Some(slot) = usize::try_from(hour).ok().and_then(|hour| hours.get_mut(hour)) {
            *slot = Some(slot.map_or(*percent, |known: f64| known.max(*percent)));
        }
    }
    hours
}

impl Shell {
    pub(super) fn open_spend(&mut self, cx: &mut Context<Self>) {
        let Some(open) = &self.open else { return };
        let account = (open.summary.key.env.clone(), open.summary.key.harness.clone());
        self.commit_view = None;
        self.diff_view = None;
        self.preview = None;
        self.turn_view = None;
        let samples = self.lib().limit_samples(&account_key(&account.0, &account.1), now_ms() - LIMIT_HOURS * HOUR_MS).unwrap_or_default();
        self.mission = None;
        self.spend_view = Some(SpendView { account: account.clone(), report: None, samples });
        self.count_spend(account, cx);
        cx.notify();
    }

    fn count_spend(&mut self, account: (EnvId, HarnessKind), cx: &mut Context<Self>) {
        let Some(env) = self.env_for(&account.0) else { return };
        let today = now_ms().div_euclid(DAY_MS);
        let first = today - DAYS + 1;
        let since = first * DAY_MS;
        let sessions: Vec<_> = self
            .summaries
            .iter()
            .filter(|summary| summary.key.env == account.0 && summary.key.harness == account.1 && summary.updated_ms >= since)
            .cloned()
            .collect();
        let titles: HashMap<SessionKey, String> = sessions
            .iter()
            .map(|summary| {
                let title = self.placements.get(&summary.key).and_then(|placement| placement.title.clone()).unwrap_or_else(|| summary.display_title());
                (summary.key.clone(), title)
            })
            .collect();
        let cache = self.spend_cache.clone();
        let remote = self.cache_dir.join("remote");
        let harness = account.1.clone();
        let task = cx.background_spawn(async move {
            let source = history::source_for(&harness)?;
            let mut inputs = Vec::new();
            for summary in &sessions {
                let stamp = summary.updated_ms;
                let known = cache.lock().ok().and_then(|cache| cache.get(&summary.key).filter(|(at, _)| *at == stamp).map(|(_, days)| days.clone()));
                let days = match known {
                    Some(days) => days,
                    None => {
                        let Ok(transcript) = source.load(&*env, summary, &remote) else { continue };
                        let days = transcript.spent_by_day(since);
                        if let Ok(mut cache) = cache.lock() {
                            cache.insert(summary.key.clone(), (stamp, days.clone()));
                        }
                        days
                    }
                };
                inputs.push(model::SpendInput {
                    key: summary.key.clone(),
                    title: titles.get(&summary.key).cloned().unwrap_or_default(),
                    project: summary.cwd.clone().unwrap_or_default(),
                    days,
                });
            }
            Some(model::spend_report(&inputs, first, today))
        });
        cx.spawn(async move |this, cx| {
            let report = task.await;
            this.update(cx, |this, cx| {
                if let Some(view) = &mut this.spend_view
                    && view.account == account
                {
                    view.report = Some(report.unwrap_or_default());
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Keeps how full the windows of an account's plan are now, for the history.
    pub(super) fn keep_limits(&self, account: &(EnvId, HarnessKind), plan: &Plan) {
        let Plan::Limits { windows, .. } = plan else { return };
        let samples: Vec<(String, f64)> = windows.iter().map(|window| (window_kind(window.kind, window.model.as_deref()), window.percent)).collect();
        if !samples.is_empty() {
            let _ = self.lib().add_limit_samples(&account_key(&account.0, &account.1), now_ms(), &samples);
        }
    }

    pub(super) fn render_spend_view(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = *cx.global::<Theme>();
        let view = self.spend_view.as_ref()?;
        let machine = view.account.0.ssh_host().map(str::to_owned).unwrap_or_else(|| t("local").to_owned());
        let subscription = matches!(self.plans.get(&view.account).and_then(|state| state.plan.as_ref()), Some(Plan::Limits { .. }));
        let header = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(48.))
            .px(px(16.))
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("{} · {} · {machine}", t("spend.title"), view.account.1.label())),
            )
            .child(
                div()
                    .id("close-spend")
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
                        this.spend_view = None;
                        cx.notify();
                    })),
            );
        let heading = |text: &str| div().pt(px(6.)).text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text_secondary).child(text.to_owned());
        let muted = |text: String| div().text_size(px(12.)).text_color(theme.text_muted).child(text);
        let mut parts: Vec<AnyElement> = Vec::new();
        match &view.report {
            None => parts.push(muted(t("spend.counting").to_owned()).into_any_element()),
            Some(report) => {
                let mut note = if subscription { t("spend.note_subscription").to_owned() } else { t("spend.note_api").to_owned() };
                if !report.complete {
                    note = format!("{note} · {}", t("plan.partial"));
                }
                parts.push(
                    div()
                        .flex()
                        .items_end()
                        .gap(px(8.))
                        .child(div().text_size(px(24.)).font_weight(FontWeight::SEMIBOLD).child(money(report.total)))
                        .child(div().pb(px(4.)).text_size(px(12.)).text_color(theme.text_muted).child(t("spend.days").replace("{n}", &DAYS.to_string())))
                        .into_any_element(),
                );
                parts.push(muted(note).into_any_element());
                // A bar per day, as high as its share of the most spent in a day.
                let most = report.days.iter().map(|(_, usd)| *usd).fold(0.0, f64::max);
                let bars = report.days.iter().map(|(day, usd)| {
                    let share = if most > 0.0 { (usd / most) as f32 } else { 0.0 };
                    let tip = format!("{} · {}", date(*day), money(*usd));
                    let high = if *usd > 0.0 { (share * DAY_CHART).max(2.) } else { 1. };
                    div()
                        .id(SharedString::from(format!("spend-day-{day}")))
                        .flex_1()
                        .h(px(DAY_CHART))
                        .flex()
                        .flex_col()
                        .justify_end()
                        .tooltip(move |_, cx| tooltip(&tip, None, cx))
                        .child(div().w_full().h(px(high)).rounded_t(px(2.)).bg(if *usd > 0.0 { theme.accent } else { theme.hover }))
                });
                parts.push(heading(t("spend.by_day")).into_any_element());
                parts.push(div().flex().items_end().gap(px(2.)).h(px(DAY_CHART)).children(bars).into_any_element());
                if let (Some((first, _)), Some((last, _))) = (report.days.first(), report.days.last()) {
                    parts.push(
                        div().flex().justify_between().text_size(px(11.)).text_color(theme.text_muted).child(date(*first)).child(date(*last)).into_any_element(),
                    );
                }
                // Weeks from Monday: day 0 of the epoch was a Thursday.
                let mut weeks: Vec<(i64, f64)> = Vec::new();
                for (day, usd) in &report.days {
                    let monday = day - (day + 3).rem_euclid(7);
                    match weeks.last_mut() {
                        Some((week, sum)) if *week == monday => *sum += usd,
                        _ => weeks.push((monday, *usd)),
                    }
                }
                parts.push(heading(t("spend.by_week")).into_any_element());
                for (monday, usd) in weeks.iter().rev() {
                    parts.push(row(t("spend.week_of").replace("{d}", &date(*monday)), money(*usd), None, &theme).into_any_element());
                }
                if !report.projects.is_empty() {
                    parts.push(heading(t("spend.by_project")).into_any_element());
                    let top = report.projects.first().map_or(0.0, |(_, usd)| *usd);
                    for (project, usd) in report.projects.iter().take(8) {
                        let name = project.rsplit(['/', '\\']).find(|part| !part.is_empty()).unwrap_or(project).to_owned();
                        let name = if name.is_empty() { t("spend.no_folder").to_owned() } else { name };
                        parts.push(row(name, money(*usd), Some((usd / top.max(1e-9)) as f32), &theme).into_any_element());
                    }
                }
                if !report.sessions.is_empty() {
                    parts.push(heading(t("spend.by_session")).into_any_element());
                    for (ix, (key, title, usd)) in report.sessions.iter().take(10).enumerate() {
                        let key = key.clone();
                        parts.push(
                            div()
                                .id(("spend-session", ix))
                                .rounded(px(5.))
                                .hover(|style| style.bg(theme.hover))
                                .child(row(title.clone(), money(*usd), None, &theme))
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.spend_view = None;
                                    this.open_session(key.clone(), cx);
                                }))
                                .into_any_element(),
                        );
                    }
                }
            }
        }
        // The limits of a subscription, each hour of the last week.
        let mut kinds: Vec<String> = view.samples.iter().map(|(_, kind, _)| kind.clone()).collect();
        kinds.sort();
        kinds.dedup();
        if subscription || !kinds.is_empty() {
            parts.push(heading(t("spend.limits")).into_any_element());
            if kinds.is_empty() {
                parts.push(muted(t("spend.limits_none").to_owned()).into_any_element());
            }
            let now = now_ms();
            for kind in kinds {
                let label = match kind.as_str() {
                    "five_hours" => t("plan.five_hours").to_owned(),
                    "week" => t("plan.week").to_owned(),
                    other => format!("{} · {}", t("plan.week"), other.trim_start_matches("week:")),
                };
                let hours = hourly(&view.samples, &kind, now);
                let bars = hours.into_iter().map(|share| {
                    let share = share.unwrap_or(0.0);
                    let high = (share / 100.0).clamp(0.0, 1.0) as f32 * LIMIT_CHART;
                    div()
                        .flex_1()
                        .h(px(LIMIT_CHART))
                        .flex()
                        .flex_col()
                        .justify_end()
                        .child(div().w_full().h(px(high)).bg(super::plan_view::fill_color(share, &theme)))
                });
                parts.push(div().text_size(px(11.5)).text_color(theme.text_muted).child(label).into_any_element());
                parts.push(div().flex().items_end().h(px(LIMIT_CHART)).bg(theme.hover).rounded(px(3.)).children(bars).into_any_element());
            }
            parts.push(muted(t("spend.limits_note").to_owned()).into_any_element());
        }
        let body = div()
            .id("spend-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p(px(16.))
            .flex()
            .flex_col()
            .gap(px(8.))
            .children(parts);
        Some(div().flex_1().min_h_0().flex().flex_col().child(header).child(body).into_any_element())
    }
}

/// A line of a list: a name, a sum, and a bar of its share under it.
fn row(name: String, amount: String, share: Option<f32>, theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(3.))
        .px(px(4.))
        .py(px(3.))
        .child(
            div()
                .flex()
                .gap(px(10.))
                .text_size(px(12.5))
                .child(div().flex_1().min_w_0().truncate().child(name))
                .child(div().flex_none().font_family(super::MONO).child(amount)),
        )
        .children(share.map(|share| div().h(px(3.)).w_full().rounded(px(2.)).bg(theme.hover).child(div().h_full().w(relative(share.clamp(0.0, 1.0))).rounded(px(2.)).bg(theme.accent))))
}

#[cfg(test)]
mod tests {
    use super::{HOUR_MS, LIMIT_HOURS, WindowKind, date, hourly, window_kind};

    #[test]
    fn each_hour_keeps_its_highest_share() {
        let now = 1_000 * HOUR_MS + 10;
        let samples = vec![
            (now - 2 * HOUR_MS, "five_hours".to_owned(), 30.0),
            (now - 2 * HOUR_MS + 60_000, "five_hours".to_owned(), 45.0),
            (now, "five_hours".to_owned(), 5.0),
            (now, "week".to_owned(), 90.0),
            (now - 8 * 24 * HOUR_MS, "five_hours".to_owned(), 99.0),
        ];
        let hours = hourly(&samples, "five_hours", now);
        assert_eq!(hours.len(), LIMIT_HOURS as usize);
        assert_eq!(hours[hours.len() - 1], Some(5.0));
        assert_eq!(hours[hours.len() - 3], Some(45.0));
        assert_eq!(hours.iter().flatten().count(), 2, "an hour nobody looked is empty, an old sample is out");
    }

    #[test]
    fn a_window_is_kept_by_its_kind() {
        assert_eq!(window_kind(WindowKind::FiveHours, None), "five_hours");
        assert_eq!(window_kind(WindowKind::Week, Some("opus")), "week:opus");
        assert_eq!(date(20_417), "2025-11-25");
    }
}
