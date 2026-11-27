//! A turn of Pi in RPC mode (`pi --mode rpc`): JSON lines both ways on the standard
//! streams. The window asks `get_state` for the session id, sends one `prompt` with the
//! pictures inline, and reads the session events until `agent_settled`, when Pi has no
//! more work of its own; closing its input then ends it. Messages come whole in
//! `message_end`; the answer being written is put together from the deltas of
//! `message_update`. A dialog an extension opens (`extension_ui_request`) is cancelled:
//! the window shows none. Written from the RPC documentation of Pi 1.6.1 (`docs/rpc.md`,
//! `docs/rpc-commands.md`, `docs/json.md`); not yet seen with a real turn.

use std::io::Write;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures_channel::mpsc::UnboundedSender;
use model::{Block, Transcript};
use serde_json::{Value, json};

use crate::{LiveEvent, PARTIAL_EVERY, TurnSpec, now_ms, prompt_entry, prompt_text};

/// The prompt command: the text with the paths of the files, the pictures inline.
pub(crate) fn prompt_command(spec: &TurnSpec) -> Value {
    let images: Vec<Value> = spec
        .attachments
        .iter()
        .filter_map(|a| a.image.as_ref())
        .map(|(media_type, bytes)| json!({"type": "image", "data": BASE64.encode(bytes.as_slice()), "mimeType": media_type}))
        .collect();
    let mut command = json!({"id": "bowyer-prompt", "type": "prompt", "message": prompt_text(spec)});
    if !images.is_empty() {
        command["images"] = Value::Array(images);
    }
    command
}

/// The answer being written: a message whose content grows by block.
#[derive(Default)]
struct Partial {
    message: Option<Value>,
    sent: Option<std::time::Instant>,
}

impl Partial {
    fn block(&mut self, index: usize, empty: Value) -> Option<&mut Value> {
        let content = self.message.as_mut()?.get_mut("content")?.as_array_mut()?;
        while content.len() <= index {
            content.push(empty.clone());
        }
        content.get_mut(index)
    }

    /// Takes one `assistantMessageEvent`; true when the answer changed.
    fn take(&mut self, event: &Value) -> bool {
        let index = event.get("contentIndex").and_then(Value::as_u64).unwrap_or(0) as usize;
        let delta = event.get("delta").and_then(Value::as_str).unwrap_or("");
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "text_delta" => self.append(index, json!({"type": "text", "text": ""}), "text", delta),
            "thinking_delta" => self.append(index, json!({"type": "thinking", "thinking": ""}), "thinking", delta),
            "text_end" | "thinking_end" => {
                let (kind, field) = if event["type"] == "text_end" { ("text", "text") } else { ("thinking", "thinking") };
                let Some(content) = event.get("content") else { return false };
                // The whole block: a string, or the block itself.
                let whole = match content.as_str() {
                    Some(text) => json!({"type": kind, field: text}),
                    None => content.clone(),
                };
                self.set(index, whole)
            }
            "toolcall_start" => {
                let call = json!({"type": "toolCall", "id": event.get("id").cloned().unwrap_or(Value::Null), "name": event.get("toolName").cloned().unwrap_or(Value::Null), "arguments": {}});
                self.set(index, call)
            }
            "toolcall_end" => match event.get("toolCall") {
                Some(call) => self.set(index, call.clone()),
                None => false,
            },
            _ => false,
        }
    }

    fn append(&mut self, index: usize, empty: Value, field: &str, delta: &str) -> bool {
        let Some(block) = self.block(index, empty) else { return false };
        let text = block.get(field).and_then(Value::as_str).unwrap_or("").to_owned() + delta;
        block[field] = Value::String(text);
        true
    }

    fn set(&mut self, index: usize, whole: Value) -> bool {
        match self.block(index, json!({"type": "text", "text": ""})) {
            Some(block) => {
                *block = whole;
                true
            }
            None => false,
        }
    }
}

