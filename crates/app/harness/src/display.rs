//! A transcript turned into what the centre pane draws: Markdown parsed once, tool calls
//! in runs that fold into one line. Built off the window thread when a session opens.

use std::sync::Arc;

use model::{Block, Entry, Role, ToolKind, ToolResult, Transcript};
use serde_json::Value;

use crate::markdown::{self, MdBlock};

#[derive(Clone, Debug)]
#[allow(dead_code)] // Kind and times feed the planned per-tool styling.
pub struct ToolView {
    pub name: String,
    pub kind: ToolKind,
    /// One line: the command, the file, the pattern.
    pub summary: String,
    /// Old and new text of an edit, for the diff under an unfolded call.
    pub diff: Option<(String, String)>,
    pub input: String,
    pub result: Option<ToolResult>,
}

#[derive(Clone, Debug)]
pub enum Piece {
    Markdown(Vec<MdBlock>),
    Thinking(String),
    /// Calls in a row, folded by default.
    Tools(Vec<ToolView>),
    Notice(String),
    /// A picture: its media type, and the picture with its size when the history keeps it.
    Image(String, Option<(Arc<ui_kit::Image>, u32, u32)>),
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct Item {
    pub role: Role,
    pub timestamp_ms: i64,
    /// When its last part came: an answer joins the turns after it.
    pub end_ms: i64,
    pub model: Option<String>,
    pub compact_summary: bool,
    pub pieces: Vec<Piece>,
    /// A prompt of a Claude Code session: the line a new session before it starts from, and
    /// its text, to put back into the composer.
    pub branch: Option<(String, String)>,
    /// A prompt: its uuid, for the files its turn changed, and its text.
    pub prompt: Option<(String, String)>,
    /// Its text as written, Markdown and all, to copy whole.
    pub text: String,
}

fn field<'a>(input: &'a Value, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|name| input.get(*name).and_then(Value::as_str))
}

fn tool_view(name: &str, kind: ToolKind, input: &Value, result: &Option<ToolResult>) -> ToolView {
    let summary = field(input, &["command", "cmd", "file_path", "filePath", "path", "pattern", "url", "query", "description", "prompt"])
        .map(str::to_owned)
        .or_else(|| {
            input.get("command").and_then(Value::as_array).map(|parts| {
                parts.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")
            })
        })
        .unwrap_or_default();
    let summary = model::shorten(summary.lines().next().unwrap_or(""), 160);
    let diff = match kind {
        ToolKind::Edit => {
            let old = field(input, &["old_string", "oldString", "old_str"]).unwrap_or("");
            let new = field(input, &["new_string", "newString", "new_str", "content", "input", "patch"]).unwrap_or("");
            (!old.is_empty() || !new.is_empty()).then(|| (old.to_owned(), new.to_owned()))
        }
        _ => None,
    };
    let input = match input {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    };
    ToolView {
        name: name.to_owned(),
        kind,
        summary,
        diff,
        input: model::shorten(&input, 4_000),
        result: result.as_ref().map(|r| ToolResult { text: model::shorten(&r.text, 6_000), is_error: r.is_error }),
    }
}

fn item_of(entry: &Entry) -> Item {
    let mut pieces: Vec<Piece> = Vec::new();
    let mut texts: Vec<&str> = Vec::new();
    for block in &entry.blocks {
        match block {
            Block::Text { text } => {
                if text.trim().is_empty() {
                    continue;
                }
                texts.push(text.trim());
                pieces.push(Piece::Markdown(markdown::parse(text)));
            }
            Block::Thinking { text } => pieces.push(Piece::Thinking(text.clone())),
            Block::ToolCall { name, kind, input, result, .. } => {
                let view = tool_view(name, *kind, input, result);
                match pieces.last_mut() {
                    Some(Piece::Tools(run)) => run.push(view),
                    _ => pieces.push(Piece::Tools(vec![view])),
                }
            }
            Block::Notice { text } => pieces.push(Piece::Notice(text.clone())),
            Block::Image { media_type, data } => {
                let picture = data.as_deref().and_then(|data| crate::shell::picture_of_base64(media_type, data));
                pieces.push(Piece::Image(media_type.clone(), picture));
            }
        }
    }
    Item {
        role: entry.role,
        timestamp_ms: entry.timestamp_ms,
        end_ms: entry.timestamp_ms,
        model: entry.model.clone(),
        compact_summary: entry.compact_summary,
        pieces,
        branch: None,
        prompt: None,
        text: texts.join("\n\n"),
    }
}

