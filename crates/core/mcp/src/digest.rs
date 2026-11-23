//! A digest of a session: what the problem was, how it was solved, in which commits, under
//! which tickets, in which files. The memory server answers "we solved something like this
//! before" with digests instead of whole conversations.
//!
//! The facts come from the transcript without any model: commits from the output of
//! `git commit`, ticket ids from the prompts, answers and commit subjects, files from the
//! edits. A small model (Haiku, or a local one) may add the problem, cause and fix in a
//! sentence each and words to find the session by; it is asked once per version of a
//! session, through [`Summarizer`], so tests use a fake.

use model::{Block, Role, SessionSummary, Transcript};
use serde_json::Value;

/// What a model is asked; a fake answers in tests.
pub trait Summarizer {
    fn summarize(&self, prompt: &str) -> Result<String, String>;
}

/// The system prompt of the model that writes digests.
pub const SYSTEM: &str = "You summarize coding sessions so that they can be found and understood later. Answer in exactly four lines that start with PROBLEM:, CAUSE:, FIX: and KEYWORDS:, and nothing else.";

/// The most of a conversation a model reads: its start and, mostly, its end.
const PROMPT_HEAD: usize = 4_000;
const PROMPT_TAIL: usize = 8_000;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Commit {
    pub hash: String,
    pub subject: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Digest {
    pub problem: String,
    pub cause: Option<String>,
    pub fix: String,
    pub commits: Vec<Commit>,
    pub tickets: Vec<String>,
    pub files: Vec<String>,
    pub keywords: Vec<String>,
    pub by_model: bool,
}

/// The shell command of a tool call: Claude Code's `command` string, or Codex's list of
/// arguments.
pub fn command_of(input: &Value) -> Option<String> {
    match input.get("command").or_else(|| input.get("cmd"))? {
        Value::String(command) => Some(command.clone()),
        Value::Array(parts) => Some(parts.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")),
        _ => None,
    }
}

/// The commit `git commit` reported: `[branch 1a2b3c4] Subject`, or
/// `[branch (root-commit) 1a2b3c4] Subject`.
pub fn commit_line(output: &str) -> Option<Commit> {
    output.lines().find_map(|line| {
        let line = line.trim();
        let rest = line.strip_prefix('[')?;
        let (inside, subject) = rest.split_once("] ")?;
        let hash = inside.split_whitespace().last()?;
        let hex = hash.len() >= 7 && hash.len() <= 40 && hash.chars().all(|c| c.is_ascii_hexdigit());
        hex.then(|| Commit { hash: hash.to_owned(), subject: subject.trim().to_owned() })
    })
}

/// Ticket ids in a text: `SHOP-142` (two to ten capitals or digits, a dash, digits) and
/// links to issues and pull requests.
pub fn tickets_in(text: &str, found: &mut Vec<String>) {
    const NOT_TICKETS: &[&str] = &["UTF", "SHA", "ISO", "RFC", "CVE", "HTTP", "TLS", "AES", "GPT", "LTS", "IPV", "MD", "X", "UTC", "GMT", "ES", "PEP"];
    for word in text.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | '[' | ']' | ',' | ';' | '"' | '\'' | '`' | '<' | '>')) {
        let word = word.trim_end_matches(['.', ':', '!', '?']);
        let ticket = if word.starts_with("http") && (word.contains("/issues/") || word.contains("/pull/")) {
            Some(word.to_owned())
        } else if let Some((prefix, number)) = word.split_once('-') {
            let prefix_ok = (2..=10).contains(&prefix.len())
                && prefix.starts_with(|c: char| c.is_ascii_uppercase())
                && prefix.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
                && !NOT_TICKETS.contains(&prefix);
            let number_ok = !number.is_empty() && number.len() <= 6 && number.chars().all(|c| c.is_ascii_digit());
            (prefix_ok && number_ok).then(|| word.to_owned())
        } else {
            None
        };
        if let Some(ticket) = ticket
            && !found.contains(&ticket)
        {
            found.push(ticket);
        }
    }
}

fn shorten(text: &str, limit: usize) -> String {
    model::shorten(&text.split_whitespace().collect::<Vec<_>>().join(" "), limit)
}