/// The turn so far: the prompt, then the messages Pi wrote and the answer being written.
fn turn_of(prompt: &[Block], started: i64, messages: &[Value], partial: &Partial) -> Transcript {
    let mut all: Vec<Value> = messages.to_vec();
    all.extend(partial.message.clone());
    let mut turn = history::pi_transcript_of_messages(&all);
    turn.entries.insert(0, prompt_entry(prompt, started));
    turn
}

pub(crate) fn run_turn(
    mut stdin: impl Write + Send + 'static,
    lines: impl Iterator<Item = String>,
    spec: &TurnSpec,
    prompt: &[Block],
    sender: &UnboundedSender<LiveEvent>,
) -> Option<String> {
    let mut send = |command: &Value| writeln!(stdin, "{command}").and_then(|()| stdin.flush()).is_ok();
    if !send(&json!({"id": "bowyer-state", "type": "get_state"})) || !send(&prompt_command(spec)) {
        return Some("Pi closed its input".into());
    }
    let started = now_ms();
    let mut messages: Vec<Value> = Vec::new();
    let mut partial = Partial::default();
    let mut error = None;
    let mut settled = false;
    for line in lines {
        let Ok(record) = serde_json::from_str::<Value>(&line) else { continue };
        let changed = match record.get("type").and_then(Value::as_str).unwrap_or("") {
            "response" => {
                match (record.get("command").and_then(Value::as_str), record.get("success").and_then(Value::as_bool)) {
                    (Some("get_state"), Some(true)) => {
                        if let Some(id) = record.pointer("/data/sessionId").and_then(Value::as_str) {
                            let _ = sender.unbounded_send(LiveEvent::Session(id.to_owned()));
                        }
                    }
                    (Some("prompt"), Some(true)) if record.pointer("/data/disposition").and_then(Value::as_str) == Some("handled") => {
                        // A command of an extension took the prompt: no run follows.
                        settled = true;
                        break;
                    }
                    (Some(command), Some(false)) if command == "prompt" || command == "parse" => {
                        error = Some(record.get("error").and_then(Value::as_str).unwrap_or("Pi refused the prompt").to_owned());
                        break;
                    }
                    _ => {}
                }
                false
            }
            "message_start" if record.pointer("/message/role").and_then(Value::as_str) == Some("assistant") => {
                let mut message = record["message"].clone();
                if !message.get("content").is_some_and(Value::is_array) {
                    message["content"] = json!([]);
                }
                partial.message = Some(message);
                true
            }
            "message_update" => {
                if let (Some(message), Some(usage)) = (partial.message.as_mut(), record.get("usage")) {
                    message["usage"] = usage.clone();
                }
                let grew = record.get("assistantMessageEvent").is_some_and(|event| partial.take(event));
                // A growing answer is redrawn now and then, not at every token.
                let due = partial.sent.is_none_or(|at| at.elapsed() >= PARTIAL_EVERY);
                if grew && due {
                    partial.sent = Some(std::time::Instant::now());
                }
                grew && due
            }
            "message_end" => {
                let message = record.get("message").cloned().unwrap_or(Value::Null);
                match message.get("role").and_then(Value::as_str) {
                    // The prompt is the window's own first entry.
                    Some("user") if messages.is_empty() => false,
                    Some(_) => {
                        if message.get("role").and_then(Value::as_str) == Some("assistant") {
                            partial.message = None;
                        }
                        messages.push(message);
                        true
                    }
                    None => false,
                }
            }
            "tool_execution_end" => {
                // The result as a message, in case Pi sends it only here; the message of
                // the same call, if it comes, takes its place.
                let id = record.get("toolCallId").cloned().unwrap_or(Value::Null);
                let content = record.pointer("/result/content").cloned().unwrap_or(json!([]));
                let is_error = record.get("isError").and_then(Value::as_bool).unwrap_or(false);
                messages.push(json!({"role": "toolResult", "toolCallId": id, "content": content, "isError": is_error}));
                true
            }
            "extension_ui_request" => {
                if matches!(record.get("method").and_then(Value::as_str), Some("select" | "confirm" | "input" | "editor"))
                    && let Some(id) = record.get("id")
                {
                    send(&json!({"type": "extension_ui_response", "id": id, "cancelled": true}));
                }
                false
            }
            "agent_settled" => {
                settled = true;
                break;
            }
            _ => false,
        };
        if changed {
            let _ = sender.unbounded_send(LiveEvent::Update(turn_of(prompt, started, &messages, &partial)));
        }
    }
    let _ = sender.unbounded_send(LiveEvent::Update(turn_of(prompt, started, &messages, &Partial::default())));
    // Closing the input asks Pi to end once its work is written.
    drop(send);
    drop(stdin);
    if error.is_some() {
        return error;
    }
    let last = messages.iter().rev().find(|m| m.get("role").and_then(Value::as_str) == Some("assistant"));
    match last.and_then(|m| m.get("stopReason")).and_then(Value::as_str) {
        Some("error") => Some(last.and_then(|m| m.get("errorMessage")).and_then(Value::as_str).unwrap_or("the model call failed").to_owned()),
        _ if !settled => Some("Pi ended before the turn did".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_channel::mpsc::unbounded;

    fn spec() -> TurnSpec {
        TurnSpec {
            harness: model::HarnessKind::Pi,
            session: None,
            cwd: "/work".into(),
            prompt: "List the files".into(),
            model: None,
            effort: None,
            permission: crate::Permission::Settings,
            ssh: None,
            attachments: vec![crate::Attachment { path: "/work/shot.png".into(), image: Some(("image/png".into(), std::sync::Arc::new(vec![1, 2, 3]))) }],
            ide: None,
            drop_turn: None,
            fork_at: None,
            mcp_config: None,
        }
    }

    /// A turn as the RPC documentation shows it: the state, the prompt's response, an answer
    /// that calls a tool, its result, the last answer, and the end of Pi's work.
    #[test]
    fn a_turn_streams_into_blocks_and_ends_when_pi_settles() {
        let lines = [
            r#"{"id":"bowyer-state","type":"response","command":"get_state","success":true,"data":{"sessionId":"s-1","sessionFile":"/h/.pi/agent/sessions/--work--/t_s-1.jsonl"}}"#,
            r#"{"id":"bowyer-prompt","type":"response","command":"prompt","success":true,"data":{"disposition":"started"}}"#,
            r#"{"type":"agent_start"}"#,
            r#"{"type":"message_start","message":{"role":"user","content":"List the files","timestamp":1}}"#,
            r#"{"type":"message_end","message":{"role":"user","content":"List the files","timestamp":1}}"#,
            r#"{"type":"message_start","message":{"role":"assistant","content":[],"stopReason":"pending","model":"claude-sonnet-5-5","timestamp":2}}"#,
            r#"{"type":"message_update","usage":{"input":10,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":11,"cost":{"total":0}},"assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"Use "}}"#,
            r#"{"type":"message_update","usage":{"input":10,"output":2,"cacheRead":0,"cacheWrite":0,"totalTokens":12,"cost":{"total":0}},"assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"ls."}}"#,
            r#"{"type":"message_update","usage":{"input":10,"output":3,"cacheRead":0,"cacheWrite":0,"totalTokens":13,"cost":{"total":0}},"assistantMessageEvent":{"type":"toolcall_start","contentIndex":1,"id":"call_1","toolName":"bash"}}"#,
            r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Use ls."},{"type":"toolCall","id":"call_1","name":"bash","arguments":{"command":"ls"}}],"model":"claude-sonnet-5-5","usage":{"input":900,"output":40,"cacheRead":3000,"cacheWrite":0,"totalTokens":3940,"cost":{"total":0.01}},"stopReason":"toolUse","timestamp":3}}"#,
            r#"{"type":"tool_execution_start","toolCallId":"call_1","toolName":"bash","args":{"command":"ls"}}"#,
            r#"{"type":"tool_execution_end","toolCallId":"call_1","toolName":"bash","result":{"content":[{"type":"text","text":"src"}],"details":{}},"isError":false}"#,
            r#"{"type":"message_start","message":{"role":"assistant","content":[],"stopReason":"pending","timestamp":4}}"#,
            r#"{"type":"message_update","usage":{"input":1,"output":1,"cacheRead":3900,"cacheWrite":0,"totalTokens":3902,"cost":{"total":0}},"assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"One folder."}}"#,
            r#"{"type":"extension_ui_request","id":"ui-1","method":"confirm","title":"Go on?"}"#,
            r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"One folder."}],"model":"claude-sonnet-5-5","usage":{"input":1,"output":3,"cacheRead":3900,"cacheWrite":0,"totalTokens":3904,"cost":{"total":0.001}},"stopReason":"stop","timestamp":5}}"#,
            r#"{"type":"agent_end","messages":[],"willRetry":false}"#,
            r#"{"type":"agent_settled","aborted":false}"#,
            r#"{"type":"never_read"}"#,
        ];
        let (sender, mut events) = unbounded();
        let written = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for Sink {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let error = run_turn(Sink(written.clone()), lines.iter().map(|l| l.to_string()), &spec(), &[Block::Text { text: "List the files".into() }], &sender);
        assert_eq!(error, None);
        let sent: Vec<Value> = String::from_utf8(written.lock().unwrap().clone()).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(sent[0]["type"], "get_state");
        assert_eq!(sent[1]["type"], "prompt");
        assert!(sent[1]["message"].as_str().unwrap().starts_with("List the files"));
        assert_eq!(sent[1]["images"][0]["mimeType"], "image/png");
        assert_eq!(sent[2], json!({"type": "extension_ui_response", "id": "ui-1", "cancelled": true}), "a dialog is cancelled");
        drop(sender);
        let mut session = None;
        let mut last = None;
        while let Ok(Some(event)) = events.try_next() {
            match event {
                LiveEvent::Session(id) => session = Some(id),
                LiveEvent::Update(turn) => last = Some(turn),
                _ => {}
            }
        }
        assert_eq!(session.as_deref(), Some("s-1"));
        let turn = last.unwrap();
        assert_eq!(turn.entries.len(), 2, "the prompt and one answer: {:?}", turn.entries);
        assert_eq!(turn.tasks(false)[0].title, "ls");
        assert_eq!(turn.tasks(false)[0].status, model::TaskStatus::Done);
        assert!(matches!(turn.entries[1].blocks.last(), Some(Block::Text { text }) if text == "One folder."));
        assert_eq!(turn.model.as_deref(), Some("claude-sonnet-5-5"));
    }

    #[test]
    fn a_refused_prompt_and_a_failed_call_are_errors() {
        let (sender, _events) = unbounded();
        let refused = [r#"{"id":"bowyer-prompt","type":"response","command":"prompt","success":false,"error":"No model selected"}"#];
        let error = run_turn(std::io::sink(), refused.iter().map(|l| l.to_string()), &spec(), &[], &sender);
        assert_eq!(error.as_deref(), Some("No model selected"));
        let failed = [
            r#"{"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"401 unauthorized","timestamp":1}}"#,
            r#"{"type":"agent_settled","aborted":false}"#,
        ];
        let error = run_turn(std::io::sink(), failed.iter().map(|l| l.to_string()), &spec(), &[], &sender);
        assert_eq!(error.as_deref(), Some("401 unauthorized"));
        let cut = [r#"{"type":"agent_start"}"#];
        assert!(run_turn(std::io::sink(), cut.iter().map(|l| l.to_string()), &spec(), &[], &sender).is_some(), "an end without agent_settled");
    }
}
