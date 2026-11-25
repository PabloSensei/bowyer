//! Claude Code's questions while a turn runs, over `--permission-prompt-tool stdio`: before
//! an action its settings do not allow by themselves, and for `AskUserQuestion`, it writes a
//! `control_request` (`can_use_tool`) line and waits for a `control_response` on its
//! standard input. The question goes to the window as [`LiveEvent::Permission`]; a thread
//! waits for the user's choice and writes the answer.
//!
//! The input stays open for the whole turn and closes at its `result` line: Claude would
//! wait for the next prompt otherwise.

use std::io::Write;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use futures_channel::mpsc::UnboundedSender;
use serde_json::{Value, json};

use crate::{AskQuestion, LiveEvent, PermissionAsk};

/// Where lines for the harness go.
pub(crate) enum Sink {
    /// Its standard input, on this computer or through an SSH connection.
    Pipe(Box<dyn Write + Send>),
    /// The input file of a detached run on an SSH host (`SshEnvironment::detached_answered`):
    /// each line goes over a connection of its own.
    Remote { ssh: envs::SshEnvironment, run: String },
}

/// The harness's input, shared by the turn's reader and the threads that wait for answers.
#[derive(Clone)]
pub(crate) struct Input(Arc<Mutex<Option<Sink>>>);

impl std::fmt::Debug for Input {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Input")
    }
}

/// Claude's tasks (a command or a subagent) by the tool call that started them, from its
/// `task_started` lines: `stop_task` takes the task id.
pub(crate) type Tasks = Arc<Mutex<std::collections::HashMap<String, String>>>;

/// Takes a `system` line: a task that starts is remembered.
pub(crate) fn take_task(line: &Value, tasks: &Tasks) {
    if line.get("subtype").and_then(Value::as_str) != Some("task_started") {
        return;
    }
    let field = |name: &str| line.get(name).and_then(Value::as_str);
    if let (Some(call), Some(task)) = (field("tool_use_id"), field("task_id"))
        && let Ok(mut tasks) = tasks.lock()
    {
        tasks.insert(call.to_owned(), task.to_owned());
    }
}

/// Asks Claude to stop the task the tool call `call` started; false when there is no such
/// task or its input is closed.
pub(crate) fn stop_task(input: &Input, tasks: &Tasks, call: &str) -> bool {
    let Some(task) = tasks.lock().ok().and_then(|tasks| tasks.get(call).cloned()) else { return false };
    let request = json!({"type": "control_request", "request_id": format!("stop-{task}"), "request": {"subtype": "stop_task", "task_id": task}});
    input.send(&format!("{request}\n"))
}

/// Asks Claude to stop its task `task`; false when its input is closed.
pub(crate) fn stop_task_id(input: &Input, task: &str) -> bool {
    let request = json!({"type": "control_request", "request_id": format!("stop-{task}"), "request": {"subtype": "stop_task", "task_id": task}});
    input.send(&format!("{request}\n"))
}

impl Input {
    pub(crate) fn new(sink: Sink) -> Self {
        Self(Arc::new(Mutex::new(Some(sink))))
    }

    /// An input that takes nothing: the prompt went in some other way and nobody answers.
    pub(crate) fn closed() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }

    /// Writes `text` as it is; false once the input is closed or the write fails.
    pub(crate) fn send(&self, text: &str) -> bool {
        let Ok(mut sink) = self.0.lock() else { return false };
        match sink.as_mut() {
            Some(Sink::Pipe(out)) => out.write_all(text.as_bytes()).and_then(|_| out.flush()).is_ok(),
            // A connection of its own for each line: a blip of the network is worth a retry,
            // since the harness waits for this answer.
            Some(Sink::Remote { ssh, run }) => (0..3).any(|attempt| {
                if attempt > 0 {
                    thread::sleep(std::time::Duration::from_secs(1));
                }
                ssh.append_input(run, text.as_bytes()).is_ok()
            }),
            None => false,
        }
    }

    /// Ends the input: a pipe closes. A remote run ends its input itself at the result.
    pub(crate) fn close(&self) {
        if let Ok(mut sink) = self.0.lock() {
            sink.take();
        }
    }
}

/// The rules of an "always" answer: only those Claude keeps in a settings file. Rules for
/// the session would end with this turn's process, and a mode change is not what the user
/// chose.
fn lasting_rules(request: &Value) -> Vec<Value> {
    request
        .get("permission_suggestions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|suggestion| {
            suggestion.get("type").and_then(Value::as_str) == Some("addRules")
                && matches!(suggestion.get("destination").and_then(Value::as_str), Some("localSettings" | "projectSettings" | "userSettings"))
        })
        .cloned()
        .collect()
}

