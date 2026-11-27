//! A one-line edit: text, a caret and a selection. Tool windows and the find bar use it,
//! so Ctrl+A, the shift keys, Backspace and the clipboard behave the same everywhere.

use std::ops::Range;

use gpui_kit::prelude::*;
use gpui_kit::*;

/// Typed text with a caret and an anchor. Offsets are bytes on character boundaries.
#[derive(Clone, Debug, Default)]
pub struct TextField {
    pub text: String,
    caret: usize,
    /// The other end of the selection; equal to `caret` when nothing is selected.
    anchor: usize,
    /// The states before the edits, the latest last, for Ctrl+Z; and the ones undone, for
    /// Ctrl+Y.
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// The kind of the last edit: typing and deleting in a row undo together.
    last_edit: Option<EditKind>,
}

/// The text, caret and anchor of a field at one moment.
#[derive(Clone, Debug)]
struct Snapshot {
    text: String,
    caret: usize,
    anchor: usize,
}

/// Edits that run together into one step of undo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditKind {
    Typing,
    Deleting,
    Other,
}

/// How many steps of undo a field keeps.
const UNDO_STEPS: usize = 200;

impl TextField {
    pub fn from_text(text: impl Into<String>) -> Self {
        let text = text.into();
        let caret = text.len();
        Self {
            text,
            caret,
            anchor: caret,
            ..Default::default()
        }
    }

    /// Keeps the state before an edit of `kind`. A run of typing (up to a space or a new
    /// line) or of deleting is one step.
    fn record(&mut self, kind: EditKind) {
        let joins = kind != EditKind::Other && self.last_edit == Some(kind) && !self.has_selection();
        self.last_edit = Some(kind);
        self.redo.clear();
        if joins {
            return;
        }
        self.undo.push(Snapshot {
            text: self.text.clone(),
            caret: self.caret,
            anchor: self.anchor,
        });
        if self.undo.len() > UNDO_STEPS {
            self.undo.remove(0);
        }
    }

    fn restore(&mut self, snapshot: Snapshot) -> Snapshot {
        let now = Snapshot {
            text: std::mem::replace(&mut self.text, snapshot.text),
            caret: self.caret,
            anchor: self.anchor,
        };
        self.caret = snapshot.caret.min(self.text.len());
        self.anchor = snapshot.anchor.min(self.text.len());
        self.last_edit = None;
        now
    }

    /// Takes back the last edit; false when there is none.
    pub fn undo(&mut self) -> bool {
        let Some(before) = self.undo.pop() else {
            return false;
        };
        let now = self.restore(before);
        self.redo.push(now);
        true
    }

    /// Makes the last undone edit again; false when there is none.
    pub fn redo(&mut self) -> bool {
        let Some(after) = self.redo.pop() else {
            return false;
        };
        let now = self.restore(after);
        self.undo.push(now);
        true
    }

    /// Moves the caret and keeps the anchor: a drag of the mouse, or Shift+click.
    pub fn select_to(&mut self, caret: usize) {
        self.caret = self.boundary(caret);
        self.last_edit = None;
    }

    /// Selects the word around `at`, or the one character there when it is no word.
    pub fn select_word_at(&mut self, at: usize) {
        let word = word_around(&self.text, at);
        self.select_range(word.start, word.end);
    }

    /// Selects the line around `at`.
    pub fn select_line_at(&mut self, at: usize) {
        let line = line_around(&self.text, at);
        self.select_range(line.start, line.end);
    }

    /// Selects from `anchor` to `caret`, the caret where it is drawn.
    pub fn select_range(&mut self, anchor: usize, caret: usize) {
        self.anchor = self.boundary(anchor);
        self.caret = self.boundary(caret);
        self.last_edit = None;
    }

    pub fn caret(&self) -> usize {
        self.caret
    }

    pub fn set_caret(&mut self, caret: usize) {
        let caret = self.boundary(caret);
        self.caret = caret;
        self.anchor = caret;
        self.last_edit = None;
    }

