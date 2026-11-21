//! The frame of the window: Bowyer draws its own title bar. On Windows the system keeps the
//! border and answers for the areas the bar names (the empty space moves the window, the
//! three buttons minimize, maximize and close); on Linux the window asks the compositor
//! for client decorations and does all of it itself, the resize border included; on macOS
//! the traffic lights of the system stay over the left end of the bar.

use super::*;

/// The height of the title bar.
pub(super) const TITLEBAR_HEIGHT: f32 = 36.;
/// What the left end of the bar keeps for the traffic lights of a macOS window.
const TRAFFIC_LIGHTS_ROOM: f32 = 76.;
/// The border of a Linux window with client decorations that resizes it.
const RESIZE_BORDER: f32 = 6.;

/// The options of the main window: a transparent system title bar and, on Linux, client
/// decorations. `app_id` groups the window under the desktop entry `bowyer.desktop`.
pub fn window_options(bounds: Bounds<Pixels>) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            // The name in the taskbar and Alt+Tab; the bar draws its own.
            title: Some(APP_NAME.into()),
            appears_transparent: true,
            traffic_light_position: if cfg!(target_os = "macos") { Some(point(px(12.), px(11.))) } else { None },
        }),
        window_decorations: if cfg!(all(unix, not(target_os = "macos"))) { Some(WindowDecorations::Client) } else { None },
        app_id: Some("bowyer".into()),
        window_min_size: Some(size(px(900.), px(560.))),
        #[cfg(unix)]
        app_owns_titlebar_drag: true,
        ..Default::default()
    }
}

/// Whether the window draws its own minimize, maximize and close buttons: Windows always,
/// Linux when the compositor gave client decorations (a compositor that draws the frame
/// itself, as KDE may, has buttons of its own), macOS never.
fn own_buttons(window: &Window) -> bool {
    if cfg!(target_os = "macos") {
        return false;
    }
    cfg!(windows) || matches!(window.window_decorations(), Decorations::Client { .. })
}

