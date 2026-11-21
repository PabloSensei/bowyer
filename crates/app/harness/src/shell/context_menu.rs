//! The menu of a row of the tree, on a right click: what a folder or a session can do
//! besides being clicked and dragged. Deleting a folder asks for a second click.

use library::TreeNodeKind;
use ui_kit::prelude::*;
use ui_kit::*;

use super::{RenameTarget, Row, Shell};
use crate::i18n::t;

pub(super) struct ContextMenu {
    position: Point<Pixels>,
    row: Row,
    /// "Delete folder" was clicked once and waits for the second click.
    confirm_delete: bool,
}

/// What a row of the menu does.
#[derive(Clone, Copy)]
enum MenuAction {
    Rename,
    NewSubfolder,
    DeleteFolder,
    TogglePin,
    ResetName,
    MoveOut,
    CopyId,
}

impl Shell {
    pub(super) fn open_context_menu(&mut self, row: Row, position: Point<Pixels>, cx: &mut Context<Self>) {
        if matches!(row.kind, TreeNodeKind::Group(_) | TreeNodeKind::More(..)) {
            return;
        }
        self.context_menu = Some(ContextMenu { position, row, confirm_delete: false });
        cx.notify();
    }

    fn menu_action(&mut self, action: MenuAction, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = &mut self.context_menu else { return };
        let row = menu.row.clone();
        if matches!(action, MenuAction::DeleteFolder) && !menu.confirm_delete {
            menu.confirm_delete = true;
            cx.notify();
            return;
        }
        self.context_menu = None;
        match (action, &row.kind) {
            (MenuAction::Rename, _) => self.start_rename(&row, window, cx),
            (MenuAction::NewSubfolder, TreeNodeKind::Folder(parent)) => {
                let created = self.lib().create_folder(Some(*parent), t("new_folder"));
                if let Ok(id) = created {
                    let _ = self.lib().set_folder_collapsed(*parent, false);
                    self.reload(cx);
                    let mut field = TextField::from_text(t("new_folder"));
                    field.select_all();
                    self.renaming = Some((RenameTarget::Folder(id), field));
                    self.rename_focus.focus(window, cx);
                }
            }
            (MenuAction::DeleteFolder, TreeNodeKind::Folder(id)) => {
                let _ = self.lib().delete_folder(*id);
                self.status = Some(format!("{}: {}", t("menu.folder_deleted"), row.label));
                self.reload(cx);
            }
            (MenuAction::TogglePin, TreeNodeKind::Session(key)) => self.toggle_pin(key, cx),
            (MenuAction::ResetName, TreeNodeKind::Session(key)) => {
                let _ = self.lib().rename_session(key, None);
                self.reload(cx);
            }
            (MenuAction::MoveOut, TreeNodeKind::Session(key)) => {
                let _ = self.lib().move_session(key, None, 0.);
                self.reload(cx);
            }
            (MenuAction::CopyId, TreeNodeKind::Session(key)) => {
                cx.write_to_clipboard(ClipboardItem::new_string(key.id.clone()));
                self.status = Some(format!("{}: {}", t("code.copied"), key.id));
            }
            _ => {}
        }
        cx.notify();
    }

    pub(super) fn render_context_menu(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = *cx.global::<Theme>();
        let menu = self.context_menu.as_ref()?;
        let mut entries: Vec<(MenuAction, &'static str, bool)> = Vec::new();
        match &menu.row.kind {
            TreeNodeKind::Folder(_) => {
                entries.push((MenuAction::Rename, t("menu.rename"), false));
                entries.push((MenuAction::NewSubfolder, t("menu.new_subfolder"), false));
                let label = if menu.confirm_delete { t("menu.confirm_delete") } else { t("menu.delete_folder") };
                entries.push((MenuAction::DeleteFolder, label, true));
            }
            TreeNodeKind::Session(key) => {
                let placement = self.placements.get(key).cloned().unwrap_or_default();
                entries.push((MenuAction::TogglePin, if placement.pinned { t("menu.unpin") } else { t("menu.pin") }, false));
                entries.push((MenuAction::Rename, t("menu.rename"), false));
                if placement.title.is_some() {
                    entries.push((MenuAction::ResetName, t("menu.reset_name"), false));
                }
                if placement.folder.is_some() {
                    entries.push((MenuAction::MoveOut, t("menu.move_out"), false));
                }
                entries.push((MenuAction::CopyId, t("menu.copy_id"), false));
            }
            TreeNodeKind::Group(_) | TreeNodeKind::More(..) => return None,
        }
        let confirm = menu.confirm_delete;
        let list = div()
            .id("context-menu")
            .min_w(px(190.))
            .p(px(4.))
            .rounded(px(9.))
            .bg(theme.popup_bg)
            .border_1()
            .border_color(theme.popup_border)
            .shadow(popup_shadow(&theme))
            .text_size(px(12.5))
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                this.context_menu = None;
                cx.notify();
            }))
            .children(entries.into_iter().enumerate().map(|(ix, (action, label, danger))| {
                div()
                    .id(("context-entry", ix))
                    .flex()
                    .items_center()
                    .h(px(28.))
                    .px(px(10.))
                    .rounded(px(6.))
                    .when(danger && confirm, |this| this.bg(theme.error).text_color(theme.over_accent))
                    .when(danger && !confirm, |this| this.text_color(theme.error))
                    .when(!(danger && confirm), |this| this.hover(|style| style.bg(theme.selection_hover)))
                    .child(label)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.menu_action(action, window, cx)))
            }));
        Some(deferred(anchored().position(menu.position).snap_to_window_with_margin(px(8.)).child(list)).with_priority(2).into_any_element())
    }
}
