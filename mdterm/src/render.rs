use std::mem::take;
use std::path::Path;

use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, LinkType, Options, Parser, Tag,
    TagEnd,
};

use crate::graphics::{self, ImageOpts, Protocol, Rendered};
use crate::style::{paint, Color, Style};
use crate::wrap::{self, Line, Span};

pub struct Config {
    pub width: usize,
    pub color: bool,
    pub show_urls: bool,
    pub image: ImageOpts,
}

const CODE_BG: Color = Color::Idx(236);
const CODE_FG: Color = Color::Idx(252);
const INLINE_CODE: Color = Color::Idx(215);
const LINK: Color = Color::Idx(39);
const BULLET: Color = Color::Idx(13);
const BORDER: Color = Color::Idx(8);
const QUOTE: Color = Color::Idx(8);

pub fn render(src: &str, base: &Path, cfg: &Config) -> String {
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_GFM
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS;
    let mut r = Renderer::new(cfg, base);
    for ev in Parser::new_ext(src, opts) {
        r.event(ev);
    }
    r.finish()
}

/// Indentation drawn before each line of a nested block (quote bar, list marker).
struct Prefix {
    first: Line,
    rest: Line,
    used: bool,
}

struct Table {
    aligns: Vec<Alignment>,
    /// (is_header, cells)
    rows: Vec<(bool, Vec<Vec<Span>>)>,
}

struct Renderer<'a> {
    cfg: &'a Config,
    base: &'a Path,
    out: String,
    styles: Vec<Style>,
    inline: Vec<Span>,
    prefixes: Vec<Prefix>,
    /// Next number for ordered lists, None for bullet lists.
    lists: Vec<Option<u64>>,
    /// Per open list: whether it's loose (items are paragraphs, separated by blank lines).
    loose: Vec<bool>,
    need_blank: bool,
    /// (language, content) while inside a code block.
    code: Option<(String, String)>,
    /// (destination, kind, index into `inline` where the link text starts)
    links: Vec<(String, LinkType, usize)>,
    /// (source, alt text) while inside an image tag.
    image: Option<(String, String)>,
    table: Option<Table>,
    in_metadata: bool,
    in_html_comment: bool,
}

impl<'a> Renderer<'a> {
    fn new(cfg: &'a Config, base: &'a Path) -> Self {
        Renderer {
            cfg,
            base,
            out: String::new(),
            styles: Vec::new(),
            inline: Vec::new(),
            prefixes: Vec::new(),
            lists: Vec::new(),
            loose: Vec::new(),
            need_blank: false,
            code: None,
            links: Vec::new(),
            image: None,
            table: None,
            in_metadata: false,
            in_html_comment: false,
        }
    }

    fn finish(mut self) -> String {
        self.flush_inline();
        let trimmed = self.out.trim_end_matches('\n').len();
        self.out.truncate(trimmed);
        self.out.push('\n');
        self.out
    }

    fn cur(&self) -> Style {
        self.styles.last().copied().unwrap_or_default()
    }

    fn push_style(&mut self, f: impl FnOnce(&mut Style)) {
        let mut st = self.cur();
        f(&mut st);
        self.styles.push(st);
    }

    fn push(&mut self, text: &str, style: Style) {
        self.inline.push(Span::new(text, style));
    }

