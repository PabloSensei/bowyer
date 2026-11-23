//! Selecting text in the conversation: a drag from one block of text (a paragraph, an
//! item, a heading, a code block) to another, in the same message or a later one, selects
//! everything between, and a bubble over the selection asks about it in the composer or
//! copies it. Dragged to the top or bottom edge of the conversation, or past it, the
//! selection scrolls the conversation and goes on.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use ui_kit::prelude::*;
use ui_kit::*;

use super::Shell;
use crate::i18n::t;

/// A place in the text of the conversation: the piece (`base` of its view, the item times
/// 1000 plus the piece), the block in it, and the byte in the block's text. Places order
/// as the conversation reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Place {
    pub block: (usize, usize),
    pub offset: usize,
}

/// The blocks of text drawn in the last frame: each block, where it is and its layout.
pub(super) type DrawnText = Rc<RefCell<Vec<((usize, usize), Bounds<Pixels>, TextLayout)>>>;

/// How near the top or bottom edge of the conversation a dragged selection scrolls it.
const AUTOSCROLL_ZONE: f32 = 28.;
/// The step of the scroll per tick at the edge of the zone; faster farther out, up to three times.
const AUTOSCROLL_STEP: f32 = 8.;
const AUTOSCROLL_TICK: Duration = Duration::from_millis(16);

pub(super) struct TextSelection {
    /// Where the drag started.
    anchor: Place,
    /// Where the pointer is, or let go.
    head: Place,
    dragging: bool,
    /// Where the pointer let go: the bubble shows there.
    bubble: Option<Point<Pixels>>,
}

impl TextSelection {
    fn ends(&self) -> (Place, Place) {
        (self.anchor.min(self.head), self.anchor.max(self.head))
    }

    /// The selected bytes of one block whose text is `len` bytes long.
    pub fn range_in(&self, block: (usize, usize), len: usize) -> Option<Range<usize>> {
        let (start, end) = self.ends();
        if block < start.block || block > end.block {
            return None;
        }
        let from = if block == start.block { start.offset.min(len) } else { 0 };
        let to = if block == end.block { end.offset.min(len) } else { len };
        (from < to).then_some(from..to)
    }
}

/// The selected text of the blocks between two places, given every block of the
/// conversation in reading order with its text: blocks join with an empty line.
pub(super) fn selected_text<'a>(blocks: impl IntoIterator<Item = ((usize, usize), &'a str)>, start: Place, end: Place) -> String {
    let mut parts = Vec::new();
    for (block, text) in blocks {
        if block < start.block || block > end.block {
            continue;
        }
        let from = if block == start.block { start.offset.min(text.len()) } else { 0 };
        let to = if block == end.block { end.offset.min(text.len()) } else { text.len() };
        if let Some(part) = text.get(from..to).filter(|part| !part.is_empty()) {
            parts.push(part);
        }
    }
    parts.join("\n\n")
}

/// The byte of the text under `position`, the nearest one when it is beside the text.
fn index_at(layout: &TextLayout, position: Point<Pixels>) -> usize {
    layout.index_for_position(position).unwrap_or_else(|index| index)
}

/// The text of a block with the selection painted over its own colours: the styles are
/// cut where the selection starts and ends, since the text drawing takes no overlap.
pub(super) fn with_selection(
    text: &str,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
    selection: Option<Range<usize>>,
    background: Hsla,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let Some(selection) = selection.filter(|s| s.start < s.end && s.end <= text.len()) else { return highlights };
    let mut cuts: Vec<usize> = highlights.iter().flat_map(|(r, _)| [r.start, r.end]).chain([selection.start, selection.end]).collect();
    cuts.retain(|&at| at <= text.len() && text.is_char_boundary(at));
    cuts.sort_unstable();
    cuts.dedup();
    cuts.windows(2)
        .filter_map(|pair| {
            let piece = pair[0]..pair[1];
            let own = highlights.iter().find(|(r, _)| r.start <= piece.start && piece.end <= r.end).map(|(_, style)| *style);
            let selected = selection.start <= piece.start && piece.end <= selection.end;
            match (own, selected) {
                (None, false) => None,
                (style, true) => Some((piece, HighlightStyle { background_color: Some(background), ..style.unwrap_or_default() })),
                (Some(style), false) => Some((piece, style)),
            }
        })
        .collect()
}

