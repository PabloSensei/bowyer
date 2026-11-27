//! The dialog of a new session: where it runs (this computer or an SSH host), in which
//! harness, and in which folder (a project the environment already has, another folder,
//! or a new one). A harness that is not installed there says so, with the command that
//! installs it, run in the window's terminal when the user asks.

use std::collections::HashMap;

use model::{EnvId, HarnessKind};
use ui_kit::prelude::*;
use ui_kit::*;

use super::{Shell, critter, harness_color};
use crate::i18n::t;
use crate::launch;

/// The harnesses a new session can run in, in this order.
pub(super) const HARNESSES: [HarnessKind; 6] =
    [HarnessKind::Claude, HarnessKind::Codex, HarnessKind::OpenCode, HarnessKind::DeepSeek, HarnessKind::Pi, HarnessKind::Antigravity];

/// How many projects of the environment the dialog lists.
const PROJECTS_SHOWN: usize = 30;

/// A plain button of the dialog.
fn small_button(id: &'static str, label: &str, theme: &Theme) -> Stateful<Div> {
    let hover = theme.hover;
    div()
        .id(id)
        .flex()
        .items_center()
        .h(px(26.))
        .px(px(10.))
        .rounded(px(6.))
        .border_1()
        .border_color(theme.border)
        .text_size(px(12.))
        .hover(move |style| style.bg(hover))
        .child(label.to_owned())
}

/// The text a new folder starts with: the home folder and a separator.
fn home_folder(home: &str) -> String {
    let separator = if home.contains('\\') { "\\" } else { "/" };
    format!("{}{separator}", home.trim_end_matches(['/', '\\']))
}

/// Where the session starts.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Place {
    /// A project the environment has sessions in.
    Project(String),
    /// A folder that is there already, typed or picked.
    Folder,
    /// A folder made for the session.
    New,
}

pub(super) struct NewSession {
    env: EnvId,
    harness: HarnessKind,
    place: Option<Place>,
    /// The folder of `Folder` and `New`.
    path: TextField,
    error: Option<String>,
    /// The start being checked, by its number: its answer opens a draft only while this
    /// dialog still waits for that very start (not after Cancel, nor in a dialog opened again).
    busy: Option<u64>,
    focus: FocusHandle,
}

/// Numbers the starts of the dialog.
static STARTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// The command that installs `harness` in an environment, by its system. The official
/// ways: the native installers of Claude Code and OpenCode, npm for Codex and for OpenCode
/// on Windows (its docs support no Windows package manager). OpenCode is OpenCode 2: its
/// own package and installer, since `opencode-ai` and `opencode.ai/install` stay on 1.
/// Pi has an installer of its own (pi.dev). DeepSeek Harness and Antigravity's `agy` have
/// none known: their docs say how.
fn install_command(harness: &HarnessKind, windows: bool) -> Option<&'static str> {
    Some(match (harness, windows) {
        (HarnessKind::Claude, true) => "irm https://claude.ai/install.ps1 | iex",
        (HarnessKind::Claude, false) => "curl -fsSL https://claude.ai/install.sh | bash",
        (HarnessKind::OpenCode, true) => "npm install -g @opencode/cli",
        (HarnessKind::OpenCode, false) => "curl -fsSL https://opencode.ai/v2/install | bash",
        (HarnessKind::Codex, _) => "npm install -g @openai/codex",
        (HarnessKind::Pi, true) => "irm https://pi.dev/install.ps1 | iex",
        (HarnessKind::Pi, false) => "curl -fsSL https://pi.dev/install.sh | sh",
        _ => return None,
    })
}

