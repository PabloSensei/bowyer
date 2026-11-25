//! A session as a Markdown file: what was said, with each tool call folded under its
//! summary. Thinking is left out; pictures show as a mark.

use model::{Block, Role, SessionSummary, Transcript};
use serde_json::Value;

/// The words the file is written with, in the window's language.
pub struct ExportLabels<'a> {
    pub you: &'a str,
    /// This computer, for a session not on a host.
    pub computer: &'a str,
    pub folder: &'a str,
    pub model: &'a str,
    pub started: &'a str,
    pub updated: &'a str,
    pub session: &'a str,
    pub summary: &'a str,
    pub picture: &'a str,
}

/// The longest a tool's input or result is written; the rest is cut.
const TOOL_TEXT: usize = 4000;

/// A fenced block whose fence no line inside can close.
fn fenced(text: &str, lang: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for ch in text.chars() {
        run = if ch == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}{lang}\n{}\n{fence}\n", text.trim_end_matches('\n'))
}

fn cut(text: &str) -> String {
    if text.len() <= TOOL_TEXT {
        return text.to_owned();
    }
    let mut end = TOOL_TEXT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…", &text[..end])
}

fn field<'a>(input: &'a Value, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|name| input.get(*name).and_then(Value::as_str))
}

/// One line for the summary of a tool call: its name and what it worked on.
fn tool_line(name: &str, input: &Value) -> String {
    let what = field(input, &["command", "cmd", "file_path", "filePath", "path", "pattern", "url", "query", "description"]).unwrap_or("");
    let what = what.lines().next().unwrap_or("");
    let what = model::shorten(what, 120).replace(['<', '>'], "");
    if what.is_empty() { name.to_owned() } else { format!("{name}: {what}") }
}

/// What a tool call was given, in the form it reads best.
fn tool_input(input: &Value) -> String {
    if let Some(command) = field(input, &["command", "cmd"]) {
        return fenced(&cut(command), "sh");
    }
    if let (Some(old), Some(new)) = (field(input, &["old_string", "oldString"]), field(input, &["new_string", "newString"])) {
        let diff: Vec<String> = old.lines().map(|line| format!("-{line}")).chain(new.lines().map(|line| format!("+{line}"))).collect();
        return fenced(&cut(&diff.join("\n")), "diff");
    }
    if let Some(content) = field(input, &["content", "contents"]) {
        return fenced(&cut(content), "");
    }
    fenced(&cut(&serde_json::to_string_pretty(input).unwrap_or_default()), "json")
}

/// The session as Markdown: a title, what it is, then each message under who said it.
pub fn export_markdown(summary: &SessionSummary, transcript: &Transcript, labels: &ExportLabels) -> String {
    let mut out = format!("# {}\n\n", summary.title.trim());
    let mut facts: Vec<String> = vec![format!("{} · {}", summary.key.harness.label(), summary.key.env.ssh_host().unwrap_or(labels.computer))];
    if let Some(cwd) = &summary.cwd {
        facts.push(format!("{}: `{cwd}`", labels.folder));
    }
    if let Some(model) = transcript.model.as_ref().or(summary.model.as_ref()) {
        facts.push(format!("{}: {model}", labels.model));
    }
    if summary.created_ms > 0 {
        facts.push(format!("{}: {}", labels.started, model::format_ms_utc(summary.created_ms)));
    }
    if summary.updated_ms > 0 {
        facts.push(format!("{}: {}", labels.updated, model::format_ms_utc(summary.updated_ms)));
    }
    facts.push(format!("{}: `{}`", labels.session, summary.key.id));
    for fact in facts {
        out.push_str(&format!("- {fact}\n"));
    }
    let mut last_role: Option<Role> = None;
    for entry in &transcript.entries {
        let mut body = String::new();
        for block in &entry.blocks {
            match block {
                Block::Text { text } if !text.trim().is_empty() => body.push_str(&format!("{}\n\n", text.trim_end())),
                Block::Image { .. } => body.push_str(&format!("*[{}]*\n\n", labels.picture)),
                Block::Notice { text } if !text.trim().is_empty() => body.push_str(&format!("> {}\n\n", text.trim().replace('\n', "\n> "))),
                Block::ToolCall { name, input, result, .. } => {
                    body.push_str(&format!("<details><summary>{}</summary>\n\n{}", tool_line(name, input), tool_input(input)));
                    if let Some(result) = result.as_ref().filter(|result| !result.text.trim().is_empty()) {
                        body.push('\n');
                        body.push_str(&fenced(&cut(&result.text), ""));
                    }
                    body.push_str("\n</details>\n\n");
                }
                _ => {}
            }
        }
        if body.is_empty() {
            continue;
        }
        while out.ends_with("\n\n") {
            out.pop();
        }
        if entry.compact_summary {
            out.push_str(&format!("\n<details><summary>{}</summary>\n\n{body}</details>\n", labels.summary));
            last_role = None;
            continue;
        }
        // A run of one side's messages goes under one heading.
        if last_role != Some(entry.role) {
            let who = match entry.role {
                Role::User => labels.you,
                Role::Assistant => summary.key.harness.label(),
                Role::System => "—",
            };
            let when = if entry.timestamp_ms > 0 { format!(" · {}", model::format_ms_utc(entry.timestamp_ms)) } else { String::new() };
            out.push_str(&format!("\n## {who}{when}\n\n"));
            last_role = Some(entry.role);
        }
        out.push_str(&body);
    }
    out.trim_end().to_owned() + "\n"
}

