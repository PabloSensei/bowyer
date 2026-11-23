//! Bowyer's memory in the window: the switch the MCP server reads on every call, the
//! command that connects it to Claude Code, Codex or OpenCode (the user runs it; the window
//! never writes a harness's config), and the notes harnesses kept for the open project.

use library::Note;
use model::HarnessKind;
use ui_kit::prelude::*;
use ui_kit::*;

use super::{MONO, Shell, switch};
use crate::i18n::t;
use crate::{launch, rerank};

/// Sessions changed within this many days get a digest by a model.
const DIGEST_DAYS: i64 = 30;
/// At most this many in one run.
const DIGEST_LIMIT: usize = 300;
/// One run at a time.
static DIGESTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
    /// The model chosen for search writes a digest of each recent session of this
    /// computer that lacks one: problem, cause, fix and words to find it by. One session at
    /// a time in the background, with the count in the status line.
    pub(super) fn build_digests(&mut self, cx: &mut Context<Self>) {
        use std::sync::atomic::Ordering;
        if DIGESTING.swap(true, Ordering::SeqCst) {
            return;
        }
        let due = mcp::sessions_to_digest(&self.lib(), super::now_ms(), DIGEST_DAYS, DIGEST_LIMIT);
        if due.is_empty() {
            DIGESTING.store(false, Ordering::SeqCst);
            self.status = Some(t("memory.digests_none").to_owned());
            cx.notify();
            return;
        }
        let total = due.len();
        let library = self.library.clone();
        let cache = self.cache_dir.join("remote");
        let ranker = self.ranker.clone();
        let label = ranker.label();
        self.status = Some(format!("{} 0/{total}", t("memory.digesting")));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let (mut done, mut failed, mut cost, mut last_error) = (0, 0, 0.0, None);
            for summary in due {
                let (library, cache, ranker) = (library.clone(), cache.clone(), ranker.clone());
                let result = cx
                    .background_spawn(async move {
                        let env = envs::LocalEnvironment::new();
                        let source = history::source_for(&summary.key.harness).ok_or("no reader for this harness")?;
                        let transcript = source.load(&env, &summary, &cache).map_err(|e| e.to_string())?;
                        let model = rerank::DigestModel::new(ranker);
                        let answer = mcp::digest::Summarizer::summarize(&model, &mcp::digest::prompt(&summary, &transcript));
                        let cost = model.cost.get();
                        let answer = answer?;
                        let replay = Replay(answer);
                        let library = library.lock().map_err(|_| "the library is locked".to_owned())?;
                        mcp::store_model_digest(&library, &summary, &transcript, &replay)?;
                        Ok::<f64, String>(cost)
                    })
                    .await;
                match result {
                    Ok(spent) => {
                        done += 1;
                        cost += spent;
                    }
                    Err(error) => {
                        failed += 1;
                        last_error = Some(error);
                    }
                }
                let progress = format!("{} {}/{total}", t("memory.digesting"), done + failed);
                if this.update(cx, |this, cx| {
                    this.status = Some(progress);
                    cx.notify();
                })
                .is_err()
                {
                    break;
                }
            }
            DIGESTING.store(false, Ordering::SeqCst);
            let cost = if cost > 0.0 { format!(" · ${cost:.4}") } else { String::new() };
            let mut status = format!("{} {done}/{total} ({label}{cost})", t("memory.digests_done"));
            if let Some(error) = last_error {
                status.push_str(&format!("; {} {failed}: {}", t("memory.digests_failed"), model::shorten(&error, 160)));
            }
            this.update(cx, |this, cx| {
                this.status = Some(status);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

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
        let body = div().flex().flex_col().gap(px(4.)).children(notes.into_iter().take(30).map(|note| {
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
                }));
        Some(self.fold("notes", t("memory.notes"), None, body, cx))
    }
}

/// An answer the model already gave, handed to [`mcp::store_model_digest`] so that the
/// library lock is held only while it stores, not while the model thinks.
struct Replay(String);

impl mcp::digest::Summarizer for Replay {
    fn summarize(&self, _: &str) -> Result<String, String> {
        Ok(self.0.clone())
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
