//! A small lexical highlighter for reading code: comments, strings, numbers, keywords, type
//! names, calls and annotations. It knows no grammar, so it is wrong now and then, but it
//! needs nothing beyond this file and colours a 2 MB file in a few milliseconds. Block
//! comments and multi-line strings carry over from one line to the next.

use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Keyword,
    String,
    Number,
    Comment,
    Function,
    Type,
    Annotation,
}

pub struct Lang {
    line_comments: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str)>,
    /// Quote characters; a tripled one (`"""`) opens a string that may span lines.
    quotes: &'static [char],
    triple_quotes: bool,
    keywords: &'static [&'static str],
    /// `@name` or `#[...]` marks.
    annotations: bool,
    /// Names starting with a capital letter are types.
    capital_types: bool,
    /// Rust: `'a` is a lifetime and `name!(` a macro call.
    rust: bool,
}

const C_LIKE: &[&str] = &[
    "if", "else", "for", "while", "do", "return", "break", "continue", "switch", "case", "default", "struct", "enum",
    "union", "typedef", "static", "const", "void", "int", "char", "long", "short", "unsigned", "signed", "float",
    "double", "bool", "true", "false", "null", "nullptr", "sizeof", "extern", "inline", "goto", "class", "public",
    "private", "protected", "virtual", "override", "template", "typename", "namespace", "using", "new", "delete",
    "this", "auto", "try", "catch", "throw",
];

const RUST: Lang = Lang {
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: &['"', '\''],
    triple_quotes: false,
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn",
        "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self",
        "Self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while", "Some",
        "None", "Ok", "Err",
    ],
    annotations: true,
    capital_types: true,
    rust: true,
};

const JS: Lang = Lang {
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: &['"', '\'', '`'],
    triple_quotes: false,
    keywords: &[
        "async", "await", "break", "case", "catch", "class", "const", "continue", "debugger", "default", "delete",
        "do", "else", "export", "extends", "false", "finally", "for", "from", "function", "if", "import", "in",
        "instanceof", "interface", "let", "new", "null", "of", "return", "static", "super", "switch", "this", "throw",
        "true", "try", "type", "typeof", "undefined", "var", "void", "while", "yield", "enum", "implements",
        "private", "public", "protected", "readonly", "as", "keyof", "declare", "namespace",
    ],
    annotations: true,
    capital_types: true,
    rust: false,
};

const PYTHON: Lang = Lang {
    line_comments: &["#"],
    block_comment: None,
    quotes: &['"', '\''],
    triple_quotes: true,
    keywords: &[
        "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif", "else", "except",
        "False", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "None", "nonlocal", "not",
        "or", "pass", "raise", "return", "True", "try", "while", "with", "yield", "self", "match", "case",
    ],
    annotations: true,
    capital_types: true,
    rust: false,
};

const GO: Lang = Lang {
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: &['"', '\'', '`'],
    triple_quotes: false,
    keywords: &[
        "break", "case", "chan", "const", "continue", "default", "defer", "else", "fallthrough", "for", "func", "go",
        "goto", "if", "import", "interface", "map", "package", "range", "return", "select", "struct", "switch",
        "type", "var", "nil", "true", "false", "error", "string", "int", "bool", "byte",
    ],
    annotations: false,
    capital_types: true,
    rust: false,
};

const JAVA: Lang = Lang {
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: &['"', '\''],
    triple_quotes: true,
    keywords: &[
        "abstract", "boolean", "break", "byte", "case", "catch", "char", "class", "const", "continue", "default",
        "do", "double", "else", "enum", "extends", "final", "finally", "float", "for", "if", "implements", "import",
        "instanceof", "int", "interface", "long", "native", "new", "package", "private", "protected", "public",
        "return", "short", "static", "super", "switch", "synchronized", "this", "throw", "throws", "try", "void",
        "volatile", "while", "true", "false", "null", "var", "record", "val", "fun", "object", "when", "is",
        "override", "data", "sealed", "open", "companion", "suspend", "internal", "lateinit", "def", "trait",
    ],
    annotations: true,
    capital_types: true,
    rust: false,
};

const C: Lang = Lang {
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: &['"', '\''],
    triple_quotes: false,
    keywords: C_LIKE,
    annotations: false,
    capital_types: true,
    rust: false,
};

