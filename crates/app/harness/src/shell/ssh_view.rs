//! Adding an SSH host: the address, an optional key of its own or a password, and a probe
//! that must answer before the host joins the environments. Keys stay where they are; the
//! application stores only the path, never the user's `~/.ssh/config`. A password goes to
//! the system's store (see `envs::secret`). A host whose key ssh does not know shows its
//! fingerprints, and its keys go to Bowyer's own file of host keys once the user trusts them.
//! A key that changed in that file is offered for replacement the same way, with a warning;
//! one in the user's own files is theirs to remove.

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
    /// Whether those keys take the place of a changed key in Bowyer's own file.
    pub key_changed: bool,
    /// The keys of `ways` that have a passphrase, found after the form opens: choosing one
    /// asks for its passphrase, kept like a password.
    pub locked_keys: Vec<PathBuf>,
    /// Whether the list of ways is open, and the button it opens under.
    pub ways_open: bool,
    pub ways_anchor: MenuAnchor,
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
    /// The chosen way needs a secret typed: a password, or the passphrase of a locked key.
    fn uses_secret(&self) -> bool {
        match self.ways.get(self.way) {
            Some(SignIn::Password) => true,
            Some(SignIn::Key(key)) => self.locked_keys.contains(key),
            _ => false,
        }
    }

    /// How a way reads in the chip and the list: a locked key says it asks a passphrase.
    fn way_label(&self, way: &SignIn) -> String {
        match way {
            SignIn::Key(key) if self.locked_keys.contains(key) => format!("{}: {} · {}", t("ssh.key"), file_name(key), t("ssh.with_passphrase")),
            SignIn::Key(key) => format!("{}: {}", t("ssh.key"), file_name(key)),
            SignIn::Password => t("ssh.password_auth").to_owned(),
            SignIn::Config => t("ssh.key_default").to_owned(),
        }
    }

    /// Picks a way from the list.
    fn choose(&mut self, way: usize) {
        self.way = way.min(self.ways.len().saturating_sub(1));
        self.ways_open = false;
        self.in_password = self.uses_secret();
        self.unknown_keys = None;
        self.message = None;
    }

    /// The locked key the secret is the passphrase of.
    fn locked_key(&self) -> Option<&PathBuf> {
        match self.ways.get(self.way) {
            Some(SignIn::Key(key)) if self.locked_keys.contains(key) => Some(key),
            _ => None,
        }
    }
}

