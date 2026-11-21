//! Adding an SSH host: the address, an optional key of its own, and a probe that must
//! answer before the host joins the environments. Keys stay where they are; the
//! application stores only the path, never the user's `~/.ssh/config`.

use std::path::PathBuf;
use std::sync::Arc;

use envs::{Environment, LocalEnvironment, SshEnvironment};
use ui_kit::prelude::*;
use ui_kit::*;

use super::{Shell, chip};
use crate::i18n::t;

pub(super) struct SshForm {
    pub host: TextField,
    /// Private keys found in `~/.ssh`; index 0 is "the SSH config and agent decide".
    pub keys: Vec<PathBuf>,
    pub key: usize,
    pub busy: bool,
    pub message: Option<(bool, String)>,
}

/// Private keys in `~/.ssh`: files with a `.pub` sibling.
fn private_keys() -> Vec<PathBuf> {
    let Ok(home) = LocalEnvironment::new().home() else { return Vec::new() };
    let dir = PathBuf::from(home).join(".ssh");
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut keys: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().is_none() && path.with_extension("pub").is_file())
        .collect();
    keys.sort();
    keys
}

impl Shell {
    /// The concrete SSH environment of a host, with its key.
    pub(super) fn ssh_env(&self, host: &str) -> Option<SshEnvironment> {
        self.ssh_hosts.iter().find(|ssh| ssh.host == host).cloned()
    }

