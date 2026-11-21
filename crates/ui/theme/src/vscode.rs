//! Import of a colour theme of VS Code (a `*-color-theme.json` of an extension): the
//! colours of the workbench and of the code become the overrides of a theme over the
//! built-in theme of the same look.
//!
//! The file is JSON with comments and trailing commas. It may `include` another theme,
//! and its `tokenColors` may be a list of TextMate rules or the path of a file with them;
//! the caller reads those files. A TextMate scope selector with a space (a descendant
//! rule) is left out: the highlighter of the window knows no nesting. Semantic token
//! colours and font styles have no token here and stay behind.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::Color;
use crate::format::{ThemeError, ThemeFile};

/// How deep `include` may nest before the file is refused.
const MAX_INCLUDE: usize = 6;

/// The tokens of the window, each with the VS Code colours that may fill it, the first
/// one present wins.
const UI_COLORS: &[(&str, &[&str])] = &[
    ("editor_bg", &["editor.background"]),
    ("island_bg", &["editor.background", "sideBar.background"]),
    (
        "main_bg",
        &["titleBar.activeBackground", "activityBar.background", "sideBar.background", "editorGroupHeader.tabsBackground"],
    ),
    ("border", &["panel.border", "sideBar.border", "editorGroup.border", "contrastBorder"]),
    ("text", &["foreground", "editor.foreground", "sideBar.foreground"]),
    ("text_secondary", &["descriptionForeground"]),
    ("text_disabled", &["disabledForeground"]),
    ("link", &["textLink.foreground"]),
    ("accent", &["button.background", "focusBorder", "activityBarBadge.background"]),
    ("over_accent", &["button.foreground", "activityBarBadge.foreground"]),
    ("selection", &["list.activeSelectionBackground"]),
    ("selection_hover", &["list.activeSelectionBackground"]),
    ("selection_muted", &["list.inactiveSelectionBackground"]),
    ("selection_inactive", &["list.inactiveSelectionBackground"]),
    ("hover", &["list.hoverBackground", "toolbar.hoverBackground"]),
    ("pressed", &["toolbar.activeBackground"]),
    ("tab_selected", &["tab.activeBackground"]),
    ("tab_selected_border", &["tab.activeBorderTop", "tab.activeBorder"]),
    ("popup_bg", &["editorWidget.background", "menu.background", "quickInput.background"]),
    ("popup_border", &["editorWidget.border", "menu.border", "widget.border"]),
    ("feedback_bg", &["notifications.background"]),
    ("feedback_border", &["notifications.border"]),
    ("control_border", &["input.border", "dropdown.border"]),
    ("match_bg", &["editor.findMatchHighlightBackground"]),
    ("code", &["editor.foreground"]),
    ("caret", &["editorCursor.foreground"]),
    ("caret_row", &["editor.lineHighlightBackground"]),
    ("line_number", &["editorLineNumber.foreground"]),
    ("line_number_current", &["editorLineNumber.activeForeground"]),
    ("indent_guide", &["editorIndentGuide.background1", "editorIndentGuide.background"]),
    ("inlay", &["editorInlayHint.foreground"]),
    ("inlay_bg", &["editorInlayHint.background"]),
    ("scrollbar_thumb", &["scrollbarSlider.background"]),
    ("scrollbar_thumb_hover", &["scrollbarSlider.hoverBackground"]),
    ("warning_mark", &["editorWarning.foreground", "editorOverviewRuler.warningForeground"]),
    ("console_input", &["terminal.foreground"]),
    ("success", &["gitDecoration.addedResourceForeground", "terminal.ansiGreen"]),
    ("info", &["editorInfo.foreground", "terminal.ansiBlue"]),
    ("error", &["errorForeground", "editorError.foreground"]),
];

