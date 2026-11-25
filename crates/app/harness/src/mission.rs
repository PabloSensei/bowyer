//! What a card of mission control shows of a session: its last few lines (the prompt,
//! what the agent said, each tool call with how it went), without the window.

use model::{Block, Entry, Role, Transcript, Usage};
use serde_json::Value;

/// How a tool call stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolState {
    Running,
    Ok,
    Failed,
}

/// One line of a card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CardLine {
    Prompt(String),
    Text(String),
    Tool { name: String, arg: String, state: ToolState },
    Notice(String),
}

/// The longest a message or an earlier answer of a card gets.
const TEXT: usize = 400;
/// The longest the last answer gets: it is what the user looks for.
const LAST_TEXT: usize = 1500;

/// Text as a card shows it: its lines kept, Markdown marks dropped (`**`, `` ` ``, headings),
/// empty lines folded, shortened.
pub fn plain(text: &str, max: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end().trim_start_matches('#').trim_start_matches('>');
        let line = line.replace("**", "").replace("__", "").replace('`', "");
        let line = line.trim_end().to_owned();
        if line.trim().is_empty() {
            if lines.last().is_some_and(|last| !last.is_empty()) {
                lines.push(String::new());
            }
            continue;
        }
        lines.push(if line.starts_with(' ') && !line.trim_start().starts_with(['-', '*']) { line.trim_start().to_owned() } else { line });
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    model::shorten(&lines.join("\n"), max)
}

/// Whitespace folded to single spaces, shortened.
fn flat(text: &str, max: usize) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    model::shorten(&joined, max)
}

/// What a tool call works on, in one line: its command, file, pattern or address.
pub fn tool_arg(input: &Value) -> String {
    let field = ["command", "cmd", "file_path", "filePath", "path", "notebook_path", "pattern", "url", "query", "description", "prompt"]
        .iter()
        .find_map(|name| input.get(*name).and_then(Value::as_str))
        .map(str::to_owned)
        .or_else(|| {
            let parts: Vec<&str> = input.get("command")?.as_array()?.iter().filter_map(Value::as_str).collect();
            // `bash -lc "cargo test"`: the command is the script the shell runs.
            Some(match parts.as_slice() {
                [shell, flag, script] if ["bash", "sh", "zsh", "pwsh", "powershell"].contains(shell) && flag.starts_with('-') => (*script).to_owned(),
                _ => parts.join(" "),
            })
        })
        .unwrap_or_default();
    flat(field.lines().next().unwrap_or(""), 160)
}

/// The last `keep` lines of a transcript. In a turn that still runs, a call without a result
/// runs; in one that ended it was cut off and shows as done.
pub fn card_lines(transcript: &Transcript, keep: usize, live: bool) -> Vec<CardLine> {
    let mut lines: Vec<CardLine> = Vec::new();
    for entry in &transcript.entries {
        if entry.compact_summary {
            continue;
        }
        for block in &entry.blocks {
            let line = match (entry.role, block) {
                (Role::User, Block::Text { text }) if !text.trim().is_empty() => CardLine::Prompt(plain(text, TEXT)),
                (Role::Assistant, Block::Text { text }) if !text.trim().is_empty() => CardLine::Text(plain(text, LAST_TEXT)),
                (_, Block::ToolCall { name, input, result, .. }) => {
                    let state = match result {
                        Some(result) if result.is_error => ToolState::Failed,
                        Some(_) => ToolState::Ok,
                        None if live => ToolState::Running,
                        None => ToolState::Ok,
                    };
                    CardLine::Tool { name: name.clone(), arg: tool_arg(input), state }
                }
                (_, Block::Notice { text }) if !text.trim().is_empty() => CardLine::Notice(flat(text, 160)),
                _ => continue,
            };
            lines.push(line);
        }
    }
    // Only the last answer stays long.
    let last = lines.iter().rposition(|line| matches!(line, CardLine::Text(_)));
    for (at, line) in lines.iter_mut().enumerate() {
        if let CardLine::Text(text) = line
            && Some(at) != last
        {
            *text = model::shorten(text, TEXT);
        }
    }
    let skip = lines.len().saturating_sub(keep);
    lines.split_off(skip)
}

