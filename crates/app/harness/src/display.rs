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
    pub model: Option<String>,
    pub compact_summary: bool,
    pub pieces: Vec<Piece>,
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
    for block in &entry.blocks {
        match block {
            Block::Text { text } => {
                if text.trim().is_empty() {
                    continue;
                }
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
        model: entry.model.clone(),
        compact_summary: entry.compact_summary,
        pieces,
    }
}

/// Items of a transcript. Assistant turns that are nothing but tool calls join the turn
/// before them, so a long agent run reads as one answer with folded work inside.
pub fn items(transcript: &Transcript) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::new();
    for entry in &transcript.entries {
        let item = item_of(entry);
        if item.pieces.is_empty() {
            continue;
        }
        if let Some(last) = items.last_mut()
            && last.role == Role::Assistant
            && item.role == Role::Assistant
        {
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
}
