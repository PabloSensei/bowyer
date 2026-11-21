//! A persistent shell session on an SSH host: one `ssh` connection running a small POSIX
//! shell loop that takes scripts one after another, so a short call costs a round trip
//! instead of a new connection (about 10 ms against 450 ms measured on a home server).
//!
//! Each request is a line of base64 with the script and a line with its standard input;
//! the loop runs the script in the user's shell with that input from a file (never the
//! session's own pipe), then answers `#bowyer# <exit> <out bytes> <err bytes>` and the
//! bytes. A host keeps at most [`MAX_SESSIONS`] of them, shared by every copy of its
//! environment. When none is free, the host has no `base64`, the handshake fails or the
//! session died before a request went out, the caller makes a connection of its own as
//! before; a request is never sent twice.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Sessions a host keeps at most; more calls at once take connections of their own.
const MAX_SESSIONS: usize = 2;
/// How long a session may take to start.
const HANDSHAKE: Duration = Duration::from_secs(15);
/// After a session failed to start, the host goes without one for this long.
const RETRY_AFTER: Duration = Duration::from_secs(60);

/// The loop on the host. `$1` is the token of the handshake. Its folder is private and
/// removed when the loop ends, which it does when the connection closes.
const LOOP: &str = r#"umask 077
command -v base64 >/dev/null 2>&1 || exit 3
if printf eA== | base64 -d >/dev/null 2>&1; then D='base64 -d'; else D='base64 -D'; fi
b="$HOME/.cache/bowyer/sessions"; mkdir -p "$b" && d=$(mktemp -d "$b/s.XXXXXX") || exit 1
trap 'rm -rf "$d"' EXIT
trap 'exit 1' HUP INT TERM
printf '#bowyer-session# %s\n' "$1"
while IFS= read -r s && IFS= read -r i; do
  printf '%s\n' "$s" | $D > "$d/s" 2>/dev/null
  if [ -n "$i" ]; then printf '%s\n' "$i" | $D > "$d/i" 2>/dev/null; else : > "$d/i"; fi
  "${SHELL:-sh}" -c "$(cat "$d/s")" < "$d/i" > "$d/o" 2> "$d/e"
  c=$?
  printf '#bowyer# %s %s %s\n' "$c" $(wc -c < "$d/o") $(wc -c < "$d/e")
  cat "$d/o" "$d/e"
  : > "$d/o"; : > "$d/e"; : > "$d/i"
done"#;

/// What a script answered.
pub(crate) struct Reply {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Default)]
struct State {
    idle: Vec<Session>,
    /// Sessions alive, idle or busy.
    count: usize,
    /// No new session before this moment: the last one failed to start.
    pause_until: Option<Instant>,
}

/// The sessions of one host.
#[derive(Default)]
pub(crate) struct Sessions {
    state: Mutex<State>,
}

/// The sessions of a host and key, shared by every environment made for them.
pub(crate) fn sessions_for(key: &str) -> Arc<Sessions> {
    static ALL: OnceLock<Mutex<HashMap<String, Arc<Sessions>>>> = OnceLock::new();
    let all = ALL.get_or_init(Default::default);
    let mut all = all.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    all.entry(key.to_owned()).or_default().clone()
}

fn base64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn token() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("{:x}{:x}", nanos, std::process::id())
}

impl Sessions {
    /// Runs `script` with `input` in a session. `None` when no session can take it: the
    /// caller makes a connection of its own, and nothing ran here.
    pub fn call(&self, start: &dyn Fn() -> Command, script: &str, input: &[u8]) -> Option<io::Result<Reply>> {
        let mut session = self.take(start)?;
        let request = format!("{}\n{}\n", base64(script.as_bytes()), base64(input));
        if session.stdin.write_all(request.as_bytes()).and_then(|()| session.stdin.flush()).is_err() {
            // The session was gone before the request reached it.
            self.forget();
            return None;
        }
        match read_reply(&mut session.stdout) {
            Ok(reply) => {
                self.give_back(session);
                Some(Ok(reply))
            }
            Err(error) => {
                // The script may have run: it is not sent again.
                self.forget();
                Some(Err(io::Error::new(io::ErrorKind::ConnectionAborted, format!("the session to the host broke: {error}"))))
            }
        }
    }

