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

use model::SessionKey;

use super::Shell;
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
    /// The session whose panel shows this terminal: the one open when it started (`None`
    /// with no session open).
    pub(super) owner: Option<SessionKey>,
    /// A harness resumed in its own interface runs here (an npm one as `node`).
    pub(super) harness: bool,
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
            owner: None,
            harness: false,
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

    /// Whether something goes on in the terminal: see [`super::quit_view::terminal_busy`].
    /// It reads the process table: not for every frame.
    pub(super) fn busy(&self) -> bool {
        !self.closed && super::quit_view::terminal_busy(self.session.programs().as_deref(), self.session.quiet_for(), self.harness)
    }

    /// The session and whether a harness was resumed in it, for a look off the window's
    /// thread at what goes on there.
    pub(super) fn watch(&self) -> Option<(Arc<TerminalSession>, bool)> {
        (!self.closed).then(|| (self.session.clone(), self.harness))
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
    /// The session whose terminals the panel shows: the open one, a new one too.
    fn terminal_owner(&self) -> Option<SessionKey> {
        self.open.as_ref().map(|open| open.summary.key.clone())
    }

    /// Where the open session's terminals are in `terminals`, in the order they opened.
    fn own_terminals(&self, cx: &App) -> Vec<usize> {
        let owner = self.terminal_owner();
        (0..self.terminals.len()).filter(|ix| self.terminals[*ix].read(cx).owner == owner).collect()
    }

    /// The tab of the open session the panel shows.
    fn active_own_terminal(&self, cx: &App) -> Option<usize> {
        let own = self.own_terminals(cx);
        let chosen = self.active_terminal.get(&self.terminal_owner()).copied();
        own.iter().copied().find(|ix| Some(self.terminals[*ix].entity_id()) == chosen).or_else(|| own.last().copied())
    }

    /// The open session's terminals go with it when the harness names a new session.
    pub(super) fn rename_terminal_owner(&mut self, from: &SessionKey, to: &SessionKey, cx: &mut Context<Self>) {
        let (from, to) = (Some(from.clone()), Some(to.clone()));
        for view in &self.terminals {
            view.update(cx, |view, _| {
                if view.owner == from {
                    view.owner = to.clone();
                }
            });
        }
        if self.terminal_shown.remove(&from) {
            self.terminal_shown.insert(to.clone());
        }
        if let Some(active) = self.active_terminal.remove(&from) {
            self.active_terminal.insert(to, active);
        }
    }

    /// Shows `line` in a new tab of the open session's terminals. False when it could not
    /// start: the caller opens an external terminal instead.
    pub(super) fn show_in_terminal(&mut self, line: Option<launch::TerminalLine>, title: String, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(mut line) = line else { return false };
        // Claude Code started in a terminal of a folder an IDE has open works with that IDE,
        // as in the IDE's own terminal.
        if let Some(cwd) = line.cwd.as_ref().and_then(|cwd| cwd.to_str())
            && let Some(ide) = self.ide_of(cwd)
        {
            line.env.extend(ide.environment().into_iter().map(|(key, value)| (key.to_owned(), value)));
        }
        let spec = TermSpec { command_line: line.line, cwd: line.cwd, env: line.env, title };
        let session = match start(&spec) {
            Ok(session) => session,
            Err(error) => {
                self.status = Some(format!("{}: {error}", t("terminal.failed")));
                return false;
            }
        };
        let owner = self.terminal_owner();
        let view = cx.new(|cx| TermView::new(session, spec, cx));
        view.update(cx, |view, _| view.owner = owner.clone());
        view.read(cx).focus_handle().focus(window, cx);
        self.active_terminal.insert(owner.clone(), view.entity_id());
        self.terminals.push(view);
        self.terminal_shown.insert(owner);
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

    /// A shell of this computer the user picked (PowerShell, Command Prompt, Git Bash…) in the
    /// folder of the open session.
    fn open_chosen_shell(&mut self, shell: terminal::Shell, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal_menu = false;
        let cwd = self.open.as_ref().filter(|open| open.summary.key.env.is_local()).and_then(|open| open.summary.cwd.clone());
        let line = launch::TerminalLine { line: shell.command_line.clone(), cwd: cwd.as_ref().map(PathBuf::from), env: Vec::new() };
        let title = format!("{} · {}", shell.name, cwd.as_deref().unwrap_or("~"));
        self.show_in_terminal(Some(line), title, window, cx);
    }

    /// Whether the + of the panel offers the shells of this computer: the open session (or
    /// none) is here, not on an SSH host.
    fn shells_here(&self) -> bool {
        self.open.as_ref().is_none_or(|open| open.summary.key.env.is_local())
    }

    /// Ctrl+` and the terminal button: shows the open session's terminals or hides them;
    /// with none, a shell in its folder.
    pub(super) fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.own_terminals(cx).is_empty() {
            self.open_shell_terminal(window, cx);
            return;
        }
        let owner = self.terminal_owner();
        if self.terminal_shown.remove(&owner) {
            self.focus.focus(window, cx);
        } else {
            self.terminal_shown.insert(owner);
            self.focus_terminal(window, cx);
        }
        cx.notify();
    }

    fn focus_terminal(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.active_own_terminal(cx) {
            self.terminals[ix].read(cx).focus_handle().focus(window, cx);
        }
    }

    fn select_terminal(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.terminals.get(ix) {
            self.active_terminal.insert(self.terminal_owner(), view.entity_id());
            self.focus_terminal(window, cx);
            cx.notify();
        }
    }

    /// Closes one tab and ends its shell; the session's panel goes with its last one.
    pub(super) fn close_terminal(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.terminals.len() {
            return;
        }
        let view = self.terminals.remove(ix);
        view.read(cx).terminate();
        let owner = view.read(cx).owner.clone();
        if self.terminals.iter().all(|other| other.read(cx).owner != owner) {
            self.terminal_shown.remove(&owner);
            self.active_terminal.remove(&owner);
            self.focus.focus(window, cx);
        } else {
            self.focus_terminal(window, cx);
        }
        cx.notify();
    }

    /// Closes every tab whose session `gone` says is gone, and ends what runs there: a
    /// deleted session, or the sessions of a host the user removed. A harness left running
    /// in such a tab would write the session back.
    pub(super) fn close_terminals_of(&mut self, gone: impl Fn(&SessionKey) -> bool, cx: &mut Context<Self>) {
        let mut owners = Vec::new();
        self.terminals.retain(|view| {
            let view = view.read(cx);
            let Some(owner) = view.owner.as_ref().filter(|owner| gone(owner)) else { return true };
            view.terminate();
            owners.push(Some(owner.clone()));
            false
        });
        for owner in owners {
            self.terminal_shown.remove(&owner);
            self.active_terminal.remove(&owner);
        }
        cx.notify();
    }

    /// Whether a terminal has the keyboard: its programs get the keys the window would take.
    pub(super) fn terminal_focused(&self, window: &Window, cx: &App) -> bool {
        self.terminals.iter().any(|view| view.read(cx).focus_handle().is_focused(window))
    }

    /// The list of shells under the panel's ▾.
    fn render_terminal_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.terminal_menu {
            return None;
        }
        let theme = *cx.global::<Theme>();
        let bounds = self.terminal_anchor.bounds()?;
        let shells = self.shells.clone().unwrap_or_default();
        let list = div()
            .id("terminal-menu")
            .min_w(px(260.))
            .p(px(4.))
            .rounded(px(9.))
            .bg(theme.popup_bg)
            .border_1()
            .border_color(theme.popup_border)
            .shadow(popup_shadow(&theme))
            .text_size(px(12.5))
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                this.terminal_menu = false;
                cx.notify();
            }))
            .child(div().px(px(10.)).pt(px(4.)).pb(px(6.)).text_size(px(11.)).text_color(theme.text_muted).child(t("terminal.pick")))
            .children(shells.into_iter().enumerate().map(|(ix, shell)| {
                let line = shell.command_line.clone();
                div()
                    .id(("terminal-shell", ix))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .h(px(28.))
                    .px(px(10.))
                    .rounded(px(6.))
                    .text_color(theme.text_secondary)
                    .hover(|style| style.bg(theme.selection_hover).text_color(theme.text))
                    .child(div().flex_none().child(shell.name.clone()))
                    .child(div().flex_1().min_w_0().truncate().font_family(super::MONO).text_size(px(11.)).text_color(theme.text_muted).child(line))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.open_chosen_shell(shell.clone(), window, cx)))
            }));
        let at = point(bounds.left(), bounds.bottom() + px(4.));
        Some(deferred(anchored().anchor(Anchor::TopLeft).position(at).snap_to_window_with_margin(px(8.)).child(list)).with_priority(2).into_any_element())
    }

    /// The terminal panel under the conversation: a tab per terminal of the open session, a
    /// new shell (on this computer, of a kind picked from a list), hide.
    pub(super) fn render_terminal_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.terminal_shown.contains(&self.terminal_owner()) {
            return None;
        }
        let active = self.active_own_terminal(cx)?;
        let theme = *cx.global::<Theme>();
        let tabs = self.own_terminals(cx).into_iter().map(|ix| {
            let term = self.terminals[ix].read(cx);
            let (title, ended) = (term.title().to_owned(), term.is_closed());
            let selected = ix == active;
            div()
                .id(("terminal-tab", ix))
                .flex()
                .flex_none()
                .items_center()
                .gap(px(6.))
                .h(px(26.))
                .pl(px(10.))
                .pr(px(4.))
                .rounded(px(6.))
                .text_size(px(12.5))
                .text_color(if selected { theme.text } else { theme.text_secondary })
                .when(selected, |this| this.bg(theme.selection_inactive))
                .when(!selected, |this| this.hover(|style| style.bg(theme.hover)))
                .when(ended, |this| this.text_color(theme.text_muted))
                .child(div().max_w(px(220.)).truncate().child(title))
                .child(
                    div()
                        .id(("terminal-tab-close", ix))
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(18.))
                        .rounded(px(4.))
                        .text_color(theme.text_muted)
                        .hover(|style| style.bg(theme.hover).text_color(theme.text))
                        .child("×")
                        .tooltip(|_, cx| tooltip(t("terminal.close"), None, cx))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            cx.stop_propagation();
                            this.close_terminal(ix, window, cx);
                        })),
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.select_terminal(ix, window, cx)))
        });
        let pick = self.shells_here().then(|| {
            div()
                .id("terminal-pick")
                .relative()
                .flex()
                .items_center()
                .justify_center()
                .w(px(16.))
                .h(px(24.))
                .rounded(px(5.))
                .text_size(px(10.))
                .text_color(theme.text_muted)
                .hover(|style| style.bg(theme.hover).text_color(theme.text))
                .when(self.terminal_menu, |this| this.bg(theme.hover))
                .child("▾")
                .child(self.terminal_anchor.canvas())
                .tooltip(|_, cx| tooltip(t("terminal.pick"), None, cx))
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    if this.shells.is_none() {
                        this.shells = Some(terminal::available_shells());
                    }
                    this.terminal_menu = !this.terminal_menu;
                    cx.notify();
                }))
        });
        let strip = div()
            .id("terminal-tabs")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(4.))
            .h(px(36.))
            .px(px(8.))
            .border_b_1()
            .border_color(theme.border)
            .child(icon("toolwindows/terminal", &theme))
            .child(div().id("terminal-tab-row").flex().items_center().gap(px(4.)).min_w_0().overflow_x_scroll().children(tabs))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .child(
                        icon_button("terminal-new", "general/add", t("terminal.new"), None, &theme)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.open_shell_terminal(window, cx))),
                    )
                    .children(pick),
            )
            .child(div().flex_1())
            .child(
                icon_button("terminal-hide", "general/hide", t("terminal.hide"), Some("Ctrl+`".into()), &theme)
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.toggle_terminal(window, cx))),
            );
        let view = self.terminals[active].clone();
        let menu = self.render_terminal_menu(cx);
        Some(
            island(&theme)
                .flex_none()
                .h(px(self.terminal_height))
                .bg(theme.editor_bg)
                .child(strip)
                .child(div().flex_1().min_h_0().child(view))
                .children(menu)
                .into_any_element(),
        )
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
