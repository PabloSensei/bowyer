//! Markdown of answers as blocks the window draws: paragraphs with inline styles, headings,
//! lists, quotes and code. No GPUI here, so the parsing is tested alone.

use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Parser, Tag, TagEnd};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inline {
    Bold,
    Italic,
    Code,
    Link,
    Strike,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rich {
    pub text: String,
    pub spans: Vec<(Range<usize>, Inline)>,
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
}

#[derive(Default)]
struct Builder {
    blocks: Vec<MdBlock>,
    text: String,
    spans: Vec<(Range<usize>, Inline)>,
    open: Vec<(usize, Inline)>,
    /// Lists: the next number of each ordered list, `None` for bullets.
    lists: Vec<Option<u64>>,
    item_marker: Option<String>,
    heading: Option<u8>,
    quote: usize,
    code: Option<(String, String)>,
}

impl Builder {
    fn rich(&mut self) -> Rich {
        let text = std::mem::take(&mut self.text);
        let spans = std::mem::take(&mut self.spans);
        Rich { text: text.trim_end().to_owned(), spans: spans.into_iter().filter(|(r, _)| r.start < r.end).collect() }
    }

    fn flush(&mut self) {
        if self.text.trim().is_empty() {
            self.text.clear();
            self.spans.clear();
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
                Tag::Link { .. } => b.start_inline(Inline::Link),
                Tag::TableCell => {
                    if !b.text.is_empty() && !b.text.ends_with('\n') {
                        b.text.push_str("  │  ");
                    }
                }
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
                TagEnd::Link => b.end_inline(Inline::Link),
                TagEnd::TableRow | TagEnd::TableHead => b.flush(),
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
    fn lists_headings_and_code() {
        let blocks = parse("## Plan\n\n1. first\n2. second\n   - nested\n\n```rust\nfn main() {}\n```\n");
        assert_eq!(blocks[0], MdBlock::Heading(2, Rich { text: "Plan".into(), spans: vec![] }));
        assert!(matches!(&blocks[1], MdBlock::Item { marker, depth: 0, .. } if marker == "1."));
        assert!(matches!(&blocks[2], MdBlock::Item { marker, .. } if marker == "2."));
        assert!(matches!(&blocks[3], MdBlock::Item { marker, depth: 1, .. } if marker == "•"));
        assert_eq!(blocks[4], MdBlock::Code { language: "rust".into(), text: "fn main() {}".into() });
    }
}
