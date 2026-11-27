//! A turn over the Agent Client Protocol (newline-delimited JSON-RPC 2.0 on the harness's
//! standard streams): `initialize`, `session/new` or `session/resume`, the model as a
//! config option, one `session/prompt`. Updates become blocks of the turn; a
//! `session/request_permission` goes to the window, which answers through the channel of
//! the event, or is answered here when the mode of the turn allows the action.
//! Checked against `opencode acp` 2.3.

use std::io::Write;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures_channel::mpsc::UnboundedSender;
use model::{Block, Entry, Role, ToolKind, ToolResult, Transcript, Usage};
use serde_json::{Value, json};

use crate::{LiveEvent, Permission, TurnSpec, now_ms, prompt_entry};

/// ACP version 1, the one agents speak.
const PROTOCOL_VERSION: u64 = 1;

/// A question of the agent before it acts: the tool call and the answers it offers.
#[derive(Clone, Debug)]
pub struct PermissionAsk {
    pub tool_call_id: String,
    /// What the agent calls the action: the command line for a shell call.
    pub title: String,
    /// `execute`, `edit`, `read`, `fetch`… as ACP names the kinds of tools.
    pub kind: String,
    /// The answers, in the agent's order: `(option id, kind, label)`, the kind being
    /// `allow_once`, `allow_always`, `reject_once` or `reject_always`.
    pub options: Vec<(String, String, String)>,
    /// The questions of a harness that asks the user (Claude's `AskUserQuestion` with
    /// several questions or several answers): the window shows each with its choices and
    /// answers with [`PermissionAsk::answers`]. Empty for a question about an action.
    pub questions: Vec<AskQuestion>,
    /// The chosen option id; `None`, or a dropped sender, cancels the question.
    pub reply: mpsc::Sender<Option<String>>,
}

/// One question of the harness to the user, with the labels of its choices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskQuestion {
    pub text: String,
    /// A short label of the question, if the harness gave one.
    pub header: String,
    pub choices: Vec<String>,
    /// Several choices may be picked.
    pub multi: bool,
}

/// The answer of the user to one question: the picked choices and a text of their own.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Answer {
    pub picked: Vec<usize>,
    pub text: String,
}

impl Answer {
    /// Nothing picked and nothing written.
    pub fn is_empty(&self) -> bool {
        self.picked.is_empty() && self.text.trim().is_empty()
    }
}

impl PermissionAsk {
    /// The reply to a question of the harness: the answer to each of its questions, in
    /// order (one for a question shown with its choices as buttons).
    pub fn answers(answers: &[Answer]) -> String {
        let answers: Vec<Value> = answers.iter().map(|answer| json!({"picked": answer.picked, "text": answer.text.trim()})).collect();
        format!("answers:{}", Value::Array(answers))
    }
}

/// The answers of a reply made by [`PermissionAsk::answers`].
pub(crate) fn parse_answers(reply: &str) -> Option<Vec<Answer>> {
    let answers: Value = serde_json::from_str(reply.strip_prefix("answers:")?).ok()?;
    answers
        .as_array()?
        .iter()
        .map(|answer| {
            let picked = answer.get("picked")?.as_array()?.iter().map(|n| n.as_u64().map(|n| n as usize)).collect::<Option<Vec<usize>>>()?;
            let text = answer.get("text").and_then(Value::as_str).unwrap_or("").to_owned();
            Some(Answer { picked, text })
        })
        .collect()
}

/// Writes lines to the agent from the turn's thread and from the threads that wait for
/// the user's answers.
#[derive(Clone)]
struct Writer(Arc<Mutex<Box<dyn Write + Send>>>);

impl Writer {
    fn send(&self, message: &Value) -> bool {
        let Ok(mut out) = self.0.lock() else { return false };
        writeln!(out, "{message}").and_then(|_| out.flush()).is_ok()
    }
}

/// The option of `kind` the agent offered, if any.
fn option_of(options: &[(String, String, String)], kinds: &[&str]) -> Option<String> {
    kinds.iter().find_map(|kind| options.iter().find(|(_, k, _)| k == kind).map(|(id, _, _)| id.clone()))
}

