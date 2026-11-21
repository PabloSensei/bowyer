//! The theme editor in the right pane: every token of the theme on screen with its colour,
//! changed by a hex value or picked with the mouse (a saturation and value square and a hue
//! bar), seen at once over the whole window, and saved as a file of the `themes` folder. A built-in theme is copied into a new theme with it as the parent; a
//! theme of the user is edited in its own file.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use theme::{Color, ThemeDraft, TokenGroup};
use ui_kit::prelude::*;
use ui_kit::*;

use super::Shell;
use super::theme_import::free_id;
use crate::i18n::t;

/// What the keys type into.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    Token(&'static str),
}

/// What a press in the picker drags.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pick {
    /// Saturation across, value up.
    Square,
    Hue,
}

#[derive(Clone, Copy)]
enum Row {
    Group(TokenGroup),
    Token(&'static str),
}

pub(super) struct ThemeEditor {
    draft: ThemeDraft,
    /// The theme on screen when the editor opened, back on screen after a cancel.
    before: String,
    name: TextField,
    hex: TextField,
    field: Option<Field>,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    /// The hue of the picker, kept apart from the colour: a grey has none of its own.
    hue: f32,
    picking: Option<Pick>,
    square: Rc<Cell<Bounds<Pixels>>>,
    hue_bar: Rc<Cell<Bounds<Pixels>>>,
}

const ROW: f32 = 26.;
/// The height of the saturation and value square.
const SQUARE: f32 = 120.;
const HUE_BAR: f32 = 14.;

fn hsla(color: Color) -> Hsla {
    let Color { r, g, b, a } = color;
    Rgba { r, g, b, a }.into()
}

/// Where `along` falls between `start` and `start + length`, in 0..=1.
fn fraction(along: Pixels, start: Pixels, length: Pixels) -> f32 {
    if length <= px(0.) {
        return 0.;
    }
    ((along - start) / length).clamp(0., 1.)
}

fn marker_shadow(black: Hsla) -> Vec<BoxShadow> {
    vec![BoxShadow { color: black.opacity(0.6), offset: point(px(0.), px(0.)), blur_radius: px(2.), spread_radius: px(0.), inset: false }]
}

fn group_label(group: TokenGroup) -> &'static str {
    match group {
        TokenGroup::Window => t("theme.group.window"),
        TokenGroup::Code => t("theme.group.code"),
        TokenGroup::Editor => t("theme.group.editor"),
        TokenGroup::Status => t("theme.group.status"),
    }
}

fn rows() -> Vec<Row> {
    TokenGroup::ALL.iter().flat_map(|group| std::iter::once(Row::Group(*group)).chain(group.tokens().map(Row::Token))).collect()
}