    pub fn selection(&self) -> Range<usize> {
        self.anchor.min(self.caret)..self.anchor.max(self.caret)
    }

    pub fn has_selection(&self) -> bool {
        self.anchor != self.caret
    }

    pub fn selected_text(&self) -> &str {
        &self.text[self.selection()]
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.text.len();
    }

    /// Replaces the selection, or puts `text` at the caret, and leaves the caret after it.
    /// One step of undo.
    pub fn insert_text(&mut self, text: &str) {
        self.record(EditKind::Other);
        self.insert(text);
    }

    /// Typed text: a run of it is one step of undo, up to a space or a new line.
    fn type_text(&mut self, text: &str) {
        let breaks = text.chars().any(char::is_whitespace);
        self.record(if breaks { EditKind::Other } else { EditKind::Typing });
        self.insert(text);
        if breaks {
            self.last_edit = None;
        }
    }

    fn insert(&mut self, text: &str) {
        let range = self.selection();
        self.text.replace_range(range.clone(), text);
        self.caret = range.start + text.len();
        self.anchor = self.caret;
    }

    /// Removes the selection, or one character before the caret.
    pub fn backspace(&mut self, word: bool) {
        if self.caret == 0 && !self.has_selection() {
            return;
        }
        self.record(if word || self.has_selection() { EditKind::Other } else { EditKind::Deleting });
        if self.delete_selection() {
            return;
        }
        let start = if word {
            self.previous_word(self.caret)
        } else {
            self.previous_boundary(self.caret)
        };
        self.text.replace_range(start..self.caret, "");
        self.caret = start;
        self.anchor = start;
    }

    /// Removes the selection, or one character after the caret.
    pub fn delete(&mut self, word: bool) {
        if self.caret == self.text.len() && !self.has_selection() {
            return;
        }
        self.record(if word || self.has_selection() { EditKind::Other } else { EditKind::Deleting });
        if self.delete_selection() {
            return;
        }
        let end = if word {
            self.next_word(self.caret)
        } else {
            self.next_boundary(self.caret)
        };
        self.text.replace_range(self.caret..end, "");
        self.anchor = self.caret;
    }

    fn delete_selection(&mut self) -> bool {
        if !self.has_selection() {
            return false;
        }
        let range = self.selection();
        self.text.replace_range(range.clone(), "");
        self.caret = range.start;
        self.anchor = range.start;
        true
    }

    fn boundary(&self, at: usize) -> usize {
        let mut at = at.min(self.text.len());
        while !self.text.is_char_boundary(at) {
            at -= 1;
        }
        at
    }

