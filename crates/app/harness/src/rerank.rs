//! Search results ranked again by a small model, on request: the full-text index finds the
//! sessions, a model orders the first of them by what the query means. Haiku is one call of
//! `claude -p` without tools, MCP servers or a saved session; it costs the user's account,
//! so it runs only when the user asks. A local model is asked over HTTP on this computer:
//! Ollama (`/api/chat`, port 11434) or LM Studio (OpenAI's `/v1/chat/completions`, port
//! 1234), found by listing their models.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use envs::Environment;
use serde_json::{Value, json};

/// How many results go to the model.
pub const TOP: usize = 30;

pub const SYSTEM: &str = "You rank search results of coding sessions. Answer with the numbers of all the results, best match first, separated by spaces, and nothing else.";

/// The question for the model: the query and the numbered results with their titles and
/// the matching text.
pub fn prompt(query: &str, hits: &[(String, String)]) -> String {
    let mut text = format!("Search: {query}\n\nResults:\n");
    for (index, (title, snippet)) in hits.iter().take(TOP).enumerate() {
        let snippet: String = snippet.chars().take(240).collect::<String>().replace('\n', " ");
        text.push_str(&format!("{}. {} — {}\n", index + 1, title.trim(), snippet.trim()));
    }
    text.push_str("\nList the numbers of all the results, best match first, worst last. Numbers only.");
    text
}

/// The order the model gave, as indices into the results: every number once, the ones it
/// left out after the rest in their old order.
pub fn parse_order(answer: &str, count: usize) -> Vec<usize> {
    let mut order: Vec<usize> = Vec::new();
    for word in answer.split(|c: char| !c.is_ascii_digit()) {
        if let Ok(number) = word.parse::<usize>()
            && (1..=count).contains(&number)
            && !order.contains(&(number - 1))
        {
            order.push(number - 1);
        }
    }
    let rest: Vec<usize> = (0..count).filter(|index| !order.contains(index)).collect();
    order.extend(rest);
    order
}

/// The text and the cost of a `claude -p --output-format json` answer.
pub fn read_answer(json: &str) -> Option<(String, Option<f64>)> {
    let value: Value = serde_json::from_str(json.trim()).ok()?;
    if value.get("is_error").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let text = value.get("result")?.as_str()?.to_owned();
    Some((text, value.get("total_cost_usd").and_then(Value::as_f64)))
}

/// The arguments of the call: Haiku, no tools, no MCP servers, nothing saved.
pub fn arguments() -> Vec<&'static str> {
    arguments_with(SYSTEM)
}

/// The same call with another system prompt.
pub fn arguments_with(system: &'static str) -> Vec<&'static str> {
    vec![
        "-p",
        "--model",
        "haiku",
        "--output-format",
        "json",
        "--no-session-persistence",
        "--tools",
        "",
        "--strict-mcp-config",
        "--system-prompt",
        system,
    ]
}

/// The model chosen for search, writing digests of sessions for Bowyer's memory: Haiku
/// through `claude -p` (no tools, no MCP servers, nothing saved), or a local model.
pub struct DigestModel {
    pub ranker: Ranker,
    /// Dollars `claude -p` reported, summed.
    pub cost: std::cell::Cell<f64>,
}

impl DigestModel {
    pub fn new(ranker: Ranker) -> Self {
        Self { ranker, cost: std::cell::Cell::new(0.0) }
    }
}

impl mcp::digest::Summarizer for DigestModel {
    fn summarize(&self, prompt: &str) -> Result<String, String> {
        match &self.ranker {
            Ranker::Haiku => {
                let env = envs::LocalEnvironment::new();
                let folder = std::env::temp_dir();
                let out = env.run_with_input("claude", &arguments_with(mcp::digest::SYSTEM), folder.to_str(), prompt.as_bytes()).map_err(|e| e.to_string())?;
                let (text, cost) = read_answer(&out).ok_or_else(|| model::shorten(out.trim(), 200))?;
                self.cost.set(self.cost.get() + cost.unwrap_or(0.0));
                Ok(text)
            }
            Ranker::Local(model) => ask_at_with(model, model.server.port(), mcp::digest::SYSTEM, prompt),
        }
    }
}

/// Who orders the results.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ranker {
    Haiku,
    Local(LocalModel),
}

/// A model of a local server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalModel {
    pub server: Server,
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Server {
    Ollama,
    LmStudio,
}

