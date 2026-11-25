//! The cost of the open session, what the next turn and a compaction would cost, and
//! whether the prompt cache still holds the context.

use model::pricing::{self, CacheState};
use ui_kit::prelude::*;
use ui_kit::*;

use super::{MONO, Shell, now_ms, section_body};
use crate::i18n::t;

fn money(usd: f64) -> String {
    match usd {
        u if u >= 100.0 => format!("${u:.0}"),
        u if u >= 1.0 => format!("${u:.2}"),
        u if u >= 0.01 => format!("${u:.3}"),
        u => format!("${u:.4}"),
    }
}

fn duration(ms: i64) -> String {
    let minutes = ms / 60_000;
    match minutes {
        0 => format!("{}s", ms / 1000),
        m if m < 60 => format!("{m} min"),
        m if m < 60 * 48 => format!("{}h {}m", m / 60, m % 60),
        m => format!("{}d", m / (60 * 24)),
    }
}

impl Shell {
    pub(super) fn render_cost(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.global::<Theme>();
        let Some(open) = &self.open else { return div().into_any_element() };
        // A running turn's tokens count too, until the history read after it replaces them.
        let mut buckets = open.usage_by_model.clone();
        let mut reported = open.cost_usd;
        let mut last_call = open.last_call_ms;
        let mut context = open.breakdown.total;
        let live_turn = self.live().filter(|live| !live.finished).and_then(|live| live.turn.as_ref());
        if let Some(turn) = live_turn {
            for (bucket, usage) in &turn.usage_by_model {
                match buckets.iter_mut().find(|(name, _)| name == bucket) {
                    Some((_, sum)) => sum.add(usage),
                    None => buckets.push((bucket.clone(), *usage)),
                }
            }
            if let Some(cost) = turn.cost_usd {
                *reported.get_or_insert(0.0) += cost;
            }
            last_call = turn.last_call_ms.or(last_call);
            context = turn.last_usage.map_or(context, |u| u.context());
        }
        let (computed, complete) = pricing::session_cost(&buckets);
        // On a subscription the sums are what the tokens would cost at API prices, and the
        // harness's own sum is its estimate: neither is billed.
        let account = (open.summary.key.env.clone(), open.summary.key.harness.clone());
        let plan = matches!(self.plans.get(&account).and_then(|state| state.plan.as_ref()), Some(live::usage::Plan::Limits { .. }));
        let model = open.model.clone().or_else(|| open.summary.model.clone()).unwrap_or_default();
        let priced = pricing::price_of(&model).is_some();

        let headline = div()
            .flex()
            .items_end()
            .gap(px(8.))
            .child(div().text_size(px(22.)).font_weight(FontWeight::SEMIBOLD).child(if priced || computed > 0.0 {
                money(computed)
            } else {
                "—".into()
            }))
            .child(div().flex_1().min_w_0().pb(px(3.)).text_size(px(12.)).text_color(theme.text_muted).child(if priced || computed > 0.0 {
                format!("{}{}", t(if plan { "cost.computed_plan" } else { "cost.computed" }), if complete { "" } else { " *" })
            } else {
                t("price.unknown").to_owned()
            }));

        let row = |label: &str, value: String, color: Option<Hsla>| {
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .text_size(px(12.))
                .child(div().text_color(theme.text_secondary).child(label.to_owned()))
                .child(
                    div()
                        .font_family(MONO)
                        .text_size(px(11.5))
                        .when_some(color, |this, color| this.text_color(color))
                        .child(value),
                )
        };

        let mut rows: Vec<AnyElement> = Vec::new();
        if let Some(reported) = reported {
            let note = if reported == 0.0 { t("cost.subscription").to_owned() } else { money(reported) };
            rows.push(row(t(if plan { "cost.reported_plan" } else { "cost.reported" }), note, None).into_any_element());
        }
        let total_usage = {
            let mut sum = model::Usage::default();
            for (_, usage) in &buckets {
                sum.add(usage);
            }
            sum
        };
        let long_cache = total_usage.cache_write_long > 0;
        let state = last_call.and_then(|last| pricing::cache_state(&model, &total_usage, last, now_ms()));
        let (cache_text, cache_color, warm) = match state {
            Some(CacheState::Warm { left_ms, ttl_ms }) => (
                format!("● {} · {} {} ({})", t("cache.warm"), duration(left_ms), t("cache.left"), duration(ttl_ms)),
                Some(theme.success),
                Some(true),
            ),
            Some(CacheState::Cold { since_ms }) => (
                format!("○ {} · {} {}", t("cache.cold"), t("cache.expired"), duration(since_ms)),
                Some(theme.text_muted),
                Some(false),
            ),
            Some(CacheState::Automatic { idle_ms }) => (format!("◐ {} · {}", t("cache.auto"), duration(idle_ms)), None, None),
            None => (t("cache.unknown").to_owned(), Some(theme.text_muted), None),
        };
        rows.push(row(t("cache"), cache_text, cache_color).into_any_element());

        let both = |f: &dyn Fn(bool) -> Option<f64>| -> Option<String> {
            match warm {
                Some(true) => Some(format!("{} ({})", money(f(true)?), t("cache.warm"))),
                Some(false) => Some(format!("{} ({})", money(f(false)?), t("cache.cold"))),
                None => Some(format!("{} … {}", money(f(true)?), money(f(false)?))),
            }
        };
        if context > 0 {
            if let Some(text) = both(&|w| pricing::context_cost(&model, context, w, long_cache)) {
                rows.push(row(t("cost.next_turn"), text, None).into_any_element());
            }
            if let Some(text) = both(&|w| pricing::compact_cost(&model, context, w, long_cache)) {
                rows.push(row(t("cost.compact"), format!("≈ {text}"), None).into_any_element());
            }
        }
        if state.is_some_and(|s| matches!(s, CacheState::Warm { .. })) {
            // The timer moves; look again in a while.
            self.repaint_after(std::time::Duration::from_secs(20), cx);
        }

        let body = section_body().child(headline).child(div().flex().flex_col().gap(px(6.)).children(rows));
        self.fold("cost", t("cost.session"), None, body, cx)
    }
}