impl Shell {
    /// The title bar: the mark and the name of the application, the title of the open
    /// session, the window buttons.
    pub(super) fn render_titlebar(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = *cx.global::<Theme>();
        let session = self.open.as_ref().map(|open| {
            self.placements.get(&open.summary.key).and_then(|p| p.title.clone()).unwrap_or_else(|| open.summary.display_title())
        });
        // The whole bar but the buttons moves the window, the title too: a long session
        // title leaves no empty space.
        let title = div()
            .id("titlebar-drag")
            .flex()
            .flex_1()
            .h_full()
            .items_center()
            .gap(px(8.))
            .min_w_0()
            .window_control_area(WindowControlArea::Drag)
            .pl(px(if cfg!(target_os = "macos") { TRAFFIC_LIGHTS_ROOM } else { 12. }))
            .child(logo(16.))
            .child(div().flex_none().font_weight(FontWeight::SEMIBOLD).child(APP_NAME))
            .children(session.map(|session| {
                div().min_w_0().truncate().text_color(theme.text_muted).child(format!("—  {session}"))
            }));
        // A second press maximizes the window or gives back its size. On Windows the system
        // does both from the area, on Linux and macOS the window asks.
        #[cfg(not(windows))]
        let title = title.on_mouse_down(MouseButton::Left, |event: &MouseDownEvent, window: &mut Window, cx: &mut App| {
            cx.stop_propagation();
            if event.click_count >= 2 {
                #[cfg(target_os = "macos")]
                window.titlebar_double_click();
                #[cfg(not(target_os = "macos"))]
                window.zoom_window();
            } else {
                window.start_window_move();
            }
        });
        let buttons = own_buttons(window).then(|| {
            let maximized = window.is_maximized();
            div()
                .flex()
                .flex_none()
                .h_full()
                .child(
                    window_button("w-min", "windows/minimizeSmall", t("window.minimize"), WindowControlArea::Min, false, &theme)
                        .h(px(TITLEBAR_HEIGHT))
                        .on_click(|_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            cx.stop_propagation();
                            window.minimize_window();
                        }),
                )
                .child(
                    window_button(
                        "w-max",
                        if maximized { "windows/restoreSmall" } else { "windows/maximizeSmall" },
                        if maximized { t("window.restore") } else { t("window.maximize") },
                        WindowControlArea::Max,
                        false,
                        &theme,
                    )
                    .h(px(TITLEBAR_HEIGHT))
                    .on_click(|_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        cx.stop_propagation();
                        window.zoom_window();
                    }),
                )
                .child(
                    window_button("w-close", "windows/closeSmall", t("window.close"), WindowControlArea::Close, true, &theme)
                        .h(px(TITLEBAR_HEIGHT))
                        .on_click(|_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            cx.stop_propagation();
                            crate::tray::close_window(window);
                        }),
                )
        });
        div()
            .id("titlebar")
            .flex()
            .flex_none()
            .items_center()
            .h(px(TITLEBAR_HEIGHT))
            .w_full()
            .text_size(px(12.5))
            .child(title)
            .children(buttons)
    }

    /// The resize border of a Linux window with client decorations: one area over the
    /// window takes a press on the border, and an element on each side and corner gives
    /// the cursor. Nothing for a window the system frames or a maximized one.
    pub(super) fn render_resize_border(&self, window: &Window) -> Vec<AnyElement> {
        if cfg!(windows) || cfg!(target_os = "macos") {
            return Vec::new();
        }
        let Decorations::Client { .. } = window.window_decorations() else { return Vec::new() };
        if window.is_maximized() || window.is_fullscreen() {
            return Vec::new();
        }
        let size = window.viewport_size();
        let border = px(RESIZE_BORDER);
        if size.width <= border * 2. || size.height <= border * 2. {
            return Vec::new();
        }
        let press = div()
            .id("window-resize")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .on_mouse_down(MouseButton::Left, |event: &MouseDownEvent, window: &mut Window, cx: &mut App| {
                if let Some(edge) = resize_edge_at(event.position, window.viewport_size()) {
                    cx.stop_propagation();
                    window.start_window_resize(edge);
                }
            })
            .into_any_element();
        let (inner_w, inner_h) = (size.width - border * 2., size.height - border * 2.);
        let (right, bottom) = (size.width - border, size.height - border);
        let zero = px(0.);
        let sides = [
            ("resize-top", border, zero, inner_w, border, CursorStyle::ResizeUp),
            ("resize-bottom", border, bottom, inner_w, border, CursorStyle::ResizeDown),
            ("resize-left", zero, border, border, inner_h, CursorStyle::ResizeLeft),
            ("resize-right", right, border, border, inner_h, CursorStyle::ResizeRight),
            ("resize-top-left", zero, zero, border, border, CursorStyle::ResizeUpLeftDownRight),
            ("resize-top-right", right, zero, border, border, CursorStyle::ResizeUpRightDownLeft),
            ("resize-bottom-left", zero, bottom, border, border, CursorStyle::ResizeUpRightDownLeft),
            ("resize-bottom-right", right, bottom, border, border, CursorStyle::ResizeUpLeftDownRight),
        ];
        std::iter::once(press)
            .chain(sides.into_iter().map(|(id, x, y, w, h, cursor)| {
                div().id(id).absolute().left(x).top(y).w(w).h(h).cursor(cursor).into_any_element()
            }))
            .collect()
    }
}

/// The edge of the window a press at `position` resizes, if it is on the border.
fn resize_edge_at(position: Point<Pixels>, size: Size<Pixels>) -> Option<ResizeEdge> {
    let border = px(RESIZE_BORDER);
    let left = position.x < border;
    let right = position.x >= size.width - border;
    let top = position.y < border;
    let bottom = position.y >= size.height - border;
    match (top, bottom, left, right) {
        (true, _, true, _) => Some(ResizeEdge::TopLeft),
        (true, _, _, true) => Some(ResizeEdge::TopRight),
        (true, _, _, _) => Some(ResizeEdge::Top),
        (_, true, true, _) => Some(ResizeEdge::BottomLeft),
        (_, true, _, true) => Some(ResizeEdge::BottomRight),
        (_, true, _, _) => Some(ResizeEdge::Bottom),
        (_, _, true, _) => Some(ResizeEdge::Left),
        (_, _, _, true) => Some(ResizeEdge::Right),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::resize_edge_at;
    use ui_kit::{ResizeEdge, point, px, size};

    #[test]
    fn the_border_names_its_edge() {
        let window = size(px(800.), px(600.));
        assert_eq!(resize_edge_at(point(px(2.), px(2.)), window), Some(ResizeEdge::TopLeft));
        assert_eq!(resize_edge_at(point(px(400.), px(1.)), window), Some(ResizeEdge::Top));
        assert_eq!(resize_edge_at(point(px(799.), px(300.)), window), Some(ResizeEdge::Right));
        assert_eq!(resize_edge_at(point(px(797.), px(598.)), window), Some(ResizeEdge::BottomRight));
        assert_eq!(resize_edge_at(point(px(400.), px(300.)), window), None);
    }
}