/// The answer the mode of the turn gives by itself, without asking the user.
fn automatic_answer(permission: Permission, kind: &str, options: &[(String, String, String)]) -> Option<String> {
    let allowed = match permission {
        Permission::Settings => false,
        Permission::AcceptEdits => kind == "edit",
        Permission::Bypass => true,
    };
    if allowed { option_of(options, &["allow_once", "allow_always"]) } else { None }
}

fn tool_kind(kind: &str, name: &str) -> ToolKind {
    match kind {
        "execute" => ToolKind::Shell,
        "edit" | "delete" | "move" => ToolKind::Edit,
        "read" | "search" => ToolKind::Read,
        "fetch" => ToolKind::Web,
        _ => ToolKind::classify(name),
    }
}

fn text_of(content: &Value) -> Option<String> {
    match content.get("type")?.as_str()? {
        "text" => content.get("text").and_then(Value::as_str).map(str::to_owned),
        "content" => text_of(content.get("content")?),
        "diff" => Some(format!("edited {}", content.get("path").and_then(Value::as_str).unwrap_or(""))),
        _ => None,
    }
}

/// The text of a tool result: its content blocks, else the raw output.
fn output_of(update: &Value) -> Option<String> {
    let texts: Vec<String> = update.get("content").and_then(Value::as_array).into_iter().flatten().filter_map(text_of).collect();
    if !texts.is_empty() {
        return Some(texts.join("\n"));
    }
    match update.get("rawOutput")? {
        Value::String(text) => Some(text.clone()),
        Value::Null => None,
        other => other.get("output").and_then(Value::as_str).map(str::to_owned).or_else(|| Some(other.to_string())),
    }
}

/// The turn as it grows from `session/update` notifications.
pub(crate) struct TurnBuilder {
    pub turn: Transcript,
}

impl TurnBuilder {
    pub fn new(prompt: &[Block]) -> Self {
        let mut turn = Transcript::default();
        turn.entries.push(prompt_entry(prompt, now_ms()));
        Self { turn }
    }

    fn answer(&mut self) -> &mut Vec<Block> {
        if self.turn.entries.last().is_none_or(|last| last.role != Role::Assistant) {
            self.turn.entries.push(Entry {
                id: "live-answer".into(),
                role: Role::Assistant,
                timestamp_ms: now_ms(),
                blocks: Vec::new(),
                model: None,
                usage: None,
                compact_summary: false,
            });
        }
        &mut self.turn.entries.last_mut().expect("an answer").blocks
    }

    fn tool_call(&mut self, id: &str) -> Option<&mut Block> {
        self.answer().iter_mut().find(|block| matches!(block, Block::ToolCall { id: other, .. } if other == id))
    }

    /// Takes one update; true when the turn changed.
    pub fn take(&mut self, update: &Value) -> bool {
        let field = |name: &str| update.get(name).and_then(Value::as_str).unwrap_or("").to_owned();
        match update.get("sessionUpdate").and_then(Value::as_str).unwrap_or("") {
            kind @ ("agent_message_chunk" | "agent_thought_chunk") => {
                let Some(text) = update.get("content").and_then(text_of) else { return false };
                let thinking = kind == "agent_thought_chunk";
                let blocks = self.answer();
                match blocks.last_mut() {
                    Some(Block::Text { text: last }) if !thinking => last.push_str(&text),
                    Some(Block::Thinking { text: last }) if thinking => last.push_str(&text),
                    _ => blocks.push(if thinking { Block::Thinking { text } } else { Block::Text { text } }),
                }
                true
            }
            "tool_call" => {
                let id = field("toolCallId");
                let name = Some(field("name")).filter(|n| !n.is_empty()).unwrap_or_else(|| field("title"));
                let block = Block::ToolCall {
                    id: id.clone(),
                    kind: tool_kind(&field("kind"), &name),
                    name,
                    input: update.get("rawInput").cloned().unwrap_or(Value::Null),
                    result: None,
                };
                match self.tool_call(&id) {
                    Some(existing) => *existing = block,
                    None => self.answer().push(block),
                }
                self.take_status(&id, update);
                true
            }
            "tool_call_update" => {
                let id = field("toolCallId");
                if self.tool_call(&id).is_none() {
                    let name = field("title");
                    self.answer().push(Block::ToolCall { id: id.clone(), kind: tool_kind(&field("kind"), &name), name, input: Value::Null, result: None });
                }
                if let Some(Block::ToolCall { input, .. }) = self.tool_call(&id)
                    && let Some(raw) = update.get("rawInput").filter(|raw| !raw.is_null())
                {
                    *input = raw.clone();
                }
                self.take_status(&id, update);
                true
            }
            "usage_update" => {
                let used = update.get("used").and_then(Value::as_u64).unwrap_or(0);
                self.turn.last_usage = Some(Usage { input: used, ..Default::default() });
                if let Some(cost) = update.pointer("/cost/amount").and_then(Value::as_f64).filter(|c| *c > 0.0) {
                    self.turn.cost_usd = Some(cost);
                }
                false
            }
            _ => false,
        }
    }