impl Shell {
    /// Opens the editor on the theme on screen.
    pub(super) fn open_theme_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = cx.global::<Theme>().id().to_owned();
        let library = self.catalog.library();
        let label = library.get(&current).map(|theme| theme.name.clone()).unwrap_or_else(|| current.clone());
        let Some(draft) = ThemeDraft::of(library, &current, &format!("{label} ({})", t("theme.copy"))) else { return };
        self.theme_editor = Some(ThemeEditor {
            name: TextField::from_text(draft.name.clone()),
            draft,
            before: current,
            hex: TextField::from_text(""),
            field: None,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            hue: 0.,
            picking: None,
            square: Rc::new(Cell::new(Bounds::default())),
            hue_bar: Rc::new(Cell::new(Bounds::default())),
        });
        let _ = window;
        cx.notify();
    }

    /// Closes the editor; without saving, the theme from before comes back.
    pub(super) fn close_theme_editor(&mut self, cx: &mut Context<Self>) {
        if let Some(editor) = self.theme_editor.take() {
            cx.set_global(self.catalog.theme(&editor.before));
        }
        cx.notify();
    }

    /// Shows the draft over the whole window.
    fn preview_theme(&self, cx: &mut Context<Self>) {
        if let Some(editor) = &self.theme_editor {
            cx.set_global(Theme::from_tokens(&editor.before, &editor.draft.tokens()));
        }
    }

    fn edit_field(&mut self, field: Field, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.theme_editor else { return };
        if let Field::Token(name) = field {
            let color = editor.draft.tokens().get(name);
            editor.hex = TextField::from_text(color.map(Color::to_text).unwrap_or_default());
            editor.hex.select_all();
            if let Some(color) = color {
                editor.hue = color.to_hsv().0;
            }
        }
        editor.field = Some(field);
        editor.focus.focus(window, cx);
        cx.notify();
    }

    fn reset_token(&mut self, name: &'static str, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.theme_editor else { return };
        editor.draft.reset(name);
        if editor.field == Some(Field::Token(name)) {
            editor.field = None;
        }
        self.preview_theme(cx);
        cx.notify();
    }

    fn theme_editor_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.theme_editor else { return };
        let Some(field) = editor.field else { return };
        cx.stop_propagation();
        match event.keystroke.key.as_str() {
            "enter" | "escape" | "tab" => {
                editor.field = None;
                self.focus.focus(window, cx);
                cx.notify();
                return;
            }
            _ => {}
        }
        match field {
            Field::Name => {
                editor.name.key_with_clipboard(event, false, cx);
                editor.draft.name = editor.name.text.clone();
            }
            Field::Token(name) => {
                if !editor.hex.key_with_clipboard(event, false, cx) {
                    return;
                }
                // A whole colour shows at once; a half-typed one waits.
                if let Some(color) = Color::from_text(&editor.hex.text) {
                    editor.draft.set(name, color);
                    self.preview_theme(cx);
                }
            }
        }
        cx.notify();
    }

    /// The colour under the pointer in the square or on the hue bar, for the token being
    /// edited; its opacity stays.
    fn pick_at(&mut self, pick: Pick, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.theme_editor else { return };
        let Some(Field::Token(name)) = editor.field else { return };
        let Some(color) = editor.draft.tokens().get(name) else { return };
        let (_, mut saturation, mut value) = color.to_hsv();
        match pick {
            Pick::Square => {
                let bounds = editor.square.get();
                saturation = fraction(position.x, bounds.origin.x, bounds.size.width);
                value = 1. - fraction(position.y, bounds.origin.y, bounds.size.height);
            }
            Pick::Hue => {
                let bounds = editor.hue_bar.get();
                editor.hue = 360. * fraction(position.x, bounds.origin.x, bounds.size.width);
            }
        }
        let picked = Color::from_hsv(editor.hue, saturation, value, color.a);
        editor.draft.set(name, picked);
        editor.hex = TextField::from_text(picked.to_text());
        self.preview_theme(cx);
        cx.notify();
    }

    fn start_pick(&mut self, pick: Pick, position: Point<Pixels>, cx: &mut Context<Self>) {
        if let Some(editor) = &mut self.theme_editor {
            editor.picking = Some(pick);
        }
        self.pick_at(pick, position, cx);
    }

    /// The square and the hue bar for the token being edited.
    fn render_picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let editor = self.theme_editor.as_ref()?;
        let Some(Field::Token(name)) = editor.field else { return None };
        let color = editor.draft.tokens().get(name)?;
        let (_, saturation, value) = color.to_hsv();
        let pure = hsla(Color::from_hsv(editor.hue, 1., 1., 1.));
        let white = hsla(Color::from_hsv(0., 0., 1., 1.));
        let black = hsla(Color::from_hsv(0., 0., 0., 1.));
        let layer = || div().absolute().top_0().left_0().size_full();
        let record = |cell: &Rc<Cell<Bounds<Pixels>>>| {
            let cell = cell.clone();
            canvas(move |bounds, _, _| cell.set(bounds), |_, _, _, _| {}).absolute().top_0().left_0().size_full()
        };
        let marker = div()
            .absolute()
            .left(relative(saturation))
            .top(relative(1. - value))
            .ml(px(-6.))
            .mt(px(-6.))
            .size(px(12.))
            .rounded_full()
            .border_2()
            .border_color(white)
            .shadow(marker_shadow(black));
        let square = div()
            .id("theme-pick-square")
            .relative()
            .w_full()
            .h(px(SQUARE))
            .rounded(px(6.))
            .cursor_crosshair()
            .bg(pure)
            .child(layer().rounded(px(6.)).bg(linear_gradient(90., linear_color_stop(white, 0.), linear_color_stop(white.opacity(0.), 1.))))
            .child(layer().rounded(px(6.)).bg(linear_gradient(180., linear_color_stop(black.opacity(0.), 0.), linear_color_stop(black, 1.))))
            .child(record(&editor.square))
            .child(marker)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.start_pick(Pick::Square, event.position, cx);
                }),
            );
        // Six gradients, each from one primary or secondary hue to the next.
        let segments = (0..6).map(|ix| {
            let from = hsla(Color::from_hsv(ix as f32 * 60., 1., 1., 1.));
            let to = hsla(Color::from_hsv((ix + 1) as f32 * 60., 1., 1., 1.));
            div().flex_1().h_full().bg(linear_gradient(90., linear_color_stop(from, 0.), linear_color_stop(to, 1.)))
        });
        let hue_marker = div()
            .absolute()
            .top(px(-2.))
            .left(relative(editor.hue / 360.))
            .ml(px(-3.))
            .w(px(6.))
            .h(px(HUE_BAR + 4.))
            .rounded(px(2.))
            .border_2()
            .border_color(white)
            .shadow(marker_shadow(black));
        let hue_bar = div()
            .id("theme-pick-hue")
            .relative()
            .flex()
            .w_full()
            .h(px(HUE_BAR))
            .cursor_pointer()
            .children(segments)
            .child(record(&editor.hue_bar))
            .child(hue_marker)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.start_pick(Pick::Hue, event.position, cx);
                }),
            );
        Some(div().flex().flex_col().gap(px(8.)).flex_none().pb(px(2.)).child(square).child(hue_bar).into_any_element())
    }

    /// Writes the theme into the `themes` folder and keeps it on screen.
    fn save_theme(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.theme_editor else { return };
        let file = editor.draft.file();
        if file.name.is_empty() {
            self.status = Some(format!("{}: {}", t("theme.save_failed"), t("theme.no_name")));
            cx.notify();
            return;
        }
        let folder = self.catalog.folder().to_path_buf();
        let target = editor.draft.path.clone().unwrap_or_else(|| {
            let id = free_id(&folder, &theme::slug(&file.name), |id| self.catalog.library().get(id).is_some());
            folder.join(format!("{id}.json"))
        });
        let id = target.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
        if let Err(error) = std::fs::create_dir_all(&folder).and_then(|()| std::fs::write(&target, file.to_json())) {
            self.status = Some(format!("{}: {error}", t("theme.save_failed")));
            cx.notify();
            return;
        }
        self.catalog.reload();
        if let Some(error) = self.catalog.library().errors().iter().find(|error| error.path.as_deref() == Some(target.as_path())) {
            self.status = Some(format!("{}: {}", t("theme.save_failed"), error.message));
            cx.notify();
            return;
        }
        let _ = self.lib().set_setting("theme", &id);
        self.theme_editor = None;
        cx.set_global(self.catalog.theme(&id));
        self.status = Some(format!("{}: {}", t("theme.saved"), file.name));
        cx.notify();
    }

    /// The editor, in place of the panes of the session on the right.
    pub(super) fn render_theme_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = *cx.global::<Theme>();
        let editor = self.theme_editor.as_ref()?;
        let focused = editor.focus.is_focused(window);
        let typing_name = editor.field == Some(Field::Name) && focused;
        let parent = editor.draft.parent.as_deref().and_then(|id| self.catalog.library().get(id)).map(|theme| theme.name.clone());
        let name_box = div()
            .id("theme-name")
            .when(editor.field == Some(Field::Name), |this| this.track_focus(&editor.focus).on_key_down(cx.listener(Self::theme_editor_key)))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.edit_field(Field::Name, window, cx)))
            .flex()
            .items_center()
            .h(px(30.))
            .px(px(8.))
            .rounded(px(6.))
            .bg(theme.editor_bg)
            .border_1()
            .border_color(if typing_name { theme.accent } else { theme.border })
            .text_size(px(13.))
            .children(text_field_children(&editor.name, theme.text, theme.over_accent, theme.selection, theme.caret, 16., typing_name));
        let rows = Arc::new(rows());
        let count = rows.len();
        let list = uniform_list(
            "theme-tokens",
            count,
            cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
                let theme = *cx.global::<Theme>();
                let Some(editor) = this.theme_editor.as_ref() else { return Vec::new() };
                let focused = editor.focus.is_focused(window);
                let tokens = editor.draft.tokens();
                range
                    .filter_map(|ix| rows.get(ix).map(|row| (ix, *row)))
                    .map(|(ix, row)| match row {
                        Row::Group(group) => div()
                            .h(px(ROW))
                            .w_full()
                            .flex()
                            .items_end()
                            .pb(px(3.))
                            .text_size(px(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.text_muted)
                            .child(group_label(group).to_uppercase())
                            .into_any_element(),
                        Row::Token(name) => {
                            let color = theme.get(name).unwrap_or(theme.main_bg);
                            let editing = editor.field == Some(Field::Token(name));
                            let changed = editor.draft.changed(name);
                            let value: AnyElement = if editing {
                                div()
                                    .flex()
                                    .items_center()
                                    .w(px(96.))
                                    .h(px(22.))
                                    .px(px(6.))
                                    .rounded(px(5.))
                                    .bg(theme.editor_bg)
                                    .border_1()
                                    .border_color(if Color::from_text(&editor.hex.text).is_some() { theme.accent } else { theme.error })
                                    .font_family(super::MONO)
                                    .children(text_field_children(&editor.hex, theme.text, theme.over_accent, theme.selection, theme.caret, 13., focused))
                                    .into_any_element()
                            } else {
                                let hex = tokens.get(name).map(Color::to_text).unwrap_or_default();
                                div().font_family(super::MONO).text_color(theme.text_secondary).child(hex).into_any_element()
                            };
                            let reset = changed.then(|| {
                                div()
                                    .id(("theme-reset", ix))
                                    .px(px(4.))
                                    .rounded(px(4.))
                                    .text_color(theme.text_muted)
                                    .hover(|style| style.bg(theme.hover).text_color(theme.text))
                                    .tooltip(|_, cx| tooltip(t("theme.reset"), None, cx))
                                    .child("↺")
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        cx.stop_propagation();
                                        this.reset_token(name, cx);
                                    }))
                            });
                            div()
                                .id(("theme-token", ix))
                                .when(editing, |this| this.track_focus(&editor.focus).on_key_down(cx.listener(Self::theme_editor_key)))
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.edit_field(Field::Token(name), window, cx)))
                                .h(px(ROW))
                                .w_full()
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .pl(px(4.))
                                .pr(px(14.))
                                .rounded(px(5.))
                                .text_size(px(12.))
                                .when(editing, |this| this.bg(theme.selection_inactive))
                                .when(!editing, |this| this.hover(|style| style.bg(theme.hover)))
                                .child(div().size(px(14.)).flex_none().rounded(px(3.)).border_1().border_color(theme.border).bg(color))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .font_family(super::MONO)
                                        .text_color(if changed { theme.text } else { theme.text_secondary })
                                        .child(if changed { format!("{name} •") } else { name.to_owned() }),
                                )
                                .children(reset)
                                .child(value)
                                .into_any_element()
                        }
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&editor.scroll)
        .size_full();
        let button = |id: &'static str, label: &'static str, primary: bool| {
            div()
                .id(id)
                .flex()
                .items_center()
                .h(px(28.))
                .px(px(12.))
                .rounded(px(6.))
                .text_size(px(12.5))
                .when(primary, |this| this.bg(theme.accent).text_color(theme.over_accent).hover(|style| style.opacity(0.9)))
                .when(!primary, |this| this.border_1().border_color(theme.border).hover(|style| style.bg(theme.hover)))
                .child(label)
        };
        let picker = self.render_picker(cx);
        Some(
            div()
                .id("theme-editor")
                .flex()
                .flex_col()
                .size_full()
                .p(px(12.))
                .gap(px(10.))
                // A drag that began in the picker goes on over the whole editor and ends when
                // the button is up, wherever it was let go.
                .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                    let Some(pick) = this.theme_editor.as_ref().and_then(|editor| editor.picking) else { return };
                    if event.pressed_button == Some(MouseButton::Left) {
                        this.pick_at(pick, event.position, cx);
                    } else if let Some(editor) = &mut this.theme_editor {
                        editor.picking = None;
                    }
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseUpEvent, _, _| {
                        if let Some(editor) = &mut this.theme_editor {
                            editor.picking = None;
                        }
                    }),
                )
                .child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).child(t("theme.editor")))
                .child(name_box)
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(theme.text_muted)
                        .child(match parent {
                            Some(parent) => format!("{} {parent} · {}", t("theme.based_on"), t("theme.hint")),
                            None => t("theme.hint").to_owned(),
                        }),
                )
                .child(div().flex_1().min_h_0().child(scrollable("theme-tokens-scroll", &editor.scroll, list, &theme, window, cx).size_full()))
                .children(picker)
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .justify_end()
                        .child(button("theme-cancel", t("theme.cancel"), false).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.close_theme_editor(cx))))
                        .child(button("theme-save", t("theme.save"), true).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.save_theme(cx)))),
                )
                .into_any_element(),
        )
    }
}