    fn previous_boundary(&self, at: usize) -> usize {
        self.text[..at]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index)
    }

    fn next_boundary(&self, at: usize) -> usize {
        at + self.text[at..].chars().next().map_or(0, char::len_utf8)
    }

    fn word_char(character: char) -> bool {
        word_char(character)
    }

    /// The start of the word before `at`, skipping the spaces between words.
    fn previous_word(&self, at: usize) -> usize {
        let mut at = at;
        while at > 0 {
            let previous = self.previous_boundary(at);
            let character = self.text[previous..at].chars().next().unwrap_or(' ');
            if Self::word_char(character) {
                break;
            }
            at = previous;
        }
        while at > 0 {
            let previous = self.previous_boundary(at);
            let character = self.text[previous..at].chars().next().unwrap_or(' ');
            if !Self::word_char(character) {
                break;
            }
            at = previous;
        }
        at
    }

    /// The end of the word after `at`, skipping the characters between words.
    fn next_word(&self, at: usize) -> usize {
        let mut at = at;
        while at < self.text.len() {
            let character = self.text[at..].chars().next().unwrap_or(' ');
            if Self::word_char(character) {
                break;
            }
            at = self.next_boundary(at);
        }
        while at < self.text.len() {
            let character = self.text[at..].chars().next().unwrap_or(' ');
            if !Self::word_char(character) {
                break;
            }
            at = self.next_boundary(at);
        }
        at
    }

    fn move_to(&mut self, to: usize, shift: bool) {
        if !shift {
            self.anchor = to;
        }
        self.caret = to;
        self.last_edit = None;
    }

    fn move_left(&mut self, word: bool, shift: bool) {
        if !shift && self.has_selection() {
            let start = self.selection().start;
            self.move_to(start, false);
            return;
        }
        let to = if word {
            self.previous_word(self.caret)
        } else {
            self.previous_boundary(self.caret)
        };
        self.move_to(to, shift);
    }

    fn move_right(&mut self, word: bool, shift: bool) {
        if !shift && self.has_selection() {
            let end = self.selection().end;
            self.move_to(end, false);
            return;
        }
        let to = if word {
            self.next_word(self.caret)
        } else {
            self.next_boundary(self.caret)
        };
        self.move_to(to, shift);
    }

    fn line_start(&self, at: usize) -> usize {
        self.text[..at].rfind('\n').map_or(0, |index| index + 1)
    }

    fn line_end(&self, at: usize) -> usize {
        self.text[at..]
            .find('\n')
            .map_or(self.text.len(), |index| at + index)
    }

    /// Editing keys. `multiline` lets Enter add a line and Up/Down move between lines.
    /// Clipboard keys are in [`TextField::key_with_clipboard`]. Returns whether the key
    /// was taken.
    pub fn key(&mut self, event: &KeyDownEvent, multiline: bool) -> bool {
        let stroke = &event.keystroke;
        let modifiers = stroke.modifiers;
        let primary = modifiers.control || modifiers.platform;
        let shift = modifiers.shift;
        match stroke.key.as_str() {
            "backspace" if !modifiers.alt => self.backspace(primary),
            "delete" if !modifiers.alt => self.delete(primary),
            "left" if !modifiers.alt => self.move_left(primary, shift),
            "right" if !modifiers.alt => self.move_right(primary, shift),
            "home" if !modifiers.alt => {
                let to = self.line_start(self.caret);
                self.move_to(to, shift);
            }
            "end" if !modifiers.alt => {
                let to = self.line_end(self.caret);
                self.move_to(to, shift);
            }
            "up" if multiline && !primary && !modifiers.alt => {
                self.vertical(-1, shift);
            }
            "down" if multiline && !primary && !modifiers.alt => {
                self.vertical(1, shift);
            }
            "a" if primary && !modifiers.alt => self.select_all(),
            "z" if primary && !modifiers.alt && !shift => {
                self.undo();
            }
            "z" | "y" if primary && !modifiers.alt => {
                self.redo();
            }
            "enter" if multiline && !primary && !modifiers.alt => {
                self.type_text("\n");
            }
            _ => {
                if primary || modifiers.alt {
                    return false;
                }
                let Some(text) = &stroke.key_char else {
                    return false;
                };
                let text = text.replace("\r\n", "\n").replace('\r', "\n");
                if text.is_empty() || text.chars().any(char::is_control) && !multiline {
                    return false;
                }
                self.type_text(&text);
            }
        }
        true
    }

    /// The field as a password field shows it: a `•` for every character, with the caret
    /// and the selection where they are in the text.
    pub fn masked(&self) -> TextField {
        let dots = |byte: usize| self.text[..byte].chars().count() * '•'.len_utf8();
        TextField {
            text: "•".repeat(self.text.chars().count()),
            caret: dots(self.caret),
            anchor: dots(self.anchor),
            ..Default::default()
        }
    }

    /// [`TextField::key`] plus Ctrl+C, Ctrl+X and Ctrl+V through the clipboard.
    pub fn key_with_clipboard(
        &mut self,
        event: &KeyDownEvent,
        multiline: bool,
        cx: &mut App,
    ) -> bool {
        let stroke = &event.keystroke;
        let modifiers = stroke.modifiers;
        if (modifiers.control || modifiers.platform) && !modifiers.alt {
            match stroke.key.as_str() {
                "c" => {
                    if self.has_selection() {
                        let text = self.selected_text().to_owned();
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                    return true;
                }
                "x" => {
                    if self.has_selection() {
                        let text = self.selected_text().to_owned();
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                        self.backspace(false);
                    }
                    return true;
                }
                "v" => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        if !multiline {
                            let line = text.lines().next().unwrap_or("");
                            self.insert_text(line);
                        } else {
                            let text = text.replace("\r\n", "\n").replace('\r', "\n");
                            self.insert_text(&text);
                        }
                    }
                    return true;
                }
                _ => {}
            }
        }
        self.key(event, multiline)
    }

    fn vertical(&mut self, step: isize, shift: bool) {
        let line = self.text[..self.caret]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        let column = self.caret - self.line_start(self.caret);
        let target_line = line as isize + step;
        if target_line < 0 {
            return;
        }
        let mut start = 0;
        for _ in 0..target_line {
            match self.text[start..].find('\n') {
                Some(index) => start += index + 1,
                None => return,
            }
        }
        let end = self.line_end(start);
        let mut to = start + column.min(end - start);
        while to > start && !self.text.is_char_boundary(to) {
            to -= 1;
        }
        self.move_to(to, shift);
    }
}