const CSHARP: Lang = Lang {
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: &['"', '\''],
    triple_quotes: false,
    keywords: &[
        "abstract", "as", "async", "await", "base", "bool", "break", "case", "catch", "class", "const", "continue",
        "default", "do", "else", "enum", "false", "finally", "for", "foreach", "if", "in", "int", "interface",
        "internal", "is", "namespace", "new", "null", "out", "override", "private", "protected", "public", "readonly",
        "ref", "return", "sealed", "static", "string", "struct", "switch", "this", "throw", "true", "try", "using",
        "var", "virtual", "void", "while", "record", "get", "set",
    ],
    annotations: false,
    capital_types: true,
    rust: false,
};

const SHELL: Lang = Lang {
    line_comments: &["#"],
    block_comment: None,
    quotes: &['"', '\''],
    triple_quotes: false,
    keywords: &[
        "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac", "in", "function",
        "return", "local", "export", "readonly", "set", "unset", "echo", "exit", "source", "cd", "true", "false",
    ],
    annotations: false,
    capital_types: false,
    rust: false,
};

const POWERSHELL: Lang = Lang {
    line_comments: &["#"],
    block_comment: Some(("<#", "#>")),
    quotes: &['"', '\''],
    triple_quotes: false,
    keywords: &[
        "if", "else", "elseif", "foreach", "for", "while", "do", "until", "switch", "function", "param", "return",
        "try", "catch", "finally", "throw", "break", "continue", "in", "begin", "process", "end",
    ],
    annotations: false,
    capital_types: false,
    rust: false,
};

const SQL: Lang = Lang {
    line_comments: &["--"],
    block_comment: Some(("/*", "*/")),
    quotes: &['\'', '"'],
    triple_quotes: false,
    keywords: &[
        "select", "from", "where", "insert", "into", "values", "update", "set", "delete", "create", "table", "index",
        "drop", "alter", "add", "join", "left", "right", "inner", "outer", "on", "and", "or", "not", "null", "is",
        "as", "order", "by", "group", "having", "limit", "offset", "primary", "key", "references", "default",
        "integer", "text", "real", "unique", "exists", "if", "begin", "commit", "with", "union", "distinct",
        "SELECT", "FROM", "WHERE", "INSERT", "INTO", "VALUES", "UPDATE", "SET", "DELETE", "CREATE", "TABLE", "INDEX",
        "DROP", "ALTER", "JOIN", "LEFT", "ON", "AND", "OR", "NOT", "NULL", "IS", "AS", "ORDER", "BY", "GROUP",
        "LIMIT", "PRIMARY", "KEY", "DEFAULT", "INTEGER", "TEXT", "EXISTS", "IF", "WITH",
    ],
    annotations: false,
    capital_types: false,
    rust: false,
};

const DATA: Lang = Lang {
    line_comments: &["#"],
    block_comment: None,
    quotes: &['"', '\''],
    triple_quotes: true,
    keywords: &["true", "false", "null", "yes", "no", "on", "off"],
    annotations: false,
    capital_types: false,
    rust: false,
};

const JSON: Lang = Lang {
    line_comments: &["//"],
    block_comment: None,
    quotes: &['"'],
    triple_quotes: false,
    keywords: &["true", "false", "null"],
    annotations: false,
    capital_types: false,
    rust: false,
};

const LUA: Lang = Lang {
    line_comments: &["--"],
    block_comment: Some(("--[[", "]]")),
    quotes: &['"', '\''],
    triple_quotes: false,
    keywords: &[
        "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in", "local", "nil",
        "not", "or", "repeat", "return", "then", "true", "until", "while",
    ],
    annotations: false,
    capital_types: false,
    rust: false,
};

const RUBY: Lang = Lang {
    line_comments: &["#"],
    block_comment: None,
    quotes: &['"', '\''],
    triple_quotes: false,
    keywords: &[
        "alias", "and", "begin", "break", "case", "class", "def", "do", "else", "elsif", "end", "ensure", "false",
        "for", "if", "in", "module", "next", "nil", "not", "or", "redo", "rescue", "retry", "return", "self",
        "super", "then", "true", "undef", "unless", "until", "when", "while", "yield", "require",
    ],
    annotations: false,
    capital_types: true,
    rust: false,
};

const MARKUP: Lang = Lang {
    line_comments: &[],
    block_comment: Some(("<!--", "-->")),
    quotes: &['"', '\''],
    triple_quotes: false,
    keywords: &[],
    annotations: false,
    capital_types: false,
    rust: false,
};

const CSS: Lang = Lang {
    line_comments: &[],
    block_comment: Some(("/*", "*/")),
    quotes: &['"', '\''],
    triple_quotes: false,
    keywords: &["important", "inherit", "none", "auto", "solid", "flex", "grid", "block", "inline", "absolute", "relative"],
    annotations: true,
    capital_types: false,
    rust: false,
};