/// Items of a transcript. Assistant turns that are nothing but tool calls join the turn
/// before them, so a long agent run reads as one answer with folded work inside.
pub fn items(transcript: &Transcript) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::new();
    let branches: std::collections::HashMap<&str, &str> = transcript.branch_points.iter().map(|(prompt, at)| (prompt.as_str(), at.as_str())).collect();
    for entry in &transcript.entries {
        let mut item = item_of(entry);
        if entry.role == Role::User && !entry.compact_summary {
            let text = entry.blocks.iter().filter_map(|block| match block {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            });
            let text = text.collect::<Vec<_>>().join("\n");
            if let Some(at) = branches.get(entry.id.as_str()) {
                item.branch = Some(((*at).to_owned(), text.clone()));
            }
            item.prompt = Some((entry.id.clone(), text));
        }
        if item.pieces.is_empty() {
            continue;
        }
        if let Some(last) = items.last_mut()
            && last.role == Role::Assistant
            && item.role == Role::Assistant
        {
            last.end_ms = last.end_ms.max(item.end_ms);
            if !item.text.is_empty() {
                if !last.text.is_empty() {
                    last.text.push_str("\n\n");
                }
                last.text.push_str(&item.text);
            }
            for piece in item.pieces {
                match (last.pieces.last_mut(), piece) {
                    (Some(Piece::Tools(run)), Piece::Tools(more)) => run.extend(more),
                    (_, piece) => last.pieces.push(piece),
                }
            }
            continue;
        }
        items.push(item);
    }
    items
}

/// "Read ×3 · Edit ×2" for a folded run.
pub fn run_summary(run: &[ToolView]) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for tool in run {
        match counts.iter_mut().find(|(name, _)| *name == tool.name) {
            Some((_, count)) => *count += 1,
            None => counts.push((&tool.name, 1)),
        }
    }
    counts
        .iter()
        .map(|(name, count)| if *count > 1 { format!("{name} ×{count}") } else { (*name).to_owned() })
        .collect::<Vec<_>>()
        .join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::Entry;
    use serde_json::json;

    fn entry(role: Role, blocks: Vec<Block>) -> Entry {
        Entry { id: String::new(), role, timestamp_ms: 0, blocks, model: None, usage: None, compact_summary: false }
    }

    fn call(name: &str) -> Block {
        Block::ToolCall { id: name.into(), name: name.into(), kind: ToolKind::classify(name), input: json!({"command": "ls"}), result: None }
    }

    /// A prompt with a branch point carries it and its text; an answer carries none.
    #[test]
    fn a_prompt_carries_where_a_branch_before_it_starts() {
        let mut prompt = entry(Role::User, vec![Block::Text { text: "Remember BANANA.".into() }]);
        prompt.id = "p2".into();
        let transcript = Transcript {
            entries: vec![prompt, entry(Role::Assistant, vec![Block::Text { text: "OK".into() }])],
            branch_points: vec![("p2".into(), "a1".into())],
            ..Transcript::default()
        };
        let shown = items(&transcript);
        assert_eq!(shown[0].branch, Some(("a1".to_owned(), "Remember BANANA.".to_owned())));
        assert_eq!(shown[1].branch, None);
    }

    #[test]
    fn tool_only_turns_fold_into_the_answer() {
        let transcript = Transcript {
            entries: vec![
                entry(Role::User, vec![Block::Text { text: "go".into() }]),
                entry(Role::Assistant, vec![Block::Text { text: "Looking.".into() }, call("Read")]),
                entry(Role::Assistant, vec![call("Read"), call("Bash")]),
                entry(Role::Assistant, vec![Block::Text { text: "Done.".into() }]),
            ],
            ..Default::default()
        };
        let items = items(&transcript);
        assert_eq!(items.len(), 2);
        let Piece::Tools(run) = &items[1].pieces[1] else { panic!("a run") };
        assert_eq!(run.len(), 3);
        assert_eq!(run_summary(run), "Read ×2 · Bash");
        assert!(matches!(items[1].pieces[2], Piece::Markdown(_)));
    }

    /// An answer joined from several turns keeps all their words as written, and ends when
    /// the last of them came.
    #[test]
    fn an_answer_keeps_its_markdown_and_when_it_ended() {
        let at = |mut entry: Entry, ms| {
            entry.timestamp_ms = ms;
            entry
        };
        let transcript = Transcript {
            entries: vec![
                at(entry(Role::User, vec![Block::Text { text: "list it".into() }]), 1_000),
                at(entry(Role::Assistant, vec![Block::Text { text: "Looking **now**.".into() }, call("Read")]), 2_000),
                at(entry(Role::Assistant, vec![call("Bash")]), 3_000),
                at(entry(Role::Assistant, vec![Block::Thinking { text: "hm".into() }, Block::Text { text: "| a | b |\n|---|---|\n| 1 | 2 |\n".into() }]), 4_000),
            ],
            ..Default::default()
        };
        let items = items(&transcript);
        assert_eq!((items[0].timestamp_ms, items[0].text.as_str()), (1_000, "list it"));
        assert_eq!((items[1].timestamp_ms, items[1].end_ms), (2_000, 4_000));
        assert_eq!(items[1].text, "Looking **now**.\n\n| a | b |\n|---|---|\n| 1 | 2 |");
    }
}
