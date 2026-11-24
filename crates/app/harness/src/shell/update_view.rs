//! Updates in the window: a look at the feed on start and twice a day, the Updates page of the
//! settings, the chip in the sidebar when a version is out, and installing it. The feed and the
//! public key are built in (`BOWYER_UPDATE_FEED`, `BOWYER_UPDATE_KEY` at build time); the
//! `update.feed` setting points elsewhere. Without a feed nothing is checked; without a key
//! nothing is installed, the window only tells.

use std::time::Duration;

use ui_kit::prelude::*;
use ui_kit::*;

use super::settings_view::SettingsPage;
use super::{MONO, Shell, now_ms, switch};
use crate::i18n::t;

/// The feed and key this build came with.
const BUILT_FEED: Option<&str> = option_env!("BOWYER_UPDATE_FEED");
const BUILT_KEY: Option<&str> = option_env!("BOWYER_UPDATE_KEY");

/// How long after the start the first look goes out, and how often after.
const FIRST_CHECK: Duration = Duration::from_secs(20);
const CHECK_EVERY: Duration = Duration::from_secs(12 * 60 * 60);

#[derive(Default)]
pub(super) struct UpdateState {
    checking: bool,
    installing: bool,
    release: Option<update::Release>,
    /// When the last look finished, and whether it found the program up to date.
    checked_at: Option<i64>,
    error: Option<String>,
}

fn public_key() -> Option<&'static str> {
    BUILT_KEY.filter(|key| !key.trim().is_empty())
}

/// A downloaded, checked package waiting for the window to be free to close.
pub(super) struct PendingUpdate {
    install: update::Install,
    package: std::path::PathBuf,
}

impl Shell {
    fn update_feed(&self) -> Option<String> {
        self.lib()
            .setting("update.feed")
            .filter(|feed| !feed.trim().is_empty())
            .or_else(|| BUILT_FEED.map(str::to_owned))
            .filter(|feed| !feed.trim().is_empty())
    }

    /// Whether no look at the feed has been made yet.
    pub(super) fn updates_unchecked(&self) -> bool {
        self.updates.checked_at.is_none() && !self.updates.checking
    }

    fn auto_updates(&self) -> bool {
        self.lib().setting("update.auto").is_none_or(|value| value != "false")
    }