    fn event(&mut self, ev: Event) {
        if self.in_metadata {
            if let Event::End(TagEnd::MetadataBlock(_)) = ev {
                self.in_metadata = false;
            }
            return;
        }
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.text(&t),
            Event::Code(t) => self.inline_code(&t),
            Event::Html(t) | Event::InlineHtml(t) => self.html(&t),
            Event::SoftBreak => self.text(" "),
            Event::HardBreak => self.push("\n", self.cur()),
            Event::Rule => {
                self.start_block();
                let w = self.avail();
                self.emit_line(vec![Span::new("─".repeat(w), Style::color(BORDER))]);
                self.need_blank = true;
            }
            Event::FootnoteReference(label) => {
                let st = Style { fg: Some(LINK), ..self.cur() };
                self.push(&format!("[^{label}]"), st);
            }
            Event::TaskListMarker(done) => {
                let (mark, c) = if done { ("[x] ", Color::Idx(10)) } else { ("[ ] ", BORDER) };
                self.push(mark, Style::color(c));
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {
                // Tight lists put item text straight in the item; a paragraph means loose.
                if let Some(l) = self.loose.last_mut() {
                    *l = true;
                }
                self.start_block();
            }
            Tag::HtmlBlock => self.start_block(),
            Tag::Heading { level, .. } => {
                self.start_block();
                let (color, marker) = match level {
                    HeadingLevel::H1 => (13, ""),
                    HeadingLevel::H2 => (14, ""),
                    HeadingLevel::H3 => (10, "### "),
                    HeadingLevel::H4 => (11, "#### "),
                    HeadingLevel::H5 => (12, "##### "),
                    HeadingLevel::H6 => (12, "###### "),
                };
                let st = Style { bold: true, fg: Some(Color::Idx(color)), ..Default::default() };
                self.styles.push(st);
                if !marker.is_empty() {
                    self.push(marker, Style { dim: true, ..st });
                }
            }
            Tag::BlockQuote(kind) => {
                self.start_block();
                let alert = kind.map(|k| match k {
                    BlockQuoteKind::Note => ("Note", 12),
                    BlockQuoteKind::Tip => ("Tip", 10),
                    BlockQuoteKind::Important => ("Important", 13),
                    BlockQuoteKind::Warning => ("Warning", 11),
                    BlockQuoteKind::Caution => ("Caution", 9),
                });
                let bar = Style::color(alert.map_or(QUOTE, |(_, c)| Color::Idx(c)));
                let line = vec![Span::new("│ ", bar)];
                self.prefixes.push(Prefix { first: line.clone(), rest: line, used: false });
                if let Some((title, c)) = alert {
                    let st = Style { bold: true, fg: Some(Color::Idx(c)), ..Default::default() };
                    self.emit_line(vec![Span::new(title, st)]);
                }
                let italic = alert.is_none();
                self.push_style(|s| s.italic |= italic);
            }
            Tag::CodeBlock(kind) => {
                self.start_block();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split([' ', ',', '{']).next().unwrap_or("").to_string()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            Tag::List(start) => {
                if !self.lists.is_empty() {
                    // A nested list hugs its parent item, even in a loose list.
                    self.flush_inline();
                    self.need_blank = false;
                }
                self.start_block();
                self.lists.push(start);
                self.loose.push(false);
            }
            Tag::Item => {
                self.start_block();
                let depth = self.lists.len().saturating_sub(1);
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => format!("{} ", ["•", "◦", "▪"][depth % 3]),
                };
                let w = wrap::width(&marker);
                self.prefixes.push(Prefix {
                    first: vec![Span::new(marker, Style::color(BULLET))],
                    rest: vec![Span::plain(" ".repeat(w))],
                    used: false,
                });
            }
            Tag::FootnoteDefinition(label) => {
                self.start_block();
                let marker = format!("[^{label}]: ");
                let w = wrap::width(&marker);
                self.prefixes.push(Prefix {
                    first: vec![Span::new(marker, Style::color(LINK))],
                    rest: vec![Span::plain(" ".repeat(w))],
                    used: false,
                });
            }
            Tag::Table(aligns) => {
                self.start_block();
                self.table = Some(Table { aligns, rows: Vec::new() });
            }
            Tag::TableHead | Tag::TableRow => {
                let head = matches!(tag, Tag::TableHead);
                if let Some(t) = &mut self.table {
                    t.rows.push((head, Vec::new()));
                }
                if head {
                    self.push_style(|s| s.bold = true);
                }
            }
            Tag::TableCell => self.inline.clear(),
            Tag::Emphasis => self.push_style(|s| s.italic = true),
            Tag::Strong => self.push_style(|s| s.bold = true),
            Tag::Strikethrough => self.push_style(|s| s.strike = true),
            Tag::Link { link_type, dest_url, .. } => {
                self.links.push((dest_url.to_string(), link_type, self.inline.len()));
                self.push_style(|s| {
                    s.underline = true;
                    s.fg = Some(LINK);
                });
            }
            Tag::Image { dest_url, .. } => self.image = Some((dest_url.to_string(), String::new())),
            Tag::MetadataBlock(_) => self.in_metadata = true,
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::HtmlBlock => {
                self.flush_inline();
                self.need_blank = true;
            }
            TagEnd::Heading(level) => {
                let spans = take(&mut self.inline);
                let lines = wrap::wrap(&spans, self.avail());
                let w = lines.iter().map(|l| wrap::line_width(l)).max().unwrap_or(0);
                let st = self.cur();
                self.styles.pop();
                for l in lines {
                    self.emit_line(l);
                }
                let rule = match level {
                    HeadingLevel::H1 => "═",
                    HeadingLevel::H2 => "─",
                    _ => "",
                };
                if !rule.is_empty() {
                    self.emit_line(vec![Span::new(rule.repeat(w), Style { bold: false, ..st })]);
                }
                self.need_blank = true;
            }
            TagEnd::BlockQuote(_) => {
                self.flush_inline();
                self.prefixes.pop();
                self.styles.pop();
                self.need_blank = true;
            }
            TagEnd::CodeBlock => {
                if let Some((lang, code)) = self.code.take() {
                    self.code_block(&lang, &code);
                }
                self.need_blank = true;
            }
            TagEnd::List(_) => {
                self.flush_inline();
                self.lists.pop();
                self.loose.pop();
                // After a nested list, the next item gets a blank line only in a loose list.
                self.need_blank |= self.loose.last().copied().unwrap_or(true);
            }
            TagEnd::Item | TagEnd::FootnoteDefinition => {
                self.flush_inline();
                if self.prefixes.last().is_some_and(|p| !p.used) {
                    self.emit_line(Vec::new()); // empty item: still show its marker
                }
                self.prefixes.pop();
                if tag == TagEnd::FootnoteDefinition {
                    self.need_blank = true;
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.table_block(t);
                }
                self.need_blank = true;
            }
            TagEnd::TableHead => {
                self.styles.pop();
            }
            TagEnd::TableCell => {
                let cell = take(&mut self.inline);
                if let Some(row) = self.table.as_mut().and_then(|t| t.rows.last_mut()) {
                    row.1.push(cell);
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                let Some((dest, kind, start)) = self.links.pop() else { return };
                if !self.cfg.show_urls
                    || dest.is_empty()
                    || dest.starts_with('#')
                    || matches!(kind, LinkType::Autolink | LinkType::Email)
                {
                    return;
                }
                let start = start.min(self.inline.len());
                let text: String = self.inline[start..].iter().map(|s| s.text.as_str()).collect();
                if text.trim() != dest {
                    let st = Style { dim: true, ..self.cur() };
                    self.push(&format!(" ({dest})"), st);
                }
            }
            TagEnd::Image => {
                if let Some((src, alt)) = self.image.take() {
                    self.image(&src, &alt);
                }
            }
            _ => {}
        }
    }

    fn text(&mut self, t: &str) {
        if let Some((_, alt)) = &mut self.image {
            alt.push_str(t);
        } else if let Some((_, code)) = &mut self.code {
            code.push_str(t);
        } else {
            self.push(t, self.cur());
        }
    }

    fn inline_code(&mut self, t: &str) {
        if let Some((_, alt)) = &mut self.image {
            alt.push_str(t);
        } else if self.cfg.color {
            let st = Style { fg: Some(INLINE_CODE), bg: Some(CODE_BG), ..self.cur() };
            self.push(t, st);
        } else {
            self.push(&format!("`{t}`"), self.cur());
        }
    }

    /// Raw HTML: draw `<img>` tags, honour `<br>`, drop comments and other tags.
    fn html(&mut self, s: &str) {
        let mut rest = s;
        let mut text = String::new();
        loop {
            if self.in_html_comment {
                match rest.find("-->") {
                    Some(i) => {
                        rest = &rest[i + 3..];
                        self.in_html_comment = false;
                        continue;
                    }
                    None => break,
                }
            }
            let Some(i) = rest.find('<') else {
                text.push_str(rest);
                break;
            };
            text.push_str(&rest[..i]);
            let after = &rest[i..];
            if let Some(stripped) = after.strip_prefix("<!--") {
                self.in_html_comment = true;
                rest = stripped;
                continue;
            }
            let end = after.find('>').map_or(after.len(), |e| e + 1);
            let tag = &after[..end];
            rest = &after[end..];
            let lower = tag.to_ascii_lowercase();
            if lower.starts_with("<img") || lower.starts_with("<br") {
                self.html_text(&take(&mut text));
                if lower.starts_with("<br") {
                    self.push("\n", self.cur());
                } else {
                    let src = html_attr(tag, "src").unwrap_or_default();
                    let alt = html_attr(tag, "alt").unwrap_or_default();
                    self.image(&src, &alt);
                }
            }
        }
        self.html_text(&text);
    }

    fn html_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let text = text
            .replace('\n', " ")
            .replace("&nbsp;", " ")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&amp;", "&");
        self.push(&text, self.cur());
    }