/// A file path relative to the session's folder when it is inside it.
fn relative(path: &str, cwd: Option<&str>) -> String {
    let path = path.replace('\\', "/");
    if let Some(cwd) = cwd {
        let cwd = cwd.replace('\\', "/");
        let cwd = cwd.trim_end_matches('/');
        if let Some(rest) = path.strip_prefix(cwd).and_then(|rest| rest.strip_prefix('/')) {
            return rest.to_owned();
        }
    }
    path
}

/// The facts of a session, read without a model.
pub fn extract(summary: &SessionSummary, transcript: &Transcript) -> Digest {
    let mut digest = Digest::default();
    let mut tickets = Vec::new();
    let mut last_answer = String::new();
    if let Some(branch) = &summary.git_branch {
        // As written: `fix/retry-3` is no ticket, `SHOP-142-double-charge` holds one.
        tickets_in(&branch.replace('/', " "), &mut tickets);
    }
    for entry in &transcript.entries {
        for block in &entry.blocks {
            match block {
                Block::Text { text } if !text.trim().is_empty() => {
                    tickets_in(text, &mut tickets);
                    if entry.role == Role::Assistant {
                        last_answer = text.clone();
                    } else if digest.problem.is_empty() && entry.role == Role::User {
                        digest.problem = shorten(text, 240);
                    }
                }
                Block::ToolCall { input, result, .. } => {
                    let Some(command) = command_of(input) else { continue };
                    if !command.contains("git commit") {
                        continue;
                    }
                    if let Some(commit) = result.as_ref().and_then(|result| commit_line(&result.text))
                        && !digest.commits.iter().any(|known| known.hash == commit.hash)
                    {
                        tickets_in(&commit.subject, &mut tickets);
                        digest.commits.push(commit);
                    }
                }
                _ => {}
            }
        }
    }
    if digest.problem.is_empty() {
        digest.problem = shorten(summary.first_prompt.as_deref().unwrap_or(&summary.title), 240);
    }
    digest.fix = shorten(&last_answer, 300);
    digest.tickets = tickets.into_iter().take(5).collect();
    let mut files: Vec<String> = Vec::new();
    for change in transcript.changes() {
        let path = relative(&change.path, summary.cwd.as_deref());
        if !files.contains(&path) {
            files.push(path);
        }
    }
    digest.files = files;
    digest
}

/// What the model reads: the conversation without tool output, commits kept, the start and
/// mostly the end of a long one.
pub fn prompt(summary: &SessionSummary, transcript: &Transcript) -> String {
    let mut text = String::new();
    for entry in &transcript.entries {
        for block in &entry.blocks {
            match block {
                Block::Text { text: part } if !part.trim().is_empty() => {
                    let who = if entry.role == Role::User { "User" } else { "Assistant" };
                    text.push_str(&format!("{who}: {}\n", shorten(part, if entry.role == Role::User { 600 } else { 400 })));
                }
                Block::ToolCall { input, result, .. } => {
                    if let Some(command) = command_of(input).filter(|command| command.contains("git commit"))
                        && let Some(commit) = result.as_ref().and_then(|result| commit_line(&result.text))
                    {
                        let _ = command;
                        text.push_str(&format!("Commit: {} {}\n", commit.hash, commit.subject));
                    }
                }
                _ => {}
            }
        }
    }
    if text.len() > PROMPT_HEAD + PROMPT_TAIL {
        let mut head = PROMPT_HEAD;
        while !text.is_char_boundary(head) {
            head -= 1;
        }
        let mut tail = text.len() - PROMPT_TAIL;
        while !text.is_char_boundary(tail) {
            tail += 1;
        }
        text = format!("{}\n…\n{}", &text[..head], &text[tail..]);
    }
    format!(
        "Session: {}\n\n{text}\nWrite four lines:\nPROBLEM: the problem or task in one sentence\nCAUSE: what caused it, in one sentence, or \"-\"\nFIX: how it was solved, in one sentence\nKEYWORDS: 6 to 12 words someone might search for this later: symptoms, error names, components, synonyms; in English and in the language of the session",
        summary.display_title()
    )
}