/// What the window shows for a question: its kind, its title, and the answers.
struct Shown {
    kind: &'static str,
    title: String,
    options: Vec<(String, String, String)>,
    questions: Vec<AskQuestion>,
}

/// The questions of an `AskUserQuestion` call.
fn questions_of(input: &Value) -> Option<Vec<AskQuestion>> {
    let questions = input.get("questions").and_then(Value::as_array)?;
    let text = |value: &Value, name: &str| value.get(name).and_then(Value::as_str).unwrap_or("").to_owned();
    let questions: Vec<AskQuestion> = questions
        .iter()
        .map(|question| AskQuestion {
            text: text(question, "question"),
            header: text(question, "header"),
            choices: question.get("options").and_then(Value::as_array).into_iter().flatten().map(|option| text(option, "label")).collect(),
            multi: question.get("multiSelect").and_then(Value::as_bool) == Some(true),
        })
        .collect();
    (!questions.is_empty() && questions.iter().all(|q| !q.text.is_empty() && !q.choices.is_empty())).then_some(questions)
}

/// How a question looks in the window; `None` for a question the window cannot show.
fn shown(request: &Value) -> Option<Shown> {
    let tool = request.get("tool_name").and_then(Value::as_str).unwrap_or("");
    let input = request.get("input").unwrap_or(&Value::Null);
    let field = |name: &str| input.get(name).and_then(Value::as_str).map(str::to_owned);
    let refuse = ("deny".to_owned(), "reject_once".to_owned(), String::new());
    if tool == "AskUserQuestion" {
        let questions = questions_of(input)?;
        if let [question] = questions.as_slice()
            && !question.multi
        {
            // One question with one answer: its choices are the buttons.
            let mut options: Vec<(String, String, String)> =
                question.choices.iter().enumerate().map(|(n, label)| (format!("choice:{n}"), "choice".to_owned(), label.clone())).collect();
            options.push(refuse);
            return Some(Shown { kind: "question", title: question.text.clone(), options, questions: Vec::new() });
        }
        // Several questions or several answers: the window gathers them and sends them at once.
        return Some(Shown { kind: "question", title: questions[0].text.clone(), options: vec![refuse], questions });
    }
    let (kind, title) = match tool {
        "Bash" | "PowerShell" => ("execute", field("command")),
        "Edit" | "Write" | "MultiEdit" => ("edit", field("file_path")),
        "NotebookEdit" => ("edit", field("notebook_path")),
        "WebFetch" => ("fetch", field("url")),
        "WebSearch" => ("fetch", field("query")),
        _ => ("other", None),
    };
    let title = title.unwrap_or_else(|| {
        let name = request.get("display_name").and_then(Value::as_str).unwrap_or(tool);
        let what = request.get("description").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| {
            let mut input = input.to_string();
            if input.len() > 300 {
                let cut = (0..=300).rev().find(|&at| input.is_char_boundary(at)).unwrap_or(0);
                input.truncate(cut);
                input.push('…');
            }
            input
        });
        format!("{name}: {what}")
    });
    let mut options = vec![("allow".to_owned(), "allow_once".to_owned(), String::new())];
    if !lasting_rules(request).is_empty() {
        options.push(("always".to_owned(), "allow_always".to_owned(), String::new()));
    }
    options.push(refuse);
    Some(Shown { kind, title, options, questions: Vec::new() })
}

/// The `control_response` line for the answer `choice` (an option id; `None` when the
/// question was closed without one).
fn response(id: &str, request: &Value, choice: Option<&str>) -> String {
    let input = request.get("input").cloned().unwrap_or_else(|| json!({}));
    let answer = match choice {
        Some("allow") => json!({"behavior": "allow", "updatedInput": input}),
        Some("always") => json!({"behavior": "allow", "updatedInput": input, "updatedPermissions": lasting_rules(request)}),
        Some(choice) if choice.starts_with("answers:") => {
            let given = crate::acp::parse_answers(choice);
            let questions = questions_of(&input);
            let answers = given
                .zip(questions)
                .filter(|(given, questions)| given.len() == questions.len() && given.iter().all(|answer| !answer.is_empty()))
                .map(|(given, questions)| {
                    let mut answers = serde_json::Map::new();
                    for (question, answer) in questions.iter().zip(given) {
                        // Several answers to one question go as one text, as Claude's own
                        // dialog sends them; a text of the user's own comes after the picks.
                        let mut parts: Vec<&str> = answer.picked.iter().filter_map(|&n| question.choices.get(n)).map(String::as_str).collect();
                        let text = answer.text.trim();
                        if !text.is_empty() {
                            parts.push(text);
                        }
                        answers.insert(question.text.clone(), json!(parts.join(", ")));
                    }
                    answers
                });
            match answers {
                Some(answers) => {
                    let mut input = input.clone();
                    input["answers"] = Value::Object(answers);
                    json!({"behavior": "allow", "updatedInput": input})
                }
                None => json!({"behavior": "deny", "message": "The user's answer was lost."}),
            }
        }
        Some(choice) if choice.starts_with("choice:") => {
            let label = choice["choice:".len()..].parse::<usize>().ok().and_then(|n| {
                let question = input.pointer("/questions/0")?;
                Some((question.get("question")?.as_str()?.to_owned(), question.pointer(&format!("/options/{n}/label"))?.as_str()?.to_owned()))
            });
            match label {
                Some((question, label)) => {
                    let mut input = input.clone();
                    input["answers"] = json!({ question: label });
                    json!({"behavior": "allow", "updatedInput": input})
                }
                None => json!({"behavior": "deny", "message": "The user's answer was lost."}),
            }
        }
        Some(_) => json!({"behavior": "deny", "message": "The user refused this action."}),
        None => json!({"behavior": "deny", "message": "The user closed the question without an answer."}),
    };
    let line = json!({"type": "control_response", "response": {"subtype": "success", "request_id": id, "response": answer}});
    format!("{line}\n")
}

