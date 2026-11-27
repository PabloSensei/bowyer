//! The list of models of the composer for the harnesses that list their own (OpenCode,
//! Antigravity's `agy`, Pi): asked once per environment and harness, searched as the user
//! types, narrowed to one provider, and open to any id typed whole.

use live::models::{ModelEntry, list_command, matches, parse_models, providers};
use model::{EnvId, HarnessKind};
use ui_kit::prelude::*;
use ui_kit::*;

use super::Shell;
use crate::i18n::t;

/// The models a harness of an environment listed, or where the asking stands.
pub(super) enum ModelList {
    Loading,
    Ready(Vec<ModelEntry>),
    Failed(String),
}

/// Rows the list shows at most: a search narrows the rest.
const SHOWN: usize = 300;

/// A row of the list: the model it sets (`None` for the harness default), its label, and
/// the provider shown beside it.
type Row = (Option<String>, String, Option<String>);

impl Shell {
    /// Whether the composer's model list of `harness` is the searchable one.
    pub(super) fn lists_models(harness: &HarnessKind) -> bool {
        list_command(harness).is_some()
    }

    /// Asks the harness of an environment for its models, once.
    pub(super) fn want_models(&mut self, env: EnvId, harness: HarnessKind, cx: &mut Context<Self>) {
        let Some((program, args)) = list_command(&harness) else { return };
        let key = (env.clone(), harness.clone());
        if matches!(self.model_lists.get(&key), Some(ModelList::Loading | ModelList::Ready(_))) {
            return;
        }
        let Some(shared) = self.env_for(&env) else { return };
        self.model_lists.insert(key.clone(), ModelList::Loading);
        let task = cx.background_spawn(async move {
            let output = if shared.id().is_local() {
                let (path, prefix) = live::resolve_program(program).ok_or_else(|| std::io::Error::other(format!("{program} is not installed")))?;
                let args: Vec<&str> = prefix.iter().map(String::as_str).chain(args.iter().copied()).collect();
                shared.run(&path.to_string_lossy(), &args, None)?
            } else {
                // The host's login shell finds the harness, as for its turns.
                let line = format!("{program} {}", args.join(" "));
                shared.run("sh", &["-c", &format!("exec \"${{SHELL:-sh}}\" -lc '{line}'")], None)?
            };
            Ok::<_, std::io::Error>(parse_models(&harness, &output))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                let list = match result {
                    Ok(models) if !models.is_empty() => ModelList::Ready(models),
                    Ok(_) => ModelList::Failed(t("models.none").to_owned()),
                    Err(error) => ModelList::Failed(error.to_string()),
                };
                this.model_lists.insert(key, list);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Opens the list: an empty search with the focus, all providers, the models asked for.
    pub(super) fn open_model_picker(&mut self, env: EnvId, harness: HarnessKind, window: &mut Window, cx: &mut Context<Self>) {
        self.model_search = TextField::from_text("");
        self.model_provider = None;
        self.model_search_focus.focus(window, cx);
        self.want_models(env, harness, cx);
    }

    /// The models of the open session's harness, when they came.
    fn listed_models(&self, harness: &HarnessKind) -> Option<&[ModelEntry]> {
        let env = self.open.as_ref()?.summary.key.env.clone();
        match self.model_lists.get(&(env, harness.clone()))? {
            ModelList::Ready(models) => Some(models),
            _ => None,
        }
    }

    /// The label of a model id: the name the harness gives it, or the id.
    pub(super) fn model_label(&self, harness: &HarnessKind, id: &str) -> String {
        self.listed_models(harness)
            .and_then(|models| models.iter().find(|m| m.id == id))
            .filter(|m| m.provider.is_none())
            .map_or_else(|| id.to_owned(), |m| m.label.clone())
    }

    /// The rows the search and the provider leave: the default first while nothing is
    /// typed, then the models, then the typed text itself when it is no model's id.
    fn model_picker_rows(&self, harness: &HarnessKind) -> Vec<Row> {
        let query = self.model_search.text.trim();
        let mut rows: Vec<Row> = Vec::new();
        if query.is_empty() {
            rows.push((None, t("model.default").to_owned(), None));
        }
        let models = self.listed_models(harness).unwrap_or_default();
        rows.extend(
            models
                .iter()
                .filter(|m| self.model_provider.is_none() || m.provider == self.model_provider)
                .filter(|m| matches(m, query))
                .take(SHOWN)
                .map(|m| (Some(m.id.clone()), m.label.clone(), m.provider.clone())),
        );
        if !query.is_empty() && !query.contains(char::is_whitespace) && !models.iter().any(|m| m.id == query) {
            rows.push((Some(query.to_owned()), t("models.use").replace("{id}", query), None));
        }
        rows
    }

    fn pick_listed(&mut self, harness: &HarnessKind, model: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.set_composer_model(harness, model.as_deref());
        self.composer_menu = None;
        self.composer_focus.focus(window, cx);
        cx.notify();
    }

    /// Keys of the search: Enter takes the first row, Esc closes the list.
    pub(super) fn model_search_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(harness) = self.open.as_ref().map(|open| open.summary.key.harness.clone()) else { return };
        match event.keystroke.key.as_str() {
            "escape" => {
                self.composer_menu = None;
                self.composer_focus.focus(window, cx);
            }
            "enter" => {
                if let Some((model, _, _)) = self.model_picker_rows(&harness).into_iter().next() {
                    self.pick_listed(&harness, model, window, cx);
                }
            }
            _ => {
                if !self.model_search.key_with_clipboard(event, false, cx) {
                    return;
                }
            }
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// The searchable list, above the model button.
    pub(super) fn render_model_picker(&self, harness: &HarnessKind, bounds: Bounds<Pixels>, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.global::<Theme>();
        let env = self.open.as_ref().map(|open| open.summary.key.env.clone());
        let state = env.and_then(|env| self.model_lists.get(&(env, harness.clone())));
        let focused = true;
        let field = div()
            .id("model-search")
            .track_focus(&self.model_search_focus)
            .on_key_down(cx.listener(Self::model_search_key))
            .flex()
            .items_center()
            .gap(px(6.))
            .h(px(28.))
            .px(px(8.))
            .mb(px(4.))
            .rounded(px(6.))
            .bg(theme.editor_bg)
            .border_1()
            .border_color(theme.accent)
            .child(icon("general/search", &theme))
            .child(if self.model_search.text.is_empty() {
                div().text_color(theme.text_muted).child(t("models.search")).into_any_element()
            } else {
                div()
                    .flex()
                    .items_center()
                    .min_w_0()
                    .children(text_field_children(&self.model_search, theme.text, theme.over_accent, theme.selection, theme.caret, 15., focused))
                    .into_any_element()
            });
        let listed = self.listed_models(harness).unwrap_or_default();
        let all = providers(listed);
        let provider_chip = |id: SharedString, label: String, chosen: bool, provider: Option<String>| {
            div()
                .id(id)
                .px(px(8.))
                .py(px(2.))
                .rounded(px(5.))
                .text_size(px(11.5))
                .when(chosen, |this| this.bg(theme.selection).text_color(theme.text))
                .when(!chosen, |this| this.text_color(theme.text_secondary).hover(|style| style.bg(theme.selection_hover)))
                .child(label)
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.model_provider = provider.clone();
                    this.model_search_focus.focus(window, cx);
                    cx.notify();
                }))
        };
        let provider_row = (all.len() > 1).then(|| {
            let mut chips = vec![provider_chip("model-provider-all".into(), t("models.all_providers").to_owned(), self.model_provider.is_none(), None).into_any_element()];
            for provider in all {
                let chosen = self.model_provider.as_ref() == Some(&provider);
                let count = listed.iter().filter(|m| m.provider.as_ref() == Some(&provider)).count();
                chips.push(
                    provider_chip(SharedString::from(format!("model-provider-{provider}")), format!("{provider} · {count}"), chosen, Some(provider.clone())).into_any_element(),
                );
            }
            div().flex().flex_wrap().gap(px(4.)).pb(px(4.)).mb(px(4.)).border_b_1().border_color(theme.border).children(chips)
        });
        let note = match state {
            Some(ModelList::Loading) | None => Some(t("models.loading").to_owned()),
            Some(ModelList::Failed(error)) => Some(format!("{}: {error}", t("models.failed"))),
            Some(ModelList::Ready(_)) => None,
        };
        let current = self.model_choice(harness);
        let rows = self.model_picker_rows(harness);
        let list = div().id("model-rows").max_h(px(300.)).overflow_y_scroll().children(rows.into_iter().enumerate().map(|(ix, (model, label, provider))| {
            let chosen = model == current;
            let harness = harness.clone();
            div()
                .id(("model-row", ix))
                .flex()
                .items_center()
                .justify_between()
                .gap(px(12.))
                .h(px(26.))
                .px(px(8.))
                .rounded(px(6.))
                .when(chosen, |this| this.bg(theme.selection).text_color(theme.text))
                .when(!chosen, |this| this.text_color(theme.text_secondary).hover(|style| style.bg(theme.selection_hover)))
                .when(ix == 0 && !self.model_search.text.trim().is_empty(), |this| this.border_1().border_color(theme.border))
                .child(div().min_w_0().truncate().child(label))
                .when_some(provider.filter(|_| self.model_provider.is_none()), |this, provider| {
                    this.child(div().flex_none().text_size(px(11.)).text_color(theme.text_muted).child(provider))
                })
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.pick_listed(&harness, model.clone(), window, cx)))
        }));
        let picker = div()
            .id("composer-menu")
            .w(px(380.))
            .p(px(6.))
            .rounded(px(9.))
            .bg(theme.popup_bg)
            .border_1()
            .border_color(theme.popup_border)
            .shadow(popup_shadow(&theme))
            .text_size(px(12.5))
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                this.composer_menu = None;
                cx.notify();
            }))
            .child(field)
            .children(provider_row)
            .when_some(note, |this, note| this.child(div().px(px(8.)).py(px(4.)).text_size(px(11.5)).text_color(theme.text_muted).child(note)))
            .child(list);
        let at = point(bounds.left(), bounds.top() - px(4.));
        deferred(anchored().anchor(Anchor::BottomLeft).position(at).snap_to_window_with_margin(px(8.)).child(picker)).with_priority(2).into_any_element()
    }
}