/// The tokens of the code, each with the TextMate scopes that stand for it, the first one
/// a rule colours wins. A rule for a parent scope colours them too (`keyword` colours
/// `keyword.control`), except for the tokens in [`OWN_SCOPE_ONLY`].
const SYNTAX_SCOPES: &[(&str, &[&str])] = &[
    ("keyword", &["keyword.control", "keyword", "storage.type", "storage"]),
    ("string", &["string.quoted.double", "string"]),
    ("number", &["constant.numeric"]),
    ("comment", &["comment.line.double-slash", "comment"]),
    ("doc", &["comment.block.documentation", "comment.block", "comment"]),
    ("doc_tag", &["storage.type.class.jsdoc", "keyword.other.documentation"]),
    ("function", &["entity.name.function", "support.function"]),
    ("annotation", &["meta.decorator", "entity.name.function.decorator", "storage.type.annotation", "punctuation.definition.annotation"]),
    ("field", &["variable.other.property", "variable.other.member", "support.variable.property"]),
    ("semantic_field", &["variable.other.property", "variable.other.member"]),
    ("semantic_parameter", &["variable.parameter"]),
    ("semantic_local", &["variable.other.readwrite", "variable.other", "variable"]),
    ("semantic_method", &["entity.name.function.member", "entity.name.function"]),
    ("semantic_type", &["entity.name.type", "entity.name.class", "support.type", "support.class"]),
    ("semantic_type_parameter", &["entity.name.type.parameter", "entity.name.type"]),
    ("semantic_constant", &["variable.other.constant", "constant.other", "constant.language"]),
];

/// Tokens a rule of a parent scope must not colour: a decorator is no function and a tag
/// of a doc comment no keyword, so they keep the parent theme's colour instead.
const OWN_SCOPE_ONLY: &[&str] = &["annotation", "doc_tag"];

/// Reads a VS Code colour theme. `name` is the label when the file has none (the name of
/// the file, say); `read` gives the text of a file the theme names by a relative path.
pub fn from_vscode(text: &str, name: &str, read: &dyn Fn(&str) -> Option<String>) -> Result<ThemeFile, ThemeError> {
    let mut colors = Map::new();
    let mut rules = Vec::new();
    let mut kind = None;
    let mut label = None;
    collect(text, read, 0, &mut colors, &mut rules, &mut kind, &mut label)?;
    if colors.is_empty() && rules.is_empty() {
        return Err(ThemeError::new("no colors and no tokenColors in the file"));
    }

    // A rule without a scope sets the plain text and the background of old themes.
    let global = rules.iter().filter(|rule| rule.selectors.is_empty());
    let global_fg = global.clone().filter_map(|rule| rule.foreground.clone()).last();
    let global_bg = global.filter_map(|rule| rule.background.clone()).last();
    let color = |key: &str| colors.get(key).and_then(Value::as_str).and_then(read_color);
    let background = color("editor.background").or(global_bg);
    let dark = match kind.as_deref() {
        Some("light" | "vs" | "hcLight" | "hc-light") => false,
        Some("dark" | "vs-dark" | "hc" | "hc-black" | "hcDark") => true,
        _ => background.as_deref().and_then(Color::from_text).is_none_or(|color| luminance(color) < 0.5),
    };
    let high_contrast = matches!(kind.as_deref(), Some("hc" | "hc-black" | "hcDark"));

    let mut ui = BTreeMap::new();
    for (token, keys) in UI_COLORS {
        if let Some(value) = keys.iter().find_map(|key| color(key)) {
            ui.insert((*token).to_owned(), value);
        }
    }
    if let Some(background) = &background {
        for token in ["editor_bg", "island_bg"] {
            ui.entry(token.to_owned()).or_insert_with(|| background.clone());
        }
    }
    if let Some(foreground) = &global_fg {
        for token in ["text", "code"] {
            ui.entry(token.to_owned()).or_insert_with(|| foreground.clone());
        }
    }
    derive(&mut ui);

    let mut syntax = BTreeMap::new();
    for (token, scopes) in SYNTAX_SCOPES {
        let own_only = OWN_SCOPE_ONLY.contains(token);
        if let Some(value) = scopes.iter().find_map(|scope| match_scope(&rules, scope, own_only)) {
            syntax.insert((*token).to_owned(), value);
        }
    }
    // The text of the code sits with the window tokens in the built-in files.
    if let Some(code) = ui.remove("code") {
        syntax.insert("code".to_owned(), code);
    }

    let parent = if high_contrast { "high-contrast" } else if dark { "dark" } else { "light" };
    Ok(ThemeFile {
        name: label.filter(|label| !label.trim().is_empty()).unwrap_or_else(|| name.to_owned()),
        dark,
        parent: Some(parent.to_owned()),
        ui,
        syntax,
    })
}