impl Shell {
    /// A block's text that the pointer can select. A drag that started in another block
    /// moves its end here while the pointer is over this one.
    /// A click that selects nothing on one of `links` (byte ranges of the text and web
    /// addresses) opens the address in the browser.
    pub(super) fn selectable(&self, block: (usize, usize), styled: StyledText, links: Vec<(std::ops::Range<usize>, String)>, cx: &mut Context<Self>) -> AnyElement {
        let layout = styled.layout().clone();
        let move_layout = layout.clone();
        let drawn_layout = layout.clone();
        let up_layout = layout.clone();
        let drawn = self.drawn_text.clone();
        // Where the block is drawn, after its text: its layout is ready then.
        let record = canvas(
            move |bounds, _, _| drawn.borrow_mut().push((block, bounds, drawn_layout.clone())),
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        div()
            .id(SharedString::from(format!("sel-{}-{}", block.0, block.1)))
            .relative()
            .cursor_text()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    let place = Place { block, offset: index_at(&layout, event.position) };
                    // Shift extends the selection there is.
                    let anchor = match &this.selection {
                        Some(selection) if event.modifiers.shift => selection.anchor,
                        _ => place,
                    };
                    this.selection = Some(TextSelection { anchor, head: place, dragging: true, bubble: None });
                    // Ctrl+C then copies the selection: the composer may keep the focus,
                    // another field (the search, say) gives it to the window.
                    if !this.composer_focus.is_focused(window) {
                        this.focus.focus(window, cx);
                    }
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                if let Some(selection) = &mut this.selection
                    && selection.dragging
                {
                    let place = Place { block, offset: index_at(&move_layout, event.position) };
                    if place != selection.head {
                        selection.head = place;
                        cx.notify();
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                    let place = Place { block, offset: index_at(&up_layout, event.position) };
                    // A click selected nothing: it began and ended here. `dragging` says
                    // nothing by now: the other blocks saw the button let go outside them
                    // first (their handler runs in the capture phase) and ended the drag.
                    let click = this.selection.as_ref().is_some_and(|selection| selection.anchor == place && selection.head == place);
                    log::debug!("text click at {:?}: click {click}, {} links", place, links.len());
                    let link = links.iter().find(|(range, _)| range.start <= place.offset && place.offset < range.end);
                    if click && let Some((_, url)) = link {
                        this.selection = None;
                        this.autoscroll = None;
                        cx.open_url(url);
                        cx.notify();
                        return;
                    }
                    this.end_selection(event.position, cx)
                }),
            )
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, event: &MouseUpEvent, _, cx| this.end_selection(event.position, cx)))
            .child(styled)
            .child(record)
            .into_any_element()
    }

    /// The place of the text nearest to `position` among the blocks drawn in the
    /// conversation: above or below it, the nearest line inside it.
    fn place_near(&self, position: Point<Pixels>) -> Option<Place> {
        let view = self.open.as_ref()?.list.viewport_bounds();
        if view.size.height <= px(2.) {
            return None;
        }
        let y = position.y.clamp(view.top() + px(1.), view.bottom() - px(1.));
        let drawn = self.drawn_text.borrow();
        let gap = |bounds: &Bounds<Pixels>| {
            if y < bounds.top() {
                bounds.top() - y
            } else if y >= bounds.bottom() {
                y - bounds.bottom()
            } else {
                px(0.)
            }
        };
        let (block, bounds, layout) = drawn
            .iter()
            .filter(|(_, bounds, _)| bounds.bottom() > view.top() && bounds.top() < view.bottom())
            .min_by(|a, b| gap(&a.1).partial_cmp(&gap(&b.1)).unwrap_or(std::cmp::Ordering::Equal))?;
        let inside = point(
            position.x.clamp(bounds.left(), bounds.right()),
            y.clamp(bounds.top(), (bounds.bottom() - px(1.)).max(bounds.top())),
        );
        Some(Place { block: *block, offset: index_at(layout, inside) })
    }

    /// A dragged selection over no block of text: its end goes to the nearest text, and at
    /// the top or bottom edge of the conversation, or past it, the conversation scrolls.
    pub(super) fn selection_drag_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let dragging = self.selection.as_ref().is_some_and(|selection| selection.dragging);
        if !dragging || event.pressed_button != Some(MouseButton::Left) {
            self.autoscroll = None;
            return;
        }
        let Some(open) = &self.open else { return };
        let view = open.list.viewport_bounds();
        let zone = px(AUTOSCROLL_ZONE);
        if event.position.y < view.top() + zone || event.position.y > view.bottom() - zone {
            self.autoscroll = Some(event.position);
            self.start_autoscroll(cx);
        } else {
            self.autoscroll = None;
        }
        let over_text = self.drawn_text.borrow().iter().any(|(_, bounds, _)| bounds.contains(&event.position));
        if !over_text
            && let Some(place) = self.place_near(event.position)
            && let Some(selection) = &mut self.selection
            && selection.head != place
        {
            selection.head = place;
            cx.notify();
        }
    }

    fn start_autoscroll(&mut self, cx: &mut Context<Self>) {
        if self.autoscrolling {
            return;
        }
        self.autoscrolling = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(AUTOSCROLL_TICK).await;
                let go = this.update(cx, |this, cx| this.autoscroll_tick(cx)).unwrap_or(false);
                if !go {
                    break;
                }
            }
        })
        .detach();
    }

    /// One step of the scroll of a selection dragged at the edge; false when it is over.
    fn autoscroll_tick(&mut self, cx: &mut Context<Self>) -> bool {
        let dragging = self.selection.as_ref().is_some_and(|selection| selection.dragging);
        let (Some(position), Some(open), true) = (self.autoscroll, &self.open, dragging) else {
            self.autoscroll = None;
            self.autoscrolling = false;
            return false;
        };
        let view = open.list.viewport_bounds();
        let zone = px(AUTOSCROLL_ZONE);
        let depth = if position.y < view.top() + zone {
            -((view.top() + zone - position.y) / zone)
        } else {
            (position.y - (view.bottom() - zone)) / zone
        };
        let step = depth.clamp(-3., 3.) * AUTOSCROLL_STEP;
        // Scrolling up leaves the end of a running turn by itself.
        open.list.scroll_by(px(step));
        if let Some(place) = self.place_near(position)
            && let Some(selection) = &mut self.selection
        {
            selection.head = place;
        }
        cx.notify();
        true
    }

    fn end_selection(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.autoscroll = None;
        let Some(selection) = &mut self.selection else { return };
        if !selection.dragging {
            return;
        }
        selection.dragging = false;
        // An empty selection stays as a caret without a bubble: a Shift+click, after a
        // scroll too, selects from it.
        if !self.selection_text().trim().is_empty()
            && let Some(selection) = &mut self.selection
        {
            selection.bubble = Some(position);
        }
        cx.notify();
    }

    /// Puts the selected text of the conversation on the clipboard. The selection stays.
    /// Returns whether there was text to copy.
    pub(super) fn copy_selection(&mut self, cx: &mut Context<Self>) -> bool {
        let selected = self.selection_text();
        if selected.trim().is_empty() {
            return false;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(selected));
        self.status = Some(t("code.copied").to_owned());
        cx.notify();
        true
    }

    /// The text of the selection, from the conversation itself: blocks scrolled out of
    /// view count too.
    pub(super) fn selection_text(&self) -> String {
        let Some(selection) = &self.selection else { return String::new() };
        let (start, end) = selection.ends();
        let Some(open) = &self.open else { return String::new() };
        let live = self.live_items();
        let items = open.items.iter().chain(live.iter().flat_map(|items| items.iter()));
        let mut blocks = Vec::new();
        for (ix, item) in items.enumerate() {
            if ix < start.block.0 / 1000 || ix > end.block.0 / 1000 {
                continue;
            }
            for (pi, piece) in item.pieces.iter().enumerate() {
                let crate::display::Piece::Markdown(markdown) = piece else { continue };
                for (bi, block) in markdown.iter().enumerate() {
                    if let Some(text) = super::block_text(block) {
                        blocks.push(((ix * 1000 + pi, bi), text));
                    }
                }
            }
        }
        selected_text(blocks.iter().map(|(block, text)| (*block, *text)), start, end)
    }

    /// The selected text as a quote at the end of the composer, which takes the focus.
    fn ask_about_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self.selection_text();
        if self.selection.take().is_none() {
            return;
        }
        let quote: String = selected.trim().lines().map(|line| format!("> {line}\n")).collect();
        let mut text = self.composer.text.trim_end().to_owned();
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&quote);
        text.push('\n');
        self.composer = TextField::from_text(text);
        self.composer_focus.focus(window, cx);
        cx.notify();
    }

    pub(super) fn render_selection_bubble(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = *cx.global::<Theme>();
        let at = self.selection.as_ref()?.bubble?;
        let button = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .flex()
                .items_center()
                .h(px(26.))
                .px(px(10.))
                .rounded(px(6.))
                .hover(|style| style.bg(theme.selection_hover))
                .child(label)
        };
        let bubble = div()
            .id("selection-bubble")
            .flex()
            .gap(px(2.))
            .p(px(3.))
            .rounded(px(8.))
            .bg(theme.popup_bg)
            .border_1()
            .border_color(theme.popup_border)
            .shadow(popup_shadow(&theme))
            .text_size(px(12.5))
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                if this.selection.as_ref().is_some_and(|s| !s.dragging) {
                    this.selection = None;
                    cx.notify();
                }
            }))
            .child(button("selection-ask", t("selection.ask")).on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.ask_about_selection(window, cx))))
            .child(button("selection-copy", t("code.copy")).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                let selected = this.selection_text();
                if this.selection.take().is_some() {
                    cx.write_to_clipboard(ClipboardItem::new_string(selected));
                    this.status = Some(t("code.copied").to_owned());
                }
                cx.notify();
            })));
        Some(deferred(anchored().position(at + point(px(-20.), px(12.))).snap_to_window_with_margin(px(8.)).child(bubble)).with_priority(2).into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use super::{Place, TextSelection, selected_text, with_selection};
    use ui_kit::{Hsla, HighlightStyle};

    #[test]
    fn a_selection_spans_blocks_and_messages_in_reading_order() {
        let blocks = [((0, 0), "First paragraph."), ((0, 1), "fn main() {}"), ((1000, 0), "Next message here.")];
        // Dragged backwards: from the next message up into the first paragraph.
        let selection = TextSelection {
            anchor: Place { block: (1000, 0), offset: 4 },
            head: Place { block: (0, 0), offset: 6 },
            dragging: false,
            bubble: None,
        };
        let (start, end) = selection.ends();
        assert_eq!(selected_text(blocks, start, end), "paragraph.\n\nfn main() {}\n\nNext");
        assert_eq!(selection.range_in((0, 0), 16), Some(6..16));
        assert_eq!(selection.range_in((0, 1), 12), Some(0..12), "a block in between is whole");
        assert_eq!(selection.range_in((1000, 0), 18), Some(0..4));
        assert_eq!(selection.range_in((2000, 0), 5), None);
        // Inside one block it is the plain range.
        let one = TextSelection { anchor: Place { block: (0, 1), offset: 3 }, head: Place { block: (0, 1), offset: 7 }, dragging: true, bubble: None };
        let (start, end) = one.ends();
        assert_eq!(selected_text(blocks, start, end), "main");
    }

    #[test]
    fn a_selection_cuts_the_styles_it_crosses() {
        let text = "fn main() — да";
        let red = HighlightStyle { color: Some(Hsla::red()), ..Default::default() };
        let pieces = with_selection(text, vec![(0..2, red), (3..7, red)], Some(1..5), Hsla::blue());
        let ranges: Vec<_> = pieces.iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(ranges, vec![0..1, 1..2, 2..3, 3..5, 5..7]);
        assert_eq!(pieces[1].1.color, Some(Hsla::red()), "its own colour stays");
        assert!(pieces[1].1.background_color.is_some() && pieces[2].1.background_color.is_some());
        assert!(pieces[4].1.background_color.is_none());
        // A selection that ends inside a letter is cut at the letter.
        let inside = text.len() - 1;
        let pieces = with_selection(text, Vec::new(), Some(10..inside), Hsla::blue());
        assert!(pieces.iter().all(|(r, _)| text.is_char_boundary(r.start) && text.is_char_boundary(r.end)));
    }
}