    pub(super) fn toggle_ssh_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ssh_form.take().is_none() {
            let mut keys = vec![PathBuf::new()];
            keys.extend(private_keys());
            self.ssh_form = Some(SshForm { host: TextField::from_text(""), keys, key: 0, busy: false, message: None });
            self.ssh_focus.focus(window, cx);
        }
        cx.notify();
    }

    /// Forgets a host: its sessions leave the tree. Nothing on the host changes, and adding
    /// it again brings them back.
    pub(super) fn remove_ssh(&mut self, host: &str, cx: &mut Context<Self>) {
        let id = model::EnvId::ssh(host);
        {
            let mut library = self.lib();
            let _ = library.remove_ssh_environment(host);
            for source in history::builtin_sources() {
                let _ = library.store_scan(&id.0, source.harness().id(), &[]);
            }
        }
        self.ssh_hosts.retain(|ssh| ssh.host != host);
        self.envs.retain(|env| env.id() != id);
        if self.open.as_ref().is_some_and(|open| open.summary.key.env == id) {
            self.open = None;
            self.selected = None;
        }
        self.status = Some(format!("{host}: {}", t("ssh.removed")));
        self.reload(cx);
    }

    fn ssh_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "enter" => self.connect_ssh(cx),
            "escape" => self.ssh_form = None,
            _ => {
                if let Some(form) = &mut self.ssh_form
                    && !form.host.key_with_clipboard(event, false, cx)
                {
                    return;
                }
            }
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn connect_ssh(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &mut self.ssh_form else { return };
        let host = form.host.text.trim().to_owned();
        if host.is_empty() || form.busy {
            return;
        }
        let identity = form.keys.get(form.key).filter(|key| !key.as_os_str().is_empty()).cloned();
        form.busy = true;
        form.message = Some((true, t("ssh.probing").to_owned()));
        let ssh = SshEnvironment::new(&host).with_identity(identity.clone());
        let probe = ssh.clone();
        let task = cx.background_spawn(async move { probe.probe() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(answer) => {
                        let identity_text = identity.as_ref().map(|p| p.to_string_lossy().into_owned());
                        let _ = this.lib().add_ssh_environment(&host, &host, identity_text.as_deref());
                        this.ssh_hosts.retain(|known| known.host != host);
                        this.ssh_hosts.push(ssh.clone());
                        this.envs.retain(|env| env.id() != ssh.id());
                        this.envs.push(Arc::new(ssh));
                        this.ssh_form = None;
                        this.status = Some(format!("{host}: {answer}"));
                        this.rescan(cx);
                    }
                    Err(error) => {
                        if let Some(form) = &mut this.ssh_form {
                            form.busy = false;
                            form.message = Some((false, model::shorten(&error.to_string(), 240)));
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    pub(super) fn render_ssh_form(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = *cx.global::<Theme>();
        let focused = self.ssh_focus.is_focused(window);
        let form = self.ssh_form.as_ref()?;
        let key_label = match form.keys.get(form.key) {
            Some(key) if !key.as_os_str().is_empty() => {
                format!("{}: {}", t("ssh.key"), key.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
            }
            _ => t("ssh.key_default").to_owned(),
        };
        let field = div()
            .id("ssh-host")
            .track_focus(&self.ssh_focus)
            .on_key_down(cx.listener(Self::ssh_key))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.ssh_focus.focus(window, cx);
                cx.notify();
            }))
            .flex()
            .items_center()
            .h(px(30.))
            .px(px(8.))
            .rounded(px(7.))
            .bg(theme.editor_bg)
            .border_1()
            .border_color(if focused { theme.accent } else { theme.border })
            .when(form.host.text.is_empty() && !focused, |this| {
                this.child(div().text_color(theme.text_muted).child("user@host"))
            })
            .when(!form.host.text.is_empty() || focused, |this| {
                this.children(text_field_children(&form.host, theme.text, theme.over_accent, theme.selection, theme.caret, 16., focused))
            });
        let busy = form.busy;
        let known: Vec<(String, Option<String>)> = self
            .ssh_hosts
            .iter()
            .map(|ssh| (ssh.host.clone(), ssh.identity.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned())))
            .collect();
        let hosts = (!known.is_empty()).then(|| {
            div().flex().flex_col().gap(px(2.)).children(known.into_iter().map(|(host, key)| {
                let target = host.clone();
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(24.))
                    .text_size(px(12.))
                    .child(div().flex_1().min_w_0().truncate().child(host.clone()))
                    .children(key.map(|key| div().text_size(px(11.)).text_color(theme.text_muted).child(key)))
                    .child(
                        div()
                            .id(SharedString::from(format!("ssh-remove-{host}")))
                            .px(px(6.))
                            .rounded(px(5.))
                            .text_size(px(11.))
                            .text_color(theme.text_secondary)
                            .hover(|style| style.bg(theme.hover).text_color(theme.error))
                            .child(t("ssh.remove"))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.remove_ssh(&target, cx))),
                    )
            }))
        });
        Some(
            div()
                .mx(px(10.))
                .mb(px(8.))
                .p(px(10.))
                .flex()
                .flex_col()
                .gap(px(8.))
                .rounded(px(10.))
                .bg(theme.popup_bg)
                .border_1()
                .border_color(theme.popup_border)
                .children(hosts)
                .child(div().font_weight(FontWeight::SEMIBOLD).text_size(px(12.5)).child(t("ssh.add")))
                .child(field)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(chip("ssh-key", key_label, &theme).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            if let Some(form) = &mut this.ssh_form {
                                form.key = (form.key + 1) % form.keys.len().max(1);
                            }
                            cx.notify();
                        })))
                        .child(
                            div()
                                .id("ssh-connect")
                                .h(px(26.))
                                .px(px(12.))
                                .flex()
                                .items_center()
                                .rounded(px(7.))
                                .bg(if busy { theme.text_disabled } else { theme.accent })
                                .text_color(theme.over_accent)
                                .text_size(px(12.))
                                .child(if busy { t("ssh.probing") } else { t("ssh.connect") })
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.connect_ssh(cx))),
                        ),
                )
                .when_some(form.message.clone(), |this, (ok, text)| {
                    this.child(
                        div()
                            .text_size(px(11.5))
                            .text_color(if ok { theme.text_muted } else { theme.error })
                            .child(text),
                    )
                })
                .into_any_element(),
        )
    }
}