/// Takes the model's four lines into the digest; a line it left out keeps what the
/// transcript gave.
pub fn apply_answer(digest: &mut Digest, answer: &str) -> bool {
    let mut any = false;
    for line in answer.lines() {
        let line = line.trim().trim_start_matches(['-', '*', ' ', '#']);
        let Some((label, value)) = line.split_once(':') else { continue };
        // Markdown a model may add: `**PROBLEM:** …`.
        let label = label.trim_matches(['*', '_', ' ']);
        let value = value.trim().trim_start_matches(['*', '_']).trim();
        if value.is_empty() {
            continue;
        }
        match label.trim().to_uppercase().as_str() {
            "PROBLEM" => digest.problem = shorten(value, 240),
            "CAUSE" if value != "-" => digest.cause = Some(shorten(value, 240)),
            "FIX" => digest.fix = shorten(value, 300),
            "KEYWORDS" => {
                digest.keywords = value.split([',', ';']).map(str::trim).filter(|word| !word.is_empty()).take(16).map(str::to_owned).collect();
            }
            _ => continue,
        }
        any = true;
    }
    digest.by_model |= any;
    any
}

/// A digest of `transcript`, with the model's account when a summarizer is given and
/// answers.
pub fn build(summary: &SessionSummary, transcript: &Transcript, summarizer: Option<&dyn Summarizer>) -> Digest {
    let mut digest = extract(summary, transcript);
    if let Some(summarizer) = summarizer
        && let Ok(answer) = summarizer.summarize(&prompt(summary, transcript))
    {
        apply_answer(&mut digest, &answer);
    }
    digest
}

