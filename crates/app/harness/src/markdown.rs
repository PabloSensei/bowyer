//! Markdown of answers as blocks the window draws: paragraphs with inline styles, headings,
//! lists, quotes, code and tables. No GPUI here, so the parsing is tested alone.

use std::ops::Range;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Parser, Tag, TagEnd};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inline {
    Bold,
    Italic,
    Code,
    Link,
    Strike,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Rich {
    pub text: String,
    pub spans: Vec<(Range<usize>, Inline)>,
    /// Where the links of the text go: Markdown links and bare `http(s)://` addresses.
    pub links: Vec<(Range<usize>, String)>,
}

impl Rich {
    /// The address of the link at byte `offset` of the text, if it is one a browser opens.
    #[cfg(test)]
    pub fn link_at(&self, offset: usize) -> Option<&str> {
        self.links.iter().find(|(range, _)| range.start <= offset && offset < range.end).map(|(_, url)| url.as_str()).filter(|url| opens_in_browser(url))
    }
}

/// Only web addresses open from the window: a link the model wrote never starts a program
/// or opens a file.
pub fn opens_in_browser(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://")
}

/// Whether `text` reads as the path of a file or folder: a name with a folder (`src/main.rs`,
/// `target\release\app.exe`, `C:\Users\me`, `~/notes`) or a file name with an extension
/// (`Cargo.toml`). Code, commands and sentences are not: they hold brackets, quotes,
/// operators, or spaces outside an absolute path.
pub fn looks_like_path(text: &str) -> bool {
    let text = text.trim();
    if text.len() < 2 || text.len() > 400 || text.contains("://") || text.starts_with('-') || text.ends_with('.') {
        return false;
    }
    if text.chars().any(|c| c.is_control() || "()[]{}<>=;,|*?\"'`$&!@#%^+".contains(c)) {
        return false;
    }
    let bytes = text.as_bytes();
    let drive = bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/');
    let absolute = drive || text.starts_with("~/") || text.starts_with("~\\") || text.starts_with("\\\\");
    if text.contains(' ') && !absolute {
        return false;
    }
    let foldered = text.contains('/') || text.contains('\\');
    let name = text.rsplit(['/', '\\']).next().unwrap_or(text);
    let extension = name.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty() && (1..=8).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric()) && ext.chars().any(|c| c.is_ascii_alphabetic())
    });
    // `a/b` alone is often a choice ("yes/no"): a folder needs a name with a dot, a drive,
    // a home, or more than one step.
    absolute || extension || (foldered && text.matches(['/', '\\']).count() >= 2)
}

/// A path without the place in the file a model often names after it: `main.rs:42`,
/// `main.rs:42:7`, `main.rs#L42`.
pub fn without_line(path: &str) -> &str {
    let path = path.trim();
    if let Some((file, line)) = path.rsplit_once("#L")
        && !line.is_empty()
        && line.chars().all(|c| c.is_ascii_digit() || c == '-' || c == 'L')
    {
        return file;
    }
    let mut path = path;
    for _ in 0..2 {
        match path.rsplit_once(':') {
            Some((file, number)) if !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()) && file.len() > 1 => path = file,
            _ => break,
        }
    }
    path
}

