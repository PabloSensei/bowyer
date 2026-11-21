//! A theme being edited in the window: a parent theme and the tokens the user changed over
//! it. It is saved as an ordinary theme file of the `themes` folder, so a theme made here
//! reads like one written by hand.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::{Color, TOKEN_NAMES, ThemeFile, ThemeLibrary, Tokens};

/// The groups the editor shows the tokens in, in the order of [`TOKEN_NAMES`]: each starts
/// at its first token.
const GROUP_STARTS: [(&str, TokenGroup); 4] = [
    ("main_bg", TokenGroup::Window),
    ("code", TokenGroup::Code),
    ("caret_row", TokenGroup::Editor),
    ("success", TokenGroup::Status),
];

/// What a token colours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenGroup {
    /// Backgrounds, text, selection, buttons, popups of the window.
    Window,
    /// The colours of code.
    Code,
    /// Caret, gutter, scrollbars, marks of an editor.
    Editor,
    /// Success, information, error.
    Status,
}

impl TokenGroup {
    pub const ALL: [TokenGroup; 4] = [Self::Window, Self::Code, Self::Editor, Self::Status];

    pub fn id(self) -> &'static str {
        match self {
            Self::Window => "window",
            Self::Code => "code",
            Self::Editor => "editor",
            Self::Status => "status",
        }
    }

    /// The tokens of the group, in their order.
    pub fn tokens(self) -> impl Iterator<Item = &'static str> {
        TOKEN_NAMES.iter().copied().filter(move |name| token_group(name) == Some(self))
    }
}

/// The group of a token, `None` for a name that is no token.
pub fn token_group(name: &str) -> Option<TokenGroup> {
    let at = TOKEN_NAMES.iter().position(|token| *token == name)?;
    GROUP_STARTS
        .iter()
        .rev()
        .find(|(start, _)| TOKEN_NAMES.iter().position(|token| token == start).is_some_and(|start| start <= at))
        .map(|(_, group)| *group)
}

/// A theme being edited.
#[derive(Clone, Debug)]
pub struct ThemeDraft {
    pub name: String,
    pub dark: bool,
    /// The theme the tokens not changed here come from.
    pub parent: Option<String>,
    /// The tokens of the parent (or of the edited theme, when it has none).
    base: Tokens<Color>,
    /// The tokens changed over `base`.
    own: BTreeMap<String, Color>,
    /// The file of a theme of the user being edited; `None` for a new theme.
    pub path: Option<PathBuf>,
}

