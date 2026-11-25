//! One program in a pseudoconsole. Four threads serve it: the reader drains the pipe, the engine
//! owns `Term`, the writer owns the blocking input pipe, the waiter ends the stream when the child
//! exits. No call here waits for the child, so the UI thread may use all of them.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError, bounded, select};

use crate::conpty::Pty;
use crate::engine::Engine;
use crate::keys::{Key, KeyEvent, ShiftEnter, encode, encode_text, wheel_report};
use crate::screen::{Modes, Screen, SearchResults};
use crate::snapshot::Snapshot;

const READ_BUFFER: usize = 64 * 1024;
/// Ceilings of the queues. Output backpressure reaches the child through the pipe.
const OUTPUT_CHUNKS: usize = 16;
const INPUT_MESSAGES: usize = 1024;
const REPLIES: usize = 256;
const EVENTS: usize = 1024;
const CONTROLS: usize = 64;
/// One write to the pipe. The boundaries fall on chars, and replies pass between chunks
/// (not inside a bracketed paste).
const WRITE_CHUNK: usize = 16 * 1024;
/// The brackets of a paste, for a program that asked for them.
const PASTE_OPEN: &[u8] = b"\x1b[200~";
const PASTE_CLOSE: &[u8] = b"\x1b[201~";
/// The shortest gap between two snapshots of the process table while output flows.
const ADOPT_INTERVAL: Duration = Duration::from_millis(250);
/// Lines one notch of the wheel moves, as Windows gives them by default. A notch is one mouse
/// report for a program that asked for mouse reports.
const WHEEL_LINES: u32 = 3;

/// What to start. `env` is the whole environment of the child, see `allowlisted`.
#[derive(Clone, Debug)]
pub struct SpawnSpec {
    pub command_line: String,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
    pub shift_enter: ShiftEnter,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalEvent {
    /// Bytes arrived, a new snapshot follows.
    Output,
    Title(String),
    Bell,
    Osc {
        code: String,
        payload: String,
    },
    /// The stream ended. `None` when the exit code never became known.
    Exited(Option<u32>),
}

/// What the user looks for in the text of the terminal, scrollback included.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchQuery {
    pub text: String,
    pub case_sensitive: bool,
    /// The text is a regular expression.
    pub regex: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputError {
    /// The program is not reading fast enough, and the queue is full.
    Busy,
    Closed,
}

impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(match self {
            Self::Busy => "the terminal input queue is full",
            Self::Closed => "the terminal is closed",
        })
    }
}

impl std::error::Error for InputError {}

pub(crate) enum Control {
    Resize,
    Scroll(i32),
    ScrollToBottom,
    /// Moves the view to this many lines back into history, 0 being the live screen.
    ScrollTo(usize),
    /// The match of the latest answer of the search the view should show.
    RevealMatch(usize),
    Search {
        query: SearchQuery,
        generation: u64,
    },
    ClearSearch,
    /// Clears the screen and the scrollback, as the clear command of a shell does.
    Clear,
}

/// What the threads of a session have in common. All of it is lock-free for readers.
pub(crate) struct Shared {
    pub(crate) screen: Snapshot<Screen>,
    pub(crate) search: Snapshot<SearchResults>,
    pub(crate) modes: AtomicU32,
    pub(crate) bytes: AtomicU64,
    /// When output last came, in milliseconds after `started`.
    pub(crate) last_output: AtomicU64,
    pub(crate) started: Instant,
    pub(crate) exit: OnceLock<u32>,
    size: AtomicU32,
    pub(crate) pty: Arc<Pty>,
}

impl Shared {
    pub(crate) fn requested_size(&self) -> (u16, u16) {
        let size = self.size.load(Ordering::SeqCst);
        ((size >> 16) as u16, size as u16)
    }

    pub(crate) fn exit_code(&self) -> Option<u32> {
        self.exit.get().copied()
    }

    /// Notes that output came now.
    pub(crate) fn output_came(&self) {
        self.last_output.store(self.started.elapsed().as_millis() as u64, Ordering::Relaxed);
    }
}

/// The smallest terminal: two columns (a wide character takes two) and one row. A panel
/// not laid out yet asks for nothing, and neither the pseudoconsole nor the screen takes
/// a size of zero.
const MIN_COLS: u16 = 2;
const MIN_ROWS: u16 = 1;