/// The language of a file name, or of the tag of a fenced code block.
pub fn lang_for(name: &str) -> Option<&'static Lang> {
    let lower = name.to_ascii_lowercase();
    let ext = lower.rsplit(['.', '/', '\\']).next().unwrap_or(&lower);
    Some(match ext {
        "rs" | "rust" => &RUST,
        "js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx" | "javascript" | "typescript" | "mts" => &JS,
        "py" | "python" | "pyi" => &PYTHON,
        "go" | "golang" => &GO,
        "java" | "kt" | "kts" | "kotlin" | "scala" | "groovy" | "gradle" | "swift" | "dart" => &JAVA,
        "c" | "h" | "cpp" | "cc" | "hpp" | "cxx" | "c++" | "objc" | "m" => &C,
        "cs" | "csharp" => &CSHARP,
        "sh" | "bash" | "zsh" | "shell" | "console" | "fish" => &SHELL,
        "ps1" | "psm1" | "powershell" | "pwsh" => &POWERSHELL,
        "sql" => &SQL,
        "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" | "env" | "properties" | "dockerfile" | "makefile" | "mk" => &DATA,
        "json" | "jsonl" | "jsonc" => &JSON,
        "lua" => &LUA,
        "rb" | "ruby" => &RUBY,
        "html" | "htm" | "xml" | "svg" | "vue" | "xaml" | "csproj" => &MARKUP,
        "css" | "scss" | "less" => &CSS,
        _ => return None,
    })
}

/// Colours lines one after another; state left open by a line (a block comment, a string
/// in triple quotes) carries into the next.
pub struct Highlighter {
    lang: &'static Lang,
    /// Inside a block comment, or a string closed by this text.
    open: Option<Open>,
}

#[derive(Clone, Copy)]
enum Open {
    Comment,
    String(char),
}

impl Highlighter {
    pub fn new(lang: &'static Lang) -> Self {
        Self { lang, open: None }
    }

    pub fn line(&mut self, text: &str) -> Vec<(Range<usize>, Kind)> {
        let lang = self.lang;
        let bytes = text.as_bytes();
        let mut out: Vec<(Range<usize>, Kind)> = Vec::new();
        let mut i = 0;
        // Continue what the previous line left open.
        match self.open {
            Some(Open::Comment) => {
                let end = lang.block_comment.map(|(_, end)| end).unwrap_or("*/");
                match text.find(end) {
                    Some(at) => {
                        out.push((0..at + end.len(), Kind::Comment));
                        i = at + end.len();
                        self.open = None;
                    }
                    None => return vec![(0..text.len(), Kind::Comment)],
                }
            }
            Some(Open::String(quote)) => {
                let close: String = std::iter::repeat_n(quote, 3).collect();
                match text.find(&close) {
                    Some(at) => {
                        out.push((0..at + 3, Kind::String));
                        i = at + 3;
                        self.open = None;
                    }
                    None => return vec![(0..text.len(), Kind::String)],
                }
            }
            None => {}
        }
        while i < bytes.len() {
            let rest = &text[i..];
            let c = rest.chars().next().unwrap_or(' ');
            if lang.line_comments.iter().any(|mark| rest.starts_with(mark)) {
                out.push((i..text.len(), Kind::Comment));
                break;
            }
            if let Some((start, end)) = lang.block_comment
                && rest.starts_with(start)
            {
                match rest[start.len()..].find(end) {
                    Some(at) => {
                        let stop = i + start.len() + at + end.len();
                        out.push((i..stop, Kind::Comment));
                        i = stop;
                    }
                    None => {
                        out.push((i..text.len(), Kind::Comment));
                        self.open = Some(Open::Comment);
                        break;
                    }
                }
                continue;
            }
            if lang.quotes.contains(&c) {
                let triple: String = std::iter::repeat_n(c, 3).collect();
                if lang.triple_quotes && rest.starts_with(&triple) {
                    match rest[3..].find(&triple) {
                        Some(at) => {
                            let stop = i + 3 + at + 3;
                            out.push((i..stop, Kind::String));
                            i = stop;
                        }
                        None => {
                            out.push((i..text.len(), Kind::String));
                            self.open = Some(Open::String(c));
                            break;
                        }
                    }
                    continue;
                }
                // A Rust lifetime ('a) is not a string; a char ('x', '\n') is.
                if c == '\'' && lang.rust {
                    let mut chars = rest[1..].chars();
                    let first = chars.next();
                    if first != Some('\\') && chars.next() != Some('\'') {
                        i += 1;
                        continue;
                    }
                }
                let mut j = i + c.len_utf8();
                let mut escaped = false;
                let mut closed = false;
                for (offset, ch) in text[j..].char_indices() {
                    if escaped {
                        escaped = false;
                    } else if ch == '\\' {
                        escaped = true;
                    } else if ch == c {
                        j += offset + ch.len_utf8();
                        closed = true;
                        break;
                    }
                }
                let stop = if closed { j } else { text.len() };
                out.push((i..stop, Kind::String));
                i = stop;
                continue;
            }
            if c.is_ascii_digit() {
                let stop = i + rest
                    .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '.' || ch == '_'))
                    .unwrap_or(rest.len());
                out.push((i..stop, Kind::Number));
                i = stop;
                continue;
            }
            if lang.annotations && (c == '@' || (c == '#' && rest.starts_with("#["))) {
                let stop = if c == '#' {
                    i + rest.find(']').map_or(rest.len(), |at| at + 1)
                } else {
                    i + 1 + rest[1..].find(|ch: char| !(ch.is_alphanumeric() || ch == '_' || ch == '.')).unwrap_or(rest.len() - 1)
                };
                out.push((i..stop, Kind::Annotation));
                i = stop;
                continue;
            }
            if c.is_alphabetic() || c == '_' || c == '$' {
                let len = rest.find(|ch: char| !(ch.is_alphanumeric() || ch == '_' || ch == '$')).unwrap_or(rest.len());
                let word = &rest[..len];
                let after = rest[len..].trim_start();
                let kind = if lang.keywords.contains(&word) {
                    Some(Kind::Keyword)
                } else if after.starts_with('(') || (after.starts_with("!(") && lang.rust) {
                    Some(Kind::Function)
                } else if lang.capital_types && word.starts_with(|ch: char| ch.is_uppercase()) && word.len() > 1 {
                    Some(Kind::Type)
                } else {
                    None
                };
                if let Some(kind) = kind {
                    out.push((i..i + len, kind));
                }
                i += len;
                continue;
            }
            i += c.len_utf8();
        }
        out
    }
}

