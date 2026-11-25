//! The files of the session's folder in the right pane, read a folder at a time when it is
//! opened, and a preview of one file in the centre: text with line numbers, or a picture.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use envs::{FileMeta, ReadRange};
use model::EnvId;
use ui_kit::prelude::*;
use ui_kit::*;

use super::{MONO, Shell, composer};
use crate::highlight;
use crate::i18n::t;

/// Bytes of a file the preview reads at most.
const PREVIEW_BYTES: u64 = 2 << 20;
/// Bytes of a picture the preview reads at most: a picture shows only whole.
const PICTURE_BYTES: u64 = 32 << 20;
const FILE_ROW: f32 = 24.;
/// Characters of a line the preview and the diff show: one line of megabytes (minified
/// code, a data file) would take the window seconds to lay out at every frame.
const LONG_LINE: usize = 2_000;
const LINE_HEIGHT: f32 = 19.;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum RightTab {
    #[default]
    Session,
    Files,
    Changes,
}

#[derive(Default)]
pub(super) struct FilesState {
    root: Option<(EnvId, String)>,
    /// Listings of the folders read so far, by path.
    dirs: HashMap<String, Arc<Vec<FileMeta>>>,
    loading: HashSet<String>,
    expanded: HashSet<String>,
    errors: HashMap<String, String>,
    /// What `.gitignore` leaves out, in one spelling (see [`same_path`]); empty outside a
    /// repository.
    ignored: Arc<HashSet<String>>,
    pub scroll: UniformListScrollHandle,
}

/// A path in one spelling, so that the listing of a folder and the answer of git compare:
/// forward slashes, no trailing one, lower case on Windows.
fn same_path(path: &str) -> String {
    mcp::normalize(path)
}

impl FilesState {
    /// Whether the entry or a folder above it is ignored.
    fn is_ignored(&self, path: &str) -> bool {
        if self.ignored.is_empty() {
            return false;
        }
        let mut path = same_path(path);
        loop {
            if self.ignored.contains(&path) {
                return true;
            }
            match path.rfind('/') {
                Some(cut) if cut > 0 => path.truncate(cut),
                _ => return false,
            }
        }
    }
}

pub(super) enum PreviewBody {
    Loading,
    Text { lines: Arc<Vec<SharedString>>, spans: Option<Arc<Vec<Vec<(std::ops::Range<usize>, highlight::Kind)>>>>, truncated: bool },
    Picture(Arc<Image>, u32, u32),
    Binary(u64),
    Error(String),
}

pub(super) struct Preview {
    pub path: String,
    pub body: PreviewBody,
    pub scroll: UniformListScrollHandle,
}

/// The icon of a file by its name.
fn file_icon(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    match ext {
        "rs" => "fileTypes/rust",
        "py" => "fileTypes/python",
        "js" | "mjs" | "cjs" | "jsx" => "fileTypes/javascript",
        "ts" | "tsx" => "fileTypes/typescript",
        "json" | "jsonl" => "fileTypes/json",
        "toml" => "fileTypes/toml",
        "yml" | "yaml" => "fileTypes/yaml",
        "md" | "markdown" => "fileTypes/markdown",
        "html" | "htm" => "fileTypes/html",
        "css" | "scss" => "fileTypes/css",
        "java" => "fileTypes/java",
        "kt" | "kts" => "fileTypes/kotlin",
        "go" => "fileTypes/go",
        "c" | "h" => "fileTypes/c",
        "cpp" | "cc" | "hpp" => "fileTypes/cpp",
        "cs" => "fileTypes/csharp",
        "sh" | "bash" | "zsh" => "fileTypes/shell",
        "ps1" => "fileTypes/powershell",
        "sql" => "fileTypes/sql",
        "xml" | "svg" => "fileTypes/xml",
        "lua" => "fileTypes/lua",
        "rb" => "fileTypes/ruby",
        "php" => "fileTypes/php",
        "swift" => "fileTypes/swift",
        "ini" | "cfg" | "conf" => "fileTypes/ini",
        _ if lower == "dockerfile" => "fileTypes/dockerfile",
        _ if lower == "makefile" => "fileTypes/makefile",
        _ if lower == ".gitignore" => "fileTypes/gitignore",
        _ => "fileTypes/text",
    }
}