fn usable_size(cols: u16, rows: u16) -> (u16, u16) {
    (cols.max(MIN_COLS), rows.max(MIN_ROWS))
}

fn pack_size(cols: u16, rows: u16) -> u32 {
    let (cols, rows) = usable_size(cols, rows);
    (cols as u32) << 16 | rows as u32
}

fn spawn_thread(name: &str, work: impl FnOnce() + Send + 'static) -> io::Result<()> {
    thread::Builder::new()
        .name(name.to_string())
        .spawn(work)
        .map(drop)
}

pub struct TerminalSession {
    shared: Arc<Shared>,
    input: Sender<Vec<Vec<u8>>>,
    control: Sender<Control>,
    events: Receiver<TerminalEvent>,
    shift_enter: ShiftEnter,
}

impl TerminalSession {
    pub fn spawn(spec: &SpawnSpec) -> io::Result<Self> {
        let (cols, rows) = usable_size(spec.cols, spec.rows);
        let spec = &SpawnSpec { cols, rows, ..spec.clone() };
        let (pty, input_pipe, output_pipe) = Pty::spawn(spec)?;
        let pty = Arc::new(pty);
        let shared = Arc::new(Shared {
            screen: Snapshot::new(Screen::default()),
            search: Snapshot::new(SearchResults::default()),
            modes: AtomicU32::new(0),
            bytes: AtomicU64::new(0),
            last_output: AtomicU64::new(0),
            started: Instant::now(),
            exit: OnceLock::new(),
            size: AtomicU32::new(pack_size(spec.cols, spec.rows)),
            pty: pty.clone(),
        });

        let (input, input_queue) = bounded(INPUT_MESSAGES);
        let (replies, reply_queue) = bounded(REPLIES);
        let (output, output_queue) = bounded(OUTPUT_CHUNKS);
        let (control, control_queue) = bounded(CONTROLS);
        let (event_sink, events) = bounded(EVENTS);
        let (output_seen, adopt_queue) = bounded(1);

        spawn_thread("pty-writer", move || {
            write_loop(input_pipe, reply_queue, input_queue);
        })?;
        spawn_thread("pty-reader", move || {
            read_loop(output_pipe, output, output_seen);
        })?;
        let adopt_pty = pty.clone();
        spawn_thread("pty-adopt", move || adopt_loop(adopt_queue, adopt_pty))?;
        let (cols, rows) = (spec.cols, spec.rows);
        let engine_shared = shared.clone();
        // `Term` is built on its own thread: its listener is not `Send`.
        spawn_thread("pty-engine", move || {
            let engine = Engine::new(cols, rows, replies, event_sink, engine_shared);
            engine.run(output_queue, control_queue);
        })?;
        let waiter_shared = shared.clone();
        spawn_thread("pty-waiter", move || {
            let code = pty.wait();
            let _ = waiter_shared.exit.set(code);
            // Programs outside the job do not go with it; the rounds end them by their
            // parent id, which the snapshot keeps after the shell is gone.
            pty.kill_tree();
            // The console goes last: a program still attached would keep the close waiting.
            pty.close_console();
            // Whatever the cleanup rounds missed still dies with the job.
            pty.terminate();
            pty.kill_tree();
        })?;

        Ok(Self {
            shared,
            input,
            control,
            events,
            shift_enter: spec.shift_enter,
        })
    }

    pub fn pid(&self) -> u32 {
        self.shared.pty.pid
    }

    /// The latest picture. Taking it never waits.
    pub fn screen(&self) -> Arc<Screen> {
        self.shared.screen.get()
    }

    /// The modes in force now, which a snapshot may lag behind.
    pub fn modes(&self) -> Modes {
        Modes::from_bits(self.shared.modes.load(Ordering::SeqCst))
    }

    /// There is one receiver, and whoever owns the session reads it.
    pub fn events(&self) -> Receiver<TerminalEvent> {
        self.events.clone()
    }

    pub fn output_bytes(&self) -> u64 {
        self.shared.bytes.load(Ordering::Relaxed)
    }

    pub fn exit_code(&self) -> Option<u32> {
        self.shared.exit_code()
    }