/// The parts of a field for drawing: its text split at the selection, with the caret bar
/// between them. `text_color` is the colour of the unselected text.
#[allow(clippy::too_many_arguments)]
pub fn text_field_children(
    field: &TextField,
    text_color: Hsla,
    selected_color: Hsla,
    selection_color: Hsla,
    caret_color: Hsla,
    caret_height: f32,
    show_caret: bool,
) -> Vec<AnyElement> {
    range_children(
        None,
        field,
        0..field.text.len(),
        text_color,
        selected_color,
        selection_color,
        caret_color,
        caret_height,
        show_caret,
    )
}

/// [`text_field_children`] with the caret and the selected text named in the render tree
/// as `<name>-caret` and `<name>-selection`, so a test of a window sees the caret of the
/// field it focused.
#[allow(clippy::too_many_arguments)]
pub fn text_field_children_named(
    name: &'static str,
    field: &TextField,
    text_color: Hsla,
    selected_color: Hsla,
    selection_color: Hsla,
    caret_color: Hsla,
    caret_height: f32,
    show_caret: bool,
) -> Vec<AnyElement> {
    range_children(
        Some(name),
        field,
        0..field.text.len(),
        text_color,
        selected_color,
        selection_color,
        caret_color,
        caret_height,
        show_caret,
    )
}

/// The lines of a multiline field: one row per line, with the caret on its own line. A
/// line is `caret_height` tall, so a caret and the text next to it share the row.
#[allow(clippy::too_many_arguments)]
pub fn text_field_lines(
    field: &TextField,
    text_color: Hsla,
    selected_color: Hsla,
    selection_color: Hsla,
    caret_color: Hsla,
    caret_height: f32,
    show_caret: bool,
) -> Vec<AnyElement> {
    lines_children(
        None,
        field,
        text_color,
        selected_color,
        selection_color,
        caret_color,
        caret_height,
        show_caret,
    )
}

/// [`text_field_lines`] with the named parts of [`text_field_children_named`].
#[allow(clippy::too_many_arguments)]
pub fn text_field_lines_named(
    name: &'static str,
    field: &TextField,
    text_color: Hsla,
    selected_color: Hsla,
    selection_color: Hsla,
    caret_color: Hsla,
    caret_height: f32,
    show_caret: bool,
) -> Vec<AnyElement> {
    lines_children(
        Some(name),
        field,
        text_color,
        selected_color,
        selection_color,
        caret_color,
        caret_height,
        show_caret,
    )
}