    fn image(&mut self, src: &str, alt: &str) {
        if self.cfg.image.protocol != Protocol::None && self.table.is_none() {
            if let Ok(img) = graphics::load(src, self.base) {
                self.flush_inline();
                match graphics::render(&img, self.avail(), &self.cfg.image) {
                    Rendered::Rows(rows) => rows.iter().for_each(|r| self.emit_raw(r)),
                    Rendered::Raw(seq) => self.emit_raw(&seq),
                }
                return;
            }
        }
        // Unsupported (e.g. SVG badges), missing, or images disabled: show alt text.
        let label = if alt.trim().is_empty() { src } else { alt };
        let st = Style { fg: Some(Color::Idx(5)), ..self.cur() };
        self.push(&format!("[image: {label}]"), st);
    }

    fn code_block(&mut self, lang: &str, code: &str) {
        let avail = self.avail();
        let code = code.replace('\t', "    ");
        let lines: Vec<&str> = code.trim_end_matches('\n').split('\n').collect();
        if !self.cfg.color {
            for l in lines {
                for part in wrap::hard_wrap(l, avail.saturating_sub(4)) {
                    self.emit_line(vec![Span::plain(format!("    {part}"))]);
                }
            }
            return;
        }
        let bg = Style { bg: Some(CODE_BG), ..Default::default() };
        let label = if lang.is_empty() { String::new() } else { format!(" {lang} ") };
        let pad = avail.saturating_sub(wrap::width(&label));
        let label_st = Style { dim: true, italic: true, ..bg };
        self.emit_line(vec![Span::new(" ".repeat(pad), bg), Span::new(label, label_st)]);
        let inner = avail.saturating_sub(2).max(1);
        let text_st = Style { fg: Some(CODE_FG), ..bg };
        for l in lines {
            for part in wrap::hard_wrap(l, inner) {
                let fill = avail.saturating_sub(1 + wrap::width(&part));
                self.emit_line(vec![
                    Span::new(" ", bg),
                    Span::new(part, text_st),
                    Span::new(" ".repeat(fill), bg),
                ]);
            }
        }
        self.emit_line(vec![Span::new(" ".repeat(avail), bg)]);
    }