fn name_of(path: &str) -> &str {
    path.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or(path)
}

/// A line as the preview and the diff show it: tabs as spaces, and a line too long cut
/// with an ellipsis.
pub(super) fn shown_line(line: &str) -> String {
    let line = line.replace('\t', "    ");
    match line.char_indices().nth(LONG_LINE) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line,
    }
}

/// What a preview shows for the first bytes of a file of `size` bytes.
fn body_of(path: &str, bytes: Vec<u8>, size: u64) -> PreviewBody {
    if let Some(media) = live::image_type(path) {
        // A picture cut short decodes to nothing: an empty frame.
        if (bytes.len() as u64) < size {
            return PreviewBody::Error(t("files.picture_too_big").replace("{size}", &bytes_text(size)));
        }
        return match composer::picture_of(media, bytes) {
            Some((image, w, h)) => PreviewBody::Picture(image, w, h),
            None => PreviewBody::Error(t("files.bad_picture").to_owned()),
        };
    }
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return PreviewBody::Binary(size);
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<SharedString> = text.lines().map(|line| SharedString::from(shown_line(line))).collect();
    let spans = highlight::highlight_lines(path, &lines).map(Arc::new);
    PreviewBody::Text { lines: Arc::new(lines), spans, truncated: size > bytes.len() as u64 }
}

impl Shell {
    /// The folder the Files tab shows: the open session's, on its environment.
    fn files_root(&self) -> Option<(EnvId, String)> {
        let open = self.open.as_ref()?;
        Some((open.summary.key.env.clone(), open.summary.cwd.clone()?))
    }

