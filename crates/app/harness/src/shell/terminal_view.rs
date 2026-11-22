//! The terminal of the window: one pseudoterminal session (`terminal` crate) drawn in the
//! centre in place of the conversation. It runs the shell of this computer in the session's
//! folder, `ssh` into the session's host, a harness resumed in its own interface, or a block
//! of code from the conversation. Selection, copy and paste, the wheel and the bar of the
//! history come from the grid of Arro (`term_grid.rs`).

use std::cell::Cell as SharedCell;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use terminal::{SpawnSpec, TerminalSession};
use ui_kit::prelude::*;
use ui_kit::*;

use super::{Shell, chip};
use super::term_grid::{self, CellMetrics, GridPanel, GridView, Position, Selection};
use crate::i18n::t;
use crate::launch;

/// The size a session starts with, before the first frame fits it to the view.
const START_COLS: u16 = 120;
const START_ROWS: u16 = 30;

/// What runs in a terminal.
#[derive(Clone, Debug)]
pub(super) struct TermSpec {
    /// The whole command line for the pseudoterminal.
    pub command_line: String,
    pub cwd: Option<PathBuf>,
    /// Variables besides the window's own.
    pub env: Vec<(String, String)>,
    /// The name in the header: the shell or the program, and where.
    pub title: String,
}

pub(super) struct TermView {
    session: Arc<TerminalSession>,
    spec: TermSpec,
    focus: FocusHandle,
    selection: Option<Selection>,
    /// Where a drag that selects started.
    dragging: Option<Position>,
    version: u64,
    closed: bool,
    metrics: Rc<SharedCell<Option<CellMetrics>>>,
}

/// The environment of a terminal: this process's own, as an external terminal would get,
/// with the terminal type of the screen.
fn environment() -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> =
        std::env::vars().filter(|(key, _)| !terminal::TERM_VARS.iter().any(|(name, _)| name.eq_ignore_ascii_case(key))).collect();
    env.extend(terminal::TERM_VARS.iter().map(|(key, value)| (key.to_string(), value.to_string())));
    env
}

/// Starts what `spec` names in a pseudoterminal.
fn start(spec: &TermSpec) -> io::Result<TerminalSession> {
    TerminalSession::spawn(&SpawnSpec {
        command_line: spec.command_line.clone(),
        cwd: spec.cwd.clone(),
        env: { let mut env = environment(); env.extend(spec.env.iter().cloned()); env },
        cols: START_COLS,
        rows: START_ROWS,
        shift_enter: Default::default(),
    })
}

impl TermView {
    fn new(session: TerminalSession, spec: TermSpec, cx: &mut Context<Self>) -> Self {
        term_grid::every_frame(cx, Self::refresh);
        Self {
            session: Arc::new(session),
            spec,
            focus: cx.focus_handle(),
            selection: None,
            dragging: None,
            version: 0,
            closed: false,
            metrics: Rc::new(SharedCell::new(None)),
        }
    }

    pub(super) fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub(super) fn title(&self) -> &str {
        &self.spec.title
    }

    pub(super) fn is_closed(&self) -> bool {
        self.closed
    }

    /// Ends the program and everything it started in the console.
    pub(super) fn terminate(&self) {
        self.session.terminate();
    }

    /// A new screen or the end of the program: draw again.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let screen = self.session.screen();
        let closed = screen.closed || self.session.is_closed();
        if screen.version != self.version || closed != self.closed {
            self.version = screen.version;
            self.closed = closed;
            cx.notify();
        }
    }

    fn copy_selection(&mut self, cx: &mut Context<Self>) -> bool {
        term_grid::copy_selection(&self.session, self.selection, cx)
    }

    fn key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let stroke = &event.keystroke;
        let modifiers = stroke.modifiers;
        // Ctrl+` goes back to the conversation: the window has it.
        if modifiers.control && stroke.key == "`" {
            return;
        }
        // Ctrl+Shift+C copies; Ctrl+C copies a selection and interrupts without one.
        let copy_key = modifiers.control && !modifiers.alt && stroke.key == "c";
        if copy_key && (modifiers.shift || self.selection.is_some()) {
            cx.stop_propagation();
            if self.copy_selection(cx) {
                self.selection = None;
                cx.notify();
            }
            return;
        }
        if self.closed {
            return;
        }
        if term_grid::send_key(&self.session, stroke, cx) {
            cx.stop_propagation();
            if self.selection.take().is_some() {
                cx.notify();
            }
        }
    }
}

impl GridPanel for TermView {
    fn pane_focus(&self, _pane: usize) -> FocusHandle {
        self.focus.clone()
    }

    fn grid_pressed(&mut self, _pane: usize, position: Position, clicks: usize, cx: &mut Context<Self>) {
        (self.selection, self.dragging) = term_grid::pressed(&self.session.screen(), position, clicks);
        cx.notify();
    }

    fn grid_moved(&mut self, _pane: usize, position: Option<Position>, _control: bool, cx: &mut Context<Self>) -> bool {
        if let (Some(anchor), Some(position)) = (self.dragging, position) {
            self.selection = Some(Selection::new(anchor, position));
            cx.notify();
        }
        false
    }