#[allow(clippy::too_many_arguments)]
fn lines_children(
    name: Option<&'static str>,
    field: &TextField,
    text_color: Hsla,
    selected_color: Hsla,
    selection_color: Hsla,
    caret_color: Hsla,
    caret_height: f32,
    show_caret: bool,
) -> Vec<AnyElement> {
    let mut children = Vec::new();
    let mut start = 0;
    loop {
        let end = field.text[start..]
            .find('\n')
            .map_or(field.text.len(), |index| start + index);
        children.push(
            div()
                .flex()
                .items_center()
                .flex_none()
                .h(px(caret_height))
                .whitespace_nowrap()
                .children(range_children(
                    name,
                    field,
                    start..end,
                    text_color,
                    selected_color,
                    selection_color,
                    caret_color,
                    caret_height,
                    show_caret,
                ))
                .into_any_element(),
        );
        if end == field.text.len() {
            return children;
        }
        start = end + 1;
    }
}

/// The text of a field of many lines, wrapped to the width it gets (and again whenever
/// that width changes), with the selection and the caret drawn where the text laid out.
/// `line_height` is the height of a line and of the caret. The layout it returns turns a
/// point of the window into a byte of the text once the element is drawn (see
/// [`index_at`]), for placing the caret and selecting with the mouse.
pub fn text_field_wrapped(
    field: &TextField,
    text_color: Hsla,
    selected_color: Hsla,
    selection_color: Hsla,
    caret_color: Hsla,
    line_height: f32,
    show_caret: bool,
) -> (AnyElement, TextLayout) {
    let selection = field.selection();
    let highlights = if selection.is_empty() {
        Vec::new()
    } else {
        vec![(
            selection,
            HighlightStyle {
                color: Some(selected_color),
                background_color: Some(selection_color),
                ..Default::default()
            },
        )]
    };
    let styled = StyledText::new(field.text.clone()).with_highlights(highlights);
    let layout = styled.layout().clone();
    let returned = layout.clone();
    let caret_at = field.caret;
    let element = div()
        .relative()
        .w_full()
        .min_w_0()
        .line_height(px(line_height))
        .text_color(text_color)
        .child(styled)
        .when(show_caret, |this| {
            // Painted after the text, which has its layout by then.
            this.child(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, _| {
                        let at = layout.position_for_index(caret_at).unwrap_or(bounds.origin);
                        window.paint_quad(fill(
                            Bounds::new(at, size(px(1.5), px(line_height))),
                            caret_color,
                        ));
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
        })
        .into_any_element();
    (element, returned)
}

/// What a drag of the mouse selects by: letters after one click, whole words after a
/// double click, whole lines (or the whole text) after a triple one, as in an editor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SelectUnit {
    #[default]
    Letter,
    Word,
    Line,
    All,
}

fn word_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn floor_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// The word of `text` the letter at the byte `at` belongs to, or that one letter when it is
/// no word (a space, a mark); the last word at the end of the text.
pub fn word_around(text: &str, at: usize) -> Range<usize> {
    let at = floor_boundary(text, at);
    let after = |at: usize| text[at..].chars().next();
    let before = |at: usize| text[..at].chars().next_back();
    let in_word = match after(at) {
        Some(letter) => word_char(letter),
        None => before(at).is_some_and(word_char),
    };
    if !in_word {
        return at..at + after(at).map_or(0, char::len_utf8);
    }
    let mut start = at;
    while let Some(character) = before(start).filter(|c| word_char(*c)) {
        start -= character.len_utf8();
    }
    let mut end = at;
    while let Some(character) = after(end).filter(|c| word_char(*c)) {
        end += character.len_utf8();
    }
    start..end
}

/// The line of `text` around the byte `at`, without its line break.
pub fn line_around(text: &str, at: usize) -> Range<usize> {
    let at = floor_boundary(text, at);
    let start = text[..at].rfind('\n').map_or(0, |index| index + 1);
    let end = text[at..].find('\n').map_or(text.len(), |index| at + index);
    start..end
}

/// The `unit` of `text` around the byte `at`.
pub fn unit_around(text: &str, at: usize, unit: SelectUnit) -> Range<usize> {
    match unit {
        SelectUnit::Letter => {
            let at = floor_boundary(text, at);
            at..at
        }
        SelectUnit::Word => word_around(text, at),
        SelectUnit::Line => line_around(text, at),
        SelectUnit::All => 0..text.len(),
    }
}

/// The anchor and the head of a selection that began on `origin` (the word or line clicked
/// first) and is dragged over `here` (the one under the mouse now): it takes both whole,
/// and the anchor goes to the far end of the origin when the drag goes back before it.
/// Places are anything ordered: bytes of one text, or places in many.
pub fn grow_selection<P: Ord + Copy>(origin: (P, P), here: (P, P)) -> (P, P) {
    if here.0 < origin.0 { (origin.1, here.0) } else { (origin.0, here.1.max(origin.1)) }
}

/// The letter of a drawn text under `position`, the nearest one when it is beside the text:
/// what a click hits, the word a double click takes.
pub fn letter_at(layout: &TextLayout, position: Point<Pixels>) -> usize {
    layout.index_for_position(position).unwrap_or_else(|index| index)
}

/// The boundary between letters of a drawn text nearest to `position`: the caret a click
/// there leaves, or the end of a selection dragged there. GPUI gives the letter under the
/// point, so a point on the right half of a letter goes past it here, as in any editor.
pub fn index_at(layout: &TextLayout, position: Point<Pixels>) -> usize {
    let index = match layout.index_for_position(position) {
        Ok(index) => index,
        Err(index) => return index,
    };
    let text = layout.text();
    let Some(letter) = text.get(index..).and_then(|rest| rest.chars().next()) else { return index };
    let next = index + letter.len_utf8();
    match (layout.position_for_index(index), layout.position_for_index(next)) {
        // The letter's both edges on one line: not one a line wraps after.
        (Some(start), Some(end)) if start.y == end.y => nearer_edge(index, next, start.x, end.x, position.x),
        _ => index,
    }
}

/// `before` or `after` a letter drawn from `left` to `right`, whichever `x` is nearer.
fn nearer_edge(before: usize, after: usize, left: Pixels, right: Pixels, x: Pixels) -> usize {
    if right > left && x - left > right - x { after } else { before }
}

/// The parts of the field that lie on one line. The caret is placed only when it belongs
/// to this line, and the fallback at the end keeps it on an empty line.
#[allow(clippy::too_many_arguments)]
fn range_children(
    name: Option<&'static str>,
    field: &TextField,
    line: Range<usize>,
    text_color: Hsla,
    selected_color: Hsla,
    selection_color: Hsla,
    caret_color: Hsla,
    caret_height: f32,
    show_caret: bool,
) -> Vec<AnyElement> {
    let selection = field.selection();
    let selected_start = selection.start.clamp(line.start, line.end);
    let selected_end = selection.end.clamp(line.start, line.end);
    let mut parts: Vec<(Range<usize>, bool)> = Vec::new();
    if selected_start > line.start {
        parts.push((line.start..selected_start, false));
    }
    if selected_start < selected_end {
        parts.push((selected_start..selected_end, true));
    }
    if selected_end < line.end {
        parts.push((selected_end..line.end, false));
    }
    let caret_in_line = show_caret && field.caret >= line.start && field.caret <= line.end;
    let mut children = Vec::new();
    let mut caret_placed = false;
    for (range, selected) in parts {
        let caret_here = caret_in_line
            && !caret_placed
            && field.caret >= range.start
            && field.caret <= range.end;
        let split = if caret_here { field.caret } else { range.end };
        if split > range.start {
            children.push(segment(
                name,
                &field.text[range.start..split],
                selected,
                text_color,
                selected_color,
                selection_color,
            ));
        }
        if caret_here {
            children.push(caret(caret_color, caret_height, name));
            caret_placed = true;
            if range.end > split {
                children.push(segment(
                    name,
                    &field.text[split..range.end],
                    selected,
                    text_color,
                    selected_color,
                    selection_color,
                ));
            }
        }
    }
    if caret_in_line && !caret_placed {
        children.push(caret(caret_color, caret_height, name));
    }
    children
}

fn segment(
    name: Option<&'static str>,
    text: &str,
    selected: bool,
    text_color: Hsla,
    selected_color: Hsla,
    selection_color: Hsla,
) -> AnyElement {
    div()
        .whitespace_nowrap()
        .text_color(if selected { selected_color } else { text_color })
        .when(selected, |this| this.bg(selection_color))
        .when_some(name.filter(|_| selected), |this, name| {
            this.debug_selector(move || format!("{name}-selection"))
        })
        .child(text.to_owned())
        .into_any_element()
}

fn caret(color: Hsla, height: f32, name: Option<&'static str>) -> AnyElement {
    div()
        .flex_none()
        .w(px(1.5))
        .h(px(height))
        .bg(color)
        .when_some(name, |this, name| {
            this.debug_selector(move || format!("{name}-caret"))
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use gpui_kit::{KeyDownEvent, Keystroke, Modifiers, px};

    use super::TextField;

    /// A double click takes the word, Cyrillic too, or the one mark that is no word; a
    /// triple one the line.
    #[test]
    fn a_click_finds_its_word_and_its_line() {
        let text = "Мы прошли мимо, snake_case!\nВторая строка";
        let word = |at: usize| &text[super::word_around(text, at)];
        let at = |part: &str| text.find(part).unwrap();
        assert_eq!(word(at("шли")), "прошли");
        assert_eq!(word(at("ли мимо") + 1), "прошли", "inside a two-byte letter");
        assert_eq!(word(at(" мимо") + 1), "мимо", "at the start of a word");
        assert_eq!(word(at("case")), "snake_case");
        assert_eq!(word(at("!")), "!");
        assert_eq!(word(at(", ") + 1), " ");
        assert_eq!(word(text.len()), "строка", "past the end: the last word");
        assert_eq!(&text[super::line_around(text, at("рая"))], "Вторая строка");
        assert_eq!(&text[super::line_around(text, 0)], "Мы прошли мимо, snake_case!");
        assert_eq!(super::unit_around(text, 5, super::SelectUnit::All), 0..text.len());
    }

    /// A drag after a double click grows by whole words both ways and keeps the first one.
    #[test]
    fn a_drag_by_words_keeps_the_first_word_whole() {
        let first = (10, 16);
        assert_eq!(super::grow_selection(first, (10, 16)), (10, 16));
        assert_eq!(super::grow_selection(first, (17, 22)), (10, 22), "forward to the end of a later word");
        assert_eq!(super::grow_selection(first, (3, 9)), (16, 3), "back: the anchor goes to the far end");
        assert_eq!(super::grow_selection(first, (12, 16)), (10, 16), "inside the first word");
    }

    /// A point on the right half of a letter is past it; on the left half, before it.
    #[test]
    fn a_point_goes_to_the_nearer_edge_of_its_letter() {
        let edge = |x: f32| super::nearer_edge(8, 10, px(40.), px(48.), px(x));
        assert_eq!(edge(41.), 8);
        assert_eq!(edge(43.9), 8);
        assert_eq!(edge(44.), 8, "the middle stays before");
        assert_eq!(edge(44.5), 10);
        assert_eq!(edge(47.9), 10);
        assert_eq!(super::nearer_edge(8, 10, px(40.), px(40.), px(40.)), 8, "a letter of no width");
    }

    fn press(key: &str, key_char: Option<&str>, modifiers: Modifiers) -> KeyDownEvent {
        KeyDownEvent {
            keystroke: Keystroke {
                modifiers,
                key: key.into(),
                key_char: key_char.map(Into::into),
            },
            is_held: false,
            prefer_character_input: false,
        }
    }

    fn control() -> Modifiers {
        Modifiers {
            control: true,
            ..Modifiers::default()
        }
    }

    fn shift() -> Modifiers {
        Modifiers {
            shift: true,
            ..Modifiers::default()
        }
    }

    fn control_shift() -> Modifiers {
        Modifiers {
            control: true,
            shift: true,
            ..Modifiers::default()
        }
    }

    #[test]
    fn typing_goes_in_at_the_caret_and_backspace_takes_it_back() {
        let mut field = TextField::default();
        assert!(field.key(&press("a", Some("a"), Modifiers::default()), false));
        assert!(field.key(&press("b", Some("b"), Modifiers::default()), false));
        assert!(field.key(&press("left", None, Modifiers::default()), false));
        assert!(field.key(&press("x", Some("x"), Modifiers::default()), false));
        assert_eq!(field.text, "axb");
        assert!(field.key(&press("backspace", None, Modifiers::default()), false));
        assert_eq!(field.text, "ab");
        assert_eq!(field.caret(), 1);
    }

    #[test]
    fn ctrl_a_selects_all_and_typing_replaces_it() {
        let mut field = TextField::from_text("alpha");
        assert!(field.key(&press("a", Some("a"), control()), false));
        assert_eq!(field.selected_text(), "alpha");
        assert!(field.key(&press("b", Some("b"), Modifiers::default()), false));
        assert_eq!(field.text, "b");
        assert_eq!(field.caret(), 1);
    }

    #[test]
    fn shift_and_ctrl_shift_move_the_end_of_the_selection() {
        let mut field = TextField::from_text("one two");
        assert!(field.key(&press("home", None, Modifiers::default()), false));
        assert!(field.key(&press("right", None, shift()), false));
        assert_eq!(field.selected_text(), "o");
        assert!(field.key(&press("right", None, control_shift()), false));
        assert_eq!(field.selected_text(), "one");
        assert!(field.key(&press("end", None, shift()), false));
        assert_eq!(field.selected_text(), "one two");
    }

    #[test]
    fn ctrl_z_takes_back_a_word_at_a_time_and_ctrl_y_makes_it_again() {
        let mut field = TextField::from_text("");
        let typed = |field: &mut TextField, text: &str| {
            for character in text.chars() {
                let key = character.to_string();
                assert!(field.key(&press(&key, Some(&key), Modifiers::default()), true));
            }
        };
        typed(&mut field, "hello world");
        assert!(field.key(&press("backspace", None, Modifiers::default()), true));
        assert!(field.key(&press("backspace", None, Modifiers::default()), true));
        assert_eq!(field.text, "hello wor");
        assert!(field.key(&press("z", Some("z"), control()), true));
        assert_eq!(field.text, "hello world", "the two deletions are one step");
        assert!(field.key(&press("z", Some("z"), control()), true));
        assert_eq!(field.text, "hello ", "the typing of a word is one step");
        assert!(field.key(&press("z", Some("z"), control()), true));
        assert_eq!(field.text, "hello");
        assert!(field.key(&press("y", Some("y"), control()), true));
        assert_eq!(field.text, "hello ");
        assert!(field.key(&press("z", Some("Z"), control_shift()), true));
        assert_eq!(field.text, "hello world");
        assert_eq!(field.caret(), field.text.len());
        // A new edit drops what was undone.
        assert!(field.key(&press("z", Some("z"), control()), true));
        typed(&mut field, "x");
        assert!(!field.redo());
        while field.undo() {}
        assert_eq!(field.text, "");
    }

    #[test]
    fn a_double_click_selects_a_word_and_a_drag_moves_the_end() {
        let mut field = TextField::from_text("say héllo_wörld, now\nnext line");
        field.select_word_at(7);
        assert_eq!(field.selected_text(), "héllo_wörld");
        field.select_word_at(field.text.find(", ").unwrap() + 1);
        assert_eq!(field.selected_text(), " ", "outside a word, the character");
        field.set_caret(0);
        field.select_to(3);
        assert_eq!(field.selected_text(), "say");
        field.select_line_at(field.text.len() - 1);
        assert_eq!(field.selected_text(), "next line");
    }

    #[test]
    fn shift_and_delete_replace_the_selection() {
        let mut field = TextField::from_text("abcd");
        assert!(field.key(&press("left", None, shift()), false));
        assert!(field.key(&press("backspace", None, Modifiers::default()), false));
        assert_eq!(field.text, "abc");
        assert!(field.key(&press("left", None, shift()), false));
        assert!(field.key(&press("delete", None, Modifiers::default()), false));
        assert_eq!(field.text, "ab");
    }
}
