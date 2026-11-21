//! Live turns in the window: the composer under the transcript, sending a prompt to the
//! harness of the open session (or of a new one), the stream of the turn into the same
//! list, and stopping it.

use std::sync::Arc;

use futures::StreamExt;
use live::{LiveEvent, Permission, TurnSpec};
use model::{EnvId, HarnessKind, SessionKey, SessionSummary, SourceStamp, TaskRun, TaskStatus};
use ui_kit::prelude::*;
use ui_kit::*;

use super::{LiveRun, OpenSession, Shell, can_run_live, chip, composer, harness_color, model_presets, now_ms};
use crate::display;
use crate::i18n::t;

impl Shell {
    /// Items of the running turn, after the items of the history.
    pub(super) fn live_items(&self) -> Option<Arc<Vec<display::Item>>> {
        let live = self.live.as_ref()?;
        let open = self.open.as_ref()?;
        let same = match (&live.session, open.summary.key.id.is_empty()) {
            (_, true) => true,
            (Some(session), false) => *session == open.summary.key.id,
            (None, false) => false,
        };
        same.then(|| live.items.clone())
    }

    pub(super) fn live_running(&self) -> bool {
        self.live.as_ref().is_some_and(|live| !live.finished)
    }

    /// Tasks of the running turn, newest first, with the ones still running marked so.
    pub(super) fn live_tasks(&self) -> Vec<TaskRun> {
        let Some(live) = &self.live else { return Vec::new() };
        live.turn.as_ref().map(|turn| turn.tasks(!live.finished)).unwrap_or_default()
    }