    fn take_status(&mut self, id: &str, update: &Value) {
        let status = update.get("status").and_then(Value::as_str).unwrap_or("");
        if !matches!(status, "completed" | "failed") {
            return;
        }
        let text = output_of(update).unwrap_or_default();
        if let Some(Block::ToolCall { result, .. }) = self.tool_call(id) {
            *result = Some(ToolResult { text, is_error: status == "failed" });
        }
    }
}

/// The prompt as ACP content: the text, pictures inline, other files as links.
fn prompt_content(spec: &TurnSpec) -> Vec<Value> {
    let mut content = Vec::new();
    // A picture alone is a prompt too; an empty text block is not.
    if !spec.prompt.is_empty() || spec.attachments.is_empty() {
        content.push(json!({"type": "text", "text": spec.prompt}));
    }
    for attachment in &spec.attachments {
        match &attachment.image {
            Some((media_type, bytes)) => content.push(json!({"type": "image", "mimeType": media_type, "data": BASE64.encode(bytes.as_slice())})),
            None => {
                let path = attachment.path.replace('\\', "/");
                let name = path.rsplit('/').next().unwrap_or(&path).to_owned();
                content.push(json!({"type": "resource_link", "uri": file_uri(&path), "name": name}));
            }
        }
    }
    content
}

