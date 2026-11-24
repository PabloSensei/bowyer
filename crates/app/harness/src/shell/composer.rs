//! The composer's extras: pictures and files sent with a prompt (pasted, dropped or picked
//! with the "+" button), commands after `/`, files of the session after `@`, and the
//! picture viewer.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use envs::SharedEnv;
use model::{EnvId, HarnessKind};
use ui_kit::prelude::*;
use ui_kit::*;

use super::{Shell, now_ms};
use crate::i18n::t;

/// Pictures larger than this go as files, not inline.
const MAX_PICTURE_BYTES: u64 = 20 << 20;
/// Suggestions shown at once.
const SHOWN: usize = 8;

/// A file waiting to go with the next prompt.
#[derive(Clone)]
pub(super) struct Attached {
    pub local: PathBuf,
    pub name: String,
    /// Media type, bytes and the decoded picture, for pictures.
    pub picture: Option<(String, Arc<Vec<u8>>, Arc<Image>)>,
}

/// Commands of the window itself; any other `/word` goes to the harness as typed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Command {
    New,
    Model,
    Permission,
    Attach,
    Stop,
    Terminal,
    Resume,
    Compact,
}

impl Command {
    const ALL: [Command; 8] = [
        Command::New,
        Command::Model,
        Command::Permission,
        Command::Attach,
        Command::Stop,
        Command::Terminal,
        Command::Resume,
        Command::Compact,
    ];

    fn name(self) -> &'static str {
        match self {
            Command::New => "new",
            Command::Model => "model",
            Command::Permission => "permission",
            Command::Attach => "attach",
            Command::Stop => "stop",
            Command::Terminal => "terminal",
            Command::Resume => "resume",
            Command::Compact => "compact",
        }
    }

    /// The command a typed line names, with `/clear` as another name of `/new`.
    pub(super) fn parse(line: &str) -> Option<Command> {
        let word = line.trim().strip_prefix('/')?;
        if word.contains(char::is_whitespace) {
            return None;
        }
        if word == "clear" {
            return Some(Command::New);
        }
        Command::ALL.into_iter().find(|command| command.name() == word)
    }

    fn applies(self, harness: &HarnessKind) -> bool {
        match self {
            Command::Compact => *harness == HarnessKind::Claude,
            _ => true,
        }
    }
}

#[derive(Clone)]
pub(super) enum Choice {
    Run(Command),
    Insert(String),
}

pub(super) struct Suggestion {
    label: String,
    detail: String,
    choice: Choice,
}

pub(super) struct Completion {
    /// Where the word being completed starts in the text.
    start: usize,
    items: Vec<Suggestion>,
    selected: usize,
    /// The list of files is still being read.
    loading: bool,
}

/// What is listed once per folder: its files for `@`, Claude's own commands for `/`.
#[derive(Default)]
pub(super) struct FolderIndex {
    key: Option<(EnvId, String)>,
    files: Option<Arc<Vec<String>>>,
    commands: Option<Arc<Vec<(String, String)>>>,
    loading_files: bool,
    loading_commands: bool,
}

/// The `@` the caret is completing: at the start or after a space, with no space since.
fn mention_start(before: &str) -> Option<usize> {
    let at = before.rfind('@')?;
    let word = &before[at + 1..];
    let starts_word = before[..at].chars().next_back().is_none_or(char::is_whitespace);
    (starts_word && !word.contains(char::is_whitespace)).then_some(at)
}

/// What a suggestion list made for the word at `start` replaces now: from there to the
/// caret, if the caret is still in that word. A click, or text put in from elsewhere, moves
/// the caret without the list knowing.
fn completion_range(text: &str, caret: usize, start: usize) -> Option<std::ops::Range<usize>> {
    let caret = caret.min(text.len());
    let before = text.get(..caret)?;
    let command = start == 0 && before.strip_prefix('/').is_some_and(|query| !query.contains(char::is_whitespace));
    (command || mention_start(before) == Some(start)).then_some(start..caret)
}

/// How well a path answers a query: lower is better; `None` when it does not.
fn file_score(path: &str, query: &str) -> Option<usize> {
    if query.is_empty() {
        return Some(path.len());
    }
    let lower = path.to_lowercase();
    let name = lower.rsplit(['/', '\\']).next().unwrap_or(&lower);
    let rank = if name.starts_with(query) {
        0
    } else if name.contains(query) {
        1
    } else if lower.contains(query) {
        2
    } else {
        let mut chars = lower.chars();
        if !query.chars().all(|q| chars.any(|c| c == q)) {
            return None;
        }
        3
    };
    Some(rank * 10_000 + path.len())
}