impl Rich {
    /// The parts of the text that name a file or folder, with the path each names: code
    /// spans that read as a path, links to something other than a web page, and absolute
    /// Windows paths written bare. Whether the path is there is for the caller to find.
    pub fn paths(&self) -> Vec<(Range<usize>, String)> {
        let fits = |r: &Range<usize>| r.start < r.end && r.end <= self.text.len() && self.text.is_char_boundary(r.start) && self.text.is_char_boundary(r.end);
        let mut found: Vec<(Range<usize>, String)> = Vec::new();
        for (range, kind) in &self.spans {
            if *kind == Inline::Code && fits(range) {
                let text = &self.text[range.clone()];
                if looks_like_path(without_line(text)) {
                    found.push((range.clone(), without_line(text).to_owned()));
                }
            }
        }
        for (range, url) in &self.links {
            let url = url.strip_prefix("file:///").or_else(|| url.strip_prefix("file://")).unwrap_or(url);
            if fits(range) && !opens_in_browser(url) && looks_like_path(without_line(url)) {
                found.push((range.clone(), without_line(url).to_owned()));
            }
        }
        // Bare `C:\…` paths: up to the next space, without a closing sentence mark.
        let bytes = self.text.as_bytes();
        let mut at = 0;
        while at + 2 < bytes.len() {
            let drive = bytes[at].is_ascii_alphabetic() && bytes[at + 1] == b':' && bytes[at + 2] == b'\\' && (at == 0 || !bytes[at - 1].is_ascii_alphanumeric());
            if !drive {
                at += 1;
                continue;
            }
            let rest = &self.text[at..];
            let mut end = at + rest.find(|c: char| c.is_whitespace() || "<>\"'`«»()".contains(c)).unwrap_or(rest.len());
            while end > at && self.text[..end].ends_with(['.', ',', ';', ':', '!', '?']) {
                end -= 1;
            }
            let range = at..end;
            let taken = found.iter().any(|(r, _)| r.start < range.end && range.start < r.end);
            if !taken && looks_like_path(&self.text[range.clone()]) {
                found.push((range.clone(), self.text[range.clone()].to_owned()));
            }
            at = end.max(at + 1);
        }
        found
    }
}

/// The bare web addresses of `text`: from `http(s)://` to the next space, without the
/// punctuation that ends a sentence or a closing bracket that has no opening one in it.
fn bare_urls(text: &str) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = ["https://", "http://"].iter().filter_map(|scheme| text[from..].find(scheme)).min() {
        let start = from + at;
        let rest = &text[start..];
        let mut end = start + rest.find(|c: char| c.is_whitespace() || "<>\"'`«»".contains(c)).unwrap_or(rest.len());
        loop {
            let url = &text[start..end];
            let Some(last) = url.chars().last() else { break };
            let unbalanced = match last {
                ')' => url.matches(')').count() > url.matches('(').count(),
                ']' => url.matches(']').count() > url.matches('[').count(),
                '.' | ',' | ';' | ':' | '!' | '?' => true,
                _ => false,
            };
            if !unbalanced {
                break;
            }
            end -= last.len_utf8();
        }
        let scheme_end = start + if rest.starts_with("https://") { 8 } else { 7 };
        if end > scheme_end {
            found.push(start..end);
        }
        from = end.max(scheme_end);
    }
    found
}