/// Runs one turn over the agent's streams. Returns the error that ended it, if any.
pub(crate) fn run_turn(
    stdin: impl Write + Send + 'static,
    mut lines: impl Iterator<Item = String>,
    spec: &TurnSpec,
    prompt: &[Block],
    sender: &UnboundedSender<LiveEvent>,
) -> Option<String> {
    let writer = Writer(Arc::new(Mutex::new(Box::new(stdin))));
    let mut builder = TurnBuilder::new(prompt);
    let mut next_id = 0u64;
    // Messages before the answer to `id`: updates go to the turn, questions to the window.
    // Updates of other sessions (a subagent's own session) are not this turn's answer; their
    // questions are still the user's to answer.
    let mut call = |method: &str, params: Value, builder: &mut TurnBuilder, quiet: bool, session: Option<&str>| -> Result<Value, String> {
        next_id += 1;
        let id = next_id;
        if !writer.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})) {
            return Err("the harness closed its input".into());
        }
        for line in lines.by_ref() {
            let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
            let method = message.get("method").and_then(Value::as_str);
            match (method, message.get("id")) {
                (None, Some(answer)) if answer.as_u64() == Some(id) => {
                    return match message.get("error") {
                        Some(error) => Err(error.get("message").and_then(Value::as_str).unwrap_or("error").to_owned()),
                        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                    };
                }
                (Some("session/update"), None) => {
                    let other = session.is_some_and(|session| message.pointer("/params/sessionId").and_then(Value::as_str).is_some_and(|id| id != session));
                    if !quiet && !other && builder.take(message.pointer("/params/update").unwrap_or(&Value::Null)) {
                        let _ = sender.unbounded_send(LiveEvent::Update(builder.turn.clone()));
                    }
                }
                (Some("session/request_permission"), Some(request)) => ask(&writer, request.clone(), &message["params"], spec.permission, sender),
                (Some(_), Some(request)) => {
                    // No file system or terminal was offered: the agent works on its own.
                    writer.send(&json!({"jsonrpc": "2.0", "id": request, "error": {"code": -32601, "message": "not offered by Bowyer"}}));
                }
                _ => {}
            }
        }
        Err("the harness exited".into())
    };

    let init = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
        "clientInfo": {"name": "bowyer", "version": env!("CARGO_PKG_VERSION")},
    });
    if let Err(error) = call("initialize", init, &mut builder, false, None) {
        return Some(format!("ACP initialize: {error}"));
    }
    let session = match &spec.session {
        Some(id) => {
            let params = json!({"sessionId": id, "cwd": spec.cwd, "mcpServers": []});
            // `resume` goes on without replaying the history; `load` replays it, and the
            // replay is not this turn.
            if call("session/resume", params.clone(), &mut builder, true, None).is_err()
                && let Err(error) = call("session/load", params, &mut builder, true, None)
            {
                return Some(format!("ACP session/load: {error}"));
            }
            id.clone()
        }
        None => match call("session/new", json!({"cwd": spec.cwd, "mcpServers": []}), &mut builder, false, None) {
            Ok(result) => match result.get("sessionId").and_then(Value::as_str) {
                Some(id) => id.to_owned(),
                None => return Some("ACP session/new gave no session id".into()),
            },
            Err(error) => return Some(format!("ACP session/new: {error}")),
        },
    };
    let _ = sender.unbounded_send(LiveEvent::Session(session.clone()));
    if let Some(model) = &spec.model {
        // The model as a config option (OpenCode), or by the older `session/set_model`
        // (Antigravity's `agy` has both).
        let params = json!({"sessionId": session, "configId": "model", "value": model});
        if let Err(error) = call("session/set_config_option", params, &mut builder, false, Some(&session)) {
            let params = json!({"sessionId": session, "modelId": model});
            if call("session/set_model", params, &mut builder, false, Some(&session)).is_err() {
                return Some(format!("the model {model}: {error}"));
            }
        }
    }
    let _ = sender.unbounded_send(LiveEvent::Update(builder.turn.clone()));
    let params = json!({"sessionId": session, "prompt": prompt_content(spec)});
    match call("session/prompt", params, &mut builder, false, Some(&session)) {
        Ok(result) => {
            let _ = sender.unbounded_send(LiveEvent::Update(builder.turn.clone()));
            match result.get("stopReason").and_then(Value::as_str).unwrap_or("end_turn") {
                "end_turn" | "cancelled" => None,
                other => Some(format!("the turn ended: {other}")),
            }
        }
        Err(error) => Some(error),
    }
}

/// Answers a permission question at once when the mode allows the action; otherwise asks
/// the window and answers from another thread when the user has chosen.
fn ask(writer: &Writer, request: Value, params: &Value, permission: Permission, sender: &UnboundedSender<LiveEvent>) {
    let call = params.get("toolCall").unwrap_or(&Value::Null);
    let text = |value: &Value, name: &str| value.get(name).and_then(Value::as_str).unwrap_or("").to_owned();
    let options: Vec<(String, String, String)> = params
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|option| (text(option, "optionId"), text(option, "kind"), text(option, "name")))
        .filter(|(id, _, _)| !id.is_empty())
        .collect();
    let kind = text(call, "kind");
    let reply = move |writer: &Writer, choice: Option<String>| {
        let outcome = match choice {
            Some(option) => json!({"outcome": "selected", "optionId": option}),
            None => json!({"outcome": "cancelled"}),
        };
        writer.send(&json!({"jsonrpc": "2.0", "id": request, "result": {"outcome": outcome}}));
    };
    if let Some(choice) = automatic_answer(permission, &kind, &options) {
        reply(writer, Some(choice));
        return;
    }
    let mut title = text(call, "title");
    if let Some(command) = call.pointer("/rawInput/command").and_then(Value::as_str) {
        title = command.to_owned();
    }
    let (answer, answers) = mpsc::channel();
    let ask = PermissionAsk { tool_call_id: text(call, "toolCallId"), title, kind, options, questions: Vec::new(), reply: answer };
    if sender.unbounded_send(LiveEvent::Permission(ask)).is_err() {
        reply(writer, None);
        return;
    }
    let writer = writer.clone();
    thread::spawn(move || reply(&writer, answers.recv().ok().flatten()));
}