impl Server {
    pub const ALL: [Server; 2] = [Server::Ollama, Server::LmStudio];

    fn port(self) -> u16 {
        match self {
            Server::Ollama => 11434,
            Server::LmStudio => 1234,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Server::Ollama => "Ollama",
            Server::LmStudio => "LM Studio",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Server::Ollama => "ollama",
            Server::LmStudio => "lmstudio",
        }
    }
}

impl Ranker {
    /// The value of the setting that remembers the choice.
    pub fn to_setting(&self) -> String {
        match self {
            Ranker::Haiku => "haiku".to_owned(),
            Ranker::Local(model) => format!("{}:{}", model.server.id(), model.name),
        }
    }

    pub fn from_setting(value: &str) -> Ranker {
        let local = value.split_once(':').and_then(|(server, name)| {
            let server = Server::ALL.into_iter().find(|s| s.id() == server)?;
            (!name.is_empty()).then(|| LocalModel { server, name: name.to_owned() })
        });
        local.map_or(Ranker::Haiku, Ranker::Local)
    }

    /// A short name for the button.
    pub fn label(&self) -> String {
        match self {
            Ranker::Haiku => "Haiku".to_owned(),
            Ranker::Local(model) => model.name.rsplit('/').next().unwrap_or(&model.name).to_owned(),
        }
    }
}

/// How long a local server may take to answer a list of its models.
const LIST_TIMEOUT: Duration = Duration::from_millis(800);
/// How long a local model may take to answer: loading a large one takes a while.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(300);

/// The models the local servers that run now offer; a server that does not answer offers none.
pub fn local_models() -> Vec<LocalModel> {
    Server::ALL.into_iter().flat_map(|server| models_at(server, server.port())).collect()
}

/// The chat models `server` offers on `port`. Embedding models answer no chat, so they are
/// left out (both servers list them with the rest).
fn models_at(server: Server, port: u16) -> Vec<LocalModel> {
    let path = match server {
        Server::Ollama => "/api/tags",
        Server::LmStudio => "/v1/models",
    };
    let Ok(body) = http(port, "GET", path, None, LIST_TIMEOUT) else { return Vec::new() };
    let Ok(value) = serde_json::from_str::<Value>(&body) else { return Vec::new() };
    let (list, key) = match server {
        Server::Ollama => (value.get("models"), "name"),
        Server::LmStudio => (value.get("data"), "id"),
    };
    let names = list.and_then(Value::as_array).into_iter().flatten().filter_map(|m| m.get(key).and_then(Value::as_str));
    names
        .filter(|name| !name.to_ascii_lowercase().contains("embed"))
        .map(|name| LocalModel { server, name: name.to_owned() })
        .collect()
}

/// The answer of a local model to the ranking question.
pub fn ask_local(model: &LocalModel, prompt: &str) -> Result<String, String> {
    ask_at(model, model.server.port(), prompt)
}

fn ask_at(model: &LocalModel, port: u16, prompt: &str) -> Result<String, String> {
    ask_at_with(model, port, SYSTEM, prompt)
}

fn ask_at_with(model: &LocalModel, port: u16, system: &str, prompt: &str) -> Result<String, String> {
    let messages = json!([{"role": "system", "content": system}, {"role": "user", "content": prompt}]);
    let (path, body) = match model.server {
        // Thinking off: the answer is a list of numbers.
        Server::Ollama => ("/api/chat", json!({"model": model.name, "messages": messages, "stream": false, "think": false, "options": {"temperature": 0}})),
        Server::LmStudio => ("/v1/chat/completions", json!({"model": model.name, "messages": messages, "stream": false, "temperature": 0})),
    };
    let answer = http(port, "POST", path, Some(&body.to_string()), ANSWER_TIMEOUT).map_err(|e| format!("{}: {e}", model.server.label()))?;
    let value: Value = serde_json::from_str(&answer).map_err(|_| model::shorten(answer.trim(), 200))?;
    if let Some(error) = value.get("error") {
        // Ollama names the error in a string, an OpenAI server in an object with a message.
        let message = error.as_str().or_else(|| error.pointer("/message").and_then(Value::as_str));
        return Err(message.map(str::to_owned).unwrap_or_else(|| error.to_string()));
    }
    let text = match model.server {
        Server::Ollama => value.pointer("/message/content"),
        Server::LmStudio => value.pointer("/choices/0/message/content"),
    };
    let text = text.and_then(Value::as_str).ok_or_else(|| model::shorten(answer.trim(), 200))?;
    Ok(without_thinking(text))
}