/// A file name for the export of a session titled `title`.
pub fn export_file_name(title: &str) -> String {
    let name: String = title
        .chars()
        .map(|ch| if ch.is_alphanumeric() || matches!(ch, ' ' | '-' | '_' | '.') { ch } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let name = model::shorten(name.trim_matches('.'), 80);
    format!("{}.md", if name.is_empty() { "session".to_owned() } else { name })
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::{EnvId, Entry, HarnessKind, SessionKey, SourceStamp, ToolKind, ToolResult};
    use serde_json::json;

    fn entry(role: Role, blocks: Vec<Block>) -> Entry {
        Entry { id: "e".into(), role, timestamp_ms: 0, blocks, model: None, usage: None, compact_summary: false }
    }

    #[test]
    fn a_session_reads_as_markdown() {
        let summary = SessionSummary {
            key: SessionKey { env: EnvId::local(), harness: HarnessKind::Claude, id: "s1".into() },
            title: "Fix the parser".into(),
            first_prompt: None,
            cwd: Some("/work/app".into()),
            git_branch: None,
            model: Some("claude-haiku-4-5".into()),
            created_ms: 0,
            updated_ms: 0,
            parent: None,
            source: SourceStamp { path: String::new(), size: 0, modified_ms: 0 },
            turn: None,
        };
        let call = Block::ToolCall {
            id: "t1".into(),
            name: "Bash".into(),
            kind: ToolKind::classify("Bash"),
            input: json!({"command": "cargo test"}),
            result: Some(ToolResult { text: "ok ``` done".into(), is_error: false }),
        };
        let transcript = Transcript {
            entries: vec![
                entry(Role::User, vec![Block::Text { text: "Fix it.".into() }, Block::Image { media_type: "image/png".into(), data: None }]),
                entry(Role::Assistant, vec![Block::Thinking { text: "secret plan".into() }, call]),
                entry(Role::Assistant, vec![Block::Text { text: "Fixed.".into() }]),
            ],
            ..Transcript::default()
        };
        let labels = ExportLabels {
            you: "You",
            computer: "This computer",
            folder: "Folder",
            model: "Model",
            started: "Started",
            updated: "Updated",
            session: "Session",
            summary: "Summary",
            picture: "picture",
        };
        let text = export_markdown(&summary, &transcript, &labels);
        assert!(text.starts_with("# Fix the parser\n\n- Claude Code · This computer\n"), "{text}");
        assert!(text.contains("- Folder: `/work/app`\n- Model: claude-haiku-4-5\n- Session: `s1`\n"), "{text}");
        assert!(text.contains("\n## You\n\nFix it.\n\n*[picture]*\n"), "{text}");
        assert_eq!(text.matches("## Claude Code").count(), 1, "one heading for a run of answers: {text}");
        assert!(text.contains("<details><summary>Bash: cargo test</summary>\n\n```sh\ncargo test\n```\n\n````\nok ``` done\n````\n\n</details>"), "{text}");
        assert!(text.contains("Fixed.\n"));
        assert!(!text.contains("\n\n\n"), "one blank line between parts: {text}");
        assert!(!text.contains("secret plan"), "thinking stays out");
    }

    #[test]
    fn a_title_makes_a_file_name() {
        assert_eq!(export_file_name("Fix: the <parser>/lexer?"), "Fix the parser lexer.md");
        assert_eq!(export_file_name("   "), "session.md");
    }
}
