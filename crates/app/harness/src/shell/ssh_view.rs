//! Adding an SSH host: the address, an optional key of its own or a password, and a probe
//! that must answer before the host joins the environments. Keys stay where they are; the
//! application stores only the path, never the user's `~/.ssh/config`. A password goes to
//! the system's store (see `envs::secret`). A host whose key ssh does not know shows its
//! fingerprints, and its keys go to Bowyer's own file of host keys once the user trusts them.

use std::path::PathBuf;
use std::sync::Arc;

use envs::{Environment, HostKeys, LocalEnvironment, SshEnvironment};
use ui_kit::prelude::*;
use ui_kit::*;

use super::{Shell, chip};
use crate::i18n::t;

pub(super) struct SshForm {
    pub host: TextField,
    pub password: TextField,
    /// Whether the keys go to the password field rather than the address.
    pub in_password: bool,
    /// The ways to sign in the chip goes through, and the chosen one.
    pub ways: Vec<SignIn>,
    pub way: usize,
    pub busy: bool,
    pub message: Option<(bool, String)>,
    /// The keys of a host ssh does not know, waiting for the user to trust them.
    pub unknown_keys: Option<HostKeys>,
}

/// How a new host signs in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SignIn {
    /// The SSH config and the agent decide.
    Config,
    /// A password kept in the system's store, where the system keeps passwords.
    Password,
    /// A private key of `~/.ssh`.
    Key(PathBuf),
}