    fn table_block(&mut self, t: Table) {
        let ncols = t.rows.iter().map(|r| r.1.len()).max().unwrap_or(0).max(t.aligns.len());
        if ncols == 0 {
            return;
        }
        let mut widths = vec![1usize; ncols];
        for (_, row) in &t.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(wrap::line_width(cell));
            }
        }
        // Shrink the widest columns until the table fits; cells wrap.
        let room = self.avail().saturating_sub(3 * ncols + 1).max(ncols);
        while widths.iter().sum::<usize>() > room {
            let (i, &w) = widths.iter().enumerate().max_by_key(|(_, w)| **w).unwrap();
            if w <= 1 {
                break;
            }
            widths[i] -= 1;
        }

        let b = Style::color(BORDER);
        let border = |l: &str, m: &str, r: &str| -> Line {
            let segs: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
            vec![Span::new(format!("{l}{}{r}", segs.join(m)), b)]
        };
        self.emit_line(border("┌", "┬", "┐"));
        let nrows = t.rows.len();
        for (ri, (head, row)) in t.rows.iter().enumerate() {
            let cells: Vec<Vec<Line>> = (0..ncols)
                .map(|i| row.get(i).map_or_else(|| vec![Vec::new()], |c| wrap::wrap(c, widths[i])))
                .collect();
            let height = cells.iter().map(Vec::len).max().unwrap_or(1);
            for li in 0..height {
                let mut line: Line = vec![Span::new("│", b)];
                for (i, cell) in cells.iter().enumerate() {
                    let content = cell.get(li).cloned().unwrap_or_default();
                    let pad = widths[i].saturating_sub(wrap::line_width(&content));
                    let (lp, rp) = match t.aligns.get(i) {
                        Some(Alignment::Right) => (pad, 0),
                        Some(Alignment::Center) => (pad / 2, pad - pad / 2),
                        _ => (0, pad),
                    };
                    line.push(Span::plain(" ".repeat(lp + 1)));
                    line.extend(content);
                    line.push(Span::plain(" ".repeat(rp + 1)));
                    line.push(Span::new("│", b));
                }
                self.emit_line(line);
            }
            if *head && ri + 1 < nrows {
                self.emit_line(border("├", "┼", "┤"));
            }
        }
        self.emit_line(border("└", "┴", "┘"));
    }

    // ---- output helpers ----

    /// Columns left for content after the current indentation.
    fn avail(&self) -> usize {
        let used: usize = self.prefixes.iter().map(|p| wrap::line_width(&p.first)).sum();
        self.cfg.width.saturating_sub(used).max(10)
    }

    fn start_block(&mut self) {
        self.flush_inline();
        if self.need_blank && !self.out.is_empty() {
            let mut spans: Line = self.prefixes.iter().flat_map(|p| p.rest.clone()).collect();
            while let Some(last) = spans.last_mut() {
                let t = last.text.trim_end().to_string();
                if t.is_empty() {
                    spans.pop();
                } else {
                    last.text = t;
                    break;
                }
            }
            self.write_spans(&spans);
            self.out.push('\n');
        }
        self.need_blank = false;
    }

    fn flush_inline(&mut self) {
        if self.inline.iter().all(|s| s.text.trim().is_empty()) {
            self.inline.clear();
            return;
        }
        let spans = take(&mut self.inline);
        for line in wrap::wrap(&spans, self.avail()) {
            self.emit_line(line);
        }
    }

    fn take_prefix(&mut self) -> Line {
        let mut out = Vec::new();
        for p in &mut self.prefixes {
            out.extend(if p.used { p.rest.clone() } else { p.first.clone() });
            p.used = true;
        }
        out
    }

    fn emit_line(&mut self, line: Line) {
        let mut spans = self.take_prefix();
        spans.extend(line);
        self.write_spans(&spans);
        self.out.push('\n');
    }

    /// Emit pre-built escape sequences (images) after the indentation.
    fn emit_raw(&mut self, raw: &str) {
        let prefix = self.take_prefix();
        self.write_spans(&prefix);
        self.out.push_str(raw);
        self.out.push('\n');
    }

    fn write_spans(&mut self, spans: &[Span]) {
        for s in spans {
            paint(&mut self.out, &s.text, s.style, self.cfg.color);
        }
    }
}