/// The `file:` URI of a path, `/` or `\` separated: a space, `#`, `%` or a letter
/// outside ASCII in it is percent-encoded, as a URI needs, so the harness reads the path
/// back as it was.
fn file_uri(path: &str) -> String {
    let path = path.replace('\\', "/");
    let mut uri = String::from("file:///");
    for byte in path.trim_start_matches('/').bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~' | b':') {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use super::*;
    use crate::{Attachment, Permission, TurnSpec};
    use futures_channel::mpsc::unbounded;
    use model::HarnessKind;

    fn spec(session: Option<&str>, permission: Permission) -> TurnSpec {
        TurnSpec {
            harness: HarnessKind::OpenCode,
            session: session.map(str::to_owned),
            cwd: "/w/app".into(),
            prompt: "Run echo".into(),
            model: Some("opencode-go/deepseek-v4.1-flash".into()),
            effort: None,
            permission,
            ssh: None,
            attachments: vec![Attachment { path: "/w/app/notes.txt".into(), image: None }],
            ide: None,
            drop_turn: None,
            fork_at: None,
            mcp_config: None,
        }
    }

    /// What `opencode acp` 2.3 sent in a real turn, shortened: a shell call that asks,
    /// its output, the answer and the usage.
    fn agent_lines(answer_to_prompt: u64) -> Vec<String> {
        [
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}}"#.to_owned(),
            r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"ses_1"}}"#.to_owned(),
            r#"{"jsonrpc":"2.0","id":3,"result":{"configOptions":[]}}"#.to_owned(),
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_1","update":{"sessionUpdate":"tool_call","toolCallId":"c1","name":"shell","title":"shell","kind":"execute","status":"pending","rawInput":{}}}}"#.to_owned(),
            r#"{"jsonrpc":"2.0","id":0,"method":"session/request_permission","params":{"sessionId":"ses_1","toolCall":{"toolCallId":"c1","title":"echo hi","kind":"execute","rawInput":{"command":"echo hi"}},"options":[{"optionId":"once","kind":"allow_once","name":"Allow once"},{"optionId":"always","kind":"allow_always","name":"Always allow"},{"optionId":"reject","kind":"reject_once","name":"Reject"}]}}"#.to_owned(),
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_1","update":{"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"in_progress","kind":"execute","title":"echo hi","rawInput":{"command":"echo hi"}}}}"#.to_owned(),
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_1","update":{"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"completed","content":[{"type":"content","content":{"type":"text","text":"hi\n"}}],"rawOutput":{"metadata":{"exit":0}}}}}"#.to_owned(),
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_1","update":{"sessionUpdate":"agent_message_chunk","messageId":"m1","content":{"type":"text","text":"do"}}}}"#.to_owned(),
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_1","update":{"sessionUpdate":"agent_message_chunk","messageId":"m1","content":{"type":"text","text":"ne"}}}}"#.to_owned(),
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_1","update":{"sessionUpdate":"usage_update","used":9748,"size":1000000,"cost":{"amount":0,"currency":"USD"}}}}"#.to_owned(),
            format!(r#"{{"jsonrpc":"2.0","id":{answer_to_prompt},"result":{{"stopReason":"end_turn"}}}}"#),
        ]
        .to_vec()
    }

    /// The agent's input, kept for the test to read.
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The messages written so far: whole lines only, as another thread may be halfway
    /// through one.
    fn sent(sink: &Sink) -> Vec<Value> {
        let text = String::from_utf8_lossy(&sink.0.lock().unwrap()).into_owned();
        let whole = &text[..text.rfind('\n').map_or(0, |end| end + 1)];
        whole.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    #[test]
    fn a_turn_asks_the_window_and_answers_with_its_choice() {
        let sink = Sink::default();
        let (sender, mut events) = unbounded();
        let spec = spec(None, Permission::Settings);
        // The window chooses as soon as it is asked.
        let answering = std::thread::spawn(move || {
            loop {
                match events.try_next() {
                    Ok(Some(LiveEvent::Permission(ask))) => {
                        assert_eq!(ask.title, "echo hi");
                        assert_eq!(ask.kind, "execute");
                        assert_eq!(ask.options.len(), 3);
                        ask.reply.send(Some("once".into())).unwrap();
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => return,
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
                }
            }
        });
        let error = run_turn(sink.clone(), agent_lines(4).into_iter(), &spec, &[Block::Text { text: "Run echo".into() }], &sender);
        assert_eq!(error, None);
        drop(sender);
        answering.join().unwrap();
        // The answer goes from another thread: give it a moment.
        for _ in 0..100 {
            if sent(&sink).iter().any(|m| m["id"] == 0) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let messages = sent(&sink);
        let methods: Vec<&str> = messages.iter().filter_map(|m| m["method"].as_str()).collect();
        assert_eq!(methods, ["initialize", "session/new", "session/set_config_option", "session/prompt"]);
        let prompt = &messages.iter().find(|m| m["method"] == "session/prompt").unwrap()["params"]["prompt"];
        assert_eq!(prompt[1]["type"], "resource_link");
        assert_eq!(prompt[1]["uri"], "file:///w/app/notes.txt");
        assert_eq!(file_uri("/w/my notes/über#1.txt"), "file:///w/my%20notes/%C3%BCber%231.txt");
        assert_eq!(file_uri(r"C:\w\a b.txt"), "file:///C:/w/a%20b.txt");
        let answer = messages.iter().find(|m| m["id"] == 0 && m.get("result").is_some()).expect("the question is answered");
        assert_eq!(answer["result"]["outcome"]["optionId"], "once");
    }

    #[test]
    fn the_turn_becomes_blocks_and_bypass_answers_by_itself() {
        let sink = Sink::default();
        let (sender, mut events) = unbounded();
        let error = run_turn(sink.clone(), agent_lines(4).into_iter(), &spec(Some("ses_1"), Permission::Bypass), &[], &sender);
        assert_eq!(error, None);
        let messages = sent(&sink);
        assert_eq!(messages[1]["method"], "session/resume");
        let answer = messages.iter().find(|m| m["id"] == 0).expect("answered at once");
        assert_eq!(answer["result"]["outcome"]["optionId"], "once");
        let mut last = None;
        let mut asked = false;
        while let Ok(Some(event)) = events.try_next() {
            match event {
                LiveEvent::Update(turn) => last = Some(turn),
                LiveEvent::Permission(_) => asked = true,
                _ => {}
            }
        }
        assert!(!asked, "bypass never asks");
        let turn = last.unwrap();
        let blocks = &turn.entries.last().unwrap().blocks;
        assert!(matches!(&blocks[0], Block::ToolCall { kind: ToolKind::Shell, input, result: Some(r), .. } if input["command"] == "echo hi" && r.text == "hi\n"));
        assert!(matches!(&blocks[1], Block::Text { text } if text == "done"), "chunks join: {blocks:?}");
        assert_eq!(turn.last_usage.map(|u| u.context()), Some(9748));
    }

    /// A tiny ACP agent in Python: it asks before a command and says what it was told.
    const STUB_AGENT: &str = r#"
import json, sys
def send(m):
    sys.stdout.write(json.dumps(m) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    m = json.loads(line)
    method, i = m.get("method"), m.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": i, "result": {"protocolVersion": 1}})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": i, "result": {"sessionId": "stub-1"}})
    elif method == "session/set_config_option":
        send({"jsonrpc": "2.0", "id": i, "result": {}})
    elif method == "session/prompt":
        prompt = i
        send({"jsonrpc": "2.0", "id": 900, "method": "session/request_permission", "params": {"sessionId": "stub-1",
              "toolCall": {"toolCallId": "c1", "title": "uname", "kind": "execute", "rawInput": {"command": "uname -s"}},
              "options": [{"optionId": "yes", "kind": "allow_once", "name": "Allow"}, {"optionId": "no", "kind": "reject_once", "name": "No"}]}})
    elif i == 900:
        choice = m["result"]["outcome"].get("optionId")
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "stub-1", "update": {
              "sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "chosen " + str(choice)}}}})
        send({"jsonrpc": "2.0", "id": prompt, "result": {"stopReason": "end_turn"}})
"#;

    /// The protocol both ways over SSH, with a question answered from the window's side:
    /// `BOWYER_SSH_HOST=user@host BOWYER_SSH_KEY=… cargo test -p live -- --ignored`. Puts a
    /// stub agent into the host's temporary folder; no harness or model runs.
    #[test]
    #[ignore = "needs an SSH host with python3"]
    fn a_turn_talks_acp_over_ssh() {
        use envs::Environment;
        use std::io::BufRead;
        let host = std::env::var("BOWYER_SSH_HOST").expect("BOWYER_SSH_HOST");
        let key = std::env::var_os("BOWYER_SSH_KEY").map(std::path::PathBuf::from);
        let ssh = envs::SshEnvironment::new(&host).with_identity(key);
        let stub = ssh.run("mktemp", &["-t", "bowyer-acp-stub.XXXXXX"], None).unwrap().trim().to_owned();
        ssh.run_with_input("sh", &["-c", "cat > \"$1\"", "sh", &stub], None, STUB_AGENT.as_bytes()).unwrap();
        let mut command = ssh.headless("python3", &["-u", &stub], Some("/tmp"));
        command.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines().map_while(Result::ok);
        let (sender, mut events) = unbounded();
        let answering = std::thread::spawn(move || {
            loop {
                match events.try_next() {
                    Ok(Some(LiveEvent::Permission(ask))) => {
                        assert_eq!(ask.title, "uname -s");
                        ask.reply.send(Some("yes".into())).unwrap();
                    }
                    Ok(Some(LiveEvent::Update(turn))) if turn.search_text(1000).contains("chosen yes") => return true,
                    Ok(Some(_)) => {}
                    Ok(None) => return false,
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
                }
            }
        });
        let mut spec = spec(None, Permission::Settings);
        spec.attachments.clear();
        let error = run_turn(stdin, lines, &spec, &[], &sender);
        drop(sender);
        let _ = child.kill();
        let _ = ssh.run("rm", &["-f", "--", &stub], None);
        assert_eq!(error, None);
        assert!(answering.join().unwrap(), "the answer chosen in the window reached the agent over SSH");
    }

    #[test]
    fn a_subagent_session_stays_out_and_a_picture_alone_has_no_text() {
        let mut lines = agent_lines(4);
        lines.insert(7, r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_child","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"CHILD"}}}}"#.to_owned());
        let (sender, mut events) = unbounded();
        let error = run_turn(Sink::default(), lines.into_iter(), &spec(None, Permission::Bypass), &[], &sender);
        assert_eq!(error, None);
        let mut last = None;
        while let Ok(Some(event)) = events.try_next() {
            if let LiveEvent::Update(turn) = event {
                last = Some(turn);
            }
        }
        assert!(!last.unwrap().search_text(1000).contains("CHILD"));

        let mut picture = spec(None, Permission::Settings);
        picture.prompt.clear();
        picture.attachments = vec![Attachment { path: "/w/a.png".into(), image: Some(("image/png".into(), Arc::new(vec![1]))) }];
        let content = prompt_content(&picture);
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "image");
    }

    #[test]
    fn edits_pass_by_themselves_only_when_edits_are_accepted() {
        let options = vec![("once".to_owned(), "allow_once".to_owned(), String::new()), ("no".to_owned(), "reject_once".to_owned(), String::new())];
        assert_eq!(automatic_answer(Permission::AcceptEdits, "edit", &options).as_deref(), Some("once"));
        assert_eq!(automatic_answer(Permission::AcceptEdits, "execute", &options), None);
        assert_eq!(automatic_answer(Permission::Settings, "edit", &options), None);
        assert_eq!(automatic_answer(Permission::Bypass, "execute", &options).as_deref(), Some("once"));
    }
}
