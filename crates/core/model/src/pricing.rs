//! API prices and the prompt cache. Prices are USD per million tokens from the public price
//! pages of 2026-11-20 (Anthropic, DeepSeek, OpenAI). A subscription pays differently; the
//! window labels these numbers as API prices.
//!
//! Some prices depend on the call, not only the model: Haiku 5.5 costs more for prompts over
//! 100K tokens, DeepSeek costs twice as much in its peak hours. [`bucket`] names the price a
//! call falls under, and the transcript sums tokens per bucket.

use crate::Usage;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_write_long: f64,
    pub cache_read: f64,
}

impl Price {
    const fn new(input: f64, output: f64, cache_write: f64, cache_write_long: f64, cache_read: f64) -> Self {
        Self { input, output, cache_write, cache_write_long, cache_read }
    }

    /// The cost of these tokens in USD.
    pub fn cost(&self, usage: &Usage) -> f64 {
        let short_writes = usage.cache_write.saturating_sub(usage.cache_write_long);
        (usage.input as f64 * self.input
            + usage.output as f64 * self.output
            + usage.cache_read as f64 * self.cache_read
            + short_writes as f64 * self.cache_write
            + usage.cache_write_long as f64 * self.cache_write_long)
            / 1e6
    }
}

/// How long a provider keeps a prompt in its cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheKind {
    /// Anthropic: 5 minutes or 1 hour after the last use, as the call asked.
    Explicit,
    /// DeepSeek: on by default, cleared "within a few hours to a few days".
    Automatic,
    /// OpenAI: automatic, kept while the prefix is in use.
    AutomaticShort,
}

const LONG_PROMPT: &str = "@long";
const PEAK: &str = "@peak";

/// The version of a Claude model from its name, as (major, minor): `claude-opus-5-5` is
/// (5, 5), `claude-sonnet-4-5-20250929` (4, 5), `claude-sonnet-4-20250514` (4, 0), the
/// older `claude-3-5-sonnet` (3, 5). A date is not a version; a bucket's mark and a `[1m]`
/// are left out.
pub fn claude_version(model: &str) -> Option<(u32, u32)> {
    let model = model.to_ascii_lowercase();
    if !model.starts_with("claude") {
        return None;
    }
    let name = model.split(['@', '[']).next().unwrap_or_default();
    let number = |part: &str| (!part.is_empty() && part.len() <= 2 && part.bytes().all(|b| b.is_ascii_digit())).then(|| part.parse::<u32>().ok()).flatten();
    let parts: Vec<&str> = name.split('-').collect();
    let at = parts.iter().position(|part| number(part).is_some())?;
    let major = number(parts[at])?;
    let minor = parts.get(at + 1).and_then(|part| number(part)).unwrap_or(0);
    Some((major, minor))
}
fn family(model: &str) -> Option<(Price, CacheKind)> {
    let m = model.to_ascii_lowercase();
    let m = m.as_str();
    let claude = |price| Some((price, CacheKind::Explicit));
    // The models before 5 had prices of their own.
    if let Some((major, minor)) = claude_version(m).filter(|(major, _)| *major < 5) {
        let price = if m.contains("opus") {
            if (major, minor) >= (4, 5) { Price::new(5.0, 25.0, 6.25, 10.0, 0.50) } else { Price::new(15.0, 75.0, 18.75, 30.0, 1.50) }
        } else if m.contains("sonnet") {
            Price::new(3.0, 15.0, 3.75, 6.0, 0.30)
        } else if m.contains("haiku") {
            match (major, minor) {
                (4, _) => Price::new(1.0, 5.0, 1.25, 2.0, 0.10),
                (3, 5) => Price::new(0.80, 4.0, 1.0, 1.6, 0.08),
                _ => Price::new(0.25, 1.25, 0.30, 0.50, 0.03),
            }
        } else {
            return None;
        };
        return claude(price);
    }
    if m.contains("fable") {
        return claude(Price::new(8.0, 40.0, 10.0, 16.0, 0.20));
    }
    if m.contains("opus") {
        return claude(Price::new(4.0, 20.0, 5.0, 8.0, 0.20));
    }
    if m.contains("sonnet") {
        return claude(Price::new(2.0, 10.0, 2.5, 4.0, 0.10));
    }
    if m.contains("haiku") {
        return claude(if m.ends_with(LONG_PROMPT) {
            Price::new(0.50, 2.50, 0.625, 1.0, 0.05)
        } else {
            Price::new(0.10, 0.50, 0.125, 0.20, 0.01)
        });
    }
    if m.contains("deepseek") {
        let peak = if m.ends_with(PEAK) { 2.0 } else { 1.0 };
        let base = if m.contains("pro") {
            Price::new(0.66, 1.98, 0.66, 0.66, 0.022)
        } else {
            // deepseek-flash, also served as deepseek-v4.1-flash and deepseek-v4-flash.
            Price::new(0.15, 0.60, 0.15, 0.15, 0.003)
        };
        let scaled = Price::new(
            base.input * peak,
            base.output * peak,
            base.cache_write * peak,
            base.cache_write_long * peak,
            base.cache_read * peak,
        );
        return Some((scaled, CacheKind::Automatic));
    }
    let openai = |price| Some((price, CacheKind::AutomaticShort));
    let table: [(&str, Price); 9] = [
        ("gpt-6.2-astra", Price::new(8.0, 40.0, 10.0, 20.0, 0.40)),
        ("gpt-6.2-sol", Price::new(2.0, 10.0, 2.5, 5.0, 0.10)),
        ("gpt-6.1-sol", Price::new(2.0, 10.0, 2.5, 5.0, 0.10)),
        ("gpt-6.1-terra", Price::new(1.0, 5.0, 1.25, 2.5, 0.05)),
        ("gpt-6-astra", Price::new(10.0, 50.0, 12.5, 25.0, 1.0)),
        ("gpt-6-sol", Price::new(2.0, 10.0, 2.5, 5.0, 0.20)),
        ("gpt-6-luna", Price::new(0.10, 0.50, 0.125, 0.25, 0.01)),
        ("gpt-5.5", Price::new(5.0, 30.0, 5.0, 5.0, 0.50)),
        ("gpt-5.3-codex", Price::new(1.75, 14.0, 1.75, 1.75, 0.175)),
    ];
    table.iter().find(|(name, _)| m.contains(name)).and_then(|(_, price)| openai(*price))
}

