//! The memory server over HTTP, for harnesses on SSH hosts. A harness there cannot start
//! `bowyer mcp` (Bowyer and its library are on this computer), so the window listens on
//! `127.0.0.1` here and a tunnel (`ssh -R`) brings that port to the host's own
//! `127.0.0.1`. The port on the host is open to every user there, so each request carries a
//! token, and each host has its own: the server knows the machine by the token, never by
//! what the request says, and answers only with that machine's sessions. The project comes
//! in the address (a harness does not tell its folder otherwise).
//!
//! This is the plain-JSON form of MCP's Streamable HTTP transport: one JSON-RPC message per
//! `POST`, its answer as `application/json`, `202` for a notification, `405` for the
//! event stream a client may ask for with `GET`.

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use library::Library;
use model::EnvId;

use crate::Server;

/// The path the server answers on.
pub const PATH: &str = "/mcp";
/// The largest request taken: a JSON-RPC message is small.
const MAX_BODY: usize = 1 << 20;
const MAX_HEAD: usize = 16 << 10;
/// A client that says nothing for this long is dropped.
const READ_TIMEOUT: Duration = Duration::from_secs(15);
/// A request has this long to arrive whole, so slow clients cannot hold every slot.
const REQUEST_TIME: Duration = Duration::from_secs(30);
/// Connections served at once; more are turned away rather than given a thread each.
const MAX_CONNECTIONS: usize = 32;

/// The tokens the server takes, each with the machine it stands for. The window adds one
/// for each host it brings the server to.
pub type Tokens = Arc<RwLock<HashMap<String, EnvId>>>;

/// A token nobody can guess: four keys of the standard library's hasher, which come from
/// the system's random source.
pub fn new_token() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    (0..4u64)
        .map(|round| {
            let mut hasher = RandomState::new().build_hasher();
            hasher.write_u64(round);
            hasher.write_u128(nanos);
            hasher.write_u32(std::process::id());
            format!("{:016x}", hasher.finish())
        })
        .collect()
}

/// Listens on `127.0.0.1:port` (`0` for any free port) and answers in the background for as
/// long as the program runs, to the tokens in `tokens` as they are at each request.
/// Returns the port.
pub fn start(port: u16, library: PathBuf, cache: PathBuf, tokens: Tokens) -> io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let port = listener.local_addr()?.port();
    let active = Arc::new(AtomicUsize::new(0));
    std::thread::Builder::new().name("mcp-http".into()).spawn(move || {
        for mut stream in listener.incoming().flatten() {
            if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                active.fetch_sub(1, Ordering::SeqCst);
                let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                continue;
            }
            let (library, cache, tokens) = (library.clone(), cache.clone(), tokens.clone());
            // The slot is given back however the request ends, a panic included.
            let slot = Slot(active.clone());
            // A connection at a time would let one slow client hold the rest.
            let _ = std::thread::Builder::new().name("mcp-http-request".into()).spawn(move || {
                let _slot = slot;
                let _ = answer(stream, &library, &cache, &tokens);
            });
        }
    })?;
    Ok(port)
}

/// A connection being served; dropping it frees its place.
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The connection read against the request's deadline.
struct Deadline<'a> {
    stream: &'a TcpStream,
    until: Instant,
}

impl Read for Deadline<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "the request took too long"));
        }
        self.stream.set_read_timeout(Some(left.min(READ_TIMEOUT)))?;
        self.stream.read(buf)
    }
}

/// The address a harness uses for `project`, through the tunnel to `port` on its host.
pub fn url(port: u16, project: &str) -> String {
    format!("http://127.0.0.1:{port}{PATH}?project={}", encode(project))
}

/// Claude Code's `--mcp-config` for the memory of `project`, with its host's token.
pub fn claude_config(port: u16, token: &str, project: &str) -> String {
    serde_json::json!({
        "mcpServers": {
            "bowyer": {
                "type": "http",
                "url": url(port, project),
                "headers": {"Authorization": format!("Bearer {token}")}
            }
        }
    })
    .to_string()
}