    /// How long the program has written nothing (since it started, if it never wrote).
    pub fn quiet_for(&self) -> Duration {
        let last = Duration::from_millis(self.shared.last_output.load(Ordering::Relaxed));
        self.shared.started.elapsed().saturating_sub(last)
    }

    /// The programs the shell of the terminal runs now, by plain name (`claude`, `cargo`):
    /// none at its prompt. A chain of shells counts as the shell. `None` when the process
    /// table cannot be read. It reads the whole table: not for every frame.
    pub fn programs(&self) -> Option<Vec<String>> {
        let table = crate::programs::process_table()?;
        Some(crate::programs::programs_below(&table, self.pid()))
    }

    /// True once the stream ended and the last picture is out. The flag lives in the
    /// snapshot itself, so a reader of the final version cannot miss the end.
    pub fn is_closed(&self) -> bool {
        self.shared.screen.get().closed
    }

    fn enqueue(&self, segments: Vec<Vec<u8>>) -> Result<(), InputError> {
        self.input.try_send(segments).map_err(|error| match error {
            TrySendError::Full(_) => InputError::Busy,
            TrySendError::Disconnected(_) => InputError::Closed,
        })
    }

    /// Bytes as they are, for what a host encoded itself.
    pub fn send(&self, bytes: impl Into<Vec<u8>>) -> Result<(), InputError> {
        self.enqueue(vec![bytes.into()])
    }

    pub fn key(&self, event: KeyEvent) -> Result<(), InputError> {
        self.send(encode(event, self.modes(), self.shift_enter))
    }

    /// One queue entry however long the text is, the writer feeds it as the program reads.
    pub fn paste(&self, text: &str) -> Result<(), InputError> {
        let modes = self.modes();
        let normalized = sanitize(text).replace('\n', "\r");
        let encoded = encode_text(&normalized, modes);
        let segments = if modes.bracketed_paste {
            vec![PASTE_OPEN.to_vec(), encoded, PASTE_CLOSE.to_vec()]
        } else {
            vec![encoded]
        };
        self.enqueue(segments)
    }

    /// For programs that never see a paste as one event: line ends go out as Shift+Enter, which
    /// can only mean a new line, so a pause in the stream cannot turn one into a submit.
    pub fn paste_lines(&self, text: &str) -> Result<(), InputError> {
        let modes = self.modes();
        let newline = encode(
            KeyEvent::new(crate::Key::Enter, crate::Mods::SHIFT),
            modes,
            self.shift_enter,
        );
        let sanitized = sanitize(text);
        let mut segments = Vec::new();
        for (index, line) in sanitized.split('\n').enumerate() {
            if index > 0 {
                segments.push(newline.clone());
            }
            segments.push(encode_text(line, modes));
        }
        self.enqueue(segments)
    }

    /// Tells a program that asked for it whether the terminal has focus.
    pub fn focus(&self, gained: bool) -> Result<(), InputError> {
        if !self.modes().focus_events {
            return Ok(());
        }
        self.send(if gained {
            &b"\x1b[I"[..]
        } else {
            &b"\x1b[O"[..]
        })
    }

    /// The size asked for last, which the screen may still lag behind.
    pub fn size(&self) -> (u16, u16) {
        self.shared.requested_size()
    }

    /// The last size wins: a full queue means a resize is already on its way.
    pub fn resize(&self, cols: u16, rows: u16) {
        self.shared
            .size
            .store(pack_size(cols, rows), Ordering::SeqCst);
        drop(self.control.try_send(Control::Resize));
    }

    /// Positive goes back into history.
    pub fn scroll(&self, lines: i32) {
        drop(self.control.try_send(Control::Scroll(lines)));
    }

    pub fn scroll_to_bottom(&self) {
        drop(self.control.try_send(Control::ScrollToBottom));
    }

    /// Moves the view to `offset` lines back into history, 0 being the live screen.
    pub fn scroll_to(&self, offset: usize) {
        drop(self.control.try_send(Control::ScrollTo(offset)));
    }