impl Digest {
    /// The text kept in the library and given to the harness, without the title line.
    pub fn render(&self) -> String {
        let mut lines = vec![format!("Problem: {}", self.problem)];
        if let Some(cause) = &self.cause {
            lines.push(format!("Cause: {cause}"));
        }
        if !self.fix.is_empty() {
            lines.push(format!("{}: {}", if self.by_model { "Fix" } else { "Last answer" }, self.fix));
        }
        if !self.commits.is_empty() {
            let commits: Vec<String> = self.commits.iter().take(5).map(|c| format!("{} {}", c.hash, model::shorten(&c.subject, 80))).collect();
            lines.push(format!("Commits: {}", commits.join("; ")));
        }
        if !self.tickets.is_empty() {
            lines.push(format!("Tickets: {}", self.tickets.join(", ")));
        }
        if !self.files.is_empty() {
            let more = self.files.len().saturating_sub(6);
            let mut files = self.files.iter().take(6).cloned().collect::<Vec<_>>().join(", ");
            if more > 0 {
                files.push_str(&format!(" (+{more})"));
            }
            lines.push(format!("Files: {files}"));
        }
        if !self.keywords.is_empty() {
            lines.push(format!("Words: {}", self.keywords.join(", ")));
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::{Entry, HarnessKind, SessionKey, SourceStamp, ToolKind, ToolResult};
    use serde_json::json;

    fn summary() -> SessionSummary {
        SessionSummary {
            key: SessionKey { env: model::EnvId::local(), harness: HarnessKind::Claude, id: "s".into() },
            title: "Double charge".into(),
            first_prompt: Some("SHOP-142: customers are charged twice".into()),
            cwd: Some("/w/shop".into()),
            git_branch: Some("fix/shop-142".into()),
            model: None,
            created_ms: 1,
            updated_ms: 2,
            parent: None,
            source: SourceStamp::default(),
            turn: None,
        }
    }

    fn entry(role: Role, blocks: Vec<Block>) -> Entry {
        Entry { id: String::new(), role, timestamp_ms: 0, blocks, model: None, usage: None, compact_summary: false }
    }

    fn call(name: &str, kind: ToolKind, input: Value, output: &str) -> Block {
        Block::ToolCall { id: "t".into(), name: name.into(), kind, input, result: Some(ToolResult { text: output.into(), is_error: false }) }
    }

    fn transcript() -> Transcript {
        Transcript {
            entries: vec![
                entry(Role::User, vec![Block::Text { text: "SHOP-142: customers are charged twice when the provider times out".into() }]),
                entry(Role::Assistant, vec![
                    call("Edit", ToolKind::Edit, json!({"file_path": "/w/shop/src/payment/retry.rs", "old_string": "a", "new_string": "b"}), "ok"),
                    call("Bash", ToolKind::Shell, json!({"command": "git commit -am \"Reuse the idempotency key (SHOP-142)\""}),
                        "[fix/shop-142 a3f9c21] Reuse the idempotency key on timeouts (SHOP-142)\n 2 files changed"),
                    call("Bash", ToolKind::Shell, json!({"command": ["bash", "-lc", "git commit -m x"]}), "[main (root-commit) 0badc0de1] First\n"),
                ]),
                entry(Role::Assistant, vec![Block::Text { text: "Fixed: retries reuse the key; see UTF-8 and SHA-256 notes, and https://git.example/shop/issues/9.".into() }]),
            ],
            ..Transcript::default()
        }
    }

    #[test]
    fn the_facts_come_from_commits_prompts_and_edits() {
        let digest = extract(&summary(), &transcript());
        assert_eq!(digest.commits, vec![
            Commit { hash: "a3f9c21".into(), subject: "Reuse the idempotency key on timeouts (SHOP-142)".into() },
            Commit { hash: "0badc0de1".into(), subject: "First".into() },
        ]);
        assert_eq!(digest.tickets, vec!["SHOP-142".to_owned(), "https://git.example/shop/issues/9".to_owned()]);
        let mut branch = summary();
        branch.git_branch = Some("fix/retry-3".into());
        branch.first_prompt = None;
        let mut silent = transcript();
        silent.entries.clear();
        assert!(extract(&branch, &silent).tickets.is_empty(), "a lower-case branch is no ticket");
        assert_eq!(digest.files, vec!["src/payment/retry.rs".to_owned()]);
        assert!(digest.problem.starts_with("SHOP-142: customers are charged twice"));
        assert!(digest.fix.starts_with("Fixed: retries reuse the key"));
        let text = digest.render();
        assert!(text.contains("Commits: a3f9c21 Reuse") && text.contains("Last answer: Fixed"), "{text}");
    }

    struct Fake(&'static str);
    impl Summarizer for Fake {
        fn summarize(&self, prompt: &str) -> Result<String, String> {
            assert!(prompt.contains("Commit: a3f9c21") && prompt.contains("KEYWORDS:"), "{prompt}");
            Ok(self.0.to_owned())
        }
    }

    #[test]
    fn a_model_adds_problem_cause_fix_and_search_words() {
        let fake = Fake("PROBLEM: Customers paid twice.\nCAUSE: -\nFIX: Retries reuse one key.\nKEYWORDS: double charge, duplicate payment, timeout");
        let digest = build(&summary(), &transcript(), Some(&fake));
        assert!(digest.by_model && digest.cause.is_none());
        assert_eq!(digest.problem, "Customers paid twice.");
        assert_eq!(digest.keywords, ["double charge", "duplicate payment", "timeout"]);
        assert!(digest.render().contains("Fix: Retries reuse one key.\nCommits: a3f9c21"));
        struct Broken;
        impl Summarizer for Broken {
            fn summarize(&self, _: &str) -> Result<String, String> {
                Err("no model".into())
            }
        }
        let plain = build(&summary(), &transcript(), Some(&Broken));
        assert_eq!(plain, extract(&summary(), &transcript()));
        let mut bold = extract(&summary(), &transcript());
        assert!(apply_answer(&mut bold, "**PROBLEM:** Paid twice.\n- **FIX:** One key.\n## KEYWORDS: double charge"));
        assert_eq!((bold.problem.as_str(), bold.fix.as_str()), ("Paid twice.", "One key."));
        assert_eq!(bold.keywords, ["double charge"]);
    }

    #[test]
    fn a_long_conversation_reaches_the_model_by_its_start_and_end() {
        let mut long = transcript();
        for i in 0..400 {
            long.entries.insert(1, entry(Role::Assistant, vec![Block::Text { text: format!("step {i} of a long and winding investigation") }]));
        }
        let text = prompt(&summary(), &long);
        assert!(text.len() < PROMPT_HEAD + PROMPT_TAIL + 1_000);
        assert!(text.contains("SHOP-142: customers") && text.contains("Commit: a3f9c21") && text.contains('…'));
    }
}