/// One TextMate rule: the plain scopes it colours and its colours.
struct Rule {
    selectors: Vec<String>,
    foreground: Option<String>,
    background: Option<String>,
}

/// Reads one file and the files it includes into the colours and rules, the included
/// ones first so the file itself wins.
fn collect(
    text: &str,
    read: &dyn Fn(&str) -> Option<String>,
    depth: usize,
    colors: &mut Map<String, Value>,
    rules: &mut Vec<Rule>,
    kind: &mut Option<String>,
    label: &mut Option<String>,
) -> Result<(), ThemeError> {
    if depth > MAX_INCLUDE {
        return Err(ThemeError::new("the includes of the theme go too deep"));
    }
    let value: Value = serde_json::from_str(&strip_jsonc(text))
        .map_err(|error| ThemeError::new(format!("not a VS Code theme: {error}")))?;
    let Value::Object(file) = value else {
        return Err(ThemeError::new("not a VS Code theme: the file is no object"));
    };
    if let Some(include) = file.get("include").and_then(Value::as_str) {
        let included = read(include).ok_or_else(|| ThemeError::new(format!("the included theme {include} is not read")))?;
        collect(&included, read, depth + 1, colors, rules, kind, label)?;
    }
    if let Some(Value::Object(own)) = file.get("colors") {
        for (key, value) in own {
            colors.insert(key.clone(), value.clone());
        }
    }
    match file.get("tokenColors") {
        Some(Value::Array(list)) => read_rules(list, rules),
        Some(Value::String(path)) => {
            // A file of rules: a list, or an object with its own `tokenColors`. A
            // TextMate theme (plist) cannot be read here.
            if let Some(text) = read(path)
                && let Ok(value) = serde_json::from_str::<Value>(&strip_jsonc(&text))
            {
                match value {
                    Value::Array(list) => read_rules(&list, rules),
                    Value::Object(object) => {
                        if let Some(Value::Array(list)) = object.get("tokenColors") {
                            read_rules(list, rules);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    if let Some(value) = file.get("type").and_then(Value::as_str) {
        *kind = Some(value.to_owned());
    }
    if let Some(value) = file.get("name").and_then(Value::as_str) {
        *label = Some(value.to_owned());
    }
    Ok(())
}

fn read_rules(list: &[Value], rules: &mut Vec<Rule>) {
    for rule in list {
        let Some(settings) = rule.get("settings") else { continue };
        let foreground = settings.get("foreground").and_then(Value::as_str).and_then(read_color);
        let background = settings.get("background").and_then(Value::as_str).and_then(read_color);
        if foreground.is_none() && background.is_none() {
            continue;
        }
        let scopes: Vec<&str> = match rule.get("scope") {
            Some(Value::String(scope)) => scope.split(',').collect(),
            Some(Value::Array(list)) => list.iter().filter_map(Value::as_str).flat_map(|scope| scope.split(',')).collect(),
            _ => Vec::new(),
        };
        let selectors: Vec<String> =
            scopes.iter().map(|scope| scope.trim()).filter(|scope| !scope.is_empty() && !scope.contains(char::is_whitespace)).map(str::to_owned).collect();
        // A rule whose every selector is a descendant one colours nothing here, unlike a
        // rule without any scope, which is the global one.
        if selectors.is_empty() && !scopes.iter().all(|scope| scope.trim().is_empty()) {
            continue;
        }
        rules.push(Rule { selectors, foreground, background });
    }
}

/// The foreground the rules give `scope`: the rule with the longest selector that is the
/// scope or a parent of it, the later rule on a tie, as VS Code does. `own_only` takes the
/// scope itself and the scopes under it, not its parents.
fn match_scope(rules: &[Rule], scope: &str, own_only: bool) -> Option<String> {
    let mut best: Option<(usize, &String)> = None;
    for rule in rules {
        let Some(foreground) = &rule.foreground else { continue };
        for selector in &rule.selectors {
            let under = |outer: &str, inner: &str| inner.len() > outer.len() && inner.starts_with(outer) && inner[outer.len()..].starts_with('.');
            let fits = scope == selector || if own_only { under(scope, selector) } else { under(selector, scope) };
            if fits && best.is_none_or(|(length, _)| selector.len() >= length) {
                best = Some((selector.len(), foreground));
            }
        }
    }
    best.map(|(_, color)| color.clone())
}

/// The tokens VS Code has no colour for, mixed from the ones it has, so that the muted
/// text and the lines match the theme rather than the parent.
fn derive(ui: &mut BTreeMap<String, String>) {
    let get = |ui: &BTreeMap<String, String>, token: &str| ui.get(token).and_then(|value| Color::from_text(value));
    let (Some(text), Some(background)) = (get(ui, "text"), get(ui, "island_bg")) else { return };
    let text = over(text, background);
    let mut put = |token: &str, amount: f32| {
        ui.entry(token.to_owned()).or_insert_with(|| mix(text, background, amount).to_text());
    };
    put("text_secondary", 0.25);
    put("text_muted", 0.42);
    put("text_disabled", 0.6);
    put("border", 0.86);
    put("hover", 0.9);
    put("selection_hover", 0.82);
}

/// `color` painted over `background`, without its alpha.
fn over(color: Color, background: Color) -> Color {
    let a = color.a;
    Color { r: color.r * a + background.r * (1. - a), g: color.g * a + background.g * (1. - a), b: color.b * a + background.b * (1. - a), a: 1. }
}

/// `amount` of the way from `from` to `to`.
fn mix(from: Color, to: Color, amount: f32) -> Color {
    let step = |a: f32, b: f32| a + (b - a) * amount;
    Color { r: step(from.r, to.r), g: step(from.g, to.g), b: step(from.b, to.b), a: 1. }
}

fn luminance(color: Color) -> f32 {
    0.2126 * color.r + 0.7152 * color.g + 0.0722 * color.b
}

/// A colour as VS Code writes it (`#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`), as a theme
/// file of the window writes it.
fn read_color(text: &str) -> Option<String> {
    let hex = text.trim().strip_prefix('#')?;
    let long = match hex.len() {
        3 | 4 => hex.chars().flat_map(|c| [c, c]).collect(),
        6 | 8 => hex.to_owned(),
        _ => return None,
    };
    Color::from_text(&long).map(Color::to_text)
}

/// JSON with comments and trailing commas, as VS Code writes its files, made plain JSON.
fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' => {
                    if let Some(next) = chars.next() {
                        out.push(next);
                    }
                }
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut last = ' ';
                for next in chars.by_ref() {
                    if last == '*' && next == '/' {
                        break;
                    }
                    last = next;
                }
                out.push(' ');
            }
            '}' | ']' => {
                let end = out.trim_end().len();
                if out[..end].ends_with(',') {
                    out.remove(end - 1);
                }
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tokens;

    /// A small theme with what real files have: comments, a trailing comma, a short
    /// colour, a colour with alpha, a scope list, a comma scope and a descendant rule.
    const SAMPLE: &str = r##"{
        // A comment the JSON reader would refuse.
        "name": "Sample Night",
        "type": "dark",
        "colors": {
            "editor.background": "#1e1e2e",
            "editor.foreground": "#cdd6f4",
            "foreground": "#cdd6f4",
            "list.hoverBackground": "#ffffff1a",
            "button.background": "#f80", /* short form */
            "sideBar.background": "#181825",
        },
        "tokenColors": [
            {"settings": {"foreground": "#cdd6f4", "background": "#1e1e2e"}},
            {"scope": ["keyword", "storage.type"], "settings": {"foreground": "#cba6f7"}},
            {"scope": "keyword.control", "settings": {"foreground": "#f38ba8"}},
            {"scope": "string, string.quoted", "settings": {"foreground": "#a6e3a1", "fontStyle": "italic"}},
            {"scope": "meta.block string", "settings": {"foreground": "#000000"}},
            {"name": "url // not a comment", "scope": "comment", "settings": {"foreground": "#6c7086"}},
            {"scope": "entity.name.function", "settings": {"foreground": "#89b4fa"}},
            {"scope": "meta.decorator.python", "settings": {"foreground": "#fab387"}},
        ],
    }"##;

    fn no_files(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn a_vscode_theme_becomes_a_theme_over_the_builtin_of_its_look() {
        let file = from_vscode(SAMPLE, "fallback", &no_files).unwrap();
        assert_eq!(file.name, "Sample Night");
        assert!(file.dark);
        assert_eq!(file.parent.as_deref(), Some("dark"));
        assert_eq!(file.ui.get("editor_bg"), Some(&"#1e1e2e".to_owned()));
        assert_eq!(file.ui.get("accent"), Some(&"#ff8800".to_owned()), "the short form");
        assert_eq!(file.ui.get("hover"), Some(&"#ffffff1a".to_owned()), "alpha is kept");
        assert_eq!(file.ui.get("main_bg"), Some(&"#181825".to_owned()));
        assert!(file.ui.contains_key("text_muted"), "mixed from text and background");
        assert_eq!(file.syntax.get("keyword"), Some(&"#f38ba8".to_owned()), "the longer selector wins");
        assert_eq!(file.syntax.get("string"), Some(&"#a6e3a1".to_owned()), "not the descendant rule");
        assert_eq!(file.syntax.get("comment"), Some(&"#6c7086".to_owned()), "a // inside a string stays");
        assert_eq!(file.syntax.get("function"), Some(&"#89b4fa".to_owned()));
        assert_eq!(file.syntax.get("annotation"), Some(&"#fab387".to_owned()), "a narrower scope of its own");
        assert_eq!(file.syntax.get("doc_tag"), None, "storage colours no tag of a doc comment");
        assert_eq!(file.syntax.get("code"), Some(&"#cdd6f4".to_owned()));
        let (tokens, warnings) = file.resolve(Some(Tokens::dark())).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(tokens.keyword, Color::from_text("#f38ba8").unwrap());
        assert_eq!(tokens.semantic_type, Tokens::dark().semantic_type, "the rest comes from the parent");
    }

    #[test]
    fn an_include_and_a_light_look_without_a_type() {
        let base = r##"{"colors": {"editor.background": "#fdfdfd", "editor.foreground": "#222222"},
            "tokenColors": [{"scope": "comment", "settings": {"foreground": "#008000"}}]}"##;
        let theme = r##"{"include": "./base.json", "colors": {"editor.foreground": "#111111"}}"##;
        let read = |path: &str| (path == "./base.json").then(|| base.to_owned());
        let file = from_vscode(theme, "My light", &read).unwrap();
        assert_eq!(file.name, "My light", "the file has no name of its own");
        assert!(!file.dark, "a light background");
        assert_eq!(file.parent.as_deref(), Some("light"));
        assert_eq!(file.syntax.get("code"), Some(&"#111111".to_owned()), "the file wins over its include");
        assert_eq!(file.syntax.get("comment"), Some(&"#008000".to_owned()));
        assert!(from_vscode(r#"{"include": "missing.json"}"#, "x", &read).is_err());
    }

    #[test]
    fn a_file_that_is_no_theme_is_refused() {
        assert!(from_vscode("not json", "x", &no_files).is_err());
        assert!(from_vscode("[1, 2]", "x", &no_files).is_err());
        assert!(from_vscode(r#"{"name": "empty"}"#, "x", &no_files).is_err());
    }

    #[test]
    fn comments_and_trailing_commas_go_strings_stay() {
        let text = "{\"a\": \"x // y /* z */\", // c\n \"b\": [1, 2,], /* d */ }";
        let value: Value = serde_json::from_str(&strip_jsonc(text)).unwrap();
        assert_eq!(value["a"], "x // y /* z */");
        assert_eq!(value["b"], serde_json::json!([1, 2]));
    }
}