/// The answer without a `<think>` block a reasoning model writes first: its numbers are not
/// the order.
fn without_thinking(text: &str) -> String {
    match (text.find("<think>"), text.find("</think>")) {
        (Some(start), Some(end)) if start < end => format!("{}{}", &text[..start], &text[end + "</think>".len()..]),
        (None, Some(end)) => text[end + "</think>".len()..].to_owned(),
        _ => text.to_owned(),
    }
}

/// One HTTP/1.1 request to a server on this computer; the body of a 2xx answer, the status
/// line and body otherwise. Reads a fixed length, chunks, or to the end.
fn http(port: u16, method: &str, path: &str, body: Option<&str>, timeout: Duration) -> io::Result<String> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(300))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let body = body.unwrap_or("");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes())?;
    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader.read_line(&mut status)?;
    let code: u16 = status.split_whitespace().nth(1).and_then(|c| c.parse().ok()).ok_or_else(|| io::Error::other(format!("not HTTP: {}", status.trim())))?;
    let (mut length, mut chunked) = (None, false);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let (name, value) = (name.trim().to_ascii_lowercase(), value.trim());
            if name == "content-length" {
                length = value.parse::<usize>().ok();
            } else if name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            }
        }
    }
    let mut bytes = Vec::new();
    if chunked {
        loop {
            let mut size = String::new();
            reader.read_line(&mut size)?;
            let size = usize::from_str_radix(size.trim().split(';').next().unwrap_or("0"), 16).map_err(io::Error::other)?;
            if size == 0 {
                break;
            }
            let mut chunk = vec![0; size];
            reader.read_exact(&mut chunk)?;
            bytes.extend_from_slice(&chunk);
            let mut end = String::new();
            reader.read_line(&mut end)?;
        }
    } else if let Some(length) = length {
        bytes.resize(length, 0);
        reader.read_exact(&mut bytes)?;
    } else {
        reader.read_to_end(&mut bytes)?;
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if (200..300).contains(&code) { Ok(text) } else { Err(io::Error::other(format!("{}: {}", status.trim(), model::shorten(text.trim(), 200)))) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server on a free port that answers one request with `response`, and the request.
    fn serve_once(response: impl Into<String>) -> (u16, std::thread::JoinHandle<String>) {
        let response = response.into();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = vec![0; 8192];
            let n = stream.read(&mut request).unwrap();
            stream.write_all(response.as_bytes()).unwrap();
            String::from_utf8_lossy(&request[..n]).into_owned()
        });
        (port, handle)
    }

    #[test]
    fn http_reads_lengths_chunks_and_errors() {
        let (port, server) = serve_once("HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}");
        assert_eq!(http(port, "POST", "/api/chat", Some("{}"), LIST_TIMEOUT).unwrap(), "{\"ok\":true}");
        let request = server.join().unwrap();
        assert!(request.starts_with("POST /api/chat HTTP/1.1\r\n") && request.ends_with("\r\n\r\n{}"), "{request}");
        let (port, _server) = serve_once("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\n\r\n");
        assert_eq!(http(port, "GET", "/v1/models", None, LIST_TIMEOUT).unwrap(), "{\"a\":1}");
        let (port, _server) = serve_once("HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nno model.");
        assert!(http(port, "GET", "/", None, LIST_TIMEOUT).unwrap_err().to_string().contains("404"));
    }

    /// A 200 answer with `body` and its length.
    fn ok(body: &str) -> String {
        format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len())
    }

    /// The JSON body of a captured request.
    fn body_of(request: &str) -> Value {
        serde_json::from_str(request.split_once("\r\n\r\n").expect("a body").1).expect("JSON")
    }

    #[test]
    fn lm_studio_lists_its_chat_models_and_answers_through_the_openai_route() {
        let (port, server) = serve_once(ok(
            r#"{"object":"list","data":[{"id":"qwen/qwen3-4b","object":"model"},{"id":"text-embedding-nomic-embed-text-v1.5","object":"model"},{"id":"x","object":"model"}]}"#,
        ));
        let models = models_at(Server::LmStudio, port);
        assert!(server.join().unwrap().starts_with("GET /v1/models HTTP/1.1\r\n"));
        let names: Vec<&str> = models.iter().map(|model| model.name.as_str()).collect();
        assert_eq!(names, ["qwen/qwen3-4b", "x"], "the embedding model is left out");

        let (port, server) = serve_once(ok(r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"2 1"}}]}"#));
        assert_eq!(ask_at(&models[0], port, "Search: x").unwrap(), "2 1");
        let request = server.join().unwrap();
        assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"), "{request}");
        let body = body_of(&request);
        assert_eq!(body["model"], "qwen/qwen3-4b");
        assert_eq!(body["stream"], false);
        assert_eq!(body["messages"][0]["content"], SYSTEM);
        assert_eq!(body["messages"][1]["content"], "Search: x");
        assert!(body.get("think").is_none(), "Ollama's switch is not sent to LM Studio");
    }

    #[test]
    fn ollama_lists_its_chat_models_and_answers_without_thinking() {
        let (port, _server) = serve_once(ok(r#"{"models":[{"name":"qwen3:8b"},{"name":"nomic-embed-text:v1"}]}"#));
        let models = models_at(Server::Ollama, port);
        assert_eq!(models, [LocalModel { server: Server::Ollama, name: "qwen3:8b".into() }]);

        let (port, server) = serve_once(ok(r#"{"message":{"role":"assistant","content":"<think>hm</think>3 1 2"}}"#));
        assert_eq!(ask_at(&models[0], port, "q").unwrap(), "3 1 2");
        let request = server.join().unwrap();
        assert!(request.starts_with("POST /api/chat HTTP/1.1\r\n"), "{request}");
        assert_eq!(body_of(&request)["think"], false);
    }

    #[test]
    fn an_error_answer_reads_as_its_message() {
        let model = LocalModel { server: Server::LmStudio, name: "m".into() };
        let (port, _server) = serve_once(ok(r#"{"error":{"message":"No models loaded","x":1}}"#));
        assert_eq!(ask_at(&model, port, "q").unwrap_err(), "No models loaded");
        let (port, _server) = serve_once(ok(r#"{"error":"model absent"}"#));
        assert_eq!(ask_at(&model, port, "q").unwrap_err(), "model absent");
        let nobody = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert!(models_at(Server::LmStudio, nobody).is_empty(), "a server that is not there offers nothing");
    }

    #[test]
    fn a_thinking_answer_keeps_what_follows_the_thought() {
        assert_eq!(without_thinking("<think>maybe 1 then 2</think>\n3 1 2"), "\n3 1 2");
        assert_eq!(without_thinking("1 2"), "1 2");
        assert_eq!(without_thinking("reasoning 5 4</think>2 1"), "2 1");
    }

    #[test]
    fn the_choice_of_ranker_goes_through_the_setting() {
        let local = Ranker::Local(LocalModel { server: Server::Ollama, name: "hf.co/org/Qwen3:IQ2_S".into() });
        assert_eq!(Ranker::from_setting(&local.to_setting()), local);
        assert_eq!(local.label(), "Qwen3:IQ2_S");
        assert_eq!(Ranker::from_setting("haiku"), Ranker::Haiku);
        assert_eq!(Ranker::from_setting("nonsense:"), Ranker::Haiku);
    }

    #[test]
    fn the_prompt_numbers_the_results() {
        let text = prompt("websocket reconnect", &[("Fix sockets".into(), "the [websocket] drops\nagain".into()), ("Release".into(), "".into())]);
        assert!(text.contains("1. Fix sockets — the [websocket] drops again"));
        assert!(text.contains("2. Release —"));
    }

    #[test]
    fn the_answer_becomes_an_order_with_every_result_once() {
        assert_eq!(parse_order("3 1", 4), vec![2, 0, 1, 3]);
        assert_eq!(parse_order("2, 2, 9, 1.", 3), vec![1, 0, 2], "repeats and numbers out of range drop");
        assert_eq!(parse_order("none fit", 2), vec![0, 1]);
    }

    #[test]
    fn a_json_answer_gives_its_text_and_cost() {
        let json = r#"{"type":"result","is_error":false,"result":"2 1","total_cost_usd":0.0012}"#;
        assert_eq!(read_answer(json), Some(("2 1".to_owned(), Some(0.0012))));
        assert_eq!(read_answer(r#"{"is_error":true,"result":"x"}"#), None);
        assert_eq!(read_answer("not json"), None);
    }
}