/// The price of a model, or of a bucket [`bucket`] made.
pub fn price_of(model: &str) -> Option<Price> {
    family(model).map(|(price, _)| price)
}

pub fn cache_kind(model: &str) -> Option<CacheKind> {
    family(model).map(|(_, kind)| kind)
}

/// DeepSeek's peak hours: 01:00–04:00 and 06:00–10:00 UTC, Monday to Friday.
fn deepseek_peak(timestamp_ms: i64) -> bool {
    let seconds = timestamp_ms.div_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let hour = seconds.rem_euclid(86_400) / 3_600;
    // 1970-01-01 was a Thursday: 0 = Thursday, so Saturday = 2, Sunday = 3.
    let weekday = days.rem_euclid(7);
    let weekend = weekday == 2 || weekday == 3;
    !weekend && ((1..4).contains(&hour) || (6..10).contains(&hour))
}

/// The name a call's tokens are summed under: the model, with a mark when the call falls
/// under a different price of the same model.
pub fn bucket(model: &str, usage: &Usage, timestamp_ms: i64) -> String {
    let lower = model.to_ascii_lowercase();
    // Only Haiku 5 costs more for long prompts.
    if lower.contains("haiku") && claude_version(&lower).is_none_or(|(major, _)| major >= 5) && usage.context() > 100_000 {
        return format!("{model}{LONG_PROMPT}");
    }
    if lower.contains("deepseek") && deepseek_peak(timestamp_ms) {
        return format!("{model}{PEAK}");
    }
    model.to_owned()
}

/// The model name of a bucket.
pub fn bucket_model(bucket: &str) -> &str {
    bucket.trim_end_matches(LONG_PROMPT).trim_end_matches(PEAK)
}

/// The cost of a session from its tokens per bucket, and whether every bucket had a price.
pub fn session_cost(usage_by_model: &[(String, Usage)]) -> (f64, bool) {
    let mut total = 0.0;
    let mut complete = true;
    for (bucket, usage) in usage_by_model {
        match price_of(bucket) {
            Some(price) => total += price.cost(usage),
            None if usage.context() + usage.output == 0 => {}
            None => complete = false,
        }
    }
    (total, complete)
}

/// The state of the prompt cache after the last call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheState {
    /// Kept for this many more milliseconds.
    Warm { left_ms: i64, ttl_ms: i64 },
    /// Gone this many milliseconds ago.
    Cold { since_ms: i64 },
    /// The provider keeps it for an unpublished time; the last call was this long ago.
    Automatic { idle_ms: i64 },
}

/// What the cache holds now. Anthropic's lifetime is the one the last write asked for: an
/// hour when any write of the session was long, five minutes otherwise.
pub fn cache_state(model: &str, total: &Usage, last_call_ms: i64, now_ms: i64) -> Option<CacheState> {
    let idle = (now_ms - last_call_ms).max(0);
    match cache_kind(model)? {
        CacheKind::Explicit => {
            let ttl_ms = if total.cache_write_long > 0 { 3_600_000 } else { 300_000 };
            Some(if idle < ttl_ms {
                CacheState::Warm { left_ms: ttl_ms - idle, ttl_ms }
            } else {
                CacheState::Cold { since_ms: idle - ttl_ms }
            })
        }
        CacheKind::Automatic | CacheKind::AutomaticShort => Some(CacheState::Automatic { idle_ms: idle }),
    }
}

/// What it costs to send the current context once more: the floor of the next turn, and
/// the input of a compaction. `warm` reads it from the cache, cold writes it again.
pub fn context_cost(model: &str, context: u64, warm: bool, long_cache: bool) -> Option<f64> {
    let price = price_of(model)?;
    let per_token = if warm {
        price.cache_read
    } else if long_cache {
        price.cache_write_long
    } else {
        price.cache_write
    };
    Some(context as f64 * per_token / 1e6)
}