/// The files of a folder, as paths relative to it: what git tracks and would track, else a
/// shallow walk without hidden folders.
fn list_files(env: &SharedEnv, cwd: &str) -> Vec<String> {
    const LIMIT: usize = 50_000;
    if let Ok(out) = env.run("git", &["ls-files", "--cached", "--others", "--exclude-standard"], Some(cwd)) {
        let files: Vec<String> = out.lines().filter(|l| !l.is_empty()).take(LIMIT).map(str::to_owned).collect();
        if !files.is_empty() {
            return files;
        }
    }
    if env.id().is_local() {
        let mut out = Vec::new();
        walk(Path::new(cwd), "", 5, &mut out);
        return out;
    }
    let args = ["." , "-maxdepth", "5", "-type", "f", "-not", "-path", "*/.*", "-not", "-path", "*/node_modules/*"];
    env.run("find", &args, Some(cwd))
        .map(|out| out.lines().take(LIMIT).map(|l| l.trim_start_matches("./").to_owned()).collect())
        .unwrap_or_default()
}

fn walk(dir: &Path, prefix: &str, depth: u32, out: &mut Vec<String>) {
    if depth == 0 || out.len() >= 20_000 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name == "node_modules" || name == "target" {
            continue;
        }
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => walk(&entry.path(), &path, depth - 1, out),
            Ok(_) => out.push(path),
            Err(_) => {}
        }
    }
}

/// Claude's own commands and skills, of the user and of the project: `(name, where from)`.
fn list_commands(env: &SharedEnv, cwd: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let home = env.home().ok();
    let mut roots = Vec::new();
    if let Some(home) = &home {
        roots.push((env.join(home, ".claude"), ("cmd.user_command", "cmd.user_skill")));
    }
    roots.push((env.join(cwd, ".claude"), ("cmd.project_command", "cmd.project_skill")));
    for (root, (command_label, skill_label)) in roots {
        let commands = env.join(&root, "commands");
        for meta in env.find_files(&commands, ".md", 4).unwrap_or_default() {
            let relative = meta.path.strip_prefix(commands.as_str()).unwrap_or(&meta.path);
            let name = relative.trim_start_matches(['/', '\\']).trim_end_matches(".md").replace(['/', '\\'], ":");
            if !name.is_empty() {
                out.push((name, t(command_label).to_owned()));
            }
        }
        let skills = env.join(&root, "skills");
        for meta in env.find_files(&skills, "SKILL.md", 2).unwrap_or_default() {
            let folder = meta.path.trim_end_matches("SKILL.md").trim_end_matches(['/', '\\']);
            let name = folder.rsplit(['/', '\\']).next().unwrap_or("").to_owned();
            if !name.is_empty() {
                out.push((name, t(skill_label).to_owned()));
            }
        }
    }
    out.sort();
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

/// A picture of the clipboard in a format the models read: PNG, JPEG, WebP and GIF as they
/// are, anything else (a Windows bitmap) converted to PNG.
fn picture_file(image: &Image) -> Option<(Vec<u8>, &'static str)> {
    Some(match image.format() {
        ImageFormat::Png => (image.bytes().to_vec(), "png"),
        ImageFormat::Jpeg => (image.bytes().to_vec(), "jpg"),
        ImageFormat::Webp => (image.bytes().to_vec(), "webp"),
        ImageFormat::Gif => (image.bytes().to_vec(), "gif"),
        _ => {
            let decoded = image::load_from_memory(image.bytes()).ok()?;
            let mut png = Vec::new();
            decoded.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
            (png, "png")
        }
    })
}

/// The format of a picture by its media type, for drawing it.
pub(crate) fn format_of(media_type: &str) -> ImageFormat {
    match media_type {
        "image/jpeg" | "image/jpg" => ImageFormat::Jpeg,
        "image/gif" => ImageFormat::Gif,
        "image/webp" => ImageFormat::Webp,
        "image/bmp" => ImageFormat::Bmp,
        "image/svg+xml" => ImageFormat::Svg,
        _ => ImageFormat::Png,
    }
}

type Picture = (Arc<Image>, u32, u32);

thread_local! {
    static PICTURES: std::cell::RefCell<std::collections::HashMap<(usize, u64), Picture>> = Default::default();
}

/// A picture of a history in base64, decoded once: the key is its length and a hash of its
/// ends, which is cheap for screenshots of many megabytes.
pub(crate) fn picture_of_base64(media_type: &str, data: &str) -> Option<Picture> {
    use base64::Engine as _;
    use std::hash::{Hash, Hasher};
    let data = data.trim();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    data.as_bytes()[..data.len().min(256)].hash(&mut hasher);
    data.as_bytes()[data.len().saturating_sub(256)..].hash(&mut hasher);
    let key = (data.len(), hasher.finish());
    if let Some(found) = PICTURES.with(|cache| cache.borrow().get(&key).cloned()) {
        return Some(found);
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(data).ok()?;
    let picture = picture_of(media_type, bytes)?;
    PICTURES.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= 128 {
            cache.clear();
        }
        cache.insert(key, picture.clone());
    });
    Some(picture)
}