    /// Looks at the feed soon after the start and then twice a day, while the user wants it.
    pub(super) fn schedule_update_checks(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let mut wait = FIRST_CHECK;
            loop {
                cx.background_executor().timer(wait).await;
                wait = CHECK_EVERY;
                let alive = this.update(cx, |this, cx| {
                    if this.auto_updates() {
                        this.check_for_update(false, cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Reads the feed in the background. `manual` checks show their errors; quiet ones keep
    /// them for the Updates page.
    pub(super) fn check_for_update(&mut self, manual: bool, cx: &mut Context<Self>) {
        let Some(feed) = self.update_feed() else {
            if manual {
                self.updates.error = Some(t("update.no_feed").to_owned());
                cx.notify();
            }
            return;
        };
        if self.updates.checking || self.updates.installing {
            return;
        }
        self.updates.checking = true;
        cx.notify();
        let task = cx.background_spawn(async move { update::check(&feed, env!("CARGO_PKG_VERSION")) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.updates.checking = false;
                this.updates.checked_at = Some(now_ms());
                match result {
                    Ok(release) => {
                        this.updates.release = release;
                        this.updates.error = None;
                    }
                    Err(error) => {
                        log::warn!("update check: {error}");
                        this.updates.error = Some(error.to_string());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Downloads the package and checks its signature, then asks about running turns and
    /// terminals like any quit; only once the window may close does the package go in (on
    /// Windows the installer closes Bowyer), and the new version starts by itself.
    fn install_update(&mut self, cx: &mut Context<Self>) {
        let Some(release) = self.updates.release.as_ref() else { return };
        let Some(package) = release.package.clone() else { return };
        // The signature has to be of this version, made after this program was built.
        let expected = update::Expected { version: release.version.clone(), signed_after: env!("BOWYER_BUILT_AT").parse().unwrap_or(0) };
        let Some(key) = public_key() else { return };
        if self.updates.installing {
            return;
        }
        let install = update::install_kind();
        let dir = install.download_dir(&self.cache_dir);
        self.updates.installing = true;
        self.updates.error = None;
        cx.notify();
        let task = cx.background_spawn(async move { update::download(&package, &dir, key, &expected) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| match result {
                Ok(path) => {
                    let update = PendingUpdate { install, package: path };
                    if this.may_quit(cx) {
                        this.apply_update(update, cx);
                    } else {
                        // The question is up; its answer installs, or gives the button back.
                        this.set_quit_update(update);
                    }
                }
                Err(error) => this.update_failed(error.to_string(), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Puts the downloaded package in place and quits, once nothing runs any more.
    pub(super) fn apply_update(&mut self, update: PendingUpdate, cx: &mut Context<Self>) {
        let task = cx.background_spawn(async move { update::apply(&update.install, &update.package) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| match result {
                Ok(()) => cx.quit(),
                Err(error) => this.update_failed(error.to_string(), cx),
            })
            .ok();
        })
        .detach();
    }

    /// The user kept the window open: the update waits for the button again.
    pub(super) fn update_given_up(&mut self) {
        self.updates.installing = false;
    }

    pub(super) fn update_failed(&mut self, error: String, cx: &mut Context<Self>) {
        log::warn!("update install: {error}");
        self.updates.installing = false;
        self.updates.error = Some(error);
        cx.notify();
    }

    /// The chip in the sidebar footer when a version is out.
    pub(super) fn render_update_chip(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let release = self.updates.release.as_ref()?;
        let accent = theme.accent;
        Some(
            div()
                .id("update-chip")
                .flex()
                .items_center()
                .h(px(24.))
                .px(px(8.))
                .rounded(px(6.))
                .text_size(px(12.))
                .text_color(accent)
                .hover(move |style| style.bg(accent.opacity(0.12)))
                .child(format!("↑ {}", release.version))
                .tooltip(|_, cx| tooltip(t("update.available_tip"), None, cx))
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.open_settings(SettingsPage::Updates, window, cx)))
                .into_any_element(),
        )
    }

    pub(super) fn settings_updates(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let auto = self.auto_updates();
        let feed = self.update_feed();
        let install = update::install_kind();
        let state = &self.updates;
        let status: String = if state.installing {
            t("update.installing").to_owned()
        } else if state.checking {
            t("update.checking").to_owned()
        } else if let Some(release) = &state.release {
            t("update.available").replace("{version}", &release.version.to_string())
        } else if state.checked_at.is_some() && state.error.is_none() {
            t("update.up_to_date").to_owned()
        } else {
            String::new()
        };
        let mut actions = div().flex().items_center().gap(px(8.));
        actions = actions.child(
            action_button("update-check", t("update.check_now"), theme)
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.check_for_update(true, cx))),
        );
        let mut hint: Option<&str> = None;
        if let Some(release) = &state.release {
            let installable = release.package.as_ref().is_some_and(|package| install.format() == Some(package.format()));
            if installable && public_key().is_some() {
                actions = actions.child(
                    action_button("update-install", t("update.install"), theme)
                        .bg(theme.accent)
                        .text_color(theme.over_accent)
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.install_update(cx))),
                );
            } else {
                hint = Some(if public_key().is_none() {
                    t("update.no_key")
                } else if matches!(install, update::Install::Unmanaged) {
                    t("update.unmanaged")
                } else {
                    t("update.no_package")
                });
                if let Some(page) = release.page.clone() {
                    actions = actions.child(
                        action_button("update-page", t("update.open_page"), theme)
                            .on_click(move |_: &ClickEvent, _, cx: &mut App| cx.open_url(&page)),
                    );
                }
            }
        }
        let notes = state.release.as_ref().map(|release| release.notes.trim().to_owned()).filter(|notes| !notes.is_empty());
        let error = state.error.clone();
        vec![
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(16.))
                        .child(
                            div()
                                .flex_1()
                                .flex()
                                .flex_col()
                                .gap(px(2.))
                                .child(div().text_size(px(13.)).child(t("update.auto")))
                                .child(div().text_size(px(11.5)).text_color(theme.text_muted).child(t("update.auto_hint"))),
                        )
                        .child(
                            div()
                                .id("settings-toggle-update-auto")
                                .child(switch(auto, true, theme))
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    let _ = this.lib().set_setting("update.auto", if auto { "false" } else { "true" });
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(10.))
                        .text_size(px(12.5))
                        .child(div().w(px(140.)).flex_none().text_color(theme.text_muted).child(t("settings.version")))
                        .child(div().font_family(MONO).child(env!("CARGO_PKG_VERSION"))),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(10.))
                        .text_size(px(12.5))
                        .child(div().w(px(140.)).flex_none().text_color(theme.text_muted).child(t("update.feed")))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family(MONO)
                                .text_color(if feed.is_some() { theme.text_secondary } else { theme.text_muted })
                                .child(feed.unwrap_or_else(|| t("update.no_feed").to_owned())),
                        ),
                )
                .child(actions)
                .when(!status.is_empty(), |this| this.child(div().text_size(px(13.)).font_weight(FontWeight::MEDIUM).child(status)))
                .when_some(hint, |this, hint| this.child(div().text_size(px(12.)).text_color(theme.text_muted).child(hint.to_owned())))
                .when_some(notes, |this, notes| this.child(super::code_box(&notes, theme)))
                .when_some(error, |this, error| this.child(div().text_size(px(12.)).text_color(theme.error).child(error)))
                .into_any_element(),
        ]
    }
}

fn action_button(id: &'static str, label: &str, theme: &Theme) -> Stateful<Div> {
    let hover = theme.hover;
    div()
        .id(id)
        .flex()
        .items_center()
        .h(px(28.))
        .px(px(12.))
        .rounded(px(6.))
        .border_1()
        .border_color(theme.border)
        .text_size(px(12.5))
        .hover(move |style| style.bg(hover))
        .child(label.to_owned())
}