fn file_name(path: &std::path::Path) -> String {
    path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
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
            let keys = private_keys();
            ways.extend(keys.iter().cloned().map(SignIn::Key));
            // Which keys have a passphrase, found in the background (`ssh-keygen` takes a
            // moment per locked key); only with a store to keep the passphrase in.
            if envs::secret::supported() && !keys.is_empty() {
                let task = cx.background_spawn(async move {
                    keys.into_iter().filter(|key| envs::key_has_passphrase(std::path::Path::new("ssh"), key)).collect::<Vec<_>>()
                });
                cx.spawn(async move |this, cx| {
                    let locked = task.await;
                    this.update(cx, |this, cx| {
                        if let Some(form) = &mut this.ssh_form {
                            form.locked_keys = locked;
                            form.in_password = form.in_password && form.uses_secret();
                            cx.notify();
                        }
                    })
                    .ok();
                })
                .detach();
            }
            self.ssh_form = Some(SshForm {
                host: TextField::from_text(""),
                password: TextField::from_text(""),
                in_password: false,
                ways,
                way: 0,
                busy: false,
                message: None,
                unknown_keys: None,
                key_changed: false,
                locked_keys: Vec::new(),
                ways_open: false,
                ways_anchor: MenuAnchor::new(),
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
            "escape" => match &mut self.ssh_form {
                Some(form) if form.ways_open => form.ways_open = false,
                _ => self.ssh_form = None,
            },
            "tab" => {
                if let Some(form) = &mut self.ssh_form
                    && form.uses_secret()
                {
                    form.in_password = !form.in_password;
                }
            }
            _ => {
                let Some(form) = &mut self.ssh_form else { return };
                if form.in_password && form.uses_secret() {
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
        let password = form.uses_secret().then(|| form.password.text.clone());
        let locked_key = form.locked_key().map(|key| file_name(key));
        if password.as_deref() == Some("") {
            form.in_password = true;
            let needed = match &locked_key {
                Some(key) => t("ssh.passphrase_needed").replace("{key}", key),
                None => t("ssh.password_needed").to_owned(),
            };
            form.message = Some((false, needed));
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
                    // An unknown host key, or a changed one in Bowyer's own file: its
                    // fingerprints, for the user to compare.
                    let keys = (envs::is_unknown_host_key(&error) || envs::changed_key_is_own(&error)).then(|| probe.scan_host_keys());
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
                            let changed = envs::changed_key_is_own(&error);
                            match keys {
                                Some(Ok(keys)) => {
                                    form.message = Some(if changed {
                                        (false, t("ssh.changed_key").to_owned())
                                    } else {
                                        (true, t("ssh.unknown_key").to_owned())
                                    });
                                    form.unknown_keys = Some(keys);
                                    form.key_changed = changed;
                                }
                                Some(Err(scan)) => {
                                    form.message = Some((false, model::shorten(&format!("{error}\n{}: {scan}", t("ssh.keyscan_failed")), 300)));
                                }
                                None => {
                                    // A changed key in the user's own file: ssh's long warning
                                    // would be cut before the file it names.
                                    let text = match envs::changed_host_key_files(&error).first() {
                                        Some(file) => t("ssh.changed_key_elsewhere").replace("{file}", file),
                                        // A locked key the host turned away: a wrong
                                        // passphrase, or a key the host does not know.
                                        None if locked_key.is_some() && envs::refused_sign_in(&error).is_some() => t("ssh.passphrase_refused")
                                            .replace("{host}", &host)
                                            .replace("{key}", locked_key.as_deref().unwrap_or_default()),
                                        // The host's sshd lets no password in: no password
                                        // typed here would help.
                                        None if ssh.password && envs::host_takes_no_password(&error) => {
                                            t("ssh.password_off").replace("{host}", &host)
                                        }
                                        // A key from ssh config or the agent the host turned
                                        // away: one with a passphrase cannot answer here.
                                        None if !ssh.password && envs::host_takes_no_password(&error) => {
                                            t("ssh.keys_refused").replace("{host}", &host)
                                        }
                                        None => model::shorten(&error, 240),
                                    };
                                    form.message = Some((false, text));
                                }
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

    /// The list of ways to sign in, under the button that opens it.
    fn render_ways_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = *cx.global::<Theme>();
        let form = self.ssh_form.as_ref().filter(|form| form.ways_open)?;
        let bounds = form.ways_anchor.bounds()?;
        let rows: Vec<(usize, String, bool)> = form.ways.iter().enumerate().map(|(ix, way)| (ix, form.way_label(way), ix == form.way)).collect();
        let list = div()
            .id("ssh-ways")
            .min_w(bounds.size.width.max(px(200.)))
            .max_w(px(420.))
            .max_h(px(320.))
            .overflow_y_scroll()
            .p(px(4.))
            .rounded(px(9.))
            .bg(theme.popup_bg)
            .border_1()
            .border_color(theme.popup_border)
            .shadow(popup_shadow(&theme))
            .text_size(px(12.5))
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                if let Some(form) = &mut this.ssh_form {
                    form.ways_open = false;
                }
                cx.notify();
            }))
            .children(rows.into_iter().map(|(ix, label, chosen)| {
                div()
                    .id(("ssh-way", ix))
                    .flex()
                    .items_center()
                    .h(px(28.))
                    .px(px(10.))
                    .rounded(px(6.))
                    .truncate()
                    .when(chosen, |this| this.bg(theme.selection).text_color(theme.text))
                    .when(!chosen, |this| this.text_color(theme.text_secondary).hover(|style| style.bg(theme.selection_hover)))
                    .child(label)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        if let Some(form) = &mut this.ssh_form {
                            form.choose(ix);
                        }
                        this.ssh_focus.focus(window, cx);
                        cx.notify();
                    }))
            }));
        let at = point(bounds.left(), bounds.bottom() + px(4.));
        Some(deferred(anchored().position(at).snap_to_window_with_margin(px(8.)).child(list)).with_priority(2).into_any_element())
    }

    pub(super) fn render_ssh_form(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = *cx.global::<Theme>();
        let focused = self.ssh_focus.is_focused(window);
        let form = self.ssh_form.as_ref()?;
        let uses_password = form.uses_secret();
        let placeholder = if form.locked_key().is_some() { t("ssh.passphrase") } else { t("ssh.password") };
        let key_label = form.ways.get(form.way).map(|way| form.way_label(way)).unwrap_or_default();
        let anchor = form.ways_anchor.clone();
        let ways_menu = self.render_ways_menu(cx);
        let form = self.ssh_form.as_ref()?;
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
                .when(masked.text.is_empty() && !in_password, |this| this.child(div().text_color(theme.text_muted).child(placeholder)))
                .when(!masked.text.is_empty() || in_password, |this| {
                    this.children(text_field_children(&masked, theme.text, theme.over_accent, theme.selection, theme.caret, 16., in_password))
                })
        });
        let fingerprints: Vec<String> = form.unknown_keys.as_ref().map(|keys| keys.fingerprints.clone()).unwrap_or_default();
        let trusting = form.unknown_keys.is_some();
        let replacing = trusting && form.key_changed;
        let busy = form.busy;
        let known: Vec<(String, Option<String>)> = self
            .ssh_hosts
            .iter()
            .map(|ssh| {
                // A key with a passphrase is named by its key, the passphrase stays unsaid.
                let how = match &ssh.identity {
                    Some(key) => Some(file_name(key)),
                    None => ssh.password.then(|| t("ssh.password_auth").to_owned()),
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
                    let stored = match envs::secret::store_kind() {
                        Some(envs::secret::Store::Keychain) => t("ssh.password_stored_keychain"),
                        Some(envs::secret::Store::SecretService) => t("ssh.password_stored_keyring"),
                        _ => t("ssh.password_stored"),
                    };
                    this.child(div().text_size(px(11.)).text_color(theme.text_muted).child(stored))
                })
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .justify_between()
                        .gap(px(6.))
                        .child(
                            chip("ssh-key", format!("{key_label}  ▾"), &theme)
                                .relative()
                                .min_w_0()
                                .border_1()
                                .border_color(theme.border)
                                .child(anchor.canvas())
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    if let Some(form) = &mut this.ssh_form {
                                        form.ways_open = !form.ways_open;
                                    }
                                    this.ssh_focus.focus(window, cx);
                                    cx.notify();
                                })),
                        )
                        .children(ways_menu)
                        .child(
                            div()
                                .id("ssh-connect")
                                .flex_none()
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
                                } else if replacing {
                                    t("ssh.replace_key")
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