/// A compaction reads the whole context and writes a summary; the summary is about a
/// twentieth of the context, between 2K and 20K tokens.
pub fn compact_cost(model: &str, context: u64, warm: bool, long_cache: bool) -> Option<f64> {
    let price = price_of(model)?;
    let summary = (context / 20).clamp(2_000, 20_000);
    Some(context_cost(model, context, warm, long_cache)? + summary as f64 * price.output / 1e6)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_versions_are_read_from_the_name() {
        assert_eq!(claude_version("claude-opus-5-5"), Some((5, 5)));
        assert_eq!(claude_version("claude-opus-5-5[1m]"), Some((5, 5)));
        assert_eq!(claude_version("claude-haiku-5-5@long"), Some((5, 5)));
        assert_eq!(claude_version("claude-sonnet-4-5-20250929"), Some((4, 5)));
        assert_eq!(claude_version("claude-sonnet-4-20250514"), Some((4, 0)));
        assert_eq!(claude_version("claude-3-5-sonnet-20241022"), Some((3, 5)));
        assert_eq!(claude_version("claude-fable-5-2"), Some((5, 2)));
        assert_eq!(claude_version("gpt-5.5"), None);
    }

    /// The models before 5 are priced as they were, not as the ones of their family now.
    #[test]
    fn older_claude_models_keep_their_prices() {
        assert_eq!(price_of("claude-opus-4-1-20250805").unwrap().input, 15.0);
        assert_eq!(price_of("claude-opus-4-5-20251101").unwrap().input, 5.0);
        assert_eq!(price_of("claude-sonnet-4-5").unwrap().output, 15.0);
        assert_eq!(price_of("claude-haiku-4-5").unwrap().input, 1.0);
        assert_eq!(price_of("claude-3-5-haiku-20241022").unwrap().input, 0.80);
        assert_eq!(price_of("claude-opus-5-5").unwrap().input, 4.0);
        let long = Usage { input: 150_000, ..Default::default() };
        assert_eq!(bucket("claude-haiku-4-5", &long, 0), "claude-haiku-4-5", "only Haiku 5 has a long-prompt price");
        assert_eq!(bucket("claude-haiku-5-5", &long, 0), "claude-haiku-5-5@long");
    }

    #[test]
    fn a_real_haiku_turn_costs_what_claude_code_reported() {
        // The usage and the cost of one `claude -p --model haiku` turn, measured.
        let usage = Usage { input: 2, output: 196, cache_read: 30_008, cache_write: 18_088, cache_write_long: 18_088 };
        let cost = price_of("claude-haiku-5-5").unwrap().cost(&usage);
        assert!((cost - 0.004_015_88).abs() < 1e-9, "{cost}");
    }

    #[test]
    fn buckets_split_long_haiku_prompts_and_deepseek_peaks() {
        let long = Usage { cache_read: 150_000, ..Default::default() };
        assert_eq!(bucket("claude-haiku-5-5", &long, 0), "claude-haiku-5-5@long");
        assert_eq!(price_of("claude-haiku-5-5@long").unwrap().input, 0.50);
        // 2026-11-16 was a Monday; 07:00 UTC is peak, 12:00 is not.
        let monday_7 = 1_794_812_400_000;
        assert!(deepseek_peak(monday_7));
        assert!(!deepseek_peak(monday_7 + 5 * 3_600_000));
        assert!(!deepseek_peak(monday_7 - 2 * 86_400_000), "Saturday");
        assert_eq!(price_of("deepseek-v4.1-flash@peak").unwrap().output, 1.20);
    }

    #[test]
    fn the_cache_cools_after_its_lifetime() {
        let long = Usage { cache_write_long: 10, cache_write: 10, ..Default::default() };
        assert_eq!(
            cache_state("claude-opus-5-5", &long, 0, 600_000),
            Some(CacheState::Warm { left_ms: 3_000_000, ttl_ms: 3_600_000 })
        );
        assert_eq!(cache_state("claude-opus-5-5", &Usage::default(), 0, 600_000), Some(CacheState::Cold { since_ms: 300_000 }));
        assert!(matches!(cache_state("deepseek-v4.1-flash", &Usage::default(), 0, 5), Some(CacheState::Automatic { .. })));
        assert_eq!(cache_state("mystery", &Usage::default(), 0, 5), None);
    }

    #[test]
    fn compaction_reads_the_context_and_writes_a_summary() {
        let warm = compact_cost("claude-sonnet-5-5", 200_000, true, true).unwrap();
        // 200K cache reads at $0.10 plus a 10K summary at $10.
        assert!((warm - (0.02 + 0.10)).abs() < 1e-9, "{warm}");
        let cold = compact_cost("claude-sonnet-5-5", 200_000, false, true).unwrap();
        assert!(cold > warm);
    }
}