/// Value of attribute `name` in an HTML tag, quoted or not.
fn html_attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{name}=");
    let mut from = 0;
    while let Some(i) = lower[from..].find(&needle).map(|i| i + from) {
        from = i + needle.len();
        if !lower[..i].ends_with(|c: char| c.is_whitespace()) {
            continue;
        }
        let v = &tag[from..];
        return Some(match v.chars().next() {
            Some(q @ ('"' | '\'')) => v[1..].split(q).next().unwrap_or("").to_string(),
            _ => v.split(|c: char| c.is_whitespace() || c == '>').next().unwrap_or("").to_string(),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(md: &str) -> String {
        let cfg = Config {
            width: 40,
            color: false,
            show_urls: true,
            image: ImageOpts { protocol: Protocol::None, max_rows: 0, cell_w: 10, cell_h: 20 },
        };
        render(md, Path::new("."), &cfg)
    }

    #[test]
    fn lists_and_links() {
        let out = plain("- one [site](https://x.io)\n  - two\n- three\n");
        assert_eq!(out, "• one site (https://x.io)\n  ◦ two\n• three\n");
    }

    #[test]
    fn loose_list_nested_has_no_gap() {
        let out = plain("1. parent\n\n   1. child\n   2. child\n\n2. next\n");
        assert_eq!(out, "1. parent\n   1. child\n   2. child\n\n2. next\n");
    }

    #[test]
    fn table_layout() {
        let out = plain("| a | bb |\n|---|---:|\n| 1 | 2 |\n");
        assert_eq!(out, "┌───┬────┐\n│ a │ bb │\n├───┼────┤\n│ 1 │  2 │\n└───┴────┘\n");
    }

    #[test]
    fn escapes_are_neutralised() {
        assert_eq!(plain("evil \x1b[2J text"), "evil \u{FFFD}[2J text\n");
    }

    #[test]
    fn html_img_attr() {
        assert_eq!(html_attr(r#"<img alt="logo" src='a b.png'>"#, "src").as_deref(), Some("a b.png"));
        assert_eq!(html_attr("<img data-src=x src=y.png>", "src").as_deref(), Some("y.png"));
    }
}