    /// One turn of the wheel over the screen cell `cell` (column and row, from 0), `lines` up
    /// when positive. The modes in force decide where it goes: see `wheel_route`.
    pub fn wheel(&self, lines: i32, cell: (u16, u16), shift: bool) -> Result<(), InputError> {
        let modes = self.modes();
        match wheel_route(modes, shift) {
            WheelRoute::History => {
                self.scroll(lines);
                Ok(())
            }
            WheelRoute::Mouse => {
                let notches = lines.unsigned_abs().div_ceil(WHEEL_LINES);
                let mut bytes = Vec::new();
                for _ in 0..notches {
                    bytes
                        .extend(wheel_report(lines > 0, cell, modes.sgr_mouse).unwrap_or_default());
                }
                if bytes.is_empty() {
                    return Ok(());
                }
                self.send(bytes)
            }
            WheelRoute::Arrows => {
                let key = KeyEvent::plain(if lines > 0 { Key::Up } else { Key::Down });
                let mut bytes = Vec::new();
                for _ in 0..lines.unsigned_abs() {
                    bytes.extend(encode(key, modes, self.shift_enter));
                }
                if bytes.is_empty() {
                    return Ok(());
                }
                self.send(bytes)
            }
        }
    }

    /// Scrolls the view to the match `index` of the latest answer of the search. The
    /// lines of an answer move when the program prints more, so the engine finds the
    /// line of the match again when the view asks for it.
    pub fn reveal_match(&self, index: usize) {
        drop(self.control.try_send(Control::RevealMatch(index)));
    }

    /// Asks for the runs `query` finds over the whole scrollback. A later search has a
    /// larger `generation`, so an answer that arrives late is dropped.
    pub fn search(&self, query: SearchQuery, generation: u64) {
        drop(self.control.try_send(Control::Search { query, generation }));
    }

    pub fn clear_search(&self) {
        drop(self.control.try_send(Control::ClearSearch));
    }

    /// What the last search found. Taking it never waits.
    pub fn search_results(&self) -> Arc<SearchResults> {
        self.shared.search.get()
    }

    /// Clears the screen and the scrollback.
    pub fn clear(&self) {
        drop(self.control.try_send(Control::Clear));
    }

    /// Kills the program and everything it started.
    pub fn terminate(&self) {
        self.shared.pty.terminate();
    }

    /// Polls snapshots. For tests and background workers, never for the UI thread.
    pub fn wait_for(&self, timeout: Duration, check: impl Fn(&Screen) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if check(&self.screen()) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Polls until the stream has ended. For tests and background workers.
    pub fn wait_closed(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while !self.is_closed() {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
        true
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        // The threads see the end of the stream and leave on their own.
        self.terminate();
    }
}

/// Where a turn of the wheel goes: the history of the terminal, a program that asked for mouse
/// reports, or the arrow keys of a full-screen program that asked for neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WheelRoute {
    History,
    Mouse,
    Arrows,
}

/// The route of a turn of the wheel, as xterm, IDEA and Windows Terminal have it: a program that
/// asked for mouse reports gets the wheel, whichever screen it draws on, and Shift keeps the
/// wheel for the history. Without mouse reports the alternate screen gets the arrow keys, as
/// xterm's alternate scroll does, and the main screen, which has history, scrolls it.
fn wheel_route(modes: Modes, shift: bool) -> WheelRoute {
    if shift {
        WheelRoute::History
    } else if modes.mouse {
        WheelRoute::Mouse
    } else if modes.alt_screen {
        WheelRoute::Arrows
    } else {
        WheelRoute::History
    }
}

/// An escape in pasted text could end the bracket and turn the rest into keystrokes.
fn sanitize(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\x1b', "")
}

fn read_loop(mut pipe: File, output: Sender<Vec<u8>>, seen: Sender<()>) {
    let mut buffer = vec![0; READ_BUFFER];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(count) => {
                if output.send(buffer[..count].to_vec()).is_err() {
                    return;
                }
                // One queued signal is enough: a snapshot covers every chunk that came
                // before the adopt thread wakes up.
                let _ = seen.try_send(());
            }
        }
    }
}

