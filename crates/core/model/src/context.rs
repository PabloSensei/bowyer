//! What fills the context window now. The harness reports one number, the size of the last
//! call; the parts are estimated from the transcript since the last compaction and scaled to
//! that number, and the rest is the system prompt, tool definitions and memory files. The
//! memory files can be measured on disk and taken out of that rest.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Block, Transcript, estimate_tokens};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextPart {
    /// The measured rest the transcript does not show: system prompt, tool schemas,
    /// attachments, and the error of the estimate.
    System,
    /// Instruction files the harness loads by itself (`CLAUDE.md`, `AGENTS.md`, automatic
    /// memory), estimated from their text.
    Memory,
    Prompts,
    Answers,
    Thinking,
    ToolCalls,
    ToolResults,
    Images,
    /// The summary of a compaction.
    Summary,
}

impl ContextPart {
    pub const ALL: [ContextPart; 9] = [
        Self::System,
        Self::Memory,
        Self::Prompts,
        Self::Answers,
        Self::Thinking,
        Self::ToolCalls,
        Self::ToolResults,
        Self::Images,
        Self::Summary,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System prompt, tools and the rest",
            Self::Memory => "Memory files (CLAUDE.md, AGENTS.md)",
            Self::Prompts => "Your messages",
            Self::Answers => "Answers",
            Self::Thinking => "Thinking",
            Self::ToolCalls => "Tool calls",
            Self::ToolResults => "Tool results",
            Self::Images => "Images",
            Self::Summary => "Compaction summary",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextBreakdown {
    /// Tokens in the context on the last call, as the harness reported it, or the estimate.
    pub total: u64,
    /// True when `total` came from the harness.
    pub measured: bool,
    /// The window of the model, when known.
    pub window: Option<u64>,
    pub parts: Vec<(ContextPart, u64)>,
}

/// An image costs about this much in the context.
const IMAGE_TOKENS: u64 = 1_500;

impl ContextBreakdown {
    pub fn of(transcript: &Transcript) -> Self {
        let start = transcript
            .entries
            .iter()
            .rposition(|entry| entry.compact_summary)
            .unwrap_or(0);
        let mut sums = [0u64; ContextPart::ALL.len()];
        let slot = |part: ContextPart| ContextPart::ALL.iter().position(|p| *p == part).unwrap();
        for entry in &transcript.entries[start..] {
            for block in &entry.blocks {
                let (part, tokens) = match block {
                    Block::Text { text } if entry.compact_summary => {
                        (ContextPart::Summary, estimate_tokens(text))
                    }
                    Block::Text { text } => match entry.role {
                        crate::Role::User => (ContextPart::Prompts, estimate_tokens(text)),
                        _ => (ContextPart::Answers, estimate_tokens(text)),
                    },
                    Block::Thinking { text } => (ContextPart::Thinking, estimate_tokens(text)),
                    Block::ToolCall { input, result, .. } => {
                        sums[slot(ContextPart::ToolCalls)] += estimate_value(input);
                        let output = result.as_ref().map_or(0, |r| estimate_tokens(&r.text));
                        (ContextPart::ToolResults, output)
                    }
                    Block::Image { .. } => (ContextPart::Images, IMAGE_TOKENS),
                    Block::Notice { .. } => continue,
                };
                sums[slot(part)] += tokens;
            }
        }
        let estimated: u64 = sums.iter().sum();
        let (total, measured) = match transcript.last_usage {
            Some(usage) if usage.context() > 0 => (usage.context(), true),
            _ => (estimated, false),
        };
        if measured {
            if estimated <= total {
                sums[slot(ContextPart::System)] = total - estimated;
            } else {
                // The estimate overshoots: scale the parts down to the measured total.
                let scale = total as f64 / estimated as f64;
                for sum in &mut sums {
                    *sum = (*sum as f64 * scale) as u64;
                }
            }
        }
        let parts = ContextPart::ALL
            .iter()
            .zip(sums)
            .filter(|(_, tokens)| *tokens > 0)
            .map(|(part, tokens)| (*part, tokens))
            .collect();
        // A context larger than the usual window means the session runs with the long one.
        let window = transcript.model.as_deref().and_then(window_of).map(|window| {
            if total > window && window < 1_000_000 { 1_000_000 } else { window }
        });
        Self { total, measured, window, parts }
    }
}

impl ContextBreakdown {
    /// Takes the memory files out of the rest: at most what the rest holds when the total
    /// was measured, added to the estimate when it was not.
    pub fn take_memory(&mut self, tokens: u64) {
        if tokens == 0 {
            return;
        }
        let system = self.parts.iter().find(|(part, _)| *part == ContextPart::System).map_or(0, |(_, t)| *t);
        let memory = if self.measured { tokens.min(system) } else { tokens };
        if memory == 0 {
            return;
        }
        if self.measured {
            for (part, t) in &mut self.parts {
                if *part == ContextPart::System {
                    *t -= memory;
                }
            }
            self.parts.retain(|(_, t)| *t > 0);
        } else {
            self.total += memory;
        }
        let at = self.parts.iter().position(|(part, _)| *part != ContextPart::System).unwrap_or(self.parts.len());
        self.parts.insert(at, (ContextPart::Memory, memory));
    }
}

fn estimate_value(value: &Value) -> u64 {
    match value {
        Value::String(text) => estimate_tokens(text),
        Value::Null => 0,
        other => estimate_tokens(&other.to_string()),
    }
}

/// The context window of well-known models, in tokens.
pub fn window_of(model: &str) -> Option<u64> {
    let model = model.to_ascii_lowercase();
    if model.contains("[1m]") {
        return Some(1_000_000);
    }
    // The 5.x Claude models and Fable have a window of a million tokens, older ones 200K.
    if model.contains("fable") || crate::pricing::claude_version(&model).is_some_and(|(major, _)| major >= 5) {
        return Some(1_000_000);
    }
    if model.starts_with("claude") {
        return Some(200_000);
    }
    if model.starts_with("gpt-5") || model.contains("codex") {
        return Some(272_000);
    }
    if model.contains("deepseek") {
        return Some(1_048_576);
    }
    None
}

/// The context of a Claude Code session as Claude itself counts it: the report of its
/// `/context` command, which a print-mode run answers without calling the model.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClaudeContext {
    pub total: u64,
    pub window: Option<u64>,
    /// Categories in the context now, in Claude's order and words ("System prompt",
    /// "System tools", "MCP tools", "Memory files", "Messages"…). Deferred tools, which
    /// are not in the context until used, free space and the compaction buffer are left out.
    pub categories: Vec<(String, u64)>,
}

/// Reads the Markdown report of Claude Code's `/context`.
pub fn parse_claude_context(report: &str) -> Option<ClaudeContext> {
    let mut context = ClaudeContext::default();
    let tokens_line = report.lines().find_map(|line| line.trim().strip_prefix("**Tokens:**"))?;
    let mut sizes = tokens_line.split('/').map(|part| part.split_whitespace().next().and_then(parse_count));
    context.total = sizes.next().flatten()?;
    context.window = sizes.next().flatten();
    let mut in_table = false;
    for line in report.lines() {
        let line = line.trim();
        if line.starts_with("###") {
            in_table = line.contains("by category");
            continue;
        }
        if !in_table || !line.starts_with('|') {
            continue;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        let [name, tokens, ..] = cells.as_slice() else { continue };
        let Some(tokens) = parse_count(tokens) else { continue };
        let skipped = name.contains("(deferred)") || matches!(*name, "Free space" | "Autocompact buffer");
        if !skipped && tokens > 0 {
            context.categories.push(((*name).to_owned(), tokens));
        }
    }
    (!context.categories.is_empty()).then_some(context)
}

/// "3.6k", "1.2M", "~300", "215".
fn parse_count(text: &str) -> Option<u64> {
    let text = text.trim().trim_start_matches('~').replace(',', "");
    let (number, scale) = match text.chars().last()? {
        'k' | 'K' => (&text[..text.len() - 1], 1_000.0),
        'm' | 'M' => (&text[..text.len() - 1], 1_000_000.0),
        _ => (text.as_str(), 1.0),
    };
    let value: f64 = number.parse().ok()?;
    Some((value * scale).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Entry, Role, Usage};

    #[test]
    fn only_claude_5_has_a_window_of_a_million() {
        assert_eq!(window_of("claude-opus-5-5"), Some(1_000_000));
        assert_eq!(window_of("claude-sonnet-4-5-20250929"), Some(200_000));
        assert_eq!(window_of("claude-haiku-4-5"), Some(200_000));
        assert_eq!(window_of("claude-3-5-sonnet-20241022"), Some(200_000));
        assert_eq!(window_of("claude-sonnet-4-5[1m]"), Some(1_000_000));
    }

    #[test]
    fn claude_counts_its_context_by_category() {
        let report = "## Context Usage\n\n**Model:** claude-haiku-5-5  \n**Tokens:** 73.9k / 600k (12%)\n\n\
### Estimated usage by category\n\n| Category | Tokens | Percentage |\n|----------|--------|------------|\n\
| System prompt | 3.6k | 0.6% |\n| System tools | 21.9k | 3.6% |\n| MCP tools | 680 | 0.1% |\n\
| MCP tools (deferred) | 33.5k | 5.6% |\n| Skills | ~7.2k | 1.2% |\n| Messages | 39.3k | 6.6% |\n\
| Free space | 493.1k | 82.2% |\n| Autocompact buffer | 33k | 5.5% |\n\n### Skills\n\n| Skill | Source | Tokens |\n\
|-------|--------|--------|\n| dataviz | Built-in | ~480 |\n";
        let context = parse_claude_context(report).unwrap();
        assert_eq!(context.total, 73_900);
        assert_eq!(context.window, Some(600_000));
        assert_eq!(
            context.categories,
            [("System prompt", 3_600), ("System tools", 21_900), ("MCP tools", 680), ("Skills", 7_200), ("Messages", 39_300)]
                .map(|(name, tokens)| (name.to_owned(), tokens))
        );
        assert_eq!(parse_claude_context("No context here"), None);
    }

    #[test]
    fn the_measured_rest_is_the_system_prompt() {
        let transcript = Transcript {
            entries: vec![Entry {
                id: "1".into(),
                role: Role::User,
                timestamp_ms: 0,
                blocks: vec![Block::Text { text: "a".repeat(400) }],
                model: None,
                usage: None,
                compact_summary: false,
            }],
            last_usage: Some(Usage { input: 10, cache_read: 1_000, ..Default::default() }),
            model: Some("claude-opus-5-5".into()),
            ..Default::default()
        };
        let breakdown = ContextBreakdown::of(&transcript);
        assert!(breakdown.measured);
        assert_eq!(breakdown.total, 1_010);
        assert_eq!(breakdown.window, Some(1_000_000));
        assert_eq!(breakdown.parts, vec![(ContextPart::System, 910), (ContextPart::Prompts, 100)]);

        let mut split = breakdown.clone();
        split.take_memory(300);
        assert_eq!(split.parts, vec![(ContextPart::System, 610), (ContextPart::Memory, 300), (ContextPart::Prompts, 100)]);
        assert_eq!(split.total, 1_010, "a measured total stays");
        let mut all = breakdown.clone();
        all.take_memory(5_000);
        assert_eq!(all.parts, vec![(ContextPart::Memory, 910), (ContextPart::Prompts, 100)], "never more than the rest");
    }
}
