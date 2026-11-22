//! The settings of the window: one page over the three panes with its sections on the left.
//! Every value lives in the library (`Library::setting`), under the same keys the chips and
//! menus elsewhere in the window use, so both stay in step. Nothing here writes a file of a
//! harness.

use model::HarnessKind;
use ui_kit::prelude::*;
use ui_kit::*;

use super::{EDGE, MONO, Shell, island, model_presets, switch};
use crate::i18n::{self, Lang, t};
use crate::{launch, tray};
use library::{Grouping, SortOrder};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SettingsPage {
    General,
    Appearance,
    Sessions,
    Harnesses,
    Hosts,
    Terminal,
    Search,
    Memory,
    Updates,
    About,
}

impl SettingsPage {
    const ALL: [SettingsPage; 10] = [
        Self::General,
        Self::Appearance,
        Self::Sessions,
        Self::Harnesses,
        Self::Hosts,
        Self::Terminal,
        Self::Search,
        Self::Memory,
        Self::Updates,
        Self::About,
    ];

    fn key(self) -> &'static str {
        match self {
            Self::General => "settings.general",
            Self::Appearance => "settings.appearance",
            Self::Sessions => "settings.sessions",
            Self::Harnesses => "settings.harnesses",
            Self::Hosts => "settings.hosts",
            Self::Terminal => "settings.terminal",
            Self::Search => "settings.search",
            Self::Memory => "settings.memory",
            Self::Updates => "settings.updates",
            Self::About => "settings.about",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::General => "general/settings",
            Self::Appearance => "general/editorAndPreview",
            Self::Sessions => "own/aiSessions",
            Self::Harnesses => "toolwindows/run",
            Self::Hosts => "toolwindows/services",
            Self::Terminal => "toolwindows/terminal",
            Self::Search => "general/search",
            Self::Memory => "toolwindows/database",
            Self::Updates => "general/download",
            Self::About => "toolwindows/notifications",
        }
    }
}

/// Where a program is on this computer's `PATH`, as a shell would find it.
fn find_program(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT".into()).split(';').map(str::to_lowercase).collect()
    } else {
        vec![String::new()]
    };
    std::env::split_paths(&path).find_map(|dir| {
        extensions.iter().map(|ext| dir.join(format!("{name}{ext}"))).find(|candidate| candidate.is_file())
    })
}

/// The program a harness runs as.
fn harness_program(harness: &HarnessKind) -> &'static str {
    match harness {
        HarnessKind::Claude => "claude",
        HarnessKind::Codex => "codex",
        HarnessKind::OpenCode => "opencode",
        _ => "dsh",
    }
}

impl Shell {
    pub(super) fn open_settings(&mut self, page: SettingsPage, window: &mut Window, cx: &mut Context<Self>) {
        self.settings = Some(page);
        // The field that had the keys may be gone with the panes: Esc goes to the window.
        self.focus.focus(window, cx);
        if page == SettingsPage::Search && self.ranker_menu.is_none() {
            self.toggle_ranker_menu(cx);
        }
        if page == SettingsPage::Terminal && self.shells.is_none() {
            self.shells = Some(terminal::available_shells());
        }
        if page == SettingsPage::Updates && self.updates_unchecked() {
            self.check_for_update(false, cx);
        }
        cx.notify();
    }

