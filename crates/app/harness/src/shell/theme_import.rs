//! Import of a colour theme: a VS Code theme (`.json`) or a scheme of the other IDE
//! (`.icls`) becomes a file of the `themes` folder and the theme of the window.

use std::path::{Path, PathBuf};

use theme::ThemeFile;
use ui_kit::prelude::*;
use ui_kit::*;

use super::Shell;
use crate::i18n::t;

impl Shell {
    pub(super) fn pick_theme(&mut self, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: None });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = chosen.await
                && let Some(path) = paths.into_iter().next()
            {
                this.update(cx, |this, cx| this.import_theme(&path, cx)).ok();
            }
        })
        .detach();
    }

    /// Reads the file, writes it into the themes folder under a free name and turns the
    /// window to it. A file that is no theme only says so.
    pub(super) fn import_theme(&mut self, path: &Path, cx: &mut Context<Self>) {
        let current = cx.global::<Theme>().id().to_owned();
        let dark = self.catalog.library().get(&current).is_none_or(|theme| theme.dark);
        let file = match read_theme(path, &current, dark) {
            Ok(file) => file,
            Err(error) => {
                self.status = Some(format!("{}: {error}", t("theme.import_failed")));
                cx.notify();
                return;
            }
        };
        let folder = self.catalog.folder().to_path_buf();
        let id = free_id(&folder, &theme::slug(&file.name), |id| self.catalog.library().get(id).is_some());
        let target = folder.join(format!("{id}.json"));
        if let Err(error) = std::fs::create_dir_all(&folder).and_then(|()| std::fs::write(&target, file.to_json())) {
            self.status = Some(format!("{}: {error}", t("theme.import_failed")));
            cx.notify();
            return;
        }
        self.catalog.reload();
        if let Some(error) = self.catalog.library().errors().iter().find(|error| error.path.as_deref() == Some(target.as_path())) {
            self.status = Some(format!("{}: {}", t("theme.import_failed"), error.message));
        } else if self.catalog.library().get(&id).is_some() {
            let _ = self.lib().set_setting("theme", &id);
            cx.set_global(self.catalog.theme(&id));
            self.status = Some(format!("{}: {}", t("theme.imported"), file.name));
        }
        cx.notify();
    }
}

/// The theme of a file by its extension. A VS Code theme reads the files it names next to
/// itself; a scheme of the other IDE has no look of its own and takes the current one.
fn read_theme(path: &Path, current: &str, dark: bool) -> Result<ThemeFile, String> {
    let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    if extension == "icls" {
        return theme::from_icls(&text, current, dark).map_err(|error| error.message);
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("Imported");
    let name = stem.trim_end_matches("-color-theme").trim_end_matches("-theme");
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let read = move |relative: &str| std::fs::read_to_string(dir.join(relative)).ok();
    theme::from_vscode(&text, name, &read).map_err(|error| error.message)
}

/// `base`, or `base-2`, `base-3`… when a theme or a file has the name already.
pub(super) fn free_id(folder: &Path, base: &str, taken: impl Fn(&str) -> bool) -> String {
    let free = |id: &str| !taken(id) && !file_of(folder, id).exists();
    if free(base) {
        return base.to_owned();
    }
    (2..).map(|n| format!("{base}-{n}")).find(|id| free(id)).unwrap_or_else(|| base.to_owned())
}

fn file_of(folder: &Path, id: &str) -> PathBuf {
    folder.join(format!("{id}.json"))
}

#[cfg(test)]
mod tests {
    use super::{free_id, read_theme};

    #[test]
    fn an_import_takes_a_free_name() {
        let folder = std::env::temp_dir().join(format!("bowyer-theme-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("night.json"), "{}").unwrap();
        assert_eq!(free_id(&folder, "night", |_| false), "night-2");
        assert_eq!(free_id(&folder, "dark", |id| id == "dark"), "dark-2");
        assert_eq!(free_id(&folder, "day", |_| false), "day");
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn a_vscode_file_reads_its_include_next_to_it() {
        let folder = std::env::temp_dir().join(format!("bowyer-vscode-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("base.json"), r##"{"colors": {"editor.background": "#202020"}}"##).unwrap();
        let path = folder.join("dusk-color-theme.json");
        std::fs::write(&path, r##"{"include": "./base.json", "tokenColors": [{"scope": "keyword", "settings": {"foreground": "#ff0000"}}]}"##).unwrap();
        let file = read_theme(&path, "dark", true).unwrap();
        assert_eq!(file.name, "dusk");
        assert_eq!(file.ui.get("editor_bg"), Some(&"#202020".to_owned()));
        assert_eq!(file.syntax.get("keyword"), Some(&"#ff0000".to_owned()));
        std::fs::remove_dir_all(&folder).unwrap();
    }
}