/// How much text and how many calls a transcript holds: its growth between two looks is
/// the card's activity.
pub fn activity(transcript: &Transcript) -> usize {
    transcript.entries.iter().map(entry_activity).sum()
}

/// How much text and how many calls one entry holds.
pub fn entry_activity(entry: &Entry) -> usize {
    entry
        .blocks
        .iter()
        .map(|block| match block {
            Block::Text { text } | Block::Thinking { text } => text.len(),
            Block::ToolCall { result, .. } => 200 + result.as_ref().map_or(0, |result| result.text.len().min(2000)),
            _ => 0,
        })
        .sum()
}

/// The growth of a transcript read again from its end: what entries not seen before hold,
/// and what seen ones gained. Entries that left the end do not count. Returns the growth
/// and what the entries hold now, by id.
pub fn growth(transcript: &Transcript, seen: &std::collections::HashMap<String, usize>) -> (usize, std::collections::HashMap<String, usize>) {
    let now: std::collections::HashMap<String, usize> = transcript.entries.iter().map(|entry| (entry.id.clone(), entry_activity(entry))).collect();
    let grown = now.iter().map(|(id, held)| held.saturating_sub(seen.get(id).copied().unwrap_or(0))).sum();
    (grown, now)
}

/// The calls of the last turn by model: everything after the last prompt of the user.
pub fn last_turn_usage(transcript: &Transcript) -> Vec<(String, Usage)> {
    let prompt = transcript
        .entries
        .iter()
        .rposition(|entry| entry.role == Role::User && entry.blocks.iter().any(|block| matches!(block, Block::Text { text } if !text.trim().is_empty())));
    let mut by_model: Vec<(String, Usage)> = Vec::new();
    for entry in &transcript.entries[prompt.map_or(0, |at| at + 1)..] {
        let Some(usage) = &entry.usage else { continue };
        let name = entry.model.clone().or_else(|| transcript.model.clone()).unwrap_or_default();
        match by_model.iter_mut().find(|(known, _)| *known == name) {
            Some((_, sum)) => sum.add(usage),
            None => by_model.push((name, *usage)),
        }
    }
    by_model
}

#[cfg(test)]
mod tests {
    use super::{CardLine, ToolState, card_lines, growth, last_turn_usage, plain, tool_arg};
    use model::{Block, Entry, Role, ToolKind, ToolResult, Transcript, Usage};
    use serde_json::json;

    fn entry(role: Role, blocks: Vec<Block>) -> Entry {
        Entry { id: "e".into(), role, timestamp_ms: 0, blocks, model: None, usage: None, compact_summary: false }
    }

    fn call(name: &str, input: serde_json::Value, result: Option<(&str, bool)>) -> Block {
        Block::ToolCall {
            id: name.into(),
            name: name.into(),
            kind: ToolKind::classify(name),
            input,
            result: result.map(|(text, is_error)| ToolResult { text: text.into(), is_error }),
        }
    }

    #[test]
    fn a_card_shows_the_last_lines_with_how_each_call_went() {
        let transcript = Transcript {
            entries: vec![
                entry(Role::User, vec![Block::Text { text: "Fix the\n  flaky test".into() }]),
                entry(
                    Role::Assistant,
                    vec![
                        Block::Thinking { text: "hidden".into() },
                        call("Bash", json!({"command": "cargo test\nsecond line"}), Some(("1 failed", true))),
                        call("Read", json!({"file_path": "src/lexer.rs"}), Some(("…", false))),
                        Block::Text { text: "Found it.".into() },
                        call("Edit", json!({"file_path": "src/lexer.rs"}), None),
                    ],
                ),
            ],
            ..Transcript::default()
        };
        let lines = card_lines(&transcript, 10, true);
        assert_eq!(
            lines,
            vec![
                CardLine::Prompt("Fix the\nflaky test".into()),
                CardLine::Tool { name: "Bash".into(), arg: "cargo test".into(), state: ToolState::Failed },
                CardLine::Tool { name: "Read".into(), arg: "src/lexer.rs".into(), state: ToolState::Ok },
                CardLine::Text("Found it.".into()),
                CardLine::Tool { name: "Edit".into(), arg: "src/lexer.rs".into(), state: ToolState::Running },
            ]
        );
        assert_eq!(card_lines(&transcript, 2, true).len(), 2, "only the last ones");
        let ended = card_lines(&transcript, 1, false);
        assert_eq!(ended, vec![CardLine::Tool { name: "Edit".into(), arg: "src/lexer.rs".into(), state: ToolState::Ok }], "a turn that ended runs nothing");
    }

