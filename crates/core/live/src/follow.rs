//! Following a turn that runs detached on an SSH host. The run goes on there whatever
//! happens to the connection that reads it; when that connection drops, another one picks
//! the output up at the byte where the first left off, until the host says the run ended.

use std::io::{self, BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use futures_channel::mpsc::UnboundedSender;

use crate::LiveEvent;

/// A connection that reads a run.
pub(crate) struct Connection {
    pub stdout: Box<dyn Read + Send>,
    pub stderr: Box<dyn Read + Send>,
    pub pid: u32,
    /// Waits for the connection to end; its exit code.
    pub wait: Box<dyn FnOnce() -> Option<i32> + Send>,
}

impl Connection {
    pub(crate) fn of(mut child: std::process::Child) -> Option<Connection> {
        Some(Connection {
            stdout: Box::new(child.stdout.take()?),
            stderr: Box::new(child.stderr.take()?),
            pid: child.id(),
            wait: Box::new(move || child.wait().ok().and_then(|status| status.code())),
        })
    }
}

/// What a stop and the follower share: whether the turn was stopped, and the process id of
/// the connection that reads it now (0 between connections).
#[derive(Clone, Debug, Default)]
pub(crate) struct Link {
    pub stopped: Arc<AtomicBool>,
    pub current: Arc<AtomicU32>,
}

/// How following a run ended.
#[derive(Debug, PartialEq)]
pub(crate) struct Outcome {
    /// The exit code: the run's when the host told it, else the connection's.
    pub code: Option<i32>,
    /// The end of the run's error output.
    pub stderr: String,
    /// The connection dropped and was not followed again (stopped, or before the run began):
    /// the run may still go on on the host.
    pub lost: bool,
}

/// What the error output of one connection said.
struct Said {
    tail: Vec<String>,
    started: bool,
    ended: bool,
}

const TAIL: usize = 20;
/// The longest wait between tries to reach the host again.
const LONGEST_PAUSE: Duration = Duration::from_secs(30);

/// The first wait before the host is tried again.
pub(crate) const FIRST_PAUSE: Duration = Duration::from_secs(2);
/// What a turn ends with when it was stopped while its host was out of reach.
pub(crate) const LOST: &str = "Stopped while the host was out of reach: the turn may still be running there.";

pub(crate) const RECONNECTING: &str = "The connection to the host dropped; the turn goes on there. Connecting again…";

/// Reads `first` and, when it drops after the run began, connections from `reopen` (given
/// the byte to go on from), sending each whole line of output to `lines`, until the host
/// says the run ended or the turn is stopped. `pause` is the first wait between tries.
pub(crate) fn follow(
    first: Connection,
    mut reopen: impl FnMut(u64) -> io::Result<Connection>,
    link: &Link,
    lines: &mpsc::Sender<String>,
    events: &UnboundedSender<LiveEvent>,
    pause: Duration,
) -> Outcome {
    let mut connection = first;
    let mut offset = 0u64;
    let mut tail: Vec<String> = Vec::new();
    let mut began = false;
    loop {
        link.current.store(connection.pid, Ordering::SeqCst);
        let stderr = std::mem::replace(&mut connection.stderr, Box::new(io::empty()));
        let err_events = events.clone();
        let known = tail.clone();
        let err_reader = thread::spawn(move || read_stderr(stderr, known, &err_events));
        offset += read_lines(std::mem::replace(&mut connection.stdout, Box::new(io::empty())), lines);
        // The process is ending: a stop must not kill whatever gets its number next.
        link.current.store(0, Ordering::SeqCst);
        let code = (connection.wait)();
        let said = err_reader.join().unwrap_or(Said { tail: Vec::new(), started: false, ended: false });
        tail = said.tail;
        began |= said.started;
        let stderr = tail.join("\n").trim().to_owned();
        if said.ended {
            return Outcome { code, stderr, lost: false };
        }
        if !began || link.stopped.load(Ordering::SeqCst) {
            return Outcome { code, stderr, lost: began };
        }
        let _ = events.unbounded_send(LiveEvent::Stderr(RECONNECTING.into()));
        let mut wait = pause;
        connection = loop {
            if !sleep_unless_stopped(wait, link) {
                return Outcome { code: None, stderr, lost: true };
            }
            if let Ok(next) = reopen(offset) {
                break next;
            }
            wait = (wait * 2).min(LONGEST_PAUSE);
        };
    }
}

/// Sends every whole line, without its line end, and returns the bytes they took. A line
/// cut by the connection's end is left for the next connection to read whole.
fn read_lines(stdout: Box<dyn Read + Send>, lines: &mpsc::Sender<String>) -> u64 {
    let mut reader = BufReader::new(stdout);
    let mut taken = 0u64;
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return taken,
            Ok(read) if line.ends_with(b"\n") => {
                taken += read as u64;
                let text = String::from_utf8_lossy(&line);
                // The reader may be gone (a harness's error ended it): the output is still
                // read to its end, so the count stays right.
                let _ = lines.send(text.trim_end_matches(['\n', '\r']).to_owned());
            }
            Ok(_) => return taken,
        }
    }
}