/// Every line of `lines` coloured, or nothing for a language this file does not know.
pub fn highlight_lines<S: AsRef<str>>(name: &str, lines: &[S]) -> Option<Vec<Vec<(Range<usize>, Kind)>>> {
    let lang = lang_for(name)?;
    let mut highlighter = Highlighter::new(lang);
    Some(lines.iter().map(|line| highlighter.line(line.as_ref())).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(name: &str, text: &str) -> Vec<(String, Kind)> {
        let mut h = Highlighter::new(lang_for(name).unwrap());
        h.line(text).into_iter().map(|(r, k)| (text[r].to_owned(), k)).collect()
    }

    #[test]
    fn rust_line() {
        let got = kinds("a.rs", r#"pub fn main() { let s = "hi"; foo(42); } // done"#);
        assert!(got.contains(&("pub".into(), Kind::Keyword)));
        assert!(got.contains(&("main".into(), Kind::Function)));
        assert!(got.contains(&("\"hi\"".into(), Kind::String)));
        assert!(got.contains(&("42".into(), Kind::Number)));
        assert!(got.contains(&("// done".into(), Kind::Comment)));
    }

    #[test]
    fn lifetimes_are_not_strings_but_chars_are() {
        let got = kinds("a.rs", "fn f<'a>(x: &'a str) -> char { 'x' }");
        assert!(!got.iter().any(|(t, k)| *k == Kind::String && t.contains("a>")));
        assert!(got.contains(&("'x'".into(), Kind::String)));
    }

    #[test]
    fn block_comments_and_triple_strings_span_lines() {
        let lines = ["x = 1 /* start", "still comment", "end */ y = 2"];
        let all = highlight_lines("a.c", &lines).unwrap();
        assert_eq!(all[1], vec![(0..13, Kind::Comment)]);
        assert_eq!(all[2][0], (0..6, Kind::Comment));
        let py = highlight_lines("a.py", &["s = \"\"\"doc", "more", "end\"\"\" # c"]).unwrap();
        assert_eq!(py[1], vec![(0..4, Kind::String)]);
        assert_eq!(py[2][1].1, Kind::Comment);
    }

    #[test]
    fn unknown_files_are_plain() {
        assert!(lang_for("notes.unknownext").is_none());
        assert!(lang_for("Cargo.toml").is_some());
        assert!(lang_for("rust").is_some(), "the tag of a fenced block works too");
    }

    #[test]
    fn unicode_text_keeps_char_boundaries() {
        let got = kinds("a.py", "привет = 'мир' # комментарий");
        assert!(got.contains(&("'мир'".into(), Kind::String)));
        assert!(got.contains(&("# комментарий".into(), Kind::Comment)));
    }
}