    /// Asks git once what the folder's repository ignores, for the dim rows of the tree.
    fn load_ignored(&mut self, cx: &mut Context<Self>) {
        let Some(asked) = self.files.root.clone() else { return };
        let Some(env) = self.env_for(&asked.0) else { return };
        let root = asked.1.clone();
        let task = cx.background_spawn(async move { vcs::ignored(&*env, &root) });
        cx.spawn(async move |this, cx| {
            let Ok(list) = task.await else { return };
            this.update(cx, |this, cx| {
                // The answer of another folder, even of the same environment, is not this one's.
                if this.files.root.as_ref() != Some(&asked) {
                    return;
                }
                this.files.ignored = Arc::new(list.iter().map(|path| same_path(path)).collect());
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn load_dir(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(asked) = self.files.root.clone() else { return };
        let Some(env) = self.env_for(&asked.0) else { return };
        if !self.files.loading.insert(path.clone()) {
            return;
        }
        let dir = path.clone();
        let task = cx.background_spawn(async move { env.read_dir(&dir) });
        cx.spawn(async move |this, cx| {
            let listing = task.await;
            this.update(cx, |this, cx| {
                if this.files.root.as_ref() != Some(&asked) {
                    return;
                }
                this.files.loading.remove(&path);
                match listing {
                    Ok(mut entries) => {
                        entries.retain(|e| name_of(&e.path) != ".git");
                        this.files.dirs.insert(path, Arc::new(entries));
                    }
                    Err(error) => {
                        this.files.errors.insert(path, error.to_string());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The rows of the tree: each entry with its depth, the contents of open folders after them.
    fn file_rows(&self) -> Vec<(usize, FileMeta)> {
        fn walk(state: &FilesState, dir: &str, depth: usize, out: &mut Vec<(usize, FileMeta)>) {
            let Some(entries) = state.dirs.get(dir) else { return };
            for entry in entries.iter() {
                out.push((depth, entry.clone()));
                if entry.is_dir && state.expanded.contains(&entry.path) && out.len() < 20_000 {
                    walk(state, &entry.path, depth + 1, out);
                }
            }
        }
        let mut out = Vec::new();
        if let Some((_, root)) = &self.files.root {
            walk(&self.files, root, 0, &mut out);
        }
        out
    }

    fn click_file(&mut self, meta: &FileMeta, cx: &mut Context<Self>) {
        if meta.is_dir {
            if !self.files.expanded.remove(&meta.path) {
                self.files.expanded.insert(meta.path.clone());
                if !self.files.dirs.contains_key(&meta.path) {
                    self.load_dir(meta.path.clone(), cx);
                }
            }
            cx.notify();
        } else {
            self.open_preview(meta.path.clone(), cx);
        }
    }

    /// Shows a file of the open session's environment in the centre.
    pub(super) fn open_preview(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(open) = &self.open else { return };
        let Some(env) = self.env_for(&open.summary.key.env) else { return };
        self.diff_view = None;
        self.mission = None;
        self.preview = Some(Preview { path: path.clone(), body: PreviewBody::Loading, scroll: UniformListScrollHandle::new() });
        self.turn_view = None;
        self.spend_view = None;
        let file = path.clone();
        let task = cx.background_spawn(async move {
            let size = env.stat(&file).map(|meta| meta.size).unwrap_or(0);
            let picture = live::image_type(&file).is_some();
            if picture && size > PICTURE_BYTES {
                return Ok(body_of(&file, Vec::new(), size));
            }
            let limit = if picture { PICTURE_BYTES } else { PREVIEW_BYTES };
            env.read(&file, ReadRange::Head(limit)).map(|bytes| body_of(&file, bytes, size))
        });
        cx.spawn(async move |this, cx| {
            let body = task.await.unwrap_or_else(|error| PreviewBody::Error(error.to_string()));
            this.update(cx, |this, cx| {
                if let Some(preview) = &mut this.preview
                    && preview.path == path
                {
                    preview.body = body;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    pub(super) fn render_right_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = *cx.global::<Theme>();
        let tab = |id: &'static str, label: &'static str, which: RightTab, current: RightTab| {
            let active = which == current;
            div()
                .id(id)
                .flex()
                .items_center()
                .h(px(26.))
                .px(px(12.))
                .rounded(px(7.))
                .text_size(px(12.5))
                .when(active, |this| this.bg(theme.selection_inactive).text_color(theme.text))
                .when(!active, |this| this.text_color(theme.text_secondary).hover(|style| style.bg(theme.hover)))
                .child(label)
        };
        let current = self.right_tab;
        div()
            .flex()
            .items_center()
            .gap(px(4.))
            .pl(px(12.))
            .pr(px(8.))
            .pt(px(8.))
            .child(tab("tab-session", t("tab.session"), RightTab::Session, current).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.right_tab = RightTab::Session;
                    cx.notify();
                },
            )))
            .child(tab("tab-files", t("tab.files"), RightTab::Files, current).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.right_tab = RightTab::Files;
                    cx.notify();
                },
            )))
            .child(tab("tab-changes", t("tab.changes"), RightTab::Changes, current).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.right_tab = RightTab::Changes;
                    this.refresh_changes(cx);
                    cx.notify();
                },
            )))
            .child(div().flex_1())
            .child(self.hide_pane_button(super::Side::Right, cx))
    }

    pub(super) fn render_files(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.global::<Theme>();
        let root = self.files_root();
        if self.files.root != root {
            self.files = FilesState { root: root.clone(), scroll: self.files.scroll.clone(), ..Default::default() };
            if let Some((_, path)) = root.clone() {
                self.load_dir(path, cx);
                self.load_ignored(cx);
            }
        }
        let Some((_, root_path)) = root else {
            return div().p(px(16.)).text_color(theme.text_muted).child(t("live.no_folder")).into_any_element();
        };
        let rows = self.file_rows();
        let message = if let Some(error) = self.files.errors.get(&root_path) {
            Some(error.clone())
        } else if rows.is_empty() && self.files.loading.contains(&root_path) {
            Some(t("loading").to_owned())
        } else if rows.is_empty() {
            Some(t("files.empty").to_owned())
        } else {
            None
        };
        let selected = self.preview.as_ref().map(|p| p.path.clone());
        let rows = Arc::new(rows);
        let count = rows.len();
        let list = uniform_list(
            "files",
            count,
            cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                range
                    .filter_map(|ix| rows.get(ix).cloned().map(|row| (ix, row)))
                    .map(|(ix, (depth, meta))| {
                        let theme = *cx.global::<Theme>();
                        let name = name_of(&meta.path).to_owned();
                        let open = meta.is_dir && this.files.expanded.contains(&meta.path);
                        let hidden = name.starts_with('.') || this.files.is_ignored(&meta.path);
                        let active = selected.as_deref() == Some(meta.path.as_str());
                        let chevron = div().size(px(14.)).flex_none().when(meta.is_dir, |this| {
                            this.child(icon_sized(if open { "general/chevronDown" } else { "general/chevronRight" }, 14., &theme))
                        });
                        div()
                            .id(("file", ix))
                            .flex()
                            .items_center()
                            .gap(px(5.))
                            .h(px(FILE_ROW))
                            .pl(px(4. + depth as f32 * 14.))
                            .pr(px(6.))
                            .rounded(px(5.))
                            .text_size(px(12.5))
                            .when(hidden, |this| this.text_color(theme.text_muted))
                            .when(active, |this| this.bg(theme.selection_inactive))
                            .when(!active, |this| this.hover(|style| style.bg(theme.hover)))
                            .child(chevron)
                            .child(icon_sized(if meta.is_dir { "general/folder" } else { file_icon(&name) }, 14., &theme))
                            .child(div().flex_1().min_w_0().truncate().child(name))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.click_file(&meta, cx)))
                            .into_any_element()
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.files.scroll)
        .size_full();
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .px(px(8.))
            .pt(px(8.))
            .child(
                div()
                    .px(px(6.))
                    .pb(px(6.))
                    .text_size(px(11.))
                    .text_color(theme.text_muted)
                    .truncate()
                    .child(root_path),
            )
            .children(message.map(|text| div().px(px(6.)).text_size(px(12.)).text_color(theme.text_muted).child(text)))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(scrollable("files-scroll", &self.files.scroll, list, &theme, window, cx).size_full()),
            )
            .into_any_element()
    }

    /// The preview in the centre, in place of the conversation.
    pub(super) fn render_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = *cx.global::<Theme>();
        let preview = self.preview.as_ref()?;
        let name = name_of(&preview.path).to_owned();
        let header = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(48.))
            .px(px(16.))
            .border_b_1()
            .border_color(theme.border)
            .child(icon_sized(file_icon(&name), 16., &theme))
            .child(div().font_weight(FontWeight::SEMIBOLD).child(name))
            .child(div().flex_1().min_w_0().truncate().text_size(px(11.5)).text_color(theme.text_muted).child(preview.path.clone()))
            .child(
                div()
                    .id("close-preview")
                    .flex()
                    .items_center()
                    .h(px(26.))
                    .px(px(10.))
                    .rounded(px(6.))
                    .text_size(px(12.))
                    .text_color(theme.text_secondary)
                    .hover(|style| style.bg(theme.hover))
                    .child(t("files.back"))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.preview = None;
                        cx.notify();
                    })),
            );
        let note = |text: String| div().p(px(20.)).text_color(theme.text_muted).child(text).into_any_element();
        let body = match &preview.body {
            PreviewBody::Loading => note(t("loading").to_owned()),
            PreviewBody::Error(error) => note(format!("{}: {error}", t("error"))),
            PreviewBody::Binary(size) => note(format!("{} · {}", t("files.binary"), bytes_text(*size))),
            PreviewBody::Picture(image, w, h) => {
                let viewport = window.viewport_size();
                let (fw, fh) = composer::fit(*w, *h, f32::from(viewport.width) * 0.5, f32::from(viewport.height) * 0.75);
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(8.))
                    .child(img(image.clone()).w(px(fw)).h(px(fh)))
                    .child(div().text_size(px(11.5)).text_color(theme.text_muted).child(format!("{w} × {h}")))
                    .into_any_element()
            }
            PreviewBody::Text { lines, spans, truncated } => {
                let lines = lines.clone();
                let spans = spans.clone();
                let digits = lines.len().max(1).to_string().len() as f32;
                let count = lines.len();
                // The list is as wide as its longest line and scrolls sideways (Shift+wheel).
                let widest = lines.iter().enumerate().max_by_key(|(_, line)| line.chars().count()).map(|(ix, _)| ix);
                let list = uniform_list(
                    "preview-lines",
                    count,
                    cx.processor(move |_, range: std::ops::Range<usize>, _, cx| {
                        let theme = *cx.global::<Theme>();
                        range
                            .map(|ix| {
                                div()
                                    .flex()
                                    .h(px(LINE_HEIGHT))
                                    .child(
                                        div()
                                            .flex_none()
                                            .w(px(digits * 8. + 16.))
                                            .pr(px(12.))
                                            .flex()
                                            .justify_end()
                                            .text_color(theme.text_disabled)
                                            .child((ix + 1).to_string()),
                                    )
                                    .child(div().flex_none().pr(px(16.)).whitespace_nowrap().text_color(theme.code).child(match &spans {
                                        Some(spans) => super::code_text(lines[ix].clone(), &spans[ix], &theme).into_any_element(),
                                        None => lines[ix].clone().into_any_element(),
                                    }))
                                    .into_any_element()
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
                .with_width_from_item(widest)
                .track_scroll(&preview.scroll)
                .size_full();
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .font_family(MONO)
                    .text_size(px(12.5))
                    .line_height(px(LINE_HEIGHT))
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .py(px(8.))
                            .overflow_hidden()
                            .child(scrollable("preview-scroll", &preview.scroll, list, &theme, window, cx).size_full()),
                    )
                    .when(*truncated, |this| {
                        this.child(
                            div()
                                .px(px(16.))
                                .py(px(6.))
                                .border_t_1()
                                .border_color(theme.border)
                                .font_family("Inter")
                                .text_size(px(11.5))
                                .text_color(theme.text_muted)
                                .child(t("files.truncated")),
                        )
                    })
                    .into_any_element()
            }
        };
        Some(div().flex_1().min_h_0().flex().flex_col().child(header).child(body).into_any_element())
    }
}

fn bytes_text(size: u64) -> String {
    match size {
        s if s >= 1 << 20 => format!("{:.1} MB", s as f64 / (1 << 20) as f64),
        s if s >= 1 << 10 => format!("{:.1} KB", s as f64 / 1024.),
        s => format!("{s} B"),
    }
}

#[cfg(test)]
mod tests {
    use super::{LONG_LINE, PreviewBody, body_of, name_of, shown_line};

    #[test]
    fn a_long_line_is_cut() {
        assert_eq!(shown_line("a\tb"), "a    b");
        let long = "é".repeat(LONG_LINE + 5);
        let shown = shown_line(&long);
        assert_eq!(shown.chars().count(), LONG_LINE + 1);
        assert!(shown.ends_with('…'));
        assert!(matches!(body_of("min.js", long.into_bytes(), 0), PreviewBody::Text { ref lines, .. } if lines[0].chars().count() == LONG_LINE + 1));
    }

    #[test]
    fn a_picture_cut_short_says_so() {
        assert!(matches!(body_of("big.png", vec![0x89, b'P', b'N', b'G'], 40 << 20), PreviewBody::Error(_)));
    }

    #[test]
    fn previews_tell_text_from_binary() {
        assert!(matches!(body_of("a.rs", b"fn main() {}\n\tx\n".to_vec(), 16), PreviewBody::Text { ref lines, spans: Some(_), truncated: false } if lines.len() == 2 && lines[1] == "    x"));
        assert!(matches!(body_of("a.bin", vec![1, 0, 2], 3), PreviewBody::Binary(3)));
        assert!(matches!(body_of("big.txt", b"x".to_vec(), 100), PreviewBody::Text { truncated: true, .. }));
        assert_eq!(name_of("/w/src/lib.rs"), "lib.rs");
        assert_eq!(name_of("C:\\w\\src\\"), "src");
    }
}