fn read_stderr(stderr: Box<dyn Read + Send>, mut tail: Vec<String>, events: &UnboundedSender<LiveEvent>) -> Said {
    let (mut started, mut ended) = (false, false);
    let mut reader = BufReader::new(stderr);
    let mut line = Vec::new();
    while matches!(reader.read_until(b'\n', &mut line), Ok(read) if read > 0) {
        let text = String::from_utf8_lossy(&line).trim_end_matches(['\n', '\r']).to_owned();
        line.clear();
        match text.trim() {
            envs::RUN_STARTED => started = true,
            envs::RUN_END => ended = true,
            "" => {}
            _ => {
                tail.push(text);
                if tail.len() > TAIL {
                    tail.remove(0);
                }
                let _ = events.unbounded_send(LiveEvent::Stderr(tail.join("\n")));
            }
        }
    }
    Said { tail, started, ended }
}

/// Waits `time`, or less if the turn is stopped; whether it was not.
fn sleep_unless_stopped(time: Duration, link: &Link) -> bool {
    let step = Duration::from_millis(100).min(time.max(Duration::from_millis(1)));
    let mut slept = Duration::ZERO;
    while slept < time {
        if link.stopped.load(Ordering::SeqCst) {
            return false;
        }
        thread::sleep(step);
        slept += step;
    }
    !link.stopped.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn connection(stdout: &str, stderr: &str, code: i32) -> Connection {
        Connection {
            stdout: Box::new(io::Cursor::new(stdout.as_bytes().to_vec())),
            stderr: Box::new(io::Cursor::new(stderr.as_bytes().to_vec())),
            pid: 0,
            wait: Box::new(move || Some(code)),
        }
    }

    fn run(first: Connection, next: Vec<Connection>, link: &Link) -> (Outcome, Vec<String>, Vec<u64>) {
        let (sender, receiver) = mpsc::channel();
        let (events, _) = futures_channel::mpsc::unbounded();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let mut next = next.into_iter();
        let reopen = {
            let asked = asked.clone();
            move |from: u64| {
                asked.lock().unwrap().push(from);
                next.next().ok_or_else(|| io::Error::other("unreachable"))
            }
        };
        let outcome = follow(first, reopen, link, &sender, &events, Duration::from_millis(1));
        drop(sender);
        let asked = asked.lock().unwrap().clone();
        (outcome, receiver.into_iter().collect(), asked)
    }

    /// A connection that drops mid-line is followed again from the end of the last whole
    /// line: nothing is lost, nothing comes twice.
    #[test]
    fn a_dropped_connection_is_followed_again_where_it_left_off() {
        let first = connection("{\"a\":1}\n{\"b\":2}\n{\"c\":", "bowyer-run-started\n", 255);
        let unreachable_then_back = vec![connection("{\"c\":3}\r\n{\"d\":4}\n", "tool said no\n\nbowyer-run-end\n", 0)];
        let link = Link::default();
        let (outcome, lines, asked) = run(first, unreachable_then_back, &link);
        assert_eq!(lines, ["{\"a\":1}", "{\"b\":2}", "{\"c\":3}", "{\"d\":4}"]);
        assert_eq!(asked, [16]);
        assert_eq!(outcome, Outcome { code: Some(0), stderr: "tool said no".into(), lost: false });
    }

    /// The host's word that the run ended is what ends it, whatever the exit code.
    #[test]
    fn a_run_ends_when_the_host_says_so() {
        let first = connection("x\n", "bowyer-run-started\nbad things\n\nbowyer-run-end\n", 255);
        let (outcome, lines, asked) = run(first, Vec::new(), &Link::default());
        assert_eq!((lines.len(), asked.len()), (1, 0));
        assert_eq!(outcome, Outcome { code: Some(255), stderr: "bad things".into(), lost: false });
    }

    /// A connection that never got the run going is not followed: there is nothing to follow.
    #[test]
    fn a_run_that_never_began_is_not_followed() {
        let first = connection("", "ssh: connect to host box port 22: Connection timed out\n", 255);
        let (outcome, _, asked) = run(first, Vec::new(), &Link::default());
        assert!(asked.is_empty());
        assert_eq!(outcome, Outcome { code: Some(255), stderr: "ssh: connect to host box port 22: Connection timed out".into(), lost: false });
    }

    /// A stop while the host is out of reach ends the following, and says the run may
    /// still go on there.
    #[test]
    fn a_stop_ends_the_tries() {
        let link = Link::default();
        link.stopped.store(true, Ordering::SeqCst);
        let first = connection("x\n", "bowyer-run-started\n", 255);
        let (outcome, _, asked) = run(first, Vec::new(), &link);
        assert!(asked.is_empty());
        assert!(outcome.lost);
    }

    /// The whole way on a real host: a run detached there loses its connection after two
    /// lines (the window kills it) and is followed again to its end. Runs `sh` only.
    /// `BOWYER_SSH_HOST=user@host cargo test -p live -- --ignored followed_again_on_a_host`.
    #[test]
    #[ignore = "needs an SSH host"]
    fn a_run_is_followed_again_on_a_host() {
        let host = std::env::var("BOWYER_SSH_HOST").expect("BOWYER_SSH_HOST");
        let mut ssh = envs::SshEnvironment::new(&host);
        let system = std::path::Path::new(r"C:\Windows\System32\OpenSSH\ssh.exe");
        if cfg!(windows) && system.is_file() {
            ssh.ssh = system.to_path_buf();
        }
        let run = format!("follow-test-{}", std::process::id());
        let spawn = |mut command: std::process::Command| {
            command.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
            command.spawn()
        };
        let script = "for i in 1 2 3 4 5 6; do echo line$i; sleep 1; done; echo oops >&2; exit 3";
        let mut child = spawn(ssh.detached("sh", &["-c", script], None, &run)).unwrap();
        // The prompt goes on standard input; here there is none.
        drop(child.stdin.take());
        let first = Connection::of(child).unwrap();
        let link = Link::default();
        let (sender, receiver) = mpsc::channel();
        let (events, _) = futures_channel::mpsc::unbounded();
        let follower = thread::spawn({
            let (link, ssh, run) = (link.clone(), ssh.clone(), run.clone());
            move || {
                let reopen = |from: u64| Connection::of(spawn(ssh.follow_detached(&run, from))?).ok_or_else(|| io::Error::other("no output"));
                follow(first, reopen, &link, &sender, &events, Duration::from_millis(500))
            }
        });
        let mut lines = Vec::new();
        for line in receiver.iter() {
            lines.push(line);
            if lines.len() == 2 {
                crate::stop_tree(link.current.load(Ordering::SeqCst));
            }
        }
        let outcome = follower.join().unwrap();
        let _ = envs::Environment::run(&ssh, "sh", &["-c", &format!("rm -f \"$HOME/.cache/bowyer/runs/{run}\".*")], None);
        assert_eq!(lines, (1..=6).map(|i| format!("line{i}")).collect::<Vec<_>>());
        assert_eq!(outcome, Outcome { code: Some(3), stderr: "oops".into(), lost: false });
    }

    /// The tries go on while the host is out of reach, a while apart.
    #[test]
    fn the_host_is_tried_until_it_answers() {
        let first = connection("x\n", "bowyer-run-started\n", 255);
        let (sender, receiver) = mpsc::channel();
        let (events, _) = futures_channel::mpsc::unbounded();
        let mut tries = 0;
        let reopen = |from: u64| {
            tries += 1;
            assert_eq!(from, 2);
            if tries < 4 { Err(io::Error::other("unreachable")) } else { Ok(connection("y\n", "bowyer-run-end\n", 0)) }
        };
        let outcome = follow(first, reopen, &Link::default(), &sender, &events, Duration::from_millis(1));
        drop(sender);
        assert_eq!(tries, 4);
        assert_eq!(receiver.into_iter().collect::<Vec<_>>(), ["x", "y"]);
        assert_eq!(outcome.code, Some(0));
    }
}