    pub(super) fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.settings = None;
        self.ranker_menu = None;
        cx.notify();
    }

    pub(super) fn notifications_enabled(&self) -> bool {
        self.lib().setting("notify.enabled").is_none_or(|value| value != "false")
    }

    fn set_flag(&mut self, key: &str, on: bool, cx: &mut Context<Self>) {
        let _ = self.lib().set_setting(key, if on { "true" } else { "false" });
        cx.notify();
    }

    fn set_language(&mut self, lang: Lang, cx: &mut Context<Self>) {
        i18n::set_lang(lang);
        let _ = self.lib().set_setting("language", lang.id());
        self.rebuild_rows();
        cx.notify();
    }

    fn set_theme(&mut self, id: &str, cx: &mut Context<Self>) {
        let _ = self.lib().set_setting("theme", id);
        cx.set_global(self.catalog.theme(id));
        cx.notify();
    }

    fn set_grouping(&mut self, grouping: Grouping, cx: &mut Context<Self>) {
        self.grouping = grouping;
        let _ = self.lib().set_setting("grouping", grouping.id());
        self.rebuild_rows();
        cx.notify();
    }

    fn set_model(&mut self, harness: &HarnessKind, model: Option<&str>, cx: &mut Context<Self>) {
        let _ = self.lib().set_setting(&format!("model.{}", harness.id()), model.unwrap_or(""));
        cx.notify();
    }

    /// The shell of the terminal of the window: `None` lets Bowyer pick.
    fn set_terminal_shell(&mut self, command_line: Option<String>, cx: &mut Context<Self>) {
        let _ = self.lib().set_setting("terminal.shell", command_line.as_deref().unwrap_or(""));
        launch::set_local_shell(command_line);
        cx.notify();
    }

    pub(super) fn render_settings(&mut self, page: SettingsPage, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.global::<Theme>();
        let nav = div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .w(px(210.))
            .flex_none()
            .p(px(10.))
            .border_r_1()
            .border_color(theme.border)
            .child(div().px(px(10.)).pt(px(6.)).pb(px(12.)).font_weight(FontWeight::SEMIBOLD).text_size(px(15.)).child(t("settings")))
            .children(SettingsPage::ALL.into_iter().map(|item| {
                let chosen = item == page;
                div()
                    .id(SharedString::from(format!("settings-nav-{}", item.key())))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(30.))
                    .px(px(10.))
                    .rounded(px(6.))
                    .text_size(px(13.))
                    .when(chosen, |this| this.bg(theme.selection_inactive).font_weight(FontWeight::MEDIUM))
                    .when(!chosen, |this| this.hover(|style| style.bg(theme.hover)))
                    .child(icon(item.icon(), &theme))
                    .child(t(item.key()))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.open_settings(item, window, cx)))
            }));
        let body = match page {
            SettingsPage::General => self.settings_general(&theme, cx),
            SettingsPage::Appearance => self.settings_appearance(&theme, cx),
            SettingsPage::Sessions => self.settings_sessions(&theme, cx),
            SettingsPage::Harnesses => self.settings_harnesses(&theme, cx),
            SettingsPage::Hosts => self.settings_hosts(&theme, cx),
            SettingsPage::Terminal => self.settings_terminal(&theme, cx),
            SettingsPage::Search => self.settings_search(&theme, cx),
            SettingsPage::Memory => self.settings_memory(&theme, cx),
            SettingsPage::Updates => self.settings_updates(&theme, cx),
            SettingsPage::About => self.settings_about(&theme, cx),
        };
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .h(px(44.))
            .px(px(20.))
            .border_b_1()
            .border_color(theme.border)
            .child(div().font_weight(FontWeight::SEMIBOLD).text_size(px(15.)).child(t(page.key())))
            .child(
                icon_button("settings-close", "general/close", t("settings.close"), Some("Esc".into()), &theme)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.close_settings(cx))),
            );
        let content = div()
            .id("settings-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.settings_scroll)
            .child(div().max_w(px(760.)).px(px(24.)).py(px(18.)).flex().flex_col().gap(px(22.)).children(body));
        let _ = window;
        div()
            .flex()
            .flex_1()
            .min_h_0()
            .px(px(EDGE))
            .pb(px(EDGE))
            .child(
                island(&theme)
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_row()
                    .bg(theme.editor_bg)
                    .child(nav)
                    .child(div().flex_1().min_w_0().h_full().flex().flex_col().child(header).child(content)),
            )
            .into_any_element()
    }

    fn settings_general(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let lang = i18n::lang();
        let languages = div().flex().flex_col().gap(px(2.)).children(Lang::ALL.into_iter().map(|option| {
            let selected = option == lang;
            div()
                .id(SharedString::from(format!("settings-lang-{}", option.id())))
                .flex()
                .items_center()
                .gap(px(10.))
                .h(px(30.))
                .px(px(10.))
                .rounded(px(6.))
                .when(selected, |this| this.bg(theme.selection_inactive))
                .when(!selected, |this| this.hover(|style| style.bg(theme.hover)))
                .child(div().w(px(12.)).flex_none().text_color(theme.accent).child(if selected { "✓" } else { "" }))
                .child(div().flex_none().text_size(px(13.)).child(option.label()))
                .when(option != Lang::En, |this| {
                    this.child(div().flex_1().min_w_0().truncate().text_size(px(11.5)).text_color(theme.text_muted).child(option.english_name()))
                })
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.set_language(option, cx)))
        }));
        let notify = self.notifications_enabled();
        let tray_close = tray::close_to_tray();
        let data = self.settings_dir.clone();
        let data_text = data.to_string_lossy().into_owned();
        vec![
            group(t("settings.language"), theme).child(languages).into_any_element(),
            group("", theme)
                .child(row(
                    t("settings.notify"),
                    Some(t("settings.notify_hint")),
                    toggle("notify", notify, theme, cx, |this, on, cx| this.set_flag("notify.enabled", on, cx)),
                    theme,
                ))
                .child(row(
                    t("settings.tray_close"),
                    Some(if tray::icon_up() { t("settings.tray_close_hint") } else { t("settings.tray_none") }),
                    toggle("tray-close", tray_close, theme, cx, |this, on, cx| {
                        tray::set_close_to_tray(on);
                        this.set_flag("tray.close", on, cx);
                    }),
                    theme,
                ))
                .into_any_element(),
            group(t("settings.data"), theme)
                .child(row(
                    t("settings.data_folder"),
                    Some(&data_text),
                    button("settings-reveal-data", t("settings.show"), theme).on_click(move |_: &ClickEvent, _, cx: &mut App| ui_kit::reveal_path(cx, &data)).into_any_element(),
                    theme,
                ))
                .into_any_element(),
        ]
    }

    fn settings_appearance(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let current = theme.id();
        let themes = self.catalog.library().themes();
        let list = div().flex().flex_col().gap(px(2.)).children(themes.into_iter().enumerate().map(|(ix, item)| {
            let chosen = item.id == current;
            let id = item.id.clone();
            div()
                .id(("settings-theme", ix))
                .flex()
                .items_center()
                .gap(px(10.))
                .h(px(30.))
                .px(px(10.))
                .rounded(px(6.))
                .text_size(px(13.))
                .when(chosen, |this| this.bg(theme.selection_inactive))
                .when(!chosen, |this| this.hover(|style| style.bg(theme.hover)))
                .child(div().w(px(12.)).flex_none().text_color(theme.accent).child(if chosen { "✓" } else { "" }))
                .child(div().flex_1().min_w_0().truncate().child(item.name.clone()))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.set_theme(&id, cx)))
        }));
        let actions = div()
            .flex()
            .gap(px(8.))
            .child(button("settings-theme-edit", t("theme.edit"), theme).on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.close_settings(cx);
                this.open_theme_editor(window, cx);
            })))
            .child(button("settings-theme-import", t("theme.import"), theme).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.pick_theme(cx))));
        vec![group(t("settings.themes"), theme).child(list).child(actions).into_any_element()]
    }

    fn settings_sessions(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let sort = self.sort;
        let sorts = choices(
            "sort",
            SortOrder::ALL.into_iter().map(|option| (t(&format!("sort.{}", option.id())).to_owned(), option == sort, option)).collect(),
            theme,
            cx,
            |this, option, cx| this.set_sort(option, cx),
        );
        let grouping = self.grouping;
        let groupings = choices(
            "grouping",
            Grouping::ALL.into_iter().map(|option| (t(&format!("group.{}", option.id())).to_owned(), option == grouping, option)).collect(),
            theme,
            cx,
            |this, option, cx| this.set_grouping(option, cx),
        );
        vec![
            group(t("settings.sessions"), theme)
                .child(row(t("settings.sort"), None, sorts, theme))
                .child(row(t("settings.grouping"), Some(t("settings.grouping_hint")), groupings, theme))
                .child(row(
                    t("tree.collapse_all"),
                    None,
                    button("settings-collapse", t("settings.apply"), theme).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_collapse_all(cx))).into_any_element(),
                    theme,
                ))
                .into_any_element(),
        ]
    }

    fn settings_harnesses(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let harnesses = [HarnessKind::Claude, HarnessKind::Codex, HarnessKind::OpenCode, HarnessKind::DeepSeek];
        harnesses
            .into_iter()
            .map(|harness| {
                let found = find_program(harness_program(&harness));
                let place = match &found {
                    Some(path) => path.to_string_lossy().into_owned(),
                    None => t("settings.not_found").replace("{program}", harness_program(&harness)),
                };
                let current = self.model_choice(&harness);
                let models: Vec<(String, bool, Option<&'static str>)> = model_presets(&harness)
                    .iter()
                    .map(|preset| (preset.unwrap_or(t("model.default")).to_owned(), *preset == current, *preset))
                    .collect();
                let id: &'static str = match harness {
                    HarnessKind::Claude => "model-claude",
                    HarnessKind::Codex => "model-codex",
                    HarnessKind::OpenCode => "model-opencode",
                    _ => "model-deepseek",
                };
                let chooser = harness.clone();
                let mut block = group(harness.label(), theme).child(row(
                    t("settings.program"),
                    None,
                    div()
                        .max_w(px(360.))
                        .truncate()
                        .font_family(MONO)
                        .text_size(px(12.))
                        .text_color(if found.is_some() { theme.text_secondary } else { theme.warning_mark })
                        .child(place)
                        .into_any_element(),
                    theme,
                ));
                if models.len() > 1 {
                    block = block.child(row(
                        t("settings.model"),
                        Some(t("settings.model_hint")),
                        choices(id, models, theme, cx, move |this, model, cx| this.set_model(&chooser, model, cx)),
                        theme,
                    ));
                }
                block.into_any_element()
            })
            .collect()
    }

    fn settings_hosts(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let hosts: Vec<(String, Option<String>)> =
            self.ssh_hosts
                .iter()
                .map(|ssh| {
                    let how = if ssh.password { Some(t("ssh.password_auth").to_owned()) } else { ssh.identity.as_ref().map(|key| key.to_string_lossy().into_owned()) };
                    (ssh.host.clone(), how)
                })
                .collect();
        let list = div().flex().flex_col().gap(px(6.)).when(hosts.is_empty(), |this| {
            this.child(div().text_size(px(12.5)).text_color(theme.text_muted).child(t("settings.no_hosts")))
        });
        let list = list.children(hosts.into_iter().enumerate().map(|(ix, (host, key))| {
            let forget = host.clone();
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(10.))
                .py(px(6.))
                .rounded(px(8.))
                .border_1()
                .border_color(theme.border)
                .child(icon("toolwindows/services", theme))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(div().text_size(px(13.)).child(host))
                        .child(div().truncate().text_size(px(11.5)).text_color(theme.text_muted).child(key.unwrap_or_else(|| t("settings.key_default").to_owned()))),
                )
                .child(
                    button(SharedString::from(format!("settings-host-forget-{ix}")), t("settings.forget"), theme)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.remove_ssh(&forget, cx))),
                )
        }));
        vec![
            group(t("settings.hosts"), theme)
                .child(div().text_size(px(12.5)).text_color(theme.text_muted).child(t("settings.hosts_hint")))
                .child(list)
                .child(div().flex().child(button("settings-host-add", t("settings.add_host"), theme).on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.close_settings(cx);
                    if this.ssh_form.is_none() {
                        this.toggle_ssh_form(window, cx);
                    }
                }))))
                .into_any_element(),
        ]
    }

    fn settings_terminal(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let chosen = self.lib().setting("terminal.shell").filter(|value| !value.is_empty());
        let mut options: Vec<(String, bool, Option<String>)> = vec![(t("settings.shell_auto").to_owned(), chosen.is_none(), None)];
        for shell in self.shells.clone().unwrap_or_default() {
            let selected = chosen.as_deref() == Some(shell.command_line.as_str());
            options.push((shell.name.clone(), selected, Some(shell.command_line.clone())));
        }
        let list = div().flex().flex_col().gap(px(2.)).children(options.into_iter().enumerate().map(|(ix, (label, selected, line))| {
            let detail = line.clone().unwrap_or_default();
            div()
                .id(("settings-shell", ix))
                .flex()
                .items_center()
                .gap(px(10.))
                .min_h(px(30.))
                .px(px(10.))
                .rounded(px(6.))
                .when(selected, |this| this.bg(theme.selection_inactive))
                .when(!selected, |this| this.hover(|style| style.bg(theme.hover)))
                .child(div().w(px(12.)).flex_none().text_color(theme.accent).child(if selected { "✓" } else { "" }))
                .child(div().flex_none().text_size(px(13.)).child(label))
                .child(div().flex_1().min_w_0().truncate().font_family(MONO).text_size(px(11.)).text_color(theme.text_muted).child(detail))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.set_terminal_shell(line.clone(), cx)))
        }));
        vec![
            group(t("settings.shell"), theme)
                .child(div().text_size(px(12.5)).text_color(theme.text_muted).child(t("settings.shell_hint")))
                .child(list)
                .into_any_element(),
        ]
    }

    fn settings_search(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        vec![
            group(t("settings.ranker"), theme)
                .child(div().text_size(px(12.5)).text_color(theme.text_muted).child(t("settings.ranker_hint")))
                .children(self.render_ranker_menu(theme, cx))
                .into_any_element(),
        ]
    }

    fn settings_memory(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let on = self.memory_enabled();
        let commands: Vec<AnyElement> = [HarnessKind::Claude, HarnessKind::Codex, HarnessKind::OpenCode]
            .into_iter()
            .filter_map(|harness| super::memory_view::connect_command(&harness).map(|command| (harness.label().to_owned(), command)))
            .enumerate()
            .map(|(ix, (label, command))| {
                let copy = command.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .text_size(px(12.5))
                            .child(label)
                            .child(button(SharedString::from(format!("settings-mcp-copy-{ix}")), t("code.copy"), theme).on_click(cx.listener(
                                move |this, _: &ClickEvent, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                                    this.status = Some(t("code.copied").to_owned());
                                    cx.notify();
                                },
                            ))),
                    )
                    .child(super::code_box(&command, theme))
                    .into_any_element()
            })
            .collect();
        vec![
            group(t("settings.memory"), theme)
                .child(row(
                    t("settings.memory_on"),
                    Some(t("settings.memory_hint")),
                    toggle("memory", on, theme, cx, |this, _, cx| this.toggle_memory(cx)),
                    theme,
                ))
                .into_any_element(),
            group(t("settings.mcp"), theme)
                .child(div().text_size(px(12.5)).text_color(theme.text_muted).child(t("settings.mcp_hint")))
                .children(commands)
                .into_any_element(),
        ]
    }

    fn settings_about(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let licenses = std::env::current_exe().ok().and_then(|exe| exe.parent().map(|dir| dir.join("licenses"))).filter(|dir| dir.is_dir());
        let _ = cx;
        let mut block = group(super::APP_NAME, theme)
            .child(row(t("settings.version"), None, div().font_family(MONO).child(env!("CARGO_PKG_VERSION")).into_any_element(), theme))
            .child(row(t("settings.license"), Some(t("settings.license_hint")), div().child("GPL-3.0-or-later").into_any_element(), theme));
        if let Some(dir) = licenses {
            block = block.child(row(
                t("settings.notices"),
                None,
                button("settings-licenses", t("settings.show"), theme).on_click(move |_: &ClickEvent, _, cx: &mut App| ui_kit::reveal_path(cx, &dir)).into_any_element(),
                theme,
            ));
        }
        vec![block.into_any_element()]
    }
}