/// A picture ready to draw, with its size in pixels.
pub(crate) fn picture_of(media_type: &str, bytes: Vec<u8>) -> Option<Picture> {
    let (width, height) = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    Some((Arc::new(Image::from_bytes(format_of(media_type), bytes)), width, height))
}

/// The size a picture of `width`×`height` takes inside a box, never larger than it is.
pub(crate) fn fit(width: u32, height: u32, max_width: f32, max_height: f32) -> (f32, f32) {
    let (w, h) = (width.max(1) as f32, height.max(1) as f32);
    let scale = (max_width / w).min(max_height / h).min(1.0);
    (w * scale, h * scale)
}

/// The size of an attached picture in the composer: low, so it sits above the text.
const THUMB_WIDTH: f32 = 64.;
const THUMB_HEIGHT: f32 = 40.;

impl Shell {
    pub(super) fn attach_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        for path in paths {
            if path.is_dir() || self.attachments.iter().any(|a| a.local == path) {
                continue;
            }
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let small = std::fs::metadata(&path).is_ok_and(|meta| meta.len() <= MAX_PICTURE_BYTES);
            let picture = live::image_type(&name).filter(|_| small).and_then(|media| {
                let bytes = std::fs::read(&path).ok()?;
                let image = Arc::new(Image::from_bytes(format_of(media), bytes.clone()));
                Some((media.to_owned(), Arc::new(bytes), image))
            });
            self.attachments.push(Attached { local: path, name, picture });
        }
        cx.notify();
    }

    pub(super) fn pick_files(&mut self, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = chosen.await {
                this.update(cx, |this, cx| this.attach_paths(paths, cx)).ok();
            }
        })
        .detach();
    }

    /// Ctrl+V with a picture or copied files: they become attachments. Returns whether the
    /// clipboard held any, so plain text still pastes as text.
    pub(super) fn paste_attachments(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(item) = cx.read_from_clipboard() else { return false };
        let mut paths = Vec::new();
        for entry in item.entries() {
            match entry {
                ClipboardEntry::Image(image) => {
                    if let Some(path) = self.save_picture(image) {
                        paths.push(path);
                    }
                }
                ClipboardEntry::ExternalPaths(list) => paths.extend(list.paths().iter().cloned()),
                _ => {}
            }
        }
        if paths.is_empty() {
            return false;
        }
        self.attach_paths(paths, cx);
        true
    }

    fn save_picture(&self, image: &Image) -> Option<PathBuf> {
        let (bytes, ext) = picture_file(image)?;
        let dir = self.cache_dir.join("attachments");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join(format!("pasted-{}.{ext}", now_ms()));
        std::fs::write(&path, bytes).ok()?;
        Some(path)
    }

    /// Reads the files and commands of the open session's folder once, off the window thread.
    fn ensure_index(&mut self, want_commands: bool, cx: &mut Context<Self>) {
        let Some(open) = &self.open else { return };
        let Some(cwd) = open.summary.cwd.clone() else { return };
        let key = (open.summary.key.env.clone(), cwd.clone());
        if self.folder_index.key.as_ref() != Some(&key) {
            self.folder_index = FolderIndex { key: Some(key.clone()), ..Default::default() };
        }
        let Some(env) = self.env_for(&key.0) else { return };
        if !want_commands && self.folder_index.files.is_none() && !self.folder_index.loading_files {
            self.folder_index.loading_files = true;
            let (env, cwd, key) = (env.clone(), cwd.clone(), key.clone());
            let task = cx.background_spawn(async move { list_files(&env, &cwd) });
            cx.spawn(async move |this, cx| {
                let files = task.await;
                this.update(cx, |this, cx| {
                    if this.folder_index.key.as_ref() == Some(&key) {
                        this.folder_index.files = Some(Arc::new(files));
                        this.folder_index.loading_files = false;
                        this.update_completion(cx);
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
        if want_commands && self.folder_index.commands.is_none() && !self.folder_index.loading_commands {
            self.folder_index.loading_commands = true;
            let task = cx.background_spawn(async move { list_commands(&env, &cwd) });
            cx.spawn(async move |this, cx| {
                let commands = task.await;
                this.update(cx, |this, cx| {
                    if this.folder_index.key.as_ref() == Some(&key) {
                        this.folder_index.commands = Some(Arc::new(commands));
                        this.folder_index.loading_commands = false;
                        this.update_completion(cx);
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    /// Looks at the word before the caret after every edit: `/command` at the start of the
    /// prompt, `@file` anywhere.
    pub(super) fn update_completion(&mut self, cx: &mut Context<Self>) {
        let caret = self.composer.caret().min(self.composer.text.len());
        let before = self.composer.text[..caret].to_owned();
        if self.completion_dismissed.as_deref() == Some(before.as_str()) {
            self.completion = None;
            return;
        }
        self.completion_dismissed = None;
        let harness = self.open.as_ref().map(|open| open.summary.key.harness.clone());
        let Some(harness) = harness else {
            self.completion = None;
            return;
        };
        if let Some(query) = before.strip_prefix('/').filter(|q| !q.contains(char::is_whitespace)) {
            let query = query.to_lowercase();
            let mut items: Vec<Suggestion> = Command::ALL
                .into_iter()
                .filter(|command| command.applies(&harness) && command.name().starts_with(&query))
                .map(|command| Suggestion {
                    label: format!("/{}", command.name()),
                    detail: t(&format!("cmd.{}", command.name())).to_owned(),
                    choice: Choice::Run(command),
                })
                .collect();
            if harness == HarnessKind::Claude {
                self.ensure_index(true, cx);
                if let Some(commands) = &self.folder_index.commands {
                    items.extend(commands.iter().filter(|(name, _)| name.to_lowercase().contains(&query)).map(
                        |(name, from)| Suggestion {
                            label: format!("/{name}"),
                            detail: from.clone(),
                            choice: Choice::Insert(format!("/{name} ")),
                        },
                    ));
                }
            }
            items.truncate(SHOWN * 4);
            self.completion = (!items.is_empty()).then_some(Completion { start: 0, items, selected: 0, loading: false });
        } else if let Some(at) = mention_start(&before) {
            let query = before[at + 1..].to_lowercase();
            self.ensure_index(false, cx);
            let loading = self.folder_index.files.is_none();
            let mut scored: Vec<(usize, &String)> = self
                .folder_index
                .files
                .iter()
                .flat_map(|files| files.iter())
                .filter_map(|path| file_score(path, &query).map(|score| (score, path)))
                .collect();
            scored.sort();
            let items: Vec<Suggestion> = scored
                .into_iter()
                .take(SHOWN * 4)
                .map(|(_, path)| {
                    let name = path.rsplit(['/', '\\']).next().unwrap_or(path).to_owned();
                    let mention = if path.contains(' ') { format!("@\"{path}\" ") } else { format!("@{path} ") };
                    Suggestion { label: name, detail: path.clone(), choice: Choice::Insert(mention) }
                })
                .collect();
            let selected = self.completion.as_ref().map_or(0, |c| c.selected).min(items.len().saturating_sub(1));
            self.completion = (loading || !items.is_empty()).then_some(Completion { start: at, items, selected, loading });
        } else {
            self.completion = None;
        }
    }

    /// Keys of the open suggestion list. Returns whether the list took the key.
    pub(super) fn completion_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(completion) = &mut self.completion else { return false };
        let stroke = &event.keystroke;
        let count = completion.items.len();
        match stroke.key.as_str() {
            "escape" => {
                let caret = self.composer.caret().min(self.composer.text.len());
                self.completion_dismissed = Some(self.composer.text[..caret].to_owned());
                self.completion = None;
            }
            "up" if count > 0 => completion.selected = (completion.selected + count - 1) % count,
            "down" if count > 0 => completion.selected = (completion.selected + 1) % count,
            "tab" | "enter" if count > 0 && !stroke.modifiers.shift => self.accept_completion(window, cx),
            _ => return false,
        }
        cx.notify();
        true
    }

    fn accept_completion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(completion) = self.completion.take() else { return };
        let Some(item) = completion.items.get(completion.selected) else { return };
        let Some(range) = completion_range(&self.composer.text, self.composer.caret(), completion.start) else {
            // The caret left the word: the list no longer answers it.
            self.update_completion(cx);
            cx.notify();
            return;
        };
        match item.choice.clone() {
            Choice::Run(command) => {
                self.composer = TextField::from_text("");
                self.run_command(command, window, cx);
            }
            Choice::Insert(text) => {
                let mut line = self.composer.text.clone();
                line.replace_range(range.clone(), &text);
                self.composer = TextField::from_text(line);
                self.composer.set_caret(range.start + text.len());
            }
        }
        cx.notify();
    }

    pub(super) fn run_command(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = &self.open else { return };
        let summary = open.summary.clone();
        let env = self.env_for(&summary.key.env);
        match command {
            Command::New => self.start_draft(summary.key.harness.clone(), window, cx),
            Command::Model => {
                self.cycle_model(&summary.key.harness, cx);
                let model = self.model_choice(&summary.key.harness).unwrap_or_else(|| t("model.default").to_owned());
                self.status = Some(format!("{}: {model}", t("cmd.model")));
            }
            Command::Permission => {
                self.cycle_permission(cx);
                self.status = Some(t(&format!("permission.{}", self.permission.id())).to_owned());
            }
            Command::Attach => self.pick_files(cx),
            Command::Stop => self.stop_live(cx),
            Command::Terminal => {
                let _ = (&env, &summary);
                self.open_shell_terminal(window, cx);
            }
            Command::Resume => {
                let _ = (&env, &summary);
                self.resume_in_terminal(window, cx);
            }
            Command::Compact => {
                let _ = (&env, &summary);
                self.compact(cx);
            }
        }
        cx.notify();
    }

    pub(super) fn render_attachments(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.attachments.is_empty() {
            return None;
        }
        let theme = *theme;
        Some(
            div()
                .flex()
                .flex_none()
                .flex_wrap()
                .gap(px(6.))
                .children(self.attachments.iter().enumerate().map(|(ix, attached)| {
                    let remove = div()
                        .id(("unattach", ix))
                        .absolute()
                        .top(px(-5.))
                        .right(px(-5.))
                        .size(px(16.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .bg(theme.popup_bg)
                        .border_1()
                        .border_color(theme.border)
                        .text_size(px(10.))
                        .text_color(theme.text_muted)
                        .child("✕")
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            if ix < this.attachments.len() {
                                this.attachments.remove(ix);
                            }
                            cx.stop_propagation();
                            cx.notify();
                        }));
                    match &attached.picture {
                        Some((_, bytes, image)) => {
                            let image = image.clone();
                            let shown = image.clone();
                            let bytes = bytes.clone();
                            let media = attached.picture.as_ref().map(|p| p.0.clone()).unwrap_or_default();
                            div()
                                .id(("attached", ix))
                                .relative()
                                .flex_none()
                                .w(px(THUMB_WIDTH))
                                .h(px(THUMB_HEIGHT))
                                .rounded(px(8.))
                                .border_1()
                                .border_color(theme.border)
                                .child(
                                    div()
                                        .size_full()
                                        .overflow_hidden()
                                        .rounded(px(7.))
                                        .child(img(shown).size_full().object_fit(ObjectFit::Cover)),
                                )
                                .child(remove)
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.viewer = picture_of(&media, bytes.to_vec()).or_else(|| Some((image.clone(), 800, 600)));
                                    cx.notify();
                                }))
                                .into_any_element()
                        }
                        None => div()
                            .id(("attached", ix))
                            .relative()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .h(px(30.))
                            .max_w(px(260.))
                            .px(px(10.))
                            .rounded(px(8.))
                            .bg(theme.hover)
                            .border_1()
                            .border_color(theme.border)
                            .text_size(px(12.))
                            .child("📎")
                            .child(div().truncate().child(attached.name.clone()))
                            .child(remove)
                            .into_any_element(),
                    }
                }))
                .into_any_element(),
        )
    }

    pub(super) fn render_completion(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let completion = self.completion.as_ref()?;
        let theme = *theme;
        // Keep the chosen item on screen: a window of SHOWN items around it.
        let first = completion.selected.saturating_sub(SHOWN - 1);
        Some(
            div()
                .flex()
                .flex_col()
                .py(px(4.))
                .rounded(px(9.))
                .bg(theme.popup_bg)
                .border_1()
                .border_color(theme.popup_border)
                .when(completion.loading && completion.items.is_empty(), |this| {
                    this.child(div().px(px(10.)).py(px(4.)).text_color(theme.text_muted).child(t("composer.indexing")))
                })
                .children(completion.items.iter().enumerate().skip(first).take(SHOWN).map(|(ix, item)| {
                    let selected = ix == completion.selected;
                    div()
                        .id(("suggestion", ix))
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .h(px(26.))
                        .px(px(10.))
                        .mx(px(4.))
                        .rounded(px(6.))
                        .when(selected, |this| this.bg(theme.selection))
                        .hover(|style| style.bg(theme.hover))
                        .child(div().flex_none().font_family(super::MONO).text_size(px(12.5)).child(item.label.clone()))
                        .child(div().flex_1().min_w_0().truncate().text_size(px(11.5)).text_color(theme.text_muted).child(item.detail.clone()))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            if let Some(completion) = &mut this.completion {
                                completion.selected = ix;
                            }
                            this.accept_completion(window, cx);
                        }))
                }))
                .into_any_element(),
        )
    }

    /// A picture over the whole window; a click or Escape closes it.
    pub(super) fn render_viewer(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (image, width, height) = self.viewer.clone()?;
        let viewport = window.viewport_size();
        let (w, h) = fit(width, height, f32::from(viewport.width) * 0.9, f32::from(viewport.height) * 0.9);
        Some(
            div()
                .id("viewer")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(hsla(0., 0., 0., 0.78))
                .occlude()
                .child(img(image).w(px(w)).h(px(h)).rounded(px(6.)))
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.viewer = None;
                    cx.notify();
                }))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: the GPUI prelude brings its own `test` attribute.
    use super::{Command, completion_range, file_score, fit, mention_start};

    #[test]
    fn mentions_start_a_word() {
        assert_eq!(mention_start("look at @src/ma"), Some(8));
        assert_eq!(mention_start("@"), Some(0));
        assert_eq!(mention_start("mail me@host"), None);
        assert_eq!(mention_start("@done and more"), None);
    }

    /// A list made for one word is not applied to another the caret moved to.
    #[test]
    fn a_stale_suggestion_list_replaces_nothing() {
        let text = "look at @src/ma and then";
        assert_eq!(completion_range(text, 15, 8), Some(8..15));
        assert_eq!(completion_range(text, 3, 8), None, "the caret before the word");
        assert_eq!(completion_range(text, 20, 8), None, "the caret in a later word");
        assert_eq!(completion_range("@ä", 2, 0), None, "not on a character's edge");
        assert_eq!(completion_range("@ä", 3, 0), Some(0..3));
        assert_eq!(completion_range("ab", 9, 0), None, "the text got shorter");
        assert_eq!(completion_range("/mod", 4, 0), Some(0..4));
        assert_eq!(completion_range("/model and", 10, 0), None);
    }

    #[test]
    fn file_names_rank_before_folders() {
        let by_name = file_score("src/main.rs", "main").unwrap();
        let by_folder = file_score("main/lib.rs", "main").unwrap();
        let scattered = file_score("src/model/accounts.rs", "mars").unwrap();
        assert!(by_name < by_folder && by_folder < scattered);
        assert_eq!(file_score("src/lib.rs", "zzz"), None);
    }

    #[test]
    fn typed_commands_parse() {
        assert_eq!(Command::parse(" /new "), Some(Command::New));
        assert_eq!(Command::parse("/clear"), Some(Command::New));
        assert_eq!(Command::parse("/review this"), None);
        assert_eq!(Command::parse("/unknown"), None);
    }

    #[test]
    fn pictures_fit_without_growing() {
        assert_eq!(fit(100, 50, 400., 300.), (100., 50.));
        assert_eq!(fit(2000, 1000, 400., 300.), (400., 200.));
    }
}