    fn grid_released(&mut self, _pane: usize, cx: &mut Context<Self>) {
        self.dragging = None;
        cx.notify();
    }

    /// The right button copies a selection, or pastes without one, as in Windows Terminal.
    fn grid_context_menu(&mut self, _pane: usize, _position: Point<Pixels>, cx: &mut Context<Self>) {
        if self.copy_selection(cx) {
            self.selection = None;
        } else if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let _ = self.session.paste(&text);
        }
        cx.notify();
    }
}

impl Render for TermView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.global::<Theme>();
        let focused = self.focus.is_focused(window);
        let ended = self.closed.then(|| {
            let code = self.session.exit_code().map(|code| format!(" ({code})")).unwrap_or_default();
            div()
                .px(px(10.))
                .py(px(4.))
                .text_size(px(12.))
                .text_color(theme.text_muted)
                .child(format!("{}{code}", t("terminal.ended")))
        });
        div()
            .id("terminal-view")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _: &MouseDownEvent, window, cx| window.focus(&this.focus, cx)))
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.island_bg)
            .child(
                div().flex_1().min_h_0().child(term_grid::grid(GridView {
                    session: self.session.clone(),
                    focused,
                    theme,
                    selection: self.selection,
                    matches: Vec::new(),
                    current: None,
                    hover: None,
                    host: Some(cx.entity()),
                    pane: 0,
                    metrics: self.metrics.clone(),
                })),
            )
            .children(ended)
    }
}

impl Drop for TermView {
    fn drop(&mut self) {
        self.session.terminate();
    }
}

impl Shell {
    /// Shows `line` in the terminal of the window, in place of the one there was. False when
    /// it could not start: the caller opens an external terminal instead.
    pub(super) fn show_in_terminal(&mut self, line: Option<launch::TerminalLine>, title: String, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(line) = line else { return false };
        let spec = TermSpec { command_line: line.line, cwd: line.cwd, env: line.env, title };
        let session = match start(&spec) {
            Ok(session) => session,
            Err(error) => {
                self.status = Some(format!("{}: {error}", t("terminal.failed")));
                return false;
            }
        };
        if let Some(old) = self.terminal.take() {
            old.read(cx).terminate();
        }
        let view = cx.new(|cx| TermView::new(session, spec, cx));
        view.read(cx).focus_handle().focus(window, cx);
        self.terminal = Some(view);
        self.terminal_shown = true;
        cx.notify();
        true
    }

    /// A shell in the folder of the open session, on its host; on this computer without one.
    pub(super) fn open_shell_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (env, cwd) = match &self.open {
            Some(open) => (self.env_for(&open.summary.key.env), open.summary.cwd.clone()),
            None => (self.envs.first().cloned(), None),
        };
        let Some(env) = env else { return };
        let title = terminal_title(&*env, cwd.as_deref());
        if !self.show_in_terminal(launch::terminal_line(&*env, cwd.as_deref(), None), title, window, cx) {
            let result = launch::open_terminal(&*env, cwd.as_deref(), None);
            self.report(result, cx);
        }
    }

    /// Ctrl+` and the terminal button: back to the terminal there is, away from it, or a new
    /// shell when there is none or its program ended.
    pub(super) fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let alive = self.terminal.as_ref().is_some_and(|view| !view.read(cx).is_closed());
        if !alive {
            self.open_shell_terminal(window, cx);
            return;
        }
        self.terminal_shown = !self.terminal_shown;
        if self.terminal_shown {
            if let Some(view) = &self.terminal {
                view.read(cx).focus_handle().focus(window, cx);
            }
        } else {
            self.focus.focus(window, cx);
        }
        cx.notify();
    }

    pub(super) fn close_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.terminal.take() {
            view.read(cx).terminate();
        }
        self.terminal_shown = false;
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// The terminal when it is the one shown in the centre.
    pub(super) fn shown_terminal(&self) -> Option<Entity<TermView>> {
        self.terminal.clone().filter(|_| self.terminal_shown)
    }

    /// The terminal with its header: what runs, back to the conversation, close.
    pub(super) fn render_terminal(&self, view: Entity<TermView>, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.global::<Theme>();
        let title = view.read(cx).title().to_owned();
        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .h(px(40.))
            .px(px(12.))
            .border_b_1()
            .border_color(theme.border)
            .child(icon("toolwindows/terminal", &theme))
            .child(div().flex_1().min_w_0().truncate().font_weight(FontWeight::MEDIUM).child(title))
            .when(self.open.is_some(), |this| {
                this.child(
                    chip("terminal-back", t("terminal.back"), &theme)
                        .tooltip(|_, cx| tooltip(t("terminal.back"), Some("Ctrl+`".into()), cx))
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.toggle_terminal(window, cx))),
                )
            })
            .child(
                icon_button("terminal-close", "general/close", t("terminal.close"), None, &theme)
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.close_terminal(window, cx))),
            );
        div().flex().flex_col().size_full().child(header).child(div().flex_1().min_h_0().child(view)).into_any_element()
    }
}