    /// An idle session, or a new one while the host has fewer than the most.
    fn take(&self, start: &dyn Fn() -> Command) -> Option<Session> {
        {
            let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            while let Some(mut session) = state.idle.pop() {
                if matches!(session.child.try_wait(), Ok(None)) {
                    return Some(session);
                }
                state.count -= 1;
            }
            if state.count >= MAX_SESSIONS || state.pause_until.is_some_and(|until| Instant::now() < until) {
                return None;
            }
            state.count += 1;
        }
        match open(start) {
            Ok(session) => Some(session),
            Err(_) => {
                let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                state.count -= 1;
                state.pause_until = Some(Instant::now() + RETRY_AFTER);
                None
            }
        }
    }

    fn give_back(&self, session: Session) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.idle.push(session);
    }

    /// A session that died is no longer counted (it is killed as it is dropped).
    fn forget(&self) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.count = state.count.saturating_sub(1);
    }

    /// Closes the idle sessions, for a host being removed and for tests; a busy one goes
    /// back to the idle ones when its call ends.
    pub fn close_idle(&self) {
        let idle = {
            let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            state.count -= state.idle.len();
            std::mem::take(&mut state.idle)
        };
        drop(idle);
    }

    /// The process ids of the idle sessions, for tests that cut a connection.
    #[cfg(test)]
    pub fn idle_pids(&self) -> Vec<u32> {
        let state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.idle.iter().map(|session| session.child.id()).collect()
    }
}

/// Starts a session and waits for its handshake: lines a login script prints before it
/// are skipped.
fn open(start: &dyn Fn() -> Command) -> io::Result<Session> {
    let token = token();
    let mut command = start();
    command.arg(format!("exec sh -c {} sh {token}", crate::shell_quote(LOOP)));
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = command.spawn()?;
    let stdin = child.stdin.take().ok_or_else(|| io::Error::other("no stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| io::Error::other("no stdout"))?;
    let expected = format!("#bowyer-session# {token}");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let ready = loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break false,
                Ok(_) if line.trim_end() == expected => break true,
                Ok(_) => {}
            }
        };
        let _ = sender.send(ready.then_some(reader));
    });
    match receiver.recv_timeout(HANDSHAKE) {
        Ok(Some(stdout)) => Ok(Session { child, stdin, stdout }),
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            Err(io::Error::other("the session did not start"))
        }
    }
}

fn read_reply(reader: &mut impl BufRead) -> io::Result<Reply> {
    let mut header = String::new();
    if reader.read_line(&mut header)? == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed"));
    }
    let mut fields = header.split_whitespace();
    let parsed = (fields.next() == Some("#bowyer#"))
        .then(|| Some((fields.next()?.parse::<i32>().ok()?, fields.next()?.parse::<usize>().ok()?, fields.next()?.parse::<usize>().ok()?)))
        .flatten();
    let Some((code, out, err)) = parsed else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("not an answer: {}", header.trim_end())));
    };
    let mut stdout = vec![0; out];
    reader.read_exact(&mut stdout)?;
    let mut stderr = vec![0; err];
    reader.read_exact(&mut stderr)?;
    Ok(Reply { code, stdout, stderr })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_carries_its_bytes_and_the_next_one_follows() {
        let mut stream: &[u8] = b"#bowyer# 0 3 0\n\x00\n\xff#bowyer# 2 0 5\nnope!";
        let mut reader = BufReader::new(&mut stream);
        let first = read_reply(&mut reader).unwrap();
        assert_eq!((first.code, first.stdout.as_slice(), first.stderr.len()), (0, &b"\x00\n\xff"[..], 0));
        let second = read_reply(&mut reader).unwrap();
        assert_eq!((second.code, second.stderr.as_slice()), (2, &b"nope!"[..]));
        assert!(read_reply(&mut reader).is_err(), "the end of the stream is no answer");
        let mut junk: &[u8] = b"hello\n";
        assert!(read_reply(&mut BufReader::new(&mut junk)).is_err());
    }
}