/// Takes a `control_request` line: a question goes to the window and a thread writes the
/// answer once the user chooses; any other request is answered with an error at once.
pub(crate) fn take_request(line: &Value, input: &Input, sender: &UnboundedSender<LiveEvent>) {
    let Some(id) = line.get("request_id").and_then(Value::as_str).map(str::to_owned) else { return };
    let request = line.get("request").cloned().unwrap_or(Value::Null);
    if request.get("subtype").and_then(Value::as_str) != Some("can_use_tool") {
        let error = json!({"type": "control_response", "response": {"subtype": "error", "request_id": id, "error": "not supported by this client"}});
        input.send(&format!("{error}\n"));
        return;
    }
    let Some(shown) = shown(&request) else {
        let refusal = json!({"type": "control_response", "response": {"subtype": "success", "request_id": id, "response": {
            "behavior": "deny",
            "message": "This window shows one question with one answer at a time: ask in plain text instead.",
        }}});
        input.send(&format!("{refusal}\n"));
        return;
    };
    let (answer, answers) = mpsc::channel();
    let ask = PermissionAsk {
        tool_call_id: id.clone(),
        title: shown.title,
        kind: shown.kind.to_owned(),
        options: shown.options,
        questions: shown.questions,
        reply: answer,
    };
    if sender.unbounded_send(LiveEvent::Permission(ask)).is_err() {
        input.send(&response(&id, &request, None));
        return;
    }
    let (input, sender) = (input.clone(), sender.clone());
    thread::spawn(move || {
        let choice = answers.recv().ok().flatten();
        if !input.send(&response(&id, &request, choice.as_deref())) && choice.is_some() {
            // The harness still waits: only Stop ends the turn now, so say why.
            let _ = sender.unbounded_send(LiveEvent::Stderr("The answer did not reach the harness; stop the turn and try again.".into()));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Answer;

    fn request(tool: &str, input: Value) -> Value {
        json!({"subtype": "can_use_tool", "tool_name": tool, "input": input, "permission_suggestions": [
            {"type": "addRules", "rules": [{"toolName": tool, "ruleContent": "mkdir a *"}], "behavior": "allow", "destination": "localSettings"},
            {"type": "addDirectories", "directories": ["/w"], "destination": "session"},
            {"type": "setMode", "mode": "acceptEdits", "destination": "session"},
        ]})
    }

    fn answer(line: &str) -> Value {
        let value: Value = serde_json::from_str(line.trim()).unwrap();
        value["response"]["response"].clone()
    }

    #[test]
    fn a_command_asks_with_three_answers_and_always_keeps_only_lasting_rules() {
        let request = request("Bash", json!({"command": "mkdir a", "description": "Make a"}));
        let shown = shown(&request).unwrap();
        assert_eq!((shown.kind, shown.title.as_str()), ("execute", "mkdir a"));
        let kinds: Vec<&str> = shown.options.iter().map(|(_, kind, _)| kind.as_str()).collect();
        assert_eq!(kinds, ["allow_once", "allow_always", "reject_once"]);

        let always = answer(&response("r1", &request, Some("always")));
        assert_eq!(always["behavior"], "allow");
        assert_eq!(always["updatedInput"]["command"], "mkdir a");
        let rules = always["updatedPermissions"].as_array().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["type"], "addRules");

        assert_eq!(answer(&response("r1", &request, Some("deny")))["behavior"], "deny");
        assert_eq!(answer(&response("r1", &request, None))["behavior"], "deny");
        let line: Value = serde_json::from_str(response("r1", &request, Some("allow")).trim()).unwrap();
        assert_eq!(line["type"], "control_response");
        assert_eq!(line["response"]["request_id"], "r1");
    }

    #[test]
    fn without_a_lasting_rule_there_is_no_always() {
        let mut request = request("mcp__notes__save", json!({"text": "x"}));
        request["permission_suggestions"] = json!([]);
        request["display_name"] = json!("save");
        let shown = shown(&request).unwrap();
        assert_eq!(shown.kind, "other");
        assert_eq!(shown.title, r#"save: {"text":"x"}"#);
        assert_eq!(shown.options.len(), 2);
    }

    #[test]
    fn a_question_offers_its_choices_and_answers_with_the_label() {
        let input = json!({"questions": [{"question": "Red or blue?", "header": "Color", "multiSelect": false,
            "options": [{"label": "Red", "description": ""}, {"label": "Blue", "description": ""}]}]});
        let request = json!({"subtype": "can_use_tool", "tool_name": "AskUserQuestion", "input": input, "requires_user_interaction": true});
        let asked = shown(&request).unwrap();
        assert_eq!((asked.kind, asked.title.as_str()), ("question", "Red or blue?"));
        assert_eq!(asked.options[1], ("choice:1".to_owned(), "choice".to_owned(), "Blue".to_owned()));
        assert_eq!(asked.options.last().unwrap().1, "reject_once");
        let picked = answer(&response("r2", &request, Some("choice:1")));
        assert_eq!(picked["behavior"], "allow");
        assert_eq!(picked["updatedInput"]["answers"]["Red or blue?"], "Blue");
        assert_eq!(picked["updatedInput"]["questions"][0]["header"], "Color");

        assert!(asked.questions.is_empty(), "one question with one answer has its buttons");
    }

    #[test]
    fn several_questions_and_answers_go_back_as_labels() {
        let input = json!({"questions": [
            {"question": "Which fruits?", "header": "Fruits", "multiSelect": true,
             "options": [{"label": "Apple"}, {"label": "Pear"}, {"label": "Plum"}]},
            {"question": "Tea or coffee?", "header": "Drink", "multiSelect": false,
             "options": [{"label": "Tea"}, {"label": "Coffee"}]}]});
        let request = json!({"subtype": "can_use_tool", "tool_name": "AskUserQuestion", "input": input});
        let asked = shown(&request).unwrap();
        assert_eq!(asked.kind, "question");
        assert_eq!(asked.questions.len(), 2);
        assert!(asked.questions[0].multi && !asked.questions[1].multi);
        assert_eq!(asked.questions[1].choices, ["Tea", "Coffee"]);
        assert_eq!(asked.options.len(), 1, "only the refusal is a button");

        let picked = |picked: Vec<usize>| Answer { picked, text: String::new() };
        let reply = PermissionAsk::answers(&[picked(vec![0, 2]), picked(vec![1])]);
        let answered = answer(&response("r3", &request, Some(&reply)));
        assert_eq!(answered["behavior"], "allow");
        assert_eq!(answered["updatedInput"]["answers"], json!({"Which fruits?": "Apple, Plum", "Tea or coffee?": "Coffee"}));
        let short = PermissionAsk::answers(&[picked(vec![0])]);
        assert_eq!(answer(&response("r3", &request, Some(&short)))["behavior"], "deny", "an answer for each question or none");
        let blank = PermissionAsk::answers(&[picked(vec![0]), Answer { picked: Vec::new(), text: "  ".into() }]);
        assert_eq!(answer(&response("r3", &request, Some(&blank)))["behavior"], "deny", "a blank text is no answer");

        // A text of the user's own, alone or after the picks.
        let own = PermissionAsk::answers(&[Answer { picked: vec![1], text: "Quince ".into() }, Answer { picked: Vec::new(), text: "Water".into() }]);
        let answered = answer(&response("r3", &request, Some(&own)));
        assert_eq!(answered["updatedInput"]["answers"], json!({"Which fruits?": "Pear, Quince", "Tea or coffee?": "Water"}));
    }

    #[test]
    fn a_question_with_buttons_takes_a_text_of_its_own() {
        let input = json!({"questions": [{"question": "Red or blue?", "multiSelect": false, "options": [{"label": "Red"}, {"label": "Blue"}]}]});
        let request = json!({"subtype": "can_use_tool", "tool_name": "AskUserQuestion", "input": input});
        let own = PermissionAsk::answers(&[Answer { picked: Vec::new(), text: "Green".into() }]);
        let answered = answer(&response("r4", &request, Some(&own)));
        assert_eq!(answered["behavior"], "allow");
        assert_eq!(answered["updatedInput"]["answers"], json!({"Red or blue?": "Green"}));
    }
}