/// The name of a shell in the header: where it runs.
fn terminal_title(env: &dyn envs::Environment, cwd: Option<&str>) -> String {
    let place = cwd.unwrap_or("~");
    match env.id().ssh_host() {
        Some(host) => format!("{host} · {place}"),
        None => place.to_owned(),
    }
}

#[cfg(all(test, windows))]
mod tests {
    use terminal::{Key, KeyEvent, Mods, SpawnSpec, TerminalSession};

    /// Ctrl+C in PowerShell drops the typed line and gives a new prompt. Starts a real shell:
    /// run with `--ignored`.
    #[test]
    #[ignore]
    fn control_c_drops_the_typed_line() {
        let session = TerminalSession::spawn(&SpawnSpec {
            command_line: std::env::var("BOWYER_TEST_SHELL").unwrap_or_else(|_| "powershell.exe -NoLogo -NoProfile".to_owned()),
            cwd: None,
            env: super::environment(),
            cols: 80,
            rows: 20,
            shift_enter: Default::default(),
        })
        .unwrap();
        let text = |session: &TerminalSession| session.screen().lines().join("\n");
        let wait = |session: &TerminalSession, what: &dyn Fn(&str) -> bool| {
            for _ in 0..100 {
                if what(&text(session)) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            false
        };
        assert!(wait(&session, &|t| t.contains("PS ")), "no prompt: {}", text(&session));
        for ch in "Get-Ch".chars() {
            session.key(KeyEvent::plain(Key::Char(ch))).unwrap();
        }
        assert!(wait(&session, &|t| t.contains("Get-Ch")), "{}", text(&session));
        session.key(KeyEvent::new(Key::Char('c'), Mods { shift: false, ctrl: true, alt: false })).unwrap();
        let prompts = |t: &str| t.matches("PS ").count();
        assert!(wait(&session, &|t| prompts(t) >= 2 && !t.contains("Get-Chc")), "{}", text(&session));
    }

    /// A block of code runs on a real host in the terminal of the window, keeps its quotes and
    /// leaves the login shell open. `BOWYER_SSH_HOST=user@host BOWYER_SSH_KEY=key`, `--ignored`.
    #[test]
    #[ignore]
    fn a_block_runs_over_ssh_in_the_window() {
        let host = std::env::var("BOWYER_SSH_HOST").expect("BOWYER_SSH_HOST");
        let key = std::env::var_os("BOWYER_SSH_KEY").map(std::path::PathBuf::from);
        let env = envs::SshEnvironment::new(&host).with_identity(key);
        let script = "printf '%s|\\n' \"a  b don\u{2019}t\"; echo \"mark-$((6*7))\"";
        let line = crate::launch::block_line(&env, Some("/tmp"), crate::launch::Shell::Posix, script).unwrap();
        let session = TerminalSession::spawn(&SpawnSpec {
            command_line: line.line,
            cwd: line.cwd,
            env: super::environment(),
            cols: 100,
            rows: 20,
            shift_enter: Default::default(),
        })
        .unwrap();
        let text = |session: &TerminalSession| session.screen().lines().join("\n");
        let wait = |what: &dyn Fn(&str) -> bool| {
            for _ in 0..150 {
                if what(&text(&session)) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            false
        };
        assert!(wait(&|t| t.contains("mark-42")), "{}", text(&session));
        assert!(text(&session).contains("a  b don\u{2019}t|"), "{}", text(&session));
        session.paste("pwd").unwrap();
        session.key(KeyEvent::plain(Key::Enter)).unwrap();
        assert!(wait(&|t| t.lines().any(|l| l.trim() == "/tmp")), "{}", text(&session));
        session.terminate();
    }

    /// Ctrl+C stops a program that runs in the shell. Run with `--ignored`.
    #[test]
    #[ignore]
    fn control_c_stops_a_running_program() {
        let session = TerminalSession::spawn(&SpawnSpec {
            command_line: std::env::var("BOWYER_TEST_SHELL").unwrap_or_else(|_| "powershell.exe -NoLogo -NoProfile".to_owned()),
            cwd: None,
            env: super::environment(),
            cols: 80,
            rows: 20,
            shift_enter: Default::default(),
        })
        .unwrap();
        let text = |session: &TerminalSession| session.screen().lines().join("\n");
        let wait = |session: &TerminalSession, what: &dyn Fn(&str) -> bool| {
            for _ in 0..100 {
                if what(&text(session)) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            false
        };
        assert!(wait(&session, &|t| t.contains("PS ")), "no prompt: {}", text(&session));
        session.paste("ping -t 127.0.0.1").unwrap();
        session.key(KeyEvent::plain(Key::Enter)).unwrap();
        assert!(wait(&session, &|t| t.matches("127.0.0.1").count() >= 3), "{}", text(&session));
        session.key(KeyEvent::new(Key::Char('c'), Mods { shift: false, ctrl: true, alt: false })).unwrap();
        assert!(wait(&session, &|t| t.lines().filter(|l| !l.trim().is_empty()).next_back().is_some_and(|l| l.starts_with("PS "))), "{}", text(&session));
    }
}