/// Takes a snapshot of the process table shortly after the shell prints something. A
/// quiet terminal leaves this thread blocked on the channel: no timer and no snapshot.
/// Output that keeps coming costs at most one snapshot per interval, and the one delayed
/// call after a burst covers a program that printed nothing but its prompt.
fn adopt_loop(signals: Receiver<()>, pty: Arc<Pty>) {
    let mut last: Option<Instant> = None;
    loop {
        if signals.recv().is_err() {
            return;
        }
        // One burst: a look at most once per interval while the output goes on, and one
        // last look when it stops, however many signals the burst sent.
        loop {
            if let Some(previous) = last {
                let rest = ADOPT_INTERVAL.saturating_sub(previous.elapsed());
                if !rest.is_zero() {
                    thread::sleep(rest);
                    while signals.try_recv().is_ok() {}
                }
            }
            pty.adopt();
            last = Some(Instant::now());
            match signals.recv_timeout(ADOPT_INTERVAL) {
                Ok(()) => {}
                Err(RecvTimeoutError::Timeout) => {
                    // The output stopped: one last look at the tree it may have started.
                    pty.adopt();
                    last = Some(Instant::now());
                    break;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    pty.adopt();
                    return;
                }
            }
        }
    }
}

/// Longest prefix of at most `WRITE_CHUNK` bytes that ends on a char boundary.
fn chunk_end(bytes: &[u8]) -> usize {
    let mut end = bytes.len().min(WRITE_CHUNK);
    while end > 0 && end < bytes.len() && bytes[end] & 0xC0 == 0x80 {
        end -= 1;
    }
    if end == 0 { bytes.len() } else { end }
}