    /// Read again from its end, a session grows by what is new; what left the end is not
    /// taken as a loss.
    #[test]
    fn a_session_grows_by_what_is_new_at_its_end() {
        let text = |id: &str, text: &str| Entry { id: id.into(), ..entry(Role::Assistant, vec![Block::Text { text: text.into() }]) };
        let first = Transcript { entries: vec![text("a", "12345"), text("b", "123")], ..Transcript::default() };
        let (grown, seen) = growth(&first, &Default::default());
        assert_eq!(grown, 8);
        let later = Transcript { entries: vec![text("b", "123456"), text("c", "1234567890")], ..Transcript::default() };
        assert_eq!(growth(&later, &seen).0, 3 + 10, "b grew by 3, c is new, a left the end");
    }

    /// The cost of the last turn: the calls after the last prompt, not the tool results.
    #[test]
    fn the_last_turn_is_the_calls_after_the_last_prompt() {
        let used = |output: u64| Entry {
            usage: Some(Usage { output, ..Usage::default() }),
            model: Some("m".into()),
            ..entry(Role::Assistant, vec![Block::Text { text: "x".into() }])
        };
        let result = entry(Role::User, vec![Block::ToolCall { id: "t".into(), name: "Bash".into(), kind: ToolKind::classify("Bash"), input: serde_json::json!({}), result: None }]);
        let transcript = Transcript {
            entries: vec![
                entry(Role::User, vec![Block::Text { text: "one".into() }]),
                used(100),
                entry(Role::User, vec![Block::Text { text: "two".into() }]),
                used(10),
                result,
                used(5),
            ],
            ..Transcript::default()
        };
        let turn = last_turn_usage(&transcript);
        assert_eq!(turn.len(), 1);
        assert_eq!(turn[0].1.output, 15);
    }

    /// A card shows an answer's lines without Markdown marks; only the last answer stays long.
    #[test]
    fn answers_keep_their_lines_without_marks() {
        assert_eq!(plain("**Why:**\n\n\n- the `tail` was short\n  - nested\n## Next\n", 100), "Why:\n\n- the tail was short\n  - nested\nNext");
        let long = "word ".repeat(200);
        let transcript = Transcript {
            entries: vec![
                entry(Role::Assistant, vec![Block::Text { text: long.clone() }]),
                entry(Role::Assistant, vec![Block::Text { text: long }]),
            ],
            ..Transcript::default()
        };
        let lines = card_lines(&transcript, 10, false);
        let size = |line: &CardLine| match line {
            CardLine::Text(text) => text.chars().count(),
            _ => 0,
        };
        assert!(size(&lines[0]) <= 401 && size(&lines[1]) > 900, "{} {}", size(&lines[0]), size(&lines[1]));
    }

    #[test]
    fn a_call_names_what_it_works_on() {
        assert_eq!(tool_arg(&json!({"command": ["git", "status"]})), "git status");
        assert_eq!(tool_arg(&json!({"command": ["bash", "-lc", "cargo test"]})), "cargo test", "the script a shell runs");
        assert_eq!(tool_arg(&json!({"pattern": "fn main"})), "fn main");
        assert_eq!(tool_arg(&json!({})), "");
    }
}