impl Rich {
    /// The styles as pieces that do not overlap, in order, each with every style over it:
    /// `**bold `code`**` gives bold, bold and code, bold. The text drawing takes no
    /// overlapping ranges, and one cut inside a character stops the program. Ranges off
    /// the text or off a character boundary are left out.
    pub fn segments(&self) -> Vec<(Range<usize>, Vec<Inline>)> {
        let fits = |at: usize| at <= self.text.len() && self.text.is_char_boundary(at);
        let spans: Vec<&(Range<usize>, Inline)> = self.spans.iter().filter(|(r, _)| r.start < r.end && fits(r.start) && fits(r.end)).collect();
        let mut cuts: Vec<usize> = spans.iter().flat_map(|(r, _)| [r.start, r.end]).collect();
        cuts.sort_unstable();
        cuts.dedup();
        cuts.windows(2)
            .filter_map(|pair| {
                let (start, end) = (pair[0], pair[1]);
                let kinds: Vec<Inline> = spans.iter().filter(|(r, _)| r.start <= start && end <= r.end).map(|(_, kind)| *kind).collect();
                (!kinds.is_empty()).then_some((start..end, kinds))
            })
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MdBlock {
    Paragraph(Rich),
    Heading(u8, Rich),
    /// One item of a list: its depth and its marker ("•" or "3.").
    Item { depth: usize, marker: String, text: Rich },
    Quote(Rich),
    Code { language: String, text: String },
    Rule,
    /// A table: the alignment of each column, and its rows, the head first.
    Table { aligns: Vec<Align>, rows: Vec<Vec<Rich>> },
}

/// How a column of a table lines up its cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Default)]
struct Builder {
    blocks: Vec<MdBlock>,
    text: String,
    spans: Vec<(Range<usize>, Inline)>,
    links: Vec<(Range<usize>, String)>,
    /// The addresses of the Markdown links open now, innermost last.
    link_urls: Vec<(usize, String)>,
    open: Vec<(usize, Inline)>,
    /// Lists: the next number of each ordered list, `None` for bullets.
    lists: Vec<Option<u64>>,
    item_marker: Option<String>,
    heading: Option<u8>,
    quote: usize,
    code: Option<(String, String)>,
    /// The table being read: its columns' alignment and the rows read so far.
    table: Option<(Vec<Align>, Vec<Vec<Rich>>)>,
    /// The cells of the row being read.
    row: Vec<Rich>,
}

impl Builder {
    fn rich(&mut self) -> Rich {
        let text = std::mem::take(&mut self.text);
        let spans = std::mem::take(&mut self.spans);
        let links = std::mem::take(&mut self.links);
        let text = text.trim_end().to_owned();
        let fits = |r: &Range<usize>| r.start < r.end && r.end <= text.len();
        let mut spans: Vec<(Range<usize>, Inline)> = spans.into_iter().filter(|(r, _)| fits(r)).collect();
        let mut links: Vec<(Range<usize>, String)> = links.into_iter().filter(|(r, _)| fits(r)).collect();
        // A bare address is a link too, outside code and Markdown links. Found in the whole
        // text: the parser cuts an address at `_` or `*`.
        let overlaps = |a: &Range<usize>, b: &Range<usize>| a.start < b.end && b.start < a.end;
        for range in bare_urls(&text) {
            let taken = links.iter().any(|(r, _)| overlaps(r, &range)) || spans.iter().any(|(r, kind)| *kind == Inline::Code && overlaps(r, &range));
            if !taken {
                spans.push((range.clone(), Inline::Link));
                links.push((range.clone(), text[range].to_owned()));
            }
        }
        Rich { spans, links, text }
    }

    fn flush(&mut self) {
        if self.text.trim().is_empty() {
            self.text.clear();
            self.spans.clear();
            self.links.clear();
            return;
        }
        let rich = self.rich();
        let block = if let Some(level) = self.heading {
            MdBlock::Heading(level, rich)
        } else if let Some(marker) = self.item_marker.take() {
            MdBlock::Item { depth: self.lists.len().saturating_sub(1), marker, text: rich }
        } else if self.quote > 0 {
            MdBlock::Quote(rich)
        } else {
            MdBlock::Paragraph(rich)
        };
        self.blocks.push(block);
    }

    fn start_inline(&mut self, kind: Inline) {
        self.open.push((self.text.len(), kind));
    }

    fn end_inline(&mut self, kind: Inline) {
        if let Some(index) = self.open.iter().rposition(|(_, k)| *k == kind) {
            let (start, kind) = self.open.remove(index);
            self.spans.push((start..self.text.len(), kind));
        }
    }
}

pub fn parse(markdown: &str) -> Vec<MdBlock> {
    let mut options = pulldown_cmark::Options::empty();
    options.insert(pulldown_cmark::Options::ENABLE_STRIKETHROUGH);
    options.insert(pulldown_cmark::Options::ENABLE_TABLES);
    let mut b = Builder::default();
    for event in Parser::new_ext(markdown, options) {
        if let Some((_, code)) = &mut b.code {
            match event {
                Event::Text(text) => code.push_str(&text),
                Event::End(TagEnd::CodeBlock) => {
                    let (language, text) = b.code.take().unwrap();
                    b.blocks.push(MdBlock::Code { language, text: text.trim_end_matches('\n').to_owned() });
                }
                _ => {}
            }
            continue;
        }
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Heading { level, .. } => {
                    b.flush();
                    b.heading = Some(match level {
                        HeadingLevel::H1 => 1,
                        HeadingLevel::H2 => 2,
                        HeadingLevel::H3 => 3,
                        _ => 4,
                    });
                }
                Tag::BlockQuote(_) => {
                    b.flush();
                    b.quote += 1;
                }
                Tag::CodeBlock(kind) => {
                    b.flush();
                    let language = match kind {
                        CodeBlockKind::Fenced(language) => language.split_whitespace().next().unwrap_or("").to_owned(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    b.code = Some((language, String::new()));
                }
                Tag::List(start) => {
                    b.flush();
                    b.lists.push(start);
                }
                Tag::Item => {
                    b.flush();
                    let marker = match b.lists.last_mut() {
                        Some(Some(number)) => {
                            let marker = format!("{number}.");
                            *number += 1;
                            marker
                        }
                        _ => "•".to_owned(),
                    };
                    b.item_marker = Some(marker);
                }
                Tag::Emphasis => b.start_inline(Inline::Italic),
                Tag::Strong => b.start_inline(Inline::Bold),
                Tag::Strikethrough => b.start_inline(Inline::Strike),
                Tag::Link { dest_url, .. } => {
                    b.link_urls.push((b.text.len(), dest_url.to_string()));
                    b.start_inline(Inline::Link);
                }
                Tag::Table(aligns) => {
                    b.flush();
                    let aligns = aligns
                        .iter()
                        .map(|align| match align {
                            Alignment::Center => Align::Center,
                            Alignment::Right => Align::Right,
                            Alignment::None | Alignment::Left => Align::Left,
                        })
                        .collect();
                    b.table = Some((aligns, Vec::new()));
                }
                Tag::TableHead | Tag::TableRow => b.row.clear(),
                // A cell's text starts clean: whatever came before belongs elsewhere.
                Tag::TableCell => b.flush(),
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => b.flush(),
                TagEnd::Heading(_) => {
                    b.flush();
                    b.heading = None;
                }
                TagEnd::BlockQuote(_) => {
                    b.flush();
                    b.quote = b.quote.saturating_sub(1);
                }
                TagEnd::List(_) => {
                    b.flush();
                    b.lists.pop();
                }
                TagEnd::Item => b.flush(),
                TagEnd::Emphasis => b.end_inline(Inline::Italic),
                TagEnd::Strong => b.end_inline(Inline::Bold),
                TagEnd::Strikethrough => b.end_inline(Inline::Strike),
                TagEnd::Link => {
                    b.end_inline(Inline::Link);
                    if let Some((start, url)) = b.link_urls.pop() {
                        b.links.push((start..b.text.len(), url));
                    }
                }
                TagEnd::TableCell => {
                    let cell = b.rich();
                    b.row.push(cell);
                }
                TagEnd::TableRow | TagEnd::TableHead => {
                    let row = std::mem::take(&mut b.row);
                    if let Some((_, rows)) = &mut b.table {
                        rows.push(row);
                    }
                }
                TagEnd::Table => {
                    if let Some((aligns, rows)) = b.table.take() {
                        b.blocks.push(MdBlock::Table { aligns, rows });
                    }
                }
                _ => {}
            },
            Event::Text(text) => b.text.push_str(&text),
            Event::Code(code) => {
                let start = b.text.len();
                b.text.push_str(&code);
                b.spans.push((start..b.text.len(), Inline::Code));
            }
            Event::SoftBreak => b.text.push(' '),
            Event::HardBreak => b.text.push('\n'),
            Event::Rule => {
                b.flush();
                b.blocks.push(MdBlock::Rule);
            }
            Event::Html(html) | Event::InlineHtml(html) => b.text.push_str(&html),
            _ => {}
        }
    }
    b.flush();
    b.blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_told_from_code_commands_and_choices() {
        for path in ["target\\test-build\\release\\app.exe", "src/main.rs", "Cargo.toml", "C:\\Users\\me\\My Files\\a.txt", "~/notes", "D:/work", "crates/core/envs/src", ".github/workflows/release.yml"] {
            assert!(looks_like_path(path), "{path}");
        }
        for text in ["cargo test -p envs", "foo.bar()", "yes/no", "Ctrl+K", "https://example.com/a.txt", "x = 1", "v1.2.3", "e.g.", "--release", "and so on"] {
            assert!(!looks_like_path(text), "{text}");
        }
        assert_eq!(without_line("src/main.rs:42:7"), "src/main.rs");
        assert_eq!(without_line("src/main.rs#L10-L20"), "src/main.rs");
        assert_eq!(without_line("C:\\a.txt"), "C:\\a.txt");
    }

    #[test]
    fn an_answer_names_its_files_in_code_links_and_bare_paths() {
        let blocks = parse("The build is in `target\\release\\app.exe:12`, see [the notes](notes/todo.md) and C:\\Users\\me\\log.txt. Run `cargo build`, or read [the site](https://example.com).");
        let MdBlock::Paragraph(rich) = &blocks[0] else { panic!("a paragraph") };
        let mut named: Vec<(String, String)> = rich.paths().into_iter().map(|(range, path)| (rich.text[range].to_owned(), path)).collect();
        named.sort();
        assert_eq!(
            named,
            [
                ("C:\\Users\\me\\log.txt".to_owned(), "C:\\Users\\me\\log.txt".to_owned()),
                ("target\\release\\app.exe:12".to_owned(), "target\\release\\app.exe".to_owned()),
                ("the notes".to_owned(), "notes/todo.md".to_owned()),
            ]
        );
    }

    #[test]
    fn a_table_keeps_its_cells_their_styles_and_alignment() {
        let blocks = parse("Before.\n\n| Model | **Price** | Power |\n|---|:---:|--:|\n| Braun | 279 zł | 750 W |\n| Bosch | `307` zł | 850 W |\n\nAfter.");
        assert_eq!(blocks.len(), 3, "{blocks:?}");
        let MdBlock::Table { aligns, rows } = &blocks[1] else { panic!("a table: {blocks:?}") };
        assert_eq!(aligns, &[Align::Left, Align::Center, Align::Right]);
        let texts: Vec<Vec<&str>> = rows.iter().map(|row| row.iter().map(|cell| cell.text.as_str()).collect()).collect();
        assert_eq!(texts, [["Model", "Price", "Power"], ["Braun", "279 zł", "750 W"], ["Bosch", "307 zł", "850 W"]]);
        assert_eq!(rows[0][1].spans, [(0..5, Inline::Bold)]);
        assert_eq!(rows[2][1].spans, [(0..3, Inline::Code)]);
        assert!(matches!(&blocks[2], MdBlock::Paragraph(rich) if rich.text == "After."));
    }

    #[test]
    fn nested_styles_become_pieces_that_do_not_overlap() {
        let blocks = parse("**Харнес (`dsh`) готов** и `код`");
        let MdBlock::Paragraph(rich) = &blocks[0] else { panic!("a paragraph") };
        let segments = rich.segments();
        let mut last = 0;
        for (range, _) in &segments {
            assert!(range.start >= last, "{segments:?}");
            assert!(rich.text.is_char_boundary(range.start) && rich.text.is_char_boundary(range.end));
            last = range.end;
        }
        let code = segments.iter().find(|(r, _)| &rich.text[r.clone()] == "dsh").unwrap();
        assert_eq!(code.1, vec![Inline::Code, Inline::Bold]);
        assert!(segments.iter().any(|(r, kinds)| &rich.text[r.clone()] == "код" && kinds == &vec![Inline::Code]));
    }

    /// Every text of the answers of a real session parses into spans on character
    /// boundaries: `BOWYER_TRANSCRIPT=path.jsonl cargo test -p harness -- --ignored spans`.
    #[test]
    #[ignore = "needs a session file"]
    fn spans_of_a_real_session_sit_on_characters() {
        let path = std::env::var("BOWYER_TRANSCRIPT").expect("BOWYER_TRANSCRIPT");
        let text = std::fs::read_to_string(path).unwrap();
        for line in text.lines() {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            let Some(content) = value.pointer("/message/content").and_then(|c| c.as_array()) else { continue };
            for block in content {
                let Some(text) = block.get("text").and_then(|t| t.as_str()) else { continue };
                for md in parse(text) {
                    let rich = match &md {
                        MdBlock::Paragraph(r) | MdBlock::Heading(_, r) | MdBlock::Quote(r) | MdBlock::Item { text: r, .. } => r,
                        _ => continue,
                    };
                    let mut last = 0;
                    for (range, kind) in rich.segments() {
                        assert!(rich.text.is_char_boundary(range.start) && rich.text.is_char_boundary(range.end));
                        if range.start < last {
                            panic!("overlap {kind:?} {range:?} in {:?}", rich.text.chars().take(160).collect::<String>());
                        }
                        last = range.end;
                    }
                }
            }
        }
    }

    #[test]
    fn inline_styles_keep_their_ranges() {
        let blocks = parse("Use **cargo** and `nextest`, *please*.");
        let MdBlock::Paragraph(rich) = &blocks[0] else { panic!() };
        assert_eq!(rich.text, "Use cargo and nextest, please.");
        assert!(rich.spans.contains(&(4..9, Inline::Bold)));
        assert!(rich.spans.contains(&(14..21, Inline::Code)));
        assert!(rich.spans.contains(&(23..29, Inline::Italic)));
    }

    #[test]
    fn links_know_where_they_go() {
        let blocks = parse(
            "1. **Kiepe** (main): https://allegro.pl/listing?string=kiepe%20studio%20degażówki\n\
             2. See [Kiepe on Ceneo](https://www.ceneo.pl/123), or (https://example.com/a_(b)).\n\
             3. Plain http://example.org. Not a link: `https://in.code` and ftp://x.y\n",
        );
        let link = |block: &MdBlock| match block {
            MdBlock::Item { text, .. } => text.links.iter().map(|(r, url)| (text.text[r.clone()].to_owned(), url.clone())).collect::<Vec<_>>(),
            _ => panic!("an item"),
        };
        let first = "https://allegro.pl/listing?string=kiepe%20studio%20degażówki";
        assert_eq!(link(&blocks[0]), [(first.to_owned(), first.to_owned())]);
        assert_eq!(
            link(&blocks[1]),
            [("Kiepe on Ceneo".to_owned(), "https://www.ceneo.pl/123".to_owned()), ("https://example.com/a_(b)".to_owned(), "https://example.com/a_(b)".to_owned())],
            "a closing bracket of the sentence stays out, the address's own stays in"
        );
        assert_eq!(link(&blocks[2]), [("http://example.org".to_owned(), "http://example.org".to_owned())], "no full stop, nothing in code");
        let MdBlock::Item { text, .. } = &blocks[1] else { unreachable!() };
        let at = text.text.find("Ceneo").unwrap();
        assert_eq!(text.link_at(at), Some("https://www.ceneo.pl/123"));
        assert_eq!(text.link_at(0), None);
        assert!(text.spans.iter().any(|(r, kind)| *kind == Inline::Link && &text.text[r.clone()] == "Kiepe on Ceneo"));
        // Only web addresses open.
        let rich = Rich { text: "x".into(), links: vec![(0..1, "file:///C:/Windows/notepad.exe".into())], ..Default::default() };
        assert_eq!(rich.link_at(0), None);
    }

    #[test]
    fn lists_headings_and_code() {
        let blocks = parse("## Plan\n\n1. first\n2. second\n   - nested\n\n```rust\nfn main() {}\n```\n");
        assert_eq!(blocks[0], MdBlock::Heading(2, Rich { text: "Plan".into(), ..Default::default() }));
        assert!(matches!(&blocks[1], MdBlock::Item { marker, depth: 0, .. } if marker == "1."));
        assert!(matches!(&blocks[2], MdBlock::Item { marker, .. } if marker == "2."));
        assert!(matches!(&blocks[3], MdBlock::Item { marker, depth: 1, .. } if marker == "•"));
        assert_eq!(blocks[4], MdBlock::Code { language: "rust".into(), text: "fn main() {}".into() });
    }
}