impl Shell {
    /// Opens the dialog, in the environment and harness of the open session when there is
    /// one.
    pub(super) fn open_new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let env = self.open.as_ref().map_or_else(EnvId::local, |open| open.summary.key.env.clone());
        let harness = self.open.as_ref().map(|open| open.summary.key.harness.clone()).filter(|h| HARNESSES.contains(h)).unwrap_or(HarnessKind::Claude);
        let place = self.open.as_ref().and_then(|open| open.summary.cwd.as_deref()).map(|cwd| Place::Project(library::project_root(cwd).0.to_owned()));
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        self.new_session = Some(NewSession { env: env.clone(), harness, place, path: TextField::from_text(""), error: None, busy: None, focus });
        self.look_for_harnesses(env, cx);
        cx.notify();
    }

    /// Finds the harnesses of `env` and their versions in the background, unless the window
    /// knows them already.
    fn look_for_harnesses(&mut self, env_id: EnvId, cx: &mut Context<Self>) {
        self.want_versions(&env_id, false, cx);
    }

    fn choose_env(&mut self, env: EnvId, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.new_session {
            if dialog.env != env {
                dialog.place = None;
                dialog.path = TextField::from_text("");
            }
            dialog.env = env.clone();
            dialog.error = None;
        }
        self.look_for_harnesses(env, cx);
        cx.notify();
    }

    /// Looks for the harnesses of the chosen environment again: after an install.
    fn look_again(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = &self.new_session else { return };
        let env = dialog.env.clone();
        self.want_versions(&env, true, cx);
    }

    /// The projects of an environment, the most recent first: the folder each session began
    /// in, a worktree counted as its repository.
    fn projects_of(&self, env: &EnvId) -> Vec<(String, String)> {
        let mut latest: HashMap<String, i64> = HashMap::new();
        for summary in self.summaries.iter().filter(|s| &s.key.env == env) {
            let Some(cwd) = summary.cwd.as_deref().filter(|cwd| !cwd.is_empty()) else { continue };
            let root = library::project_root(cwd).0.trim_end_matches(['/', '\\']).to_owned();
            let at = latest.entry(root).or_insert(0);
            *at = (*at).max(summary.updated_ms);
        }
        let mut projects: Vec<(String, i64)> = latest.into_iter().collect();
        projects.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        projects
            .into_iter()
            .take(PROJECTS_SHOWN)
            .map(|(root, _)| {
                let name = root.rsplit(['/', '\\']).next().unwrap_or(&root).to_owned();
                (name, root)
            })
            .collect()
    }

    /// Opens the folder picker of the system for a folder of this computer.
    fn browse_folder(&mut self, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: None });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = chosen.await
                && let Some(path) = paths.into_iter().next()
            {
                this.update(cx, |this, cx| {
                    if let Some(dialog) = &mut this.new_session {
                        dialog.path = TextField::from_text(path.to_string_lossy().into_owned());
                        dialog.place = Some(Place::Folder);
                        dialog.error = None;
                        cx.notify();
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    /// Runs the install command of the chosen harness in the window's terminal, in the
    /// chosen environment: the user sees it and answers what it asks.
    fn install_harness(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &self.new_session else { return };
        let Some(env) = self.env_for(&dialog.env) else { return };
        let windows = env.id().is_local() && cfg!(windows);
        let Some(command) = install_command(&dialog.harness, windows) else { return };
        let shell = if windows { launch::Shell::PowerShell } else { launch::Shell::Posix };
        let title = format!("{} · {}", t("new.install"), dialog.harness.label());
        // On a host the command runs in the login shell, whose PATH has npm (nvm and the
        // like set it up there only). The commands hold no single quote.
        let command = if env.id().is_local() { command.to_owned() } else { format!("\"${{SHELL:-sh}}\" -lc '{command}'") };
        let line = launch::block_line(&*env, None, shell, &command);
        if !self.show_in_terminal(line, title, window, cx) {
            let result = launch::run_in_terminal(&*env, None, shell, &command);
            self.report(result, cx);
        }
        if let Some(dialog) = &mut self.new_session {
            dialog.error = Some(t("new.install_started").to_owned());
        }
        cx.notify();
    }

    /// Starts the session: the folder is checked (or made), then a draft opens there.
    fn start_new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.new_session else { return };
        if dialog.busy.is_some() {
            return;
        }
        let (env_id, harness) = (dialog.env.clone(), dialog.harness.clone());
        let (path, make) = match &dialog.place {
            Some(Place::Project(root)) => (root.clone(), false),
            Some(Place::Folder) => (dialog.path.text.trim().to_owned(), false),
            Some(Place::New) => (dialog.path.text.trim().to_owned(), true),
            None => {
                dialog.error = Some(t("new.pick_folder").to_owned());
                cx.notify();
                return;
            }
        };
        if path.is_empty() {
            dialog.error = Some(t("new.pick_folder").to_owned());
            cx.notify();
            return;
        }
        let Some(env) = self.env_for(&env_id) else { return };
        let attempt = STARTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(dialog) = &mut self.new_session {
            dialog.busy = Some(attempt);
            dialog.error = None;
        }
        let task = cx.background_spawn(async move {
            // `~/` is the home folder, here and on a host: a quoted word would not expand.
            let folder = match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
                Some(rest) => {
                    let home = env.home().map_err(|error| error.to_string())?;
                    env.join(&home, rest)
                }
                None => path,
            };
            let made = folder.clone();
            if env.id().is_local() {
                if make {
                    std::fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
                } else if !std::path::Path::new(&folder).is_dir() {
                    return Err(t("new.no_folder").replace("{path}", &folder));
                }
                return Ok(made);
            }
            let check = if make { env.run("mkdir", &["-p", &folder], None) } else { env.run("test", &["-d", &folder], None) };
            check.map(|_| made).map_err(|error| if make { error.to_string() } else { t("new.no_folder").replace("{path}", &folder) })
        });
        let window_handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = cx.update_window(window_handle, |_, window, cx| {
                this.update(cx, |this, cx| match result {
                    // Cancelled while the folder was checked: nothing opens.
                    _ if this.new_session.as_ref().and_then(|dialog| dialog.busy) != Some(attempt) => {}
                    Ok(cwd) => {
                        this.new_session = None;
                        this.start_draft_at(library::SessionStart { env: env_id, harness, cwd }, window, cx);
                    }
                    Err(error) => {
                        if let Some(dialog) = &mut this.new_session {
                            dialog.busy = None;
                            dialog.error = Some(error);
                            cx.notify();
                        }
                    }
                })
                .ok();
            });
        })
        .detach();
        cx.notify();
    }

    fn new_session_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.new_session else { return };
        match event.keystroke.key.as_str() {
            "escape" => {
                self.new_session = None;
                cx.stop_propagation();
                cx.notify();
            }
            "enter" => {
                cx.stop_propagation();
                self.start_new_session(window, cx);
            }
            _ => {
                if matches!(dialog.place, Some(Place::Folder | Place::New)) && dialog.path.key_with_clipboard(event, false, cx) {
                    dialog.error = None;
                    cx.stop_propagation();
                    cx.notify();
                }
            }
        }
    }

    pub(super) fn render_new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = *cx.global::<Theme>();
        let env_id = self.new_session.as_ref()?.env.clone();
        // Each installed harness with its version and, when a newer one is out, its update.
        let mut marks: HashMap<HarnessKind, AnyElement> = HashMap::new();
        for harness in HARNESSES.iter() {
            let id = SharedString::from(format!("new-update-{}", harness.id()));
            if let Some(mark) = self.version_mark(&env_id, harness, id, true, true, cx) {
                marks.insert(harness.clone(), mark);
            }
        }
        let installed: Option<Vec<HarnessKind>> = self.harness_versions.found(&env_id).map(|found| found.keys().cloned().collect());
        let dialog = self.new_session.as_ref()?;
        let focused = dialog.focus.is_focused(window);
        let chosen_harness = dialog.harness.clone();
        // A host that did not answer is not stopped at: the turn says what it finds there.
        let unknown = self.harness_versions.unknown(&env_id);
        let checking = installed.is_none() && !unknown;
        let harness_ready = unknown || installed.as_ref().is_some_and(|list| list.contains(&chosen_harness));
        let local = env_id.is_local();

        let section = |title: &'static str| div().mt(px(6.)).text_size(px(11.5)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text_secondary).child(t(title));
        let option = |id: SharedString, chosen: bool| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(8.))
                .h(px(28.))
                .px(px(10.))
                .rounded(px(6.))
                .border_1()
                .border_color(if chosen { theme.accent } else { theme.border })
                .when(chosen, |this| this.bg(theme.selection))
                .when(!chosen, |this| this.hover(|style| style.bg(theme.selection_hover)))
        };

        // Environments: this computer and the hosts.
        let envs: Vec<(EnvId, String)> = self
            .envs
            .iter()
            .map(|env| {
                let id = env.id();
                let label = if id.is_local() { t("group.local").to_owned() } else { id.ssh_host().unwrap_or(&id.0).to_owned() };
                (id, label)
            })
            .collect();
        let env_row = div().flex().flex_wrap().gap(px(6.)).children(envs.into_iter().map(|(id, label)| {
            let chosen = id == env_id;
            let target = id.clone();
            option(SharedString::from(format!("new-env-{}", id.0)), chosen)
                .child(label)
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.choose_env(target.clone(), cx)))
        }));

        // Harnesses, each with whether it is installed there.
        let harness_row = div().flex().flex_wrap().gap(px(6.)).children(HARNESSES.iter().map(|harness| {
            let chosen = *harness == chosen_harness;
            let state = match &installed {
                None if unknown => t("new.unchecked"),
                None => t("new.checking"),
                Some(list) if list.contains(harness) => "",
                Some(_) => t("new.not_installed"),
            };
            let target = harness.clone();
            let mark = marks.remove(harness);
            option(SharedString::from(format!("new-harness-{}", harness.id())), chosen)
                .child(critter(harness, harness_color(harness), 12.))
                .child(harness.label().to_owned())
                .when(!state.is_empty(), |this| this.child(div().text_size(px(11.)).text_color(theme.text_muted).child(state)))
                .children(mark)
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    if let Some(dialog) = &mut this.new_session {
                        dialog.harness = target.clone();
                        dialog.error = None;
                    }
                    cx.notify();
                }))
        }));

        // A harness that is not there: how to get it.
        let windows = local && cfg!(windows);
        let install = (!checking && !harness_ready).then(|| {
            let command = install_command(&chosen_harness, windows);
            let place = if local { t("group.local").to_owned() } else { env_id.ssh_host().unwrap_or(&env_id.0).to_owned() };
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .p(px(10.))
                .rounded(px(8.))
                .bg(theme.island_bg)
                .border_1()
                .border_color(theme.border)
                .child(t("new.missing").replace("{harness}", chosen_harness.label()).replace("{env}", &place))
                .children(command.map(|command| {
                    div()
                        .px(px(8.))
                        .py(px(4.))
                        .rounded(px(5.))
                        .bg(theme.editor_bg)
                        .font_family("JetBrains Mono")
                        .text_size(px(12.))
                        .child(command)
                }))
                .child(
                    div()
                        .flex()
                        .gap(px(6.))
                        .when(command.is_some(), |this| {
                            this.child(small_button("new-install", t("new.install"), &theme).on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.install_harness(window, cx))))
                        })
                        .when(command.is_none(), |this| this.child(div().text_color(theme.text_muted).child(t("new.install_docs"))))
                        .child(small_button("new-look-again", t("new.look_again"), &theme).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.look_again(cx)))),
                )
        });

        // Projects of the environment, another folder, a new folder.
        let place = dialog.place.clone();
        let projects = self.projects_of(&env_id);
        let project_list = div()
            .id("new-projects")
            .flex()
            .flex_col()
            .gap(px(4.))
            .max_h(px(220.))
            .overflow_y_scroll()
            .children(projects.into_iter().map(|(name, root)| {
                let chosen = place.as_ref() == Some(&Place::Project(root.clone()));
                let target = root.clone();
                option(SharedString::from(format!("new-project-{root}")), chosen)
                    .child(div().flex_none().child(name))
                    .child(div().min_w_0().truncate().text_size(px(11.)).text_color(theme.text_muted).child(root))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        if let Some(dialog) = &mut this.new_session {
                            dialog.place = Some(Place::Project(target.clone()));
                            dialog.error = None;
                        }
                        cx.notify();
                    }))
            }));
        let other = |id: &'static str, label: &'static str, kind: Place| {
            let chosen = place.as_ref() == Some(&kind);
            option(SharedString::from(id), chosen).child(t(label)).on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                // A new folder starts in the home folder: asked of the host on the click and
                // in the background (a slow host would freeze the window), never while drawing.
                let asks_home = match &this.new_session {
                    Some(dialog) if matches!(kind, Place::New) && dialog.path.text.is_empty() => {
                        this.env_for(&dialog.env).map(|env| (dialog.env.clone(), env))
                    }
                    _ => None,
                };
                if let Some((env_id, env)) = asks_home {
                    let task = cx.background_spawn(async move { env.home().ok() });
                    cx.spawn(async move |this, cx| {
                        let Some(home) = task.await else { return };
                        this.update(cx, |this, cx| {
                            let Some(dialog) = &mut this.new_session else { return };
                            // The user went on meanwhile: what they did stays.
                            if dialog.env != env_id || dialog.place != Some(Place::New) || !dialog.path.text.is_empty() {
                                return;
                            }
                            dialog.path = TextField::from_text(home_folder(&home));
                            cx.notify();
                        })
                        .ok();
                    })
                    .detach();
                }
                if let Some(dialog) = &mut this.new_session {
                    dialog.place = Some(kind.clone());
                    dialog.error = None;
                    dialog.focus.focus(window, cx);
                }
                cx.notify();
            }))
        };
        let other_row = div()
            .flex()
            .flex_wrap()
            .gap(px(6.))
            .child(other("new-place-folder", "new.other_folder", Place::Folder))
            .child(other("new-place-new", "new.new_folder", Place::New))
            .when(local, |this| {
                this.child(small_button("new-browse", t("new.browse"), &theme).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.browse_folder(cx))))
            });
        let path_field = matches!(place, Some(Place::Folder | Place::New)).then(|| {
            let hint = if place == Some(Place::New) { t("new.new_folder_hint") } else { t("new.folder_hint") };
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .h(px(30.))
                        .px(px(8.))
                        .rounded(px(7.))
                        .bg(theme.editor_bg)
                        .border_1()
                        .border_color(if focused { theme.accent } else { theme.border })
                        .overflow_hidden()
                        .children(text_field_children(&dialog.path, theme.text, theme.over_accent, theme.selection, theme.caret, 16., focused)),
                )
                .child(div().text_size(px(11.)).text_color(theme.text_muted).child(hint))
        });

        let error = dialog.error.clone();
        let busy = dialog.busy.is_some();
        let can_start = harness_ready && place.is_some() && !busy;
        let start = div()
            .id("new-start")
            .flex()
            .items_center()
            .h(px(30.))
            .px(px(14.))
            .rounded(px(8.))
            .bg(if can_start { theme.accent } else { theme.text_disabled })
            .text_color(theme.over_accent)
            .font_weight(FontWeight::MEDIUM)
            .child(if busy { t("new.starting") } else { t("new.start") })
            .when(can_start, |this| this.on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.start_new_session(window, cx))));
        let cancel = small_button("new-cancel", t("theme.cancel"), &theme).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
            this.new_session = None;
            cx.notify();
        }));

        let panel = div()
            .id("new-session")
            .track_focus(&dialog.focus)
            .on_key_down(cx.listener(Self::new_session_key))
            .w(px(560.))
            .max_w(relative(0.92))
            .max_h(relative(0.9))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(8.))
            .p(px(16.))
            .rounded(px(12.))
            .bg(theme.popup_bg)
            .border_1()
            .border_color(theme.popup_border)
            .shadow(popup_shadow(&theme))
            .text_size(px(13.))
            .occlude()
            .child(div().font_weight(FontWeight::SEMIBOLD).text_size(px(15.)).child(t("new.title")))
            .child(section("new.environment"))
            .child(env_row)
            .child(section("new.harness"))
            .child(harness_row)
            .children(install)
            .child(section("new.project"))
            .child(project_list)
            .child(other_row)
            .children(path_field)
            .children(error.map(|error| div().text_size(px(12.)).text_color(theme.error).child(error)))
            .child(div().flex().justify_end().gap(px(8.)).mt(px(4.)).child(cancel).child(start));
        Some(
            div()
                .id("new-session-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(hsla(0., 0., 0., 0.45))
                .occlude()
                .child(panel)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use model::HarnessKind;

    use super::install_command;

    #[test]
    fn each_harness_installs_the_official_way() {
        assert_eq!(install_command(&HarnessKind::Claude, true), Some("irm https://claude.ai/install.ps1 | iex"));
        assert_eq!(install_command(&HarnessKind::Claude, false), Some("curl -fsSL https://claude.ai/install.sh | bash"));
        assert_eq!(install_command(&HarnessKind::Codex, false), Some("npm install -g @openai/codex"));
        assert_eq!(install_command(&HarnessKind::OpenCode, true), Some("npm install -g @opencode/cli"));
        assert_eq!(install_command(&HarnessKind::OpenCode, false), Some("curl -fsSL https://opencode.ai/v2/install | bash"), "OpenCode 2, not 1");
        assert_eq!(install_command(&HarnessKind::DeepSeek, false), None, "no known installer: the docs say how");
    }
}