/// A request as read from the connection.
#[derive(Debug, Default, PartialEq)]
struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    authorization: Option<String>,
    origin: bool,
    /// What `Content-Length` announced.
    length: usize,
    body: Vec<u8>,
}

/// The request line and the headers; the body stays unread.
fn read_head(reader: &mut impl BufRead) -> io::Result<Request> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut words = line.split_whitespace();
    let method = words.next().unwrap_or_default().to_owned();
    let target = words.next().unwrap_or_default();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(key), decode(value))
        })
        .collect();
    let mut request = Request { method, path: path.to_owned(), query, ..Default::default() };
    let mut head = line.len();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        head += header.len();
        if head > MAX_HEAD {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "headers too long"));
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else { continue };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => request.length = value.parse().map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad length"))?,
            "authorization" => request.authorization = Some(value.to_owned()),
            "origin" => request.origin = true,
            _ => {}
        }
    }
    if request.length > MAX_BODY {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "body too large"));
    }
    Ok(request)
}

fn read_body(reader: &mut impl Read, request: &mut Request) -> io::Result<()> {
    request.body = vec![0; request.length];
    reader.read_exact(&mut request.body)
}

type Refusal = (u16, &'static str);

/// Whether a request may in, judged by what comes before its body: the machine its token
/// stands for and the project it asks about, or why not.
fn admit(request: &Request, tokens: &Tokens) -> Result<(EnvId, String), Refusal> {
    if request.path != PATH {
        return Err((404, "Not Found"));
    }
    // A page in a browser sends its origin; a harness does not. Pages have no business here.
    if request.origin {
        return Err((403, "Forbidden"));
    }
    let given = request.authorization.as_deref().and_then(|value| value.strip_prefix("Bearer ")).map(str::trim).unwrap_or_default();
    let machine = tokens.read().ok().and_then(|tokens| {
        // Every token is compared, so the time does not tell which one came close.
        tokens.iter().fold(None, |found, (token, env)| if same(given, token) { Some(env.clone()) } else { found })
    });
    let Some(machine) = machine.filter(|_| !given.is_empty()) else { return Err((401, "Unauthorized")) };
    if request.method != "POST" {
        return Err((405, "Method Not Allowed"));
    }
    let project = request.query.iter().find(|(key, _)| key == "project").map(|(_, value)| value.clone());
    let Some(project) = project.filter(|project| !project.is_empty()) else { return Err((400, "Bad Request")) };
    Ok((machine, project))
}

/// The answer to an admitted request: status, its words, and a JSON body if any.
fn serve(library: &Path, cache: &Path, machine: EnvId, project: String, body: &[u8]) -> (u16, &'static str, Option<String>) {
    let library = match Library::open(library) {
        Ok(library) => library,
        Err(_) => return (503, "Service Unavailable", None),
    };
    let server = Server::new(library, project, cache).for_machine(machine);
    match server.handle(&String::from_utf8_lossy(body)) {
        Some(reply) => (200, "OK", Some(reply.to_string())),
        None => (202, "Accepted", None),
    }
}

fn answer(mut stream: TcpStream, library: &Path, cache: &Path, tokens: &Tokens) -> io::Result<()> {
    let deadline = Deadline { stream: &stream, until: Instant::now() + REQUEST_TIME };
    // Only the head may come before the token is checked: without one nobody gets to send
    // a megabyte.
    let mut reader = BufReader::new(deadline.take(MAX_HEAD as u64));
    let (status, reason, body) = match read_head(&mut reader) {
        Err(_) => (400, "Bad Request", None),
        Ok(mut request) => match admit(&request, tokens) {
            Err((status, reason)) => (status, reason, None),
            Ok((machine, project)) => {
                reader.get_mut().set_limit(request.length as u64);
                match read_body(&mut reader, &mut request) {
                    Ok(()) => serve(library, cache, machine, project, &request.body),
                    Err(_) => (400, "Bad Request", None),
                }
            }
        },
    };
    drop(reader);
    let body = body.unwrap_or_default();
    let kind = if body.is_empty() { "" } else { "Content-Type: application/json\r\n" };
    let allow = if status == 405 { "Allow: POST\r\n" } else { "" };
    write!(stream, "HTTP/1.1 {status} {reason}\r\n{kind}{allow}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())?;
    stream.flush()
}

/// Compares without telling by its time how much matched.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => (byte as char).to_string(),
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |byte: u8| (byte as char).to_digit(16);
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                (Some(high), Some(low)) => {
                    out.push((high * 16 + low) as u8);
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            b'+' => out.push(b' '),
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(target: &str, token: Option<&str>, body: &str) -> String {
        let auth = token.map(|token| format!("Authorization: Bearer {token}\r\n")).unwrap_or_default();
        format!("POST {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len())
    }

    fn read_request(text: &str) -> Request {
        let mut reader = BufReader::new(text.as_bytes());
        let mut request = read_head(&mut reader).unwrap();
        read_body(&mut reader, &mut request).unwrap();
        request
    }

    fn tokens(pairs: &[(&str, EnvId)]) -> Tokens {
        Arc::new(RwLock::new(pairs.iter().map(|(token, env)| (token.to_string(), env.clone())).collect()))
    }

    /// What the server answers to a whole request, and the reply's JSON if any.
    fn respond(text: &str, library: &Path, cache: &Path, tokens: &Tokens) -> (u16, Option<String>) {
        let request = read_request(text);
        match admit(&request, tokens) {
            Ok((machine, project)) => {
                let (status, _, body) = serve(library, cache, machine, project, &request.body);
                (status, body)
            }
            Err((status, _)) => (status, None),
        }
    }

    #[test]
    fn a_request_is_read_with_its_query_and_token() {
        let request = read_request(&post("/mcp?project=%2Fhome%2Fdev%2Fapi+x", Some("t0k"), "{}"));
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/mcp");
        assert_eq!(request.query, vec![("project".into(), "/home/dev/api x".into())]);
        assert_eq!(request.authorization.as_deref(), Some("Bearer t0k"));
        assert_eq!(request.body, b"{}");
        assert!(!request.origin);
    }

    #[test]
    fn the_address_carries_the_project_and_not_the_machine() {
        let url = url(41234, "/home/dev/my api");
        assert_eq!(url, "http://127.0.0.1:41234/mcp?project=/home/dev/my%20api");
        let request = read_request(&post(url.trim_start_matches("http://127.0.0.1:41234"), None, ""));
        assert_eq!(request.query, vec![("project".into(), "/home/dev/my api".into())]);
        let config: serde_json::Value = serde_json::from_str(&claude_config(41234, "abc", "/p")).unwrap();
        assert_eq!(config["mcpServers"]["bowyer"]["type"], "http");
        assert_eq!(config["mcpServers"]["bowyer"]["headers"]["Authorization"], "Bearer abc");
    }

    #[test]
    fn only_a_token_and_no_page_get_in() {
        let dir = test_support::TempDir::new("mcp-http");
        let library = dir.path().join("library.db");
        Library::open(&library).unwrap();
        let cache = dir.path().join("cache");
        let tokens = tokens(&[("secret", EnvId::ssh("dev@box"))]);
        let initialize = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#;
        let ask = |text: String| respond(&text, &library, &cache, &tokens).0;
        let target = "/mcp?project=%2Fw";
        assert_eq!(ask(post(target, None, initialize)), 401);
        assert_eq!(ask(post(target, Some("secreT"), initialize)), 401);
        assert_eq!(ask(post(target, Some(""), initialize)), 401);
        assert_eq!(ask(post(target, Some("secret"), initialize)), 200);
        assert_eq!(ask(post("/other", Some("secret"), initialize)), 404);
        assert_eq!(ask(post("/mcp", Some("secret"), initialize)), 400, "no project");
        let page = post(target, Some("secret"), initialize).replacen("Host:", "Origin: https://evil.example\r\nHost:", 1);
        assert_eq!(ask(page), 403);
        let get = "GET /mcp?project=%2Fw HTTP/1.1\r\nAuthorization: Bearer secret\r\n\r\n".to_owned();
        assert_eq!(ask(get), 405);
        let notification = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        assert_eq!(ask(post(target, Some("secret"), notification)), 202);
        assert!(new_token() != new_token() && new_token().len() == 64);
    }

    /// A token stands for its machine whatever the address says: a host that asks for this
    /// computer's sessions, or another host's, gets none.
    #[test]
    fn the_token_and_not_the_address_names_the_machine() {
        use model::{HarnessKind, SessionKey, SessionSummary, SourceStamp};
        let dir = test_support::TempDir::new("mcp-http-machine");
        let path = dir.path().join("library.db");
        let mut library = Library::open(&path).unwrap();
        let session = |env: EnvId, id: &str, title: &str| SessionSummary {
            key: SessionKey { env, harness: HarnessKind::Claude, id: id.into() },
            title: title.into(),
            first_prompt: Some(format!("{title} please")),
            cwd: Some("/w".into()),
            git_branch: None,
            model: None,
            created_ms: 1,
            updated_ms: 2,
            parent: None,
            source: SourceStamp::default(),
            turn: None,
        };
        library.store_scan("local", "claude", &[session(EnvId::local(), "l1", "local secret plans")]).unwrap();
        let other = EnvId::ssh("other@host");
        library.store_scan(&other.0, "claude", &[session(other.clone(), "o1", "other host plans")]).unwrap();
        drop(library);
        let cache = dir.path().join("cache");
        let tokens = tokens(&[("box-token", EnvId::ssh("dev@box")), ("other-token", other)]);
        let list = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_sessions","arguments":{"query":"plans","all_projects":true}}}"#;
        for target in ["/mcp?project=%2Fw", "/mcp?env=local&project=%2Fw", "/mcp?env=ssh%3Aother%40host&project=%2Fw"] {
            let (status, reply) = respond(&post(target, Some("box-token"), list), &path, &cache, &tokens);
            assert_eq!(status, 200);
            let reply = reply.unwrap();
            assert!(!reply.contains("local secret") && !reply.contains("other host"), "{target}: {reply}");
        }
        let (_, reply) = respond(&post("/mcp?env=local&project=%2Fw", Some("other-token"), list), &path, &cache, &tokens);
        assert!(reply.unwrap().contains("other host plans"));
    }

    /// The whole way on a real host: a tunnel brings the server to the host's own
    /// `127.0.0.1`, `curl` there asks `initialize` with the token, and the tunnel ends once
    /// its input does. Writes nothing on the host. `BOWYER_SSH_HOST=user@host cargo test -p
    /// mcp -- --ignored through_a_tunnel` (a host with `curl` that allows TCP forwarding).
    #[test]
    #[ignore = "needs an SSH host"]
    fn the_server_answers_through_a_tunnel() {
        use envs::Environment;
        let host = std::env::var("BOWYER_SSH_HOST").expect("BOWYER_SSH_HOST");
        let dir = test_support::TempDir::new("mcp-http-tunnel");
        let library = dir.path().join("library.db");
        Library::open(&library).unwrap();
        let local = start(0, library, dir.path().join("cache"), tokens(&[("secret", EnvId::ssh(&host))])).unwrap();
        let mut ssh = envs::SshEnvironment::new(&host);
        // The system's ssh, as the window finds it, not one a Unix-like shell puts first.
        let system = std::path::Path::new(r"C:\Windows\System32\OpenSSH\ssh.exe");
        if cfg!(windows) && system.is_file() {
            ssh.ssh = system.to_path_buf();
        }
        let remote = 41_000 + (std::process::id() % 7_000) as u16;
        let mut tunnel = ssh.tunnel(remote, local).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
        let stdin = tunnel.stdin.take().unwrap();
        let mut lines = BufReader::new(tunnel.stdout.take().unwrap()).lines();
        assert!(lines.any(|line| line.is_ok_and(|line| line.trim() == envs::TUNNEL_READY)), "the tunnel did not come up");
        let body = r#"{"jsonrpc":"2.0","id":3,"method":"initialize","params":{}}"#;
        let curl = format!(
            "curl -s -X POST -H 'Authorization: Bearer secret' -H 'Content-Type: application/json' --data '{body}' '{}'",
            url(remote, "/tmp/project")
        );
        let reply = ssh.run("sh", &["-c", &curl], None).unwrap();
        assert!(reply.contains(r#""id":3"#) && reply.contains(r#""name":"bowyer""#), "{reply}");
        let refused = ssh.run("sh", &["-c", &curl.replace("Bearer secret", "Bearer wrong").replace("curl -s", "curl -s -o /dev/null -w '%{http_code}'")], None).unwrap();
        assert_eq!(refused.trim(), "401");
        drop(stdin);
        let started = std::time::Instant::now();
        while tunnel.try_wait().unwrap().is_none() {
            assert!(started.elapsed() < Duration::from_secs(20), "the tunnel outlived its input");
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    #[test]
    fn a_running_server_answers_initialize() {
        let dir = test_support::TempDir::new("mcp-http-live");
        let library = dir.path().join("library.db");
        Library::open(&library).unwrap();
        let tokens = tokens(&[("secret", EnvId::ssh("dev@box"))]);
        let port = start(0, library, dir.path().join("cache"), tokens.clone()).unwrap();
        let ask = |token: &str| {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let body = r#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{}}"#;
            stream.write_all(post("/mcp?project=%2Fw", Some(token), body).as_bytes()).unwrap();
            let mut reply = String::new();
            stream.read_to_string(&mut reply).unwrap();
            reply
        };
        let reply = ask("secret");
        assert!(reply.starts_with("HTTP/1.1 200 OK\r\n"), "{reply}");
        assert!(reply.contains("Content-Type: application/json"), "{reply}");
        assert!(reply.contains(r#""id":7"#) && reply.contains(r#""name":"bowyer""#), "{reply}");
        // A token added while the server runs is taken at once.
        assert!(ask("later").starts_with("HTTP/1.1 401"));
        tokens.write().unwrap().insert("later".into(), EnvId::ssh("late@host"));
        assert!(ask("later").starts_with("HTTP/1.1 200"));
    }

    /// Idle connections hold at most their share of places, and give them back when they go.
    #[test]
    fn idle_connections_cannot_hold_the_server() {
        let dir = test_support::TempDir::new("mcp-http-slots");
        let library = dir.path().join("library.db");
        Library::open(&library).unwrap();
        let port = start(0, library, dir.path().join("cache"), tokens(&[("secret", EnvId::ssh("dev@box"))])).unwrap();
        let idle: Vec<TcpStream> = (0..MAX_CONNECTIONS).map(|_| TcpStream::connect(("127.0.0.1", port)).unwrap()).collect();
        let ask = || {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let body = r#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{}}"#;
            let _ = stream.write_all(post("/mcp?project=%2Fw", Some("secret"), body).as_bytes());
            let mut reply = String::new();
            let _ = stream.read_to_string(&mut reply);
            reply
        };
        let started = Instant::now();
        while !ask().starts_with("HTTP/1.1 503") {
            assert!(started.elapsed() < Duration::from_secs(5), "the server took more than its share");
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(idle);
        let started = Instant::now();
        while !ask().starts_with("HTTP/1.1 200") {
            assert!(started.elapsed() < Duration::from_secs(5), "the places were not given back");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A refused request does not wait for the body it announced.
    #[test]
    fn a_request_without_the_token_is_refused_before_its_body() {
        let dir = test_support::TempDir::new("mcp-http-early");
        let library = dir.path().join("library.db");
        Library::open(&library).unwrap();
        let port = start(0, library, dir.path().join("cache"), tokens(&[("secret", EnvId::ssh("dev@box"))])).unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(b"POST /mcp?project=%2Fw HTTP/1.1\r\nAuthorization: Bearer nope\r\nContent-Length: 1000000\r\n\r\n").unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("HTTP/1.1 401"), "{reply}");
    }
}