/// Replies go first: a program waiting for one may stop reading input until it gets it.
/// Inside a bracketed paste they wait for its end: between the brackets the program takes
/// every byte as pasted text, an answer to its question among them.
fn write_loop<W: Write>(mut pipe: W, replies: Receiver<Vec<u8>>, input: Receiver<Vec<Vec<u8>>>) {
    let write_replies = |pipe: &mut W| -> io::Result<()> {
        while let Ok(reply) = replies.try_recv() {
            pipe.write_all(&reply)?;
        }
        Ok(())
    };
    loop {
        if write_replies(&mut pipe).is_err() {
            return;
        }
        select! {
            recv(replies) -> reply => {
                let Ok(reply) = reply else { return };
                if pipe.write_all(&reply).is_err() {
                    return;
                }
            }
            recv(input) -> segments => {
                let Ok(segments) = segments else { return };
                let mut in_paste = false;
                for segment in segments {
                    if segment == PASTE_OPEN {
                        in_paste = true;
                    }
                    let mut rest = segment.as_slice();
                    while !rest.is_empty() {
                        let (chunk, tail) = rest.split_at(chunk_end(rest));
                        rest = tail;
                        if pipe.write_all(chunk).is_err() {
                            return;
                        }
                        if segment == PASTE_CLOSE {
                            in_paste = false;
                        }
                        if !in_paste && write_replies(&mut pipe).is_err() {
                            return;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_end_on_char_boundaries() {
        let text = "я".repeat(WRITE_CHUNK);
        let bytes = text.as_bytes();

        let end = chunk_end(bytes);

        assert!(end <= WRITE_CHUNK);
        assert!(text.is_char_boundary(end));
    }

    /// A pipe that answers a question of the program while the paste goes out: the
    /// answer must not land between the brackets.
    struct Recorder {
        written: Arc<std::sync::Mutex<Vec<u8>>>,
        replies: Sender<Vec<u8>>,
        asked: bool,
    }

    impl Write for Recorder {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.written.lock().unwrap().extend_from_slice(bytes);
            if !self.asked && bytes != PASTE_OPEN {
                self.asked = true;
                self.replies.send(b"\x1b[?62c".to_vec()).unwrap();
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn an_answer_waits_for_the_end_of_a_paste() {
        let (reply_sink, replies) = bounded(4);
        let (input_sink, input) = bounded(4);
        let written = Arc::new(std::sync::Mutex::new(Vec::new()));
        let pipe = Recorder { written: written.clone(), replies: reply_sink.clone(), asked: false };
        let text = vec![b'x'; WRITE_CHUNK * 3];
        input_sink.send(vec![PASTE_OPEN.to_vec(), text, PASTE_CLOSE.to_vec()]).unwrap();
        drop(input_sink);
        drop(reply_sink);
        write_loop(pipe, replies, input);
        let written = written.lock().unwrap();
        let answer = written.windows(6).position(|window| window == b"\x1b[?62c").expect("the answer went out");
        let close = written.windows(PASTE_CLOSE.len()).position(|window| window == PASTE_CLOSE).unwrap();
        assert!(answer > close, "the answer came after the paste");
    }

    /// A panel not laid out yet asks for no size: the terminal takes the smallest one.
    #[test]
    fn a_size_of_nothing_becomes_the_smallest_one() {
        assert_eq!(usable_size(0, 0), (MIN_COLS, MIN_ROWS));
        let unpacked = |packed: u32| ((packed >> 16) as u16, packed as u16);
        assert_eq!(unpacked(pack_size(0, 5)), (2, 5));
        assert_eq!(unpacked(pack_size(80, 0)), (80, 1));
        assert_eq!(unpacked(pack_size(120, 40)), (120, 40));
    }

    #[test]
    fn a_paste_cannot_close_its_own_bracket() {
        assert_eq!(sanitize("a\x1b[201~b\r\nc\rd"), "a[201~b\nc\nd");
    }

    #[test]
    fn the_wheel_goes_to_the_program_that_asked_for_the_mouse() {
        let alt = Modes {
            alt_screen: true,
            ..Modes::default()
        };
        let alt_mouse = Modes { mouse: true, ..alt };
        let main_mouse = Modes {
            mouse: true,
            ..Modes::default()
        };
        assert_eq!(wheel_route(Modes::default(), false), WheelRoute::History);
        assert_eq!(wheel_route(alt, false), WheelRoute::Arrows);
        assert_eq!(wheel_route(alt_mouse, false), WheelRoute::Mouse);
        assert_eq!(wheel_route(main_mouse, false), WheelRoute::Mouse);
        // Shift scrolls the history whatever the program asked for.
        assert_eq!(wheel_route(main_mouse, true), WheelRoute::History);
        assert_eq!(wheel_route(alt_mouse, true), WheelRoute::History);
        assert_eq!(wheel_route(alt, true), WheelRoute::History);
    }

    /// A job the shell put in the background goes with the tab, and the tab ends: the job
    /// was the last to hold the terminal.
    #[cfg(unix)]
    #[test]
    fn closing_a_terminal_ends_its_background_jobs() {
        let line = "set -m; sleep 300 & echo job:$!; sleep 300".to_owned();
        let session = TerminalSession::spawn(&SpawnSpec {
            command_line: line,
            cwd: None,
            env: crate::allowlisted(),
            cols: 80,
            rows: 12,
            shift_enter: ShiftEnter::default(),
        })
        .expect("spawn");
        assert!(session.wait_for(Duration::from_secs(15), |screen| screen.contains("job:")));
        let text = session.screen().lines().join("\n");
        let job: i32 = text.split("job:").nth(1).and_then(|rest| rest.split_whitespace().next()).and_then(|pid| pid.parse().ok()).expect("the job's number");
        session.terminate();
        let started = std::time::Instant::now();
        // SAFETY: signal 0 only asks whether the process is there.
        while unsafe { libc::kill(job, 0) } == 0 {
            if started.elapsed() >= Duration::from_secs(10) {
                // What is left of it, for a failure on a system this was not tried on.
                let table = std::process::Command::new("ps").args(["-A", "-o", "pid,ppid,pgid,stat,tty,command"]).output().map(|out| String::from_utf8_lossy(&out.stdout).into_owned()).unwrap_or_default();
                let left: Vec<&str> = table.lines().filter(|line| line.contains("sleep 300") || line.trim_start().starts_with("PID")).collect();
                panic!("the background job {job} outlived its terminal:\n{}", left.join("\n"));
            }
            thread::sleep(Duration::from_millis(50));
        }
        let started = std::time::Instant::now();
        while !session.is_closed() {
            assert!(started.elapsed() < Duration::from_secs(10), "the tab did not end");
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// A program that prints one line, starts a long child and then stays quiet.
    #[cfg(windows)]
    fn quiet_session() -> TerminalSession {
        let line = crate::command_line(
            std::path::Path::new("cmd.exe"),
            &[
                "/c".to_string(),
                "echo ready & ping -n 30 127.0.0.1 >NUL".to_string(),
            ],
        );
        let session = TerminalSession::spawn(&SpawnSpec {
            command_line: line,
            cwd: None,
            env: crate::allowlisted(),
            cols: 80,
            rows: 12,
            shift_enter: ShiftEnter::default(),
        })
        .expect("spawn");
        assert!(session.wait_for(Duration::from_secs(15), |screen| screen.contains("ready")));
        session
    }

    /// A shell at its prompt runs nothing; while a command runs, the terminal names it; the
    /// quiet time counts from the last output.
    #[cfg(windows)]
    #[test]
    fn a_terminal_names_what_its_shell_runs() {
        let session = TerminalSession::spawn(&SpawnSpec {
            command_line: crate::command_line(std::path::Path::new("cmd.exe"), &["/q".to_string(), "/k".to_string(), "echo ready".to_string()]),
            cwd: None,
            env: crate::allowlisted(),
            cols: 80,
            rows: 12,
            shift_enter: ShiftEnter::default(),
        })
        .expect("spawn");
        assert!(session.wait_for(Duration::from_secs(15), |screen| screen.contains("ready")));
        assert_eq!(session.programs(), Some(Vec::new()), "the shell at its prompt");
        session.send(b"ping -n 30 127.0.0.1 >NUL\r".to_vec()).unwrap();
        let started = Instant::now();
        while session.programs() != Some(vec!["ping".to_owned()]) {
            assert!(started.elapsed() < Duration::from_secs(15), "the command is not seen: {:?}", session.programs());
            thread::sleep(Duration::from_millis(50));
        }
        thread::sleep(Duration::from_millis(600));
        assert!(session.quiet_for() >= Duration::from_millis(500), "nothing was written meanwhile: {:?}", session.quiet_for());
        session.terminate();
    }

    /// The same on Unix, where `ps` tells what the shell runs.
    #[cfg(unix)]
    #[test]
    fn a_terminal_names_what_its_shell_runs() {
        let session = TerminalSession::spawn(&SpawnSpec {
            command_line: crate::command_line(std::path::Path::new("sh"), &["-c".to_string(), "echo ready; exec sh -i".to_string()]),
            cwd: None,
            env: crate::allowlisted(),
            cols: 80,
            rows: 12,
            shift_enter: ShiftEnter::default(),
        })
        .expect("spawn");
        assert!(session.wait_for(Duration::from_secs(15), |screen| screen.contains("ready")));
        thread::sleep(Duration::from_millis(300));
        assert_eq!(session.programs(), Some(Vec::new()), "the shell at its prompt");
        session.send(b"sleep 30\n".to_vec()).unwrap();
        let started = Instant::now();
        while session.programs() != Some(vec!["sleep".to_owned()]) {
            assert!(started.elapsed() < Duration::from_secs(15), "the command is not seen: {:?}", session.programs());
            thread::sleep(Duration::from_millis(50));
        }
        session.terminate();
    }

    #[cfg(windows)]
    #[test]
    fn a_quiet_terminal_takes_no_process_snapshots() {
        let session = quiet_session();
        // The burst that printed the line and its delayed snapshot pass first.
        thread::sleep(Duration::from_millis(600));
        let before = session.shared.pty.adopt_calls();
        thread::sleep(Duration::from_millis(1200));
        let after = session.shared.pty.adopt_calls();
        assert_eq!(before, after, "a quiet terminal took process snapshots");
    }

    #[cfg(windows)]
    #[test]
    fn the_job_of_a_session_holds_the_program_of_its_shell() {
        let session = quiet_session();
        let shell = session.pid();
        let started = Instant::now();
        let program = loop {
            let program = crate::sys::process_table()
                .into_iter()
                .find(|process| {
                    process.parent == Some(shell) && process.name.eq_ignore_ascii_case("PING.EXE")
                })
                .map(|process| process.pid);
            if let Some(program) = program {
                break program;
            }
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "the shell started no program"
            );
            thread::sleep(Duration::from_millis(20));
        };
        let started = Instant::now();
        while !session.shared.pty.job_ids().contains(&program) {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the job does not hold the program {program}; job {:?}, snapshots {}, tree {:?}",
                session.shared.pty.job_ids(),
                session.shared.pty.adopt_calls(),
                crate::sys::process_table()
                    .into_iter()
                    .filter(|p| p.pid == program || p.pid == shell || p.parent == Some(shell))
                    .map(|p| (p.pid, p.parent, p.name))
                    .collect::<Vec<_>>()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}