impl ThemeDraft {
    /// A draft of the theme `id`. A theme of the user is edited in its own file; a built-in
    /// one becomes a new theme called `copy_name`, with the built-in as its parent.
    pub fn of(library: &ThemeLibrary, id: &str, copy_name: &str) -> Option<Self> {
        let theme = library.get(id)?;
        let file = theme
            .path
            .as_ref()
            .filter(|_| !theme.builtin)
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| ThemeFile::parse(&text).ok());
        let Some(file) = file else {
            return Some(Self {
                name: copy_name.to_owned(),
                dark: theme.dark,
                parent: Some(theme.id.clone()),
                base: theme.tokens,
                own: BTreeMap::new(),
                path: None,
            });
        };
        let parent_tokens = file.parent.as_deref().and_then(|parent| library.get(parent)).map(|parent| parent.tokens);
        let own = file
            .ui
            .iter()
            .chain(file.syntax.iter())
            .filter(|(name, _)| TOKEN_NAMES.contains(&name.as_str()))
            .filter_map(|(name, value)| Some((name.clone(), Color::from_text(value)?)))
            .collect();
        Some(Self {
            name: file.name,
            dark: file.dark,
            parent: file.parent.filter(|_| parent_tokens.is_some()),
            base: parent_tokens.unwrap_or(theme.tokens),
            own,
            path: theme.path.clone(),
        })
    }

    /// The tokens of the theme as it stands.
    pub fn tokens(&self) -> Tokens<Color> {
        let mut tokens = self.base;
        tokens.dark = self.dark;
        for (name, color) in &self.own {
            tokens.set(name, *color);
        }
        tokens
    }

    /// Changes a token; the colour of the parent is no change. False for a name that is
    /// no token.
    pub fn set(&mut self, name: &str, color: Color) -> bool {
        let Some(base) = self.base.get(name) else { return false };
        if base == color && self.parent.is_some() {
            self.own.remove(name);
        } else {
            self.own.insert(name.to_owned(), color);
        }
        true
    }

    /// Gives a token back the colour of the parent.
    pub fn reset(&mut self, name: &str) {
        self.own.remove(name);
    }

    /// Whether the token differs from the parent.
    pub fn changed(&self, name: &str) -> bool {
        self.own.contains_key(name)
    }

    /// The file of the theme: the changed tokens, the colours of code under `syntax`.
    pub fn file(&self) -> ThemeFile {
        let mut ui = BTreeMap::new();
        let mut syntax = BTreeMap::new();
        for (name, color) in &self.own {
            let group = if token_group(name) == Some(TokenGroup::Code) { &mut syntax } else { &mut ui };
            group.insert(name.clone(), color.to_text());
        }
        ThemeFile { name: self.name.trim().to_owned(), dark: self.dark, parent: self.parent.clone(), ui, syntax }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_token_has_a_group() {
        assert!(TOKEN_NAMES.iter().all(|name| token_group(name).is_some()));
        assert_eq!(token_group("main_bg"), Some(TokenGroup::Window));
        assert_eq!(token_group("keyword"), Some(TokenGroup::Code));
        assert_eq!(token_group("semantic_annotation"), Some(TokenGroup::Code));
        assert_eq!(token_group("caret"), Some(TokenGroup::Editor));
        assert_eq!(token_group("error"), Some(TokenGroup::Status));
        assert_eq!(token_group("nope"), None);
        let count: usize = TokenGroup::ALL.iter().map(|group| group.tokens().count()).sum();
        assert_eq!(count, TOKEN_NAMES.len());
    }

    #[test]
    fn a_built_in_theme_is_copied_and_saved_as_its_changes() {
        let library = ThemeLibrary::builtin();
        let mut draft = ThemeDraft::of(library, "dark", "My dark").unwrap();
        assert_eq!(draft.parent.as_deref(), Some("dark"));
        assert!(draft.path.is_none());
        let red = Color::from_text("#ff0000").unwrap();
        assert!(draft.set("accent", red));
        assert!(draft.set("keyword", red));
        assert!(!draft.set("nope", red));
        assert_eq!(draft.tokens().accent, red);
        let dark = library.get("dark").unwrap().tokens;
        assert!(draft.set("caret", dark.caret), "the colour of the parent");
        assert!(!draft.changed("caret"), "is no change");

        let file = draft.file();
        assert_eq!(file.ui.get("accent").map(String::as_str), Some("#ff0000"));
        assert_eq!(file.syntax.get("keyword").map(String::as_str), Some("#ff0000"));
        assert_eq!(file.ui.len() + file.syntax.len(), 2);
        let read = ThemeFile::parse(&file.to_json()).unwrap();
        let (tokens, warnings) = read.resolve(Some(dark)).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(tokens.accent, red);
        assert_eq!(tokens.main_bg, dark.main_bg);

        draft.reset("accent");
        assert_eq!(draft.tokens().accent, dark.accent);
    }

    #[test]
    fn a_theme_of_the_user_is_edited_in_its_file() {
        let folder = std::env::temp_dir().join(format!("bowyer-draft-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("dusk.json"),
            r##"{"name": "Dusk", "dark": true, "parent": "dark", "ui": {"accent": "#123456"}, "syntax": {"string": "#00ff00"}}"##,
        )
        .unwrap();
        let library = ThemeLibrary::load(&folder);
        let draft = ThemeDraft::of(&library, "dusk", "unused").unwrap();
        std::fs::remove_dir_all(&folder).unwrap();
        assert_eq!(draft.name, "Dusk");
        assert_eq!(draft.parent.as_deref(), Some("dark"));
        assert!(draft.path.as_ref().is_some_and(|path| path.ends_with("dusk.json")));
        assert!(draft.changed("accent") && draft.changed("string") && !draft.changed("main_bg"));
        assert_eq!(draft.tokens().accent, Color::from_text("#123456").unwrap());
        let file = draft.file();
        assert_eq!(file.syntax.get("string").map(String::as_str), Some("#00ff00"));
    }
}
