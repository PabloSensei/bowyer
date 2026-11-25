//! The plan of the open session's account, in the right pane: the five-hour and weekly
//! windows of a subscription, or what an API account spent this month, summed from the
//! history of this machine (or host) at API prices.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use live::usage::{Plan, WindowKind};
use model::{EnvId, HarnessKind, SessionKey, pricing};
use ui_kit::prelude::*;
use ui_kit::*;

use super::{Shell, now_ms};
use crate::i18n::t;

/// How old a plan or a month's sum may get before the window looks again.
const STALE_MS: i64 = 5 * 60_000;
/// How soon after the last look a finished turn or a click may look again.
const AGAIN_MS: i64 = 20_000;
/// How often the login of Claude Code on each machine with a subscription's limits is asked,
/// open session or not; one without them is asked every [`STALE_MS`].
const LOGIN_STALE_MS: i64 = 30 * 60_000;

/// The plan of one account: the harness on one machine.
#[derive(Default)]
pub(super) struct PlanState {
    pub(super) at_ms: i64,
    pub(super) plan: Option<Plan>,
    /// Who Claude Code runs as there, from the same answer.
    pub(super) account: Option<live::usage::Account>,
    pub(super) busy: bool,
}

/// This month's spending of one harness on one machine.
#[derive(Default)]
pub(super) struct MonthState {
    pub(super) at_ms: i64,
    pub(super) busy: bool,
    pub(super) sum: Option<MonthSum>,
}

#[derive(Clone, Copy)]
pub(super) struct MonthSum {
    pub(super) usd: f64,
    pub(super) complete: bool,
    pub(super) sessions: usize,
}

/// The tokens of this month per session, with the stamp of the history they came from:
/// another look reads only the sessions that changed.
pub(super) type MonthCache = Arc<Mutex<HashMap<SessionKey, (i64, i64, model::SpentSince)>>>;

type Account = (EnvId, HarnessKind);

impl Shell {
    fn open_account(&self) -> Option<Account> {
        let key = &self.open.as_ref()?.summary.key;
        Some((key.env.clone(), key.harness.clone()))
    }

    /// Looks at the open session's plan and month when they are old; `soon` (a finished
    /// turn, a click) looks sooner.
    pub(super) fn look_at_plan(&mut self, soon: bool, cx: &mut Context<Self>) {
        if self.right_hidden || self.right_tab != super::files_view::RightTab::Session {
            return;
        }
        let Some(account) = self.open_account() else { return };
        let wait = if soon { AGAIN_MS } else { STALE_MS };
        let now = now_ms();
        if account.1 == HarnessKind::Claude {
            let state = self.plans.entry(account.clone()).or_default();
            if !state.busy && now - state.at_ms >= wait {
                state.busy = true;
                self.probe_plan(account.clone(), cx);
            }
        }
        let api = match account.1 {
            HarnessKind::DeepSeek => true,
            HarnessKind::Claude => matches!(self.plans.get(&account).and_then(|state| state.plan.as_ref()), Some(Plan::Api)),
            _ => false,
        };
        if api {
            let state = self.months.entry(account.clone()).or_default();
            if !state.busy && now - state.at_ms >= wait {
                state.busy = true;
                self.count_month(account, cx);
            }
        }
    }

    /// Asks Claude Code on every machine how it is signed in, every half hour: the tree
    /// marks the ones that pay by API key, and the hosts' settings say who each runs as.
    pub(super) fn look_at_logins(&mut self, cx: &mut Context<Self>) {
        let now = now_ms();
        let envs: Vec<EnvId> = self.envs.iter().map(|env| env.id()).collect();
        for env in envs {
            let account = (env, HarnessKind::Claude);
            let state = self.plans.entry(account.clone()).or_default();
            // A machine without a subscription's limits is asked more often: a new login there
            // shows soon.
            let wait = if matches!(state.plan, Some(Plan::Limits { .. })) { LOGIN_STALE_MS } else { STALE_MS };
            if !state.busy && now - state.at_ms >= wait {
                state.busy = true;
                self.probe_plan(account, cx);
            }
        }
    }

    /// Whether Claude Code on this machine is known to pay by API key.
    pub(super) fn pays_by_key(&self, env: &EnvId) -> bool {
        matches!(self.plans.get(&(env.clone(), HarnessKind::Claude)).and_then(|state| state.plan.as_ref()), Some(Plan::Api))
    }

    /// Who Claude Code runs as on this machine, in one line: the way it is signed in and the
    /// account. `None` before the first answer.
    pub(super) fn login_line(&self, env: &EnvId) -> Option<String> {
        let state = self.plans.get(&(env.clone(), HarnessKind::Claude))?;
        let mut parts: Vec<String> = Vec::new();
        if let Some(account) = &state.account {
            parts.extend(account.method.clone());
            parts.extend(account.email.clone().or_else(|| account.organization.clone()));
        }
        if parts.is_empty() {
            match state.plan.as_ref()? {
                Plan::Limits { name, .. } => parts.push(t("plan.subscription").replace("{name}", &capitalized(name.as_deref().unwrap_or("")))),
                Plan::Api => parts.push(t("plan.api").to_owned()),
                Plan::Unknown(error) => parts.push(error.clone()),
            }
        }
        Some(parts.join(" · "))
    }