    pub(super) fn model_choice(&self, harness: &HarnessKind) -> Option<&'static str> {
        let presets = model_presets(harness);
        let index = self
            .lib()
            .setting(&format!("model.{}", harness.id()))
            .and_then(|value| presets.iter().position(|p| p.unwrap_or("") == value))
            .unwrap_or(0);
        presets[index]
    }

    pub(super) fn cycle_model(&mut self, harness: &HarnessKind, cx: &mut Context<Self>) {
        let presets = model_presets(harness);
        let current = self.model_choice(harness);
        let index = presets.iter().position(|p| *p == current).unwrap_or(0);
        let next = presets[(index + 1) % presets.len()];
        let _ = self.lib().set_setting(&format!("model.{}", harness.id()), next.unwrap_or(""));
        cx.notify();
    }

    pub(super) fn cycle_permission(&mut self, cx: &mut Context<Self>) {
        let index = Permission::ALL.iter().position(|p| *p == self.permission).unwrap_or(0);
        self.permission = Permission::ALL[(index + 1) % Permission::ALL.len()];
        cx.notify();
    }

    /// A new session of `harness` in the folder of the open session, or the home folder.
    pub(super) fn start_draft(&mut self, harness: HarnessKind, window: &mut Window, cx: &mut Context<Self>) {
        let env = self.open.as_ref().map_or_else(EnvId::local, |open| open.summary.key.env.clone());
        let shared = self.env_for(&env);
        let cwd = self
            .open
            .as_ref()
            .and_then(|open| open.summary.cwd.clone())
            .or_else(|| shared.as_ref().and_then(|env| env.home().ok()));
        let summary = SessionSummary {
            key: SessionKey { env, harness, id: String::new() },
            title: t("new_session").to_owned(),
            first_prompt: None,
            cwd,
            git_branch: None,
            model: None,
            created_ms: now_ms(),
            updated_ms: now_ms(),
            turn: None,
            parent: None,
            source: SourceStamp::default(),
        };
        self.selected = None;
        self.live = None;
        self.open = Some(OpenSession {
            git: summary.cwd.as_deref().and_then(|cwd| shared.as_ref().and_then(|env| env.git_info(cwd))),
            summary,
            model: None,
            usage: Default::default(),
            cost_usd: None,
            items: Arc::new(Vec::new()),
            list: ListState::new(0, ListAlignment::Top, px(1200.)),
            breakdown: Default::default(),
            changes: Vec::new(),
            tasks: Vec::new(),
            last_call_ms: None,
            usage_by_model: Vec::new(),
            follower: Default::default(),
            shown_offset: 0,
            // A new session is opened from the history once the harness names it.
            memory_tokens: 0,
            exact: None,
            exact_busy: false,
        });
        self.composer_focus.focus(window, cx);
        cx.notify();
    }

    /// Asks Claude Code how it counts the context of the open session: its `/context` in a
    /// print-mode run answers from the session file without a call to the model, and
    /// without persistence it writes nothing to the session.
    pub(super) fn exact_context(&mut self, cx: &mut Context<Self>) {
        let Some(open) = &self.open else { return };
        let summary = &open.summary;
        if open.exact_busy || summary.key.harness != HarnessKind::Claude || summary.key.id.is_empty() {
            return;
        }
        let Some(env) = self.env_for(&summary.key.env) else { return };
        let key = summary.key.clone();
        let cwd = summary.cwd.clone();
        let model = open.model.clone().or_else(|| summary.model.clone());
        let offset = open.shown_offset;
        if let Some(open) = &mut self.open {
            open.exact_busy = true;
        }
        let id = key.id.clone();
        let task = cx.background_spawn(async move {
            let mut args = vec!["-p", "--resume", id.as_str(), "--no-session-persistence", "--output-format", "json"];
            if let Some(model) = &model {
                args.extend(["--model", model.as_str()]);
            }
            args.push("/context");
            let out = if env.id().is_local() {
                env.run_with_input("claude", &args, cwd.as_deref(), b"")
            } else {
                // On a host the harness is on the PATH of the login shell of the user.
                let script = r#"p=$1; shift; exec "${SHELL:-sh}" -lc 'exec "$0" "$@"' "$p" "$@""#;
                let mut remote = vec!["-c", script, "sh", "claude"];
                remote.extend(args);
                env.run_with_input("sh", &remote, cwd.as_deref(), b"")
            }
            .map_err(|error| error.to_string())?;
            let line = out.lines().rev().find(|line| line.trim_start().starts_with('{')).unwrap_or("");
            let report = serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|answer| answer.get("result").and_then(|result| result.as_str()).map(str::to_owned))
                .unwrap_or_else(|| out.clone());
            model::parse_claude_context(&report).ok_or_else(|| model::shorten(report.trim(), 200))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                let Some(open) = &mut this.open else { return };
                if open.summary.key != key {
                    return;
                }
                open.exact_busy = false;
                match result {
                    Ok(exact) => open.exact = Some((offset, exact)),
                    Err(error) => this.status = Some(format!("{}: {error}", t("context.exact_failed"))),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Compacts the open session. A Claude session runs `/compact` as a turn here, so the
    /// summary shows in the conversation; other harnesses get a terminal with their own
    /// command. The typed draft and the attachments stay.
    pub(super) fn compact(&mut self, cx: &mut Context<Self>) {
        let Some(open) = &self.open else { return };
        let summary = open.summary.clone();
        if summary.key.harness != HarnessKind::Claude || summary.key.id.is_empty() {
            if let Some(env) = self.env_for(&summary.key.env) {
                let result = crate::launch::compact_in_terminal(&*env, &summary);
                self.report(result, cx);
            }
            return;
        }
        if self.live_running() {
            return;
        }
        let draft = std::mem::replace(&mut self.composer, TextField::from_text("/compact"));
        let attachments = std::mem::take(&mut self.attachments);
        self.send(cx);
        self.composer = draft;
        self.attachments = attachments;
        cx.notify();
    }

    pub(super) fn send(&mut self, cx: &mut Context<Self>) {
        let prompt = self.composer.text.trim().to_owned();
        if (prompt.is_empty() && self.attachments.is_empty()) || self.live_running() || self.uploading {
            return;
        }
        let Some(open) = &self.open else { return };
        let summary = open.summary.clone();
        if !can_run_live(&summary.key.harness) {
            self.status = Some(format!("{}: {}", summary.key.harness, t("live.unsupported")));
            cx.notify();
            return;
        }
        let ssh = match summary.key.env.ssh_host() {
            Some(host) => match self.ssh_env(host) {
                Some(ssh) => Some(ssh),
                None => {
                    self.status = Some(format!("{host}: {}", t("ssh.unknown")));
                    cx.notify();
                    return;
                }
            },
            None => None,
        };
        let Some(cwd) = summary.cwd.clone() else {
            self.status = Some(t("live.no_folder").to_owned());
            cx.notify();
            return;
        };
        let session = (!summary.key.id.is_empty()).then(|| summary.key.id.clone());
        let mut spec = TurnSpec {
            harness: summary.key.harness.clone(),
            session,
            cwd,
            prompt,
            model: self.model_choice(&summary.key.harness).map(str::to_owned),
            permission: self.permission,
            ssh,
            attachments: Vec::new(),
        };
        let attached = std::mem::take(&mut self.attachments);
        let backup = attached.clone();
        let harness = spec.harness.clone();
        // Claude takes pictures inline; every other file must be on the machine of the turn.
        let inline = move |a: &composer::Attached| harness == HarnessKind::Claude && a.picture.is_some();
        let as_live = |a: composer::Attached, path: String| live::Attachment {
            path,
            image: a.picture.map(|(media, bytes, _)| (media, bytes)),
        };
        match spec.ssh.clone() {
            Some(ssh) if attached.iter().any(|a| !inline(a)) => {
                self.uploading = true;
                self.status = Some(t("live.uploading").to_owned());
                let task = cx.background_spawn(async move {
                    let mut out = Vec::new();
                    for a in attached {
                        let path = if inline(&a) {
                            a.name.clone()
                        } else {
                            ssh.upload(&a.local, &format!("{}-{}", now_ms(), a.name))?
                        };
                        out.push(as_live(a, path));
                    }
                    Ok::<_, std::io::Error>(out)
                });
                cx.spawn(async move |this, cx| {
                    let uploaded = task.await;
                    this.update(cx, |this, cx| {
                        this.uploading = false;
                        match uploaded {
                            Ok(list) => {
                                spec.attachments = list;
                                this.start_turn(spec, backup, cx);
                            }
                            Err(error) => {
                                this.attachments = backup;
                                this.status = Some(format!("{}: {error}", t("error")));
                                cx.notify();
                            }
                        }
                    })
                    .ok();
                })
                .detach();
                cx.notify();
            }
            _ => {
                spec.attachments = attached
                    .into_iter()
                    .map(|a| {
                        let path = a.local.to_string_lossy().into_owned();
                        as_live(a, path)
                    })
                    .collect();
                self.start_turn(spec, backup, cx);
            }
        }
    }

    fn start_turn(&mut self, spec: TurnSpec, attached: Vec<composer::Attached>, cx: &mut Context<Self>) {
        let session = spec.session.clone();
        let turn = match live::start(spec) {
            Ok(turn) => turn,
            Err(error) => {
                self.attachments = attached;
                self.status = Some(format!("{}: {error}", t("error")));
                cx.notify();
                return;
            }
        };
        self.composer = TextField::from_text("");
        self.completion = None;
        let (stop, mut events) = turn.into_parts();
        self.live = Some(LiveRun {
            stop,
            session,
            items: Arc::new(Vec::new()),
            turn: None,
            started: std::time::Instant::now(),
            follow: true,
            asks: Vec::new(),
            stopping: Vec::new(),
            picks: Default::default(),
            texts: Default::default(),
            typing: None,
            answer_focus: cx.focus_handle(),
            finished: false,
            error: None,
            stderr: String::new(),
        });
        cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                let keep = this.update(cx, |this, cx| this.on_live_event(event, cx)).is_ok();
                if !keep {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    /// Stops one command or agent of the running turn; the turn goes on.
    fn stop_command(&mut self, call: String, command: Option<String>, cx: &mut Context<Self>) {
        let Some(live) = &mut self.live else { return };
        if live.finished || live.stopping.contains(&call) {
            return;
        }
        live.stopping.push(call.clone());
        let stop = live.stop.clone();
        let target = call.clone();
        // A list of processes or a round trip to the host: off the window thread.
        let task = cx.background_spawn(async move { stop.stop_command(&target, command.as_deref()) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                // A stopped command stays "stopping" until its result comes: a second click
                // would find nothing to stop.
                if let Err(error) = result {
                    if let Some(live) = &mut this.live {
                        live.stopping.retain(|id| *id != call);
                    }
                    this.status = Some(format!("{}: {error}", t("live.stop_command_failed")));
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Answers the question of the harness about `tool_call_id` with an option it offered.
    fn answer_permission(&mut self, tool_call_id: &str, option: String, cx: &mut Context<Self>) {
        let Some(live) = &mut self.live else { return };
        if let Some(at) = live.asks.iter().position(|ask| ask.tool_call_id == tool_call_id) {
            let ask = live.asks.remove(at);
            let _ = ask.reply.send(Some(option));
        }
        live.picks.remove(tool_call_id);
        live.texts.remove(tool_call_id);
        if live.typing.as_ref().is_some_and(|(call, _)| call == tool_call_id) {
            live.typing = None;
        }
        cx.notify();
    }

    /// Picks or unpicks a choice of a question with several answers.
    fn toggle_pick(&mut self, tool_call_id: &str, question: usize, choice: usize, cx: &mut Context<Self>) {
        let Some(live) = &mut self.live else { return };
        let Some(ask) = live.asks.iter().find(|ask| ask.tool_call_id == tool_call_id) else { return };
        let Some(multi) = ask.questions.get(question).map(|q| q.multi) else { return };
        let count = ask.questions.len();
        let picks = live.picks.entry(tool_call_id.to_owned()).or_insert_with(|| vec![Vec::new(); count]);
        let picked = &mut picks[question];
        if let Some(at) = picked.iter().position(|&n| n == choice) {
            picked.remove(at);
        } else if multi {
            picked.push(choice);
            picked.sort_unstable();
        } else {
            *picked = vec![choice];
            // One answer: a choice takes the place of a text of the user's own.
            if let Some(field) = live.texts.get_mut(tool_call_id).and_then(|texts| texts.get_mut(question)) {
                *field = TextField::from_text("");
            }
        }
        cx.notify();
    }

    /// The answers given so far to a question of the harness, one per question (one for a
    /// question shown with buttons).
    fn answers_of(live: &LiveRun, ask: &live::PermissionAsk) -> Vec<live::Answer> {
        let picks = live.picks.get(&ask.tool_call_id);
        let texts = live.texts.get(&ask.tool_call_id);
        (0..ask.questions.len().max(1))
            .map(|q| live::Answer {
                picked: picks.and_then(|picks| picks.get(q)).cloned().unwrap_or_default(),
                text: texts.and_then(|texts| texts.get(q)).map(|field| field.text.clone()).unwrap_or_default(),
            })
            .collect()
    }

    /// Sends the answers once every question has one.
    fn send_answers(&mut self, tool_call_id: &str, cx: &mut Context<Self>) {
        let Some(live) = &self.live else { return };
        let Some(ask) = live.asks.iter().find(|ask| ask.tool_call_id == tool_call_id) else { return };
        let answers = Self::answers_of(live, ask);
        if answers.iter().any(live::Answer::is_empty) {
            return;
        }
        self.answer_permission(tool_call_id, live::PermissionAsk::answers(&answers), cx);
    }

    /// Puts the caret into the own answer to a question.
    fn start_typing(&mut self, tool_call_id: &str, question: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(live) = &mut self.live else { return };
        let Some(ask) = live.asks.iter().find(|ask| ask.tool_call_id == tool_call_id) else { return };
        let count = ask.questions.len().max(1);
        let texts = live.texts.entry(tool_call_id.to_owned()).or_insert_with(|| vec![TextField::from_text(""); count]);
        if question >= texts.len() {
            return;
        }
        live.typing = Some((tool_call_id.to_owned(), question));
        live.answer_focus.focus(window, cx);
        cx.notify();
    }

    /// Keys of the own answer being typed: Enter sends the answers when every question
    /// has one, Escape leaves the field.
    fn answer_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(live) = &mut self.live else { return };
        let Some((call, question)) = live.typing.clone() else { return };
        match event.keystroke.key.as_str() {
            "enter" => {
                cx.stop_propagation();
                self.send_answers(&call, cx);
                return;
            }
            "escape" => {
                live.typing = None;
                self.focus.focus(window, cx);
                cx.stop_propagation();
                cx.notify();
                return;
            }
            _ => {}
        }
        let Some(field) = live.texts.get_mut(&call).and_then(|texts| texts.get_mut(question)) else { return };
        if !field.key_with_clipboard(event, false, cx) {
            return;
        }
        // One answer: a text of the user's own takes the place of a choice.
        let single = live.asks.iter().find(|ask| ask.tool_call_id == call).and_then(|ask| ask.questions.get(question)).is_some_and(|q| !q.multi);
        if single
            && !field.text.trim().is_empty()
            && let Some(picked) = live.picks.get_mut(&call).and_then(|picks| picks.get_mut(question))
        {
            picked.clear();
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// The field for an answer of the user's own to question `q` of `ask`.
    fn own_answer_field(&self, ask: &live::PermissionAsk, ix: usize, q: usize, window: &Window, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(live) = self.live.as_ref() else { return div().into_any_element() };
        let active = live.typing.as_ref().is_some_and(|(call, at)| *call == ask.tool_call_id && *at == q);
        let focused = active && live.answer_focus.is_focused(window);
        let field = live.texts.get(&ask.tool_call_id).and_then(|texts| texts.get(q));
        let text = field.map(|field| field.text.as_str()).unwrap_or("");
        let call = ask.tool_call_id.clone();
        div()
            .id(("ask-own", ix * 64 + q))
            .when(active, |this| this.track_focus(&live.answer_focus).on_key_down(cx.listener(Self::answer_key)))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.start_typing(&call, q, window, cx)))
            .flex()
            .items_center()
            .h(px(26.))
            .px(px(8.))
            .rounded(px(6.))
            .text_size(px(12.))
            .bg(theme.editor_bg)
            .border_1()
            .border_color(if focused { theme.accent } else { theme.border })
            .child(match field {
                Some(field) if focused || !text.is_empty() => div()
                    .flex()
                    .items_center()
                    .min_w_0()
                    .children(text_field_children(field, theme.text, theme.over_accent, theme.selection, theme.caret, 14., focused))
                    .into_any_element(),
                _ => div().text_color(theme.text_muted).child(t("ask.own")).into_any_element(),
            })
            .into_any_element()
    }

    /// The questions of the running turn, above the composer: what the harness wants to
    /// do and the answers it offers.
    fn render_asks(&self, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        // Only above the session the turn runs in, like its items.
        self.live_items()?;
        let live = self.live.as_ref()?;
        if live.asks.is_empty() {
            return None;
        }
        let harness = self.open.as_ref().map(|open| open.summary.key.harness.label()).unwrap_or("");
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .mb(px(8.))
                .children(live.asks.iter().enumerate().map(|(ix, ask)| {
                    let what = match ask.kind.as_str() {
                        "execute" => t("ask.execute"),
                        "edit" | "delete" | "move" => t("ask.edit"),
                        "fetch" => t("ask.fetch"),
                        "question" => t("ask.question"),
                        _ => t("ask.other"),
                    };
                    // A question of the harness: its choices are the answers, in plain text.
                    let question = ask.kind == "question";
                    // Several questions or several answers: picked here, sent at once.
                    let form = !ask.questions.is_empty();
                    let picks = live.picks.get(&ask.tool_call_id);
                    let complete = Self::answers_of(live, ask).iter().all(|answer| !answer.is_empty());
                    let questions = ask.questions.iter().enumerate().map(|(q, item)| {
                        let picked = picks.and_then(|picks| picks.get(q));
                        let chips = item.choices.iter().enumerate().map(|(n, label)| {
                            let on = picked.is_some_and(|picked| picked.contains(&n));
                            let call = ask.tool_call_id.clone();
                            div()
                                .id(("ask-choice", ix * 4096 + q * 64 + n))
                                .flex()
                                .items_center()
                                .h(px(24.))
                                .px(px(10.))
                                .rounded(px(6.))
                                .text_size(px(12.))
                                .border_1()
                                .when(on, |this| this.bg(theme.accent.opacity(0.18)).border_color(theme.accent).text_color(theme.text))
                                .when(!on, |this| this.border_color(theme.border).text_color(theme.text_secondary).hover(|s| s.bg(theme.hover)))
                                .child(if item.multi { format!("{} {label}", if on { "☑" } else { "☐" }) } else { label.clone() })
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle_pick(&call, q, n, cx)))
                        });
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .p(px(8.))
                            .rounded(px(6.))
                            .bg(theme.main_bg)
                            .child(
                                div()
                                    .flex()
                                    .gap(px(6.))
                                    .items_baseline()
                                    .when(!item.header.is_empty(), |this| {
                                        this.child(div().text_size(px(11.)).text_color(theme.text_secondary).child(item.header.clone()))
                                    })
                                    .child(div().text_size(px(13.)).child(item.text.clone())),
                            )
                            .child(div().flex().flex_wrap().gap(px(6.)).children(chips))
                            .child(self.own_answer_field(ask, ix, q, window, theme, cx))
                    });
                    let questions: Vec<AnyElement> = questions.map(IntoElement::into_any_element).collect();
                    // A question with buttons: an answer of the user's own goes under them.
                    let own = (question && !form).then(|| self.own_answer_field(ask, ix, 0, window, theme, cx));
                    let send_call = ask.tool_call_id.clone();
                    let send = (form || (question && complete)).then(|| {
                        div()
                            .id(("ask-send", ix))
                            .flex()
                            .items_center()
                            .h(px(26.))
                            .px(px(11.))
                            .rounded(px(6.))
                            .text_size(px(12.))
                            .when(complete, |this| this.bg(theme.accent).text_color(theme.over_accent))
                            .when(!complete, |this| this.border_1().border_color(theme.border).text_color(theme.text_secondary))
                            .child(if form { t("ask.send") } else { t("ask.send_one") })
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.send_answers(&send_call, cx)))
                    });
                    let buttons = ask.options.iter().enumerate().map(|(n, (id, kind, name))| {
                        let label = match kind.as_str() {
                            "allow_once" => t("ask.allow_once").to_owned(),
                            "allow_always" => t("ask.allow_always").to_owned(),
                            "reject_once" if question => t("ask.dismiss").to_owned(),
                            "reject_once" => t("ask.reject_once").to_owned(),
                            "reject_always" => t("ask.reject_always").to_owned(),
                            _ => name.clone(),
                        };
                        let primary = kind == "allow_once";
                        let reject = kind.starts_with("reject");
                        let (call, option) = (ask.tool_call_id.clone(), id.clone());
                        div()
                            .id(("ask-option", ix * 16 + n))
                            .flex()
                            .items_center()
                            .h(px(26.))
                            .px(px(11.))
                            .rounded(px(6.))
                            .text_size(px(12.))
                            .when(primary, |this| this.bg(theme.accent).text_color(theme.over_accent))
                            .when(!primary, |this| {
                                this.border_1()
                                    .border_color(theme.border)
                                    .text_color(if reject { theme.error } else { theme.text_secondary })
                                    .hover(|s| s.bg(theme.hover))
                            })
                            .child(label)
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.answer_permission(&call, option.clone(), cx)))
                    });
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .p(px(12.))
                        .rounded(px(10.))
                        .border_1()
                        .border_color(theme.warning_mark)
                        .bg(theme.island_bg)
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme.text_secondary)
                                .child(format!("{harness} · {what}")),
                        )
                        .when(!form, |this| {
                            this.child(
                                div()
                                    .p(px(8.))
                                    .rounded(px(6.))
                                    .bg(theme.main_bg)
                                    .when(!question, |this| this.font_family(super::MONO))
                                    .text_size(if question { px(13.) } else { px(12.) })
                                    .child(ask.title.clone()),
                            )
                        })
                        .children(questions)
                        .children(own)
                        .child(div().flex().flex_wrap().gap(px(6.)).children(send).children(buttons))
                })),
        )
        .map(IntoElement::into_any_element)
    }

    pub(super) fn stop_live(&mut self, cx: &mut Context<Self>) {
        if let Some(live) = &mut self.live
            && !live.finished
        {
            live.asks.clear();
            let stop = live.stop.clone();
            // Over SSH the stop is a round trip to the host: off the window thread.
            cx.background_spawn(async move { stop.stop() }).detach();
            self.status = Some(t("live.stopped").to_owned());
            cx.notify();
        }
    }

    fn on_live_event(&mut self, event: LiveEvent, cx: &mut Context<Self>) {
        let Some(live) = &mut self.live else { return };
        match event {
            LiveEvent::Session(id) => {
                live.session = Some(id.clone());
                if let Some(open) = &mut self.open
                    && open.summary.key.id.is_empty()
                {
                    open.summary.key.id = id;
                    self.selected = Some(open.summary.key.clone());
                }
            }
            LiveEvent::Update(turn) => {
                let items = Arc::new(display::items(&turn));
                let old = live.items.len();
                let new = items.len();
                live.items = items;
                live.turn = Some(turn);
                if let Some(open) = &self.open {
                    let base = open.items.len();
                    // A growing last item can leave the view short of the end before the
                    // next update: following is remembered, not measured each time.
                    let following = live.follow || open.list.is_scrolled_to_end().unwrap_or(true) || old == 0;
                    live.follow = following;
                    open.list.splice(base..base + old, new);
                    if new > 0 {
                        open.list.remeasure_items(base + new - 1..base + new);
                    }
                    if following && base + new > 0 {
                        open.list.scroll_to_end();
                    }
                }
            }
            LiveEvent::Stderr(text) => live.stderr = text,
            LiveEvent::Permission(ask) => {
                live.asks.push(ask);
                // The turn waits for the user: call them back if they went elsewhere.
                self.attention = Some((None, super::Call::Asks));
            }
            LiveEvent::Finished { ok, error } => {
                live.finished = true;
                // Questions left open are cancelled with their senders.
                live.asks.clear();
                // A turn of the window ended: call the user back if they went elsewhere.
                self.attention = Some((None, if ok { super::Call::Done } else { super::Call::Failed }));
                live.error = if ok { None } else { error.or_else(|| Some(live.stderr.clone())) };
                let seconds = live.started.elapsed().as_secs_f32();
                self.status = Some(match &live.error {
                    None => format!("{} · {seconds:.1}s", t("live.done")),
                    Some(error) => format!("{}: {}", t("error"), model::shorten(error, 300)),
                });
                // The harness wrote the turn into its own history: read it back from there,
                // and open the session once the scan has it.
                if let Some(session) = live.session.clone()
                    && let Some(open) = &self.open
                {
                    let key = SessionKey { id: session, ..open.summary.key.clone() };
                    self.pending_open = Some(key);
                }
                if self.right_tab == super::files_view::RightTab::Changes {
                    self.refresh_changes(cx);
                }
                self.rescan(cx);
            }
        }
        cx.notify();
    }

    fn composer_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.key == "escape" && self.viewer.take().is_some() {
            cx.notify();
            cx.stop_propagation();
            return;
        }
        if self.completion_key(event, window, cx) {
            cx.stop_propagation();
            return;
        }
        let stroke = &event.keystroke;
        if stroke.key == "enter" && !stroke.modifiers.shift && !stroke.modifiers.alt {
            if let Some(command) = composer::Command::parse(&self.composer.text) {
                self.composer = TextField::from_text("");
                self.completion = None;
                self.run_command(command, window, cx);
            } else {
                self.send(cx);
            }
            cx.stop_propagation();
            return;
        }
        let primary = stroke.modifiers.control || stroke.modifiers.platform;
        let paste = (primary && stroke.key == "v") || (stroke.modifiers.shift && stroke.key == "insert");
        if paste && !stroke.modifiers.alt && self.paste_attachments(cx) {
            cx.stop_propagation();
            return;
        }
        if stroke.key == "escape" && self.live_running() {
            self.stop_live(cx);
            cx.stop_propagation();
            return;
        }
        if self.composer.key_with_clipboard(event, true, cx) {
            self.update_completion(cx);
            cx.notify();
            cx.stop_propagation();
        }
    }

    pub(super) fn render_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.global::<Theme>();
        let Some(open) = &self.open else { return div().into_any_element() };
        let harness = open.summary.key.harness.clone();
        let focused = self.composer_focus.is_focused(window);
        let running = self.live_running();
        let supported = can_run_live(&harness);
        let model_label = self.model_choice(&harness).unwrap_or(t("model.default")).to_owned();
        let permission_label = t(&format!("permission.{}", self.permission.id()));
        let placeholder = if supported { t("composer.placeholder") } else { t("live.unsupported") };
        let field = div()
            .min_h(px(44.))
            .max_h(px(220.))
            .overflow_hidden()
            .flex()
            .flex_col()
            .text_size(px(13.5))
            .when(self.composer.text.is_empty() && !focused, |this| {
                this.child(div().text_color(theme.text_muted).child(placeholder))
            })
            .when(!self.composer.text.is_empty() || focused, |this| {
                this.children(text_field_lines(
                    &self.composer,
                    theme.text,
                    theme.over_accent,
                    theme.selection,
                    theme.caret,
                    20.,
                    focused,
                ))
            });
        let elapsed = self
            .live
            .as_ref()
            .filter(|live| !live.finished)
            .map(|live| format!("{:.0}s", live.started.elapsed().as_secs_f32()));
        if running {
            // Repaint once a second for the timer.
            self.repaint_after(std::time::Duration::from_secs(1), cx);
        }
        let action = if running {
            div()
                .id("stop")
                .flex()
                .items_center()
                .gap(px(6.))
                .h(px(30.))
                .px(px(14.))
                .rounded(px(8.))
                .bg(theme.error)
                .text_color(theme.over_accent)
                .font_weight(FontWeight::MEDIUM)
                .child(t("live.stop"))
                .children(elapsed)
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.stop_live(cx)))
        } else {
            div()
                .id("send")
                .flex()
                .items_center()
                .h(px(30.))
                .px(px(14.))
                .rounded(px(8.))
                .bg(if supported { theme.accent } else { theme.text_disabled })
                .text_color(theme.over_accent)
                .font_weight(FontWeight::MEDIUM)
                .child(t("live.send"))
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.send(cx)))
        };
        let harness_for_model = harness.clone();
        let completion = self.render_completion(&theme, cx);
        let attachments = self.render_attachments(&theme, cx);
        let asks = self.render_asks(&theme, window, cx);
        div()
            .id("composer-area")
            .p(px(12.))
            .border_t_1()
            .border_color(theme.border)
            .children(asks)
            .child(
                div()
                    .id("composer")
                    .track_focus(&self.composer_focus)
                    .on_key_down(cx.listener(Self::composer_key))
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.composer_focus.focus(window, cx);
                        cx.notify();
                    }))
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .p(px(10.))
                    .rounded(px(12.))
                    .bg(theme.island_bg)
                    .border_1()
                    .border_color(if focused { theme.accent } else { theme.border })
                    .children(completion)
                    .children(attachments)
                    .child(field)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(4.))
                                    .child(
                                        div()
                                            .id("attach")
                                            .size(px(24.))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded(px(6.))
                                            .text_size(px(16.))
                                            .text_color(theme.text_muted)
                                            .hover(|style| style.bg(theme.hover).text_color(theme.text))
                                            .child("+")
                                            .tooltip(|_, cx| tooltip(t("composer.attach"), None, cx))
                                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.pick_files(cx))),
                                    )
                                    .child(div().size(px(8.)).rounded_full().bg(harness_color(&harness)))
                                    .child(chip("model-chip", model_label, &theme).on_click(cx.listener(
                                        move |this, _: &ClickEvent, _, cx| this.cycle_model(&harness_for_model, cx),
                                    )))
                                    .child(chip("permission-chip", permission_label, &theme).on_click(cx.listener(
                                        |this, _: &ClickEvent, _, cx| this.cycle_permission(cx),
                                    ))),
                            )
                            .child(action),
                    ),
            )
            .into_any_element()
    }

    /// Running and finished commands of the live turn, for the right pane.
    pub(super) fn render_live_tasks(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = *cx.global::<Theme>();
        let live = self.live.as_ref()?;
        let tasks = self.live_tasks();
        let running = !live.finished;
        // While the harness asks, the command it asks about has not started yet.
        let stops = running && live.asks.is_empty() && live.stop.stops_commands();
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .p(px(10.))
                .rounded(px(10.))
                .border_1()
                .border_color(if running { theme.accent } else { theme.border })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .text_size(px(12.))
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(if running { t("live.running") } else { t("live.done") }))
                        .when(running, |this| {
                            this.child(
                                div()
                                    .id("stop-turn")
                                    .px(px(8.))
                                    .py(px(2.))
                                    .rounded(px(6.))
                                    .bg(theme.error.opacity(0.15))
                                    .text_color(theme.error)
                                    .child(t("live.stop"))
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.stop_live(cx))),
                            )
                        }),
                )
                .children(tasks.iter().rev().take(20).enumerate().map(|(ix, task)| {
                    let color = match task.status {
                        TaskStatus::Running => theme.accent,
                        TaskStatus::Done => theme.success,
                        TaskStatus::Failed => theme.error,
                    };
                    // A running command or agent stops alone; the turn goes on.
                    let stoppable = stops && task.status == TaskStatus::Running;
                    let stopping = live.stopping.contains(&task.id);
                    let (call, command) = (task.id.clone(), task.command.clone());
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .h(px(22.))
                        .text_size(px(12.))
                        .child(div().size(px(6.)).flex_none().rounded_full().bg(color))
                        .child(div().flex_1().min_w_0().truncate().font_family(super::MONO).text_size(px(11.5)).child(task.title.clone()))
                        .when(stoppable, |this| {
                            this.child(
                                div()
                                    .id(("stop-command", ix))
                                    .flex_none()
                                    .px(px(6.))
                                    .rounded(px(5.))
                                    .text_size(px(11.))
                                    .text_color(if stopping { theme.text_secondary } else { theme.error })
                                    .when(!stopping, |this| this.hover(|s| s.bg(theme.error.opacity(0.12))))
                                    .child(if stopping { t("live.stopping_command") } else { t("live.stop_command") })
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.stop_command(call.clone(), command.clone(), cx))),
                            )
                        })
                }))
                .when_some(live.error.clone(), |this, error| {
                    this.child(div().text_size(px(11.5)).text_color(theme.error).child(model::shorten(&error, 400)))
                })
                .into_any_element(),
        )
    }
}