// Pieces ---------------------------------------------------------------------------------

/// A titled block of settings.
fn group(title: &str, theme: &Theme) -> Div {
    div().flex().flex_col().gap(px(12.)).when(!title.is_empty(), |this| {
        this.child(div().text_size(px(13.5)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(title.to_owned()))
    })
}

/// One setting: its name and hint on the left, its control on the right.
fn row(title: &str, hint: Option<&str>, control: AnyElement, theme: &Theme) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(16.))
        .py(px(4.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(div().text_size(px(13.)).child(title.to_owned()))
                .when_some(hint, |this, hint| this.child(div().text_size(px(11.5)).text_color(theme.text_muted).child(hint.to_owned()))),
        )
        .child(div().flex_none().child(control))
}

/// A plain button with a label.
fn button(id: impl Into<ElementId>, label: &str, theme: &Theme) -> Stateful<Div> {
    let hover = theme.hover;
    div()
        .id(id)
        .flex()
        .items_center()
        .h(px(26.))
        .px(px(12.))
        .rounded(px(6.))
        .border_1()
        .border_color(theme.border)
        .text_size(px(12.5))
        .hover(move |style| style.bg(hover))
        .child(label.to_owned())
}

/// A switch that calls `set` with the new state.
fn toggle(
    id: &'static str,
    on: bool,
    theme: &Theme,
    cx: &mut Context<Shell>,
    set: impl Fn(&mut Shell, bool, &mut Context<Shell>) + 'static,
) -> AnyElement {
    div()
        .id(SharedString::from(format!("settings-toggle-{id}")))
        .cursor_pointer()
        .child(switch(on, true, theme))
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| set(this, !on, cx)))
        .into_any_element()
}

/// One choice out of a few, as joined buttons.
fn choices<T: Clone + 'static>(
    id: &'static str,
    options: Vec<(String, bool, T)>,
    theme: &Theme,
    cx: &mut Context<Shell>,
    choose: impl Fn(&mut Shell, T, &mut Context<Shell>) + Clone + 'static,
) -> AnyElement {
    div()
        .flex()
        .flex_wrap()
        .gap(px(2.))
        .p(px(2.))
        .rounded(px(8.))
        .bg(theme.main_bg)
        .border_1()
        .border_color(theme.border)
        .children(options.into_iter().enumerate().map(|(ix, (label, selected, value))| {
            let choose = choose.clone();
            div()
                .id(SharedString::from(format!("settings-{id}-{ix}")))
                .h(px(24.))
                .px(px(10.))
                .flex()
                .items_center()
                .rounded(px(6.))
                .text_size(px(12.))
                .when(selected, |this| this.bg(theme.accent).text_color(theme.over_accent))
                .when(!selected, |this| this.text_color(theme.text_secondary).hover(|style| style.bg(theme.hover)))
                .child(label)
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| choose(this, value.clone(), cx)))
        }))
        .into_any_element()
}