    fn probe_plan(&mut self, account: Account, cx: &mut Context<Self>) {
        let Some(env) = self.env_for(&account.0) else {
            self.plans.remove(&account);
            return;
        };
        let local = account.0.is_local();
        // An empty folder of the user's own: no project settings, no hooks of a project. A
        // host's is made by the command itself.
        let own = self.cache_dir.join("usage");
        let task = cx.background_spawn(async move {
            let folder = local.then(|| {
                let _ = std::fs::create_dir_all(&own);
                own.to_string_lossy().into_owned()
            });
            let request = live::usage::claude_request();
            let (program, args) = live::usage::claude_command(local);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            match env.run_with_input(&program, &args, folder.as_deref(), request.as_bytes()) {
                Ok(output) => (live::usage::parse_claude(&output), live::usage::parse_account(&output)),
                Err(error) => (Plan::Unknown(model::shorten(&error.to_string(), 200)), None),
            }
        });
        cx.spawn(async move |this, cx| {
            let (plan, login) = task.await;
            this.update(cx, |this, cx| {
                let api = plan == Plan::Api;
                this.keep_limits(&account, &plan);
                let state = this.plans.entry(account).or_default();
                state.busy = false;
                state.at_ms = now_ms();
                state.plan = Some(plan);
                if login.is_some() {
                    state.account = login;
                }
                if api {
                    this.look_at_plan(false, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn count_month(&mut self, account: Account, cx: &mut Context<Self>) {
        let Some(env) = self.env_for(&account.0) else {
            self.months.remove(&account);
            return;
        };
        let start = model::month_start_utc_ms(now_ms());
        let sessions: Vec<_> = self
            .summaries
            .iter()
            .filter(|summary| summary.key.env == account.0 && summary.key.harness == account.1 && summary.updated_ms >= start)
            .cloned()
            .collect();
        let cache = self.month_cache.clone();
        let remote = self.cache_dir.join("remote");
        let harness = account.1.clone();
        let task = cx.background_spawn(async move {
            let source = history::source_for(&harness)?;
            // A session resumed into a new file repeats the calls before it: each counts once.
            let mut sum = model::UsageSum::default();
            let mut counted = 0;
            for summary in &sessions {
                let stamp = summary.updated_ms;
                let known = cache.lock().ok().and_then(|cache| {
                    cache.get(&summary.key).filter(|(month, at, _)| *month == start && *at == stamp).map(|(_, _, usage)| usage.clone())
                });
                let usage = match known {
                    Some(usage) => usage,
                    None => {
                        let Ok(transcript) = source.load(&*env, summary, &remote) else { continue };
                        let usage = transcript.spent_since(start);
                        if let Ok(mut cache) = cache.lock() {
                            cache.insert(summary.key.clone(), (start, stamp, usage.clone()));
                        }
                        usage
                    }
                };
                if !usage.is_empty() {
                    counted += 1;
                }
                sum.add_session(&usage);
            }
            let (usd, complete) = pricing::session_cost(&sum.buckets);
            Some(MonthSum { usd, complete, sessions: counted })
        });
        cx.spawn(async move |this, cx| {
            let sum = task.await;
            this.update(cx, |this, cx| {
                let state = this.months.entry(account).or_default();
                state.busy = false;
                state.at_ms = now_ms();
                state.sum = sum.or(state.sum);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The plan section of the right pane, for the harnesses that have one.
    pub(super) fn render_plan(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = *cx.global::<Theme>();
        let account = self.open_account()?;
        let plan = self.plans.get(&account);
        let month = self.months.get(&account);
        let body: AnyElement = match account.1 {
            HarnessKind::Claude => match plan.and_then(|state| state.plan.as_ref()) {
                None => muted(t("loading"), &theme),
                Some(Plan::Unknown(error)) => div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(muted(t("plan.unknown"), &theme))
                    .child(div().text_size(px(11.)).text_color(theme.text_muted).child(error.clone()))
                    .into_any_element(),
                Some(Plan::Api) => month_view(month, &theme),
                Some(Plan::Limits { name, windows, extra }) => {
                    let now = now_ms();
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(10.))
                        .children(name.as_ref().map(|name| muted(t("plan.subscription").replace("{name}", &capitalized(name)), &theme)))
                        .children(windows.iter().map(|window| {
                            let label = match (window.kind, &window.model) {
                                (WindowKind::FiveHours, _) => t("plan.five_hours").to_owned(),
                                (WindowKind::Week, None) => t("plan.week").to_owned(),
                                (WindowKind::Week, Some(model)) => format!("{} · {model}", t("plan.week")),
                            };
                            let resets = window.resets_ms.filter(|at| *at > now).map(|at| t("plan.resets").replace("{t}", &until(at - now)));
                            meter(label, window.percent, resets, &theme)
                        }))
                        .children(extra.as_ref().map(|spend| {
                            let used = money(spend.used, &spend.currency);
                            let text = match spend.limit {
                                Some(limit) => format!("{used} / {}", money(limit, &spend.currency)),
                                None => used,
                            };
                            div()
                                .flex()
                                .justify_between()
                                .text_size(px(12.))
                                .child(div().text_color(theme.text_muted).child(t("plan.extra")))
                                .child(text)
                        }))
                        .into_any_element()
                }
            },
            HarnessKind::DeepSeek => month_view(month, &theme),
            _ => return None,
        };
        let history = div()
            .id("plan-history")
            .px(px(5.))
            .rounded(px(4.))
            .text_size(px(11.5))
            .text_color(theme.text_muted)
            .hover(|style| style.bg(theme.hover).text_color(theme.text))
            .child(t("spend.open"))
            .tooltip(|_, cx| tooltip(t("spend.open_tip"), None, cx))
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.open_spend(cx)));
        let refresh = div()
            .id("plan-refresh")
            .px(px(4.))
            .rounded(px(4.))
            .text_size(px(12.))
            .text_color(theme.text_muted)
            .hover(|style| style.bg(theme.hover).text_color(theme.text))
            .child("↻")
            .tooltip(|_, cx| tooltip(t("plan.refresh"), None, cx))
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.look_at_plan(true, cx)));
        // Who Claude Code runs as on this machine, under the plan.
        let login = (account.1 == HarnessKind::Claude)
            .then(|| self.plans.get(&account).and_then(|state| state.account.as_ref()))
            .flatten()
            .map(|login| [login.method.clone(), login.email.clone().or_else(|| login.organization.clone())].into_iter().flatten().collect::<Vec<_>>().join(" · "))
            .filter(|line| !line.is_empty());
        let body = match login {
            Some(line) => div().flex().flex_col().gap(px(6.)).child(body).child(div().truncate().text_size(px(11.5)).text_color(theme.text_muted).child(line)).into_any_element(),
            None => body,
        };
        let actions = div().flex().items_center().gap(px(4.)).child(history).child(refresh);
        Some(self.fold("plan", t("plan.title"), Some(actions.into_any_element()), body, cx))
    }
}

fn muted(text: impl Into<SharedString>, theme: &Theme) -> AnyElement {
    div().text_size(px(12.)).text_color(theme.text_muted).child(text.into()).into_any_element()
}

/// The colour of a share used: calm, then a warning from 70%, an error from 90%.
pub(super) fn fill_color(percent: f64, theme: &Theme) -> Hsla {
    match percent {
        p if p >= 90.0 => theme.error,
        p if p >= 70.0 => theme.warning_mark,
        _ => theme.accent,
    }
}

/// A window of the plan: its name, the share used and when it starts again.
fn meter(label: String, percent: f64, resets: Option<String>, theme: &Theme) -> impl IntoElement {
    let color = fill_color(percent, theme);
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .flex()
                .justify_between()
                .text_size(px(12.))
                .child(label)
                .child(div().font_weight(FontWeight::SEMIBOLD).child(format!("{:.0}%", percent))),
        )
        .child(
            div()
                .h(px(6.))
                .w_full()
                .rounded(px(3.))
                .overflow_hidden()
                .bg(theme.hover)
                .child(div().h_full().w(relative((percent / 100.0).clamp(0.0, 1.0) as f32)).bg(color)),
        )
        .children(resets.map(|text| div().text_size(px(11.)).text_color(theme.text_muted).child(text)))
}

fn month_view(month: Option<&MonthState>, theme: &Theme) -> AnyElement {
    match month.and_then(|state| state.sum) {
        None => muted(t("plan.counting"), theme),
        Some(sum) => {
            let mut note = t("plan.month_note").replace("{n}", &sum.sessions.to_string());
            if !sum.complete {
                note = format!("{note} · {}", t("plan.partial"));
            }
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(muted(t("plan.api"), theme))
                .child(
                    div()
                        .flex()
                        .items_end()
                        .gap(px(8.))
                        .child(div().text_size(px(22.)).font_weight(FontWeight::SEMIBOLD).child(money(sum.usd, "USD")))
                        .child(div().pb(px(3.)).text_size(px(12.)).text_color(theme.text_muted).child(t("plan.month"))),
                )
                .child(div().text_size(px(11.)).text_color(theme.text_muted).child(note))
                .into_any_element()
        }
    }
}

fn money(amount: f64, currency: &str) -> String {
    match currency {
        "USD" => format!("${amount:.2}"),
        "EUR" => format!("€{amount:.2}"),
        other => format!("{amount:.2} {other}"),
    }
}

fn capitalized(name: &str) -> String {
    let mut chars = name.chars();
    chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// "45 min", "2 h 10 min", "3 d 4 h".
fn until(ms: i64) -> String {
    let minutes = (ms + 59_999) / 60_000;
    match minutes {
        m if m < 60 => format!("{m} {}", t("unit.min")),
        m if m < 60 * 24 => format!("{} {} {} {}", m / 60, t("unit.h"), m % 60, t("unit.min")),
        m => format!("{} {} {} {}", m / (60 * 24), t("unit.d"), m % (60 * 24) / 60, t("unit.h")),
    }
}
