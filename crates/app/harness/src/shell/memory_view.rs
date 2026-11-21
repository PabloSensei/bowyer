//! Bowyer's memory in the window: the switch the MCP server reads on every call, the
//! command that connects it to Claude Code, Codex or OpenCode (the user runs it; the window
//! never writes a harness's config), and the notes harnesses kept for the open project.

use library::Note;
use model::HarnessKind;
use ui_kit::prelude::*;
use ui_kit::*;

use super::{MONO, Shell, switch};
use crate::i18n::t;
use crate::launch;

/// The command that adds the memory server to a harness for every project, or `None` for
/// a harness without one.
pub(super) fn connect_command(harness: &HarnessKind) -> Option<String> {
    let add = match harness {
        HarnessKind::Claude => "claude mcp add --scope user bowyer --",
        HarnessKind::Codex => "codex mcp add bowyer --",
        HarnessKind::OpenCode => "opencode mcp add --global bowyer --",
        _ => return None,
    };
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "bowyer".into());
    let exe = if cfg!(windows) { format!("'{}'", exe.replace('\'', "''")) } else { envs::shell_quote(&exe) };
    Some(format!("{add} {exe} mcp"))
}

impl Shell {
    pub(super) fn memory_enabled(&self) -> bool {
        self.lib().setting("memory.enabled").is_none_or(|value| value != "false")
    }

    pub(super) fn toggle_memory(&mut self, cx: &mut Context<Self>) {
        let next = !self.memory_enabled();
        let _ = self.lib().set_setting("memory.enabled", if next { "true" } else { "false" });
        cx.notify();
    }

    /// The open session's folder, when it has one.
    fn open_project(&self) -> Option<String> {
        self.open.as_ref().and_then(|open| open.summary.cwd.clone())
    }

    fn toggle_project_memory(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.open_project() else { return };
        let on = mcp::memory_on(&self.lib(), &project);
        let result = mcp::set_project_memory(&self.lib(), &project, !on);
        self.report(result, cx);
        cx.notify();
    }

    /// The notes of the open session's project, as the memory server files them.
    pub(super) fn project_notes(&self) -> Vec<Note> {
        let Some(cwd) = self.open.as_ref().and_then(|open| open.summary.cwd.clone()) else { return Vec::new() };
        self.lib().notes(Some(&mcp::normalize(&cwd))).unwrap_or_default()
    }

    /// The memory block on top of the MCP list.
    pub(super) fn render_memory_block(&mut self, harness: &HarnessKind, connected: bool, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = *cx.global::<Theme>();
        let enabled = self.memory_enabled();
        let project = self.open_project();
        let project_on = project.as_deref().is_none_or(|project| mcp::memory_on(&self.lib(), project));
        let command = connect_command(harness)?;
        let copy_command = command.clone();
        let run_command = command.clone();
        let button = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .flex()
                .items_center()
                .h(px(22.))
                .px(px(8.))
                .rounded(px(5.))
                .text_size(px(11.5))
                .bg(theme.hover)
                .hover(|style| style.bg(theme.selection_hover))
                .child(label)
        };
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .mx(px(4.))
            .mb(px(6.))
            .p(px(10.))
            .rounded(px(9.))
            .border_1()
            .border_color(theme.border)
            .child(
                div()
                    .id("memory-switch")
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(switch(enabled, true, &theme))
                    .child(div().flex_1().font_weight(FontWeight::SEMIBOLD).child(t("memory.title")))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(if connected { theme.success } else { theme.text_muted })
                            .child(if connected { t("memory.connected") } else { t("memory.not_connected") }),
                    )
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_memory(cx))),
            )
            .when(enabled && project.is_some(), |this| {
                this.child(
                    div()
                        .id("memory-project-switch")
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .child(switch(project_on, true, &theme))
                        .child(div().flex_1().text_size(px(12.)).child(t("memory.this_project")))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_project_memory(cx))),
                )
            })
            .child(div().text_size(px(11.5)).text_color(theme.text_secondary).child(t("memory.about")))
            .when(!connected, |this| {
                this.child(
                    div()
                        .p(px(8.))
                        .rounded(px(6.))
                        .bg(theme.main_bg)
                        .font_family(MONO)
                        .text_size(px(11.))
                        .child(command.clone()),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(6.))
                        .child(button("memory-copy", t("code.copy")).on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy_command.clone()));
                            this.status = Some(t("code.copied").to_owned());
                            cx.notify();
                        })))
                        .child(button("memory-run", t("memory.run")).on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            let env = envs::LocalEnvironment::new();
                            let shell = if cfg!(windows) { launch::Shell::PowerShell } else { launch::Shell::Posix };
                            let result = launch::run_in_terminal(&env, None, shell, &run_command);
                            if result.is_ok() {
                                this.status = Some(t("code.started").to_owned());
                            }
                            this.report(result, cx);
                        }))),
                )
            })
            .into_any_element()
            .into()
    }

    /// The notes of the project in the Session tab.
    pub(super) fn render_notes(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = *cx.global::<Theme>();
        let notes = self.project_notes();
        if notes.is_empty() {
            return None;
        }
        Some(
            super::section(t("memory.notes"), &theme)
                .child(div().flex().flex_col().gap(px(4.)).children(notes.into_iter().take(30).map(|note| {
                    let id = note.id;
                    div()
                        .id(("note", id as usize))
                        .group("note-row")
                        .flex()
                        .items_start()
                        .gap(px(6.))
                        .text_size(px(12.))
                        .child(div().flex_1().min_w_0().child(note.text.clone()))
                        .child(
                            div()
                                .id(("note-delete", id as usize))
                                .flex_none()
                                .px(px(5.))
                                .rounded(px(4.))
                                .text_size(px(11.))
                                .text_color(theme.text_muted)
                                .opacity(0.)
                                .group_hover("note-row", |style| style.opacity(1.))
                                .hover(|style| style.bg(theme.hover).text_color(theme.error))
                                .child("✕")
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    let _ = this.lib().delete_note(id);
                                    cx.notify();
                                })),
                        )
                })))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::connect_command;
    use model::HarnessKind;

    #[test]
    fn each_harness_gets_its_own_add_command() {
        assert!(connect_command(&HarnessKind::Claude).unwrap().starts_with("claude mcp add --scope user bowyer -- "));
        assert!(connect_command(&HarnessKind::Codex).unwrap().starts_with("codex mcp add bowyer -- "));
        assert!(connect_command(&HarnessKind::OpenCode).unwrap().starts_with("opencode mcp add --global bowyer -- "));
        assert!(connect_command(&HarnessKind::OpenCode).unwrap().ends_with(" mcp"));
        assert!(connect_command(&HarnessKind::DeepSeek).is_none());
    }
}