impl SshForm {
    fn uses_password(&self) -> bool {
        self.ways.get(self.way) == Some(&SignIn::Password)
    }
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
            let mut ways = vec![SignIn::Config];
            if envs::secret::supported() {
                ways.push(SignIn::Password);
            }
            ways.extend(private_keys().into_iter().map(SignIn::Key));
            self.ssh_form = Some(SshForm {
                host: TextField::from_text(""),
                password: TextField::from_text(""),
                in_password: false,
                ways,
                way: 0,
                busy: false,
                message: None,
                unknown_keys: None,
            });
            self.ssh_focus.focus(window, cx);
        }
        cx.notify();
    }

    /// Forgets a host: its sessions leave the tree. Nothing on the host changes, and adding
    /// it again brings them back.
    pub(super) fn remove_ssh(&mut self, host: &str, cx: &mut Context<Self>) {
        let id = model::EnvId::ssh(host);
        // Its password leaves the system's store with it.
        let _ = envs::secret::remove(host);
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
            "tab" => {
                if let Some(form) = &mut self.ssh_form
                    && form.uses_password()
                {
                    form.in_password = !form.in_password;
                }
            }
            _ => {
                let Some(form) = &mut self.ssh_form else { return };
                if form.in_password && form.uses_password() {
                    // A password never leaves its field through the clipboard.
                    let modifiers = event.keystroke.modifiers;
                    let copy = (modifiers.control || modifiers.platform) && matches!(event.keystroke.key.as_str(), "c" | "x");
                    if !copy && !form.password.key_with_clipboard(event, false, cx) {
                        return;
                    }
                } else if !form.host.key_with_clipboard(event, false, cx) {
                    return;
                }
                form.unknown_keys = None;
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
        let password = form.uses_password().then(|| form.password.text.clone());
        if password.as_deref() == Some("") {
            form.in_password = true;
            form.message = Some((false, t("ssh.password_needed").to_owned()));
            cx.notify();
            return;
        }
        let identity = match form.ways.get(form.way) {
            Some(SignIn::Key(key)) => Some(key.clone()),
            _ => None,
        };
        // Keys the user agreed to trust go to Bowyer's own file before the probe.
        if let Some(keys) = form.unknown_keys.take()
            && let Err(error) = SshEnvironment::trust_host_keys(&keys)
        {
            form.message = Some((false, error.to_string()));
            cx.notify();
            return;
        }
        form.busy = true;
        form.message = Some((true, t("ssh.probing").to_owned()));
        let ssh = SshEnvironment::new(&host).with_identity(identity.clone()).with_password(password.is_some());
        let probe = ssh.clone();
        let task = cx.background_spawn(async move {
            // The askpass program reads the password from the store, so it goes there first;
            // a failed probe puts back what was kept before.
            let before = envs::secret::load(&probe.host);
            if let Some(password) = &password {
                envs::secret::store(&probe.host, password).map_err(|error| (error.to_string(), None))?;
            }
            let answer = probe.probe().map_err(|error| error.to_string());
            match answer {
                Ok(answer) => {
                    if password.is_none() {
                        // A host that signs in with a key keeps no password.
                        let _ = envs::secret::remove(&probe.host);
                    }
                    Ok(answer)
                }
                Err(error) => {
                    if password.is_some() {
                        let _ = match &before {
                            Some(old) => envs::secret::store(&probe.host, old),
                            None => envs::secret::remove(&probe.host),
                        };
                    }
                    // An unknown host key: its fingerprints, for the user to compare.
                    let keys = envs::is_unknown_host_key(&error).then(|| probe.scan_host_keys());
                    Err((error, keys))
                }
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(answer) => {
                        let identity_text = identity.as_ref().map(|p| p.to_string_lossy().into_owned());
                        let _ = this.lib().add_ssh_environment(&host, &host, identity_text.as_deref());
                        // A host added again drops its old sessions to the host, which may
                        // sign in another way.
                        if let Some(old) = this.ssh_hosts.iter().find(|known| known.host == host) {
                            old.close_sessions();
                        }
                        this.ssh_hosts.retain(|known| known.host != host);
                        this.ssh_hosts.push(ssh.clone());
                        this.envs.retain(|env| env.id() != ssh.id());
                        this.envs.push(Arc::new(ssh));
                        this.ssh_form = None;
                        this.status = Some(format!("{host}: {answer}"));
                        this.rescan(cx);
                    }
                    Err((error, keys)) => {
                        if let Some(form) = &mut this.ssh_form {
                            form.busy = false;
                            match keys {
                                Some(Ok(keys)) => {
                                    form.message = Some((true, t("ssh.unknown_key").to_owned()));
                                    form.unknown_keys = Some(keys);
                                }
                                Some(Err(scan)) => {
                                    form.message = Some((false, model::shorten(&format!("{error}\n{}: {scan}", t("ssh.keyscan_failed")), 300)));
                                }
                                None => form.message = Some((false, model::shorten(&error, 240))),
                            }
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
        let uses_password = form.uses_password();
        let key_label = match form.ways.get(form.way) {
            Some(SignIn::Key(key)) => {
                format!("{}: {}", t("ssh.key"), key.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
            }
            Some(SignIn::Password) => t("ssh.password_auth").to_owned(),
            _ => t("ssh.key_default").to_owned(),
        };
        let in_host = focused && !(uses_password && form.in_password);
        let in_password = focused && uses_password && form.in_password;
        let field = div()
            .id("ssh-host")
            .track_focus(&self.ssh_focus)
            .on_key_down(cx.listener(Self::ssh_key))
            .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                // Enter in the focused form arrives as a click too: only the mouse picks the
                // address over the password.
                if event.is_keyboard() {
                    return;
                }
                if let Some(form) = &mut this.ssh_form {
                    form.in_password = false;
                }
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
            .border_color(if in_host { theme.accent } else { theme.border })
            .when(form.host.text.is_empty() && !in_host, |this| {
                this.child(div().text_color(theme.text_muted).child("user@host"))
            })
            .when(!form.host.text.is_empty() || in_host, |this| {
                this.children(text_field_children(&form.host, theme.text, theme.over_accent, theme.selection, theme.caret, 16., in_host))
            });
        let masked = form.password.masked();
        let password_field = uses_password.then(|| {
            div()
                .id("ssh-password")
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    if let Some(form) = &mut this.ssh_form {
                        form.in_password = true;
                    }
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
                .border_color(if in_password { theme.accent } else { theme.border })
                .when(masked.text.is_empty() && !in_password, |this| this.child(div().text_color(theme.text_muted).child(t("ssh.password"))))
                .when(!masked.text.is_empty() || in_password, |this| {
                    this.children(text_field_children(&masked, theme.text, theme.over_accent, theme.selection, theme.caret, 16., in_password))
                })
        });
        let fingerprints: Vec<String> = form.unknown_keys.as_ref().map(|keys| keys.fingerprints.clone()).unwrap_or_default();
        let trusting = form.unknown_keys.is_some();
        let busy = form.busy;
        let known: Vec<(String, Option<String>)> = self
            .ssh_hosts
            .iter()
            .map(|ssh| {
                let how = if ssh.password {
                    Some(t("ssh.password_auth").to_owned())
                } else {
                    ssh.identity.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned())
                };
                (ssh.host.clone(), how)
            })
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
                .children(password_field)
                .when(uses_password, |this| {
                    this.child(div().text_size(px(11.)).text_color(theme.text_muted).child(t("ssh.password_stored")))
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(chip("ssh-key", key_label, &theme).on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            if let Some(form) = &mut this.ssh_form {
                                form.way = (form.way + 1) % form.ways.len().max(1);
                                form.in_password = form.uses_password();
                                form.unknown_keys = None;
                                form.message = None;
                            }
                            this.ssh_focus.focus(window, cx);
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
                                .child(if busy {
                                    t("ssh.probing")
                                } else if trusting {
                                    t("ssh.trust")
                                } else {
                                    t("ssh.connect")
                                })
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
                .when(!fingerprints.is_empty(), |this| {
                    this.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .font_family(super::MONO)
                            .text_size(px(11.))
                            .children(fingerprints.into_iter().map(|fingerprint| div().child(fingerprint))),
                    )
                })
                .into_any_element(),
        )
    }
}
