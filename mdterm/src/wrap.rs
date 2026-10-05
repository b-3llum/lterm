use std::mem::take;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::style::Style;

#[derive(Clone, Debug)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

impl Span {
    pub fn new(text: impl Into<String>, style: Style) -> Span {
        Span { text: text.into(), style }
    }

    pub fn plain(text: impl Into<String>) -> Span {
        Span::new(text, Style::default())
    }
}

pub type Line = Vec<Span>;

pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

pub fn line_width(line: &[Span]) -> usize {
    line.iter().map(|s| width(&s.text)).sum()
}

enum Tok {
    /// A run of non-space text, possibly spanning several styles.
    Word(Vec<Span>),
    Space(Style),
    Break,
}

fn tokenize(spans: &[Span]) -> Vec<Tok> {
    let mut toks = Vec::new();
    let mut word: Vec<Span> = Vec::new();
    for sp in spans {
        let mut cur = String::new();
        for ch in sp.text.chars() {
            if ch.is_whitespace() {
                if !cur.is_empty() {
                    word.push(Span::new(take(&mut cur), sp.style));
                }
                if !word.is_empty() {
                    toks.push(Tok::Word(take(&mut word)));
                }
                toks.push(if ch == '\n' { Tok::Break } else { Tok::Space(sp.style) });
            } else {
                cur.push(ch);
            }
        }
        if !cur.is_empty() {
            word.push(Span::new(cur, sp.style));
        }
    }
    if !word.is_empty() {
        toks.push(Tok::Word(word));
    }
    toks
}

fn push_merged(line: &mut Line, text: &str, style: Style) {
    match line.last_mut() {
        Some(last) if last.style == style => last.text.push_str(text),
        _ => line.push(Span::new(text, style)),
    }
}

/// Word-wrap styled text to `max` columns. `\n` forces a line break.
pub fn wrap(spans: &[Span], max: usize) -> Vec<Line> {
    let max = max.max(1);
    let mut lines = Vec::new();
    let mut line: Line = Vec::new();
    let mut lw = 0;
    let mut space: Option<Style> = None;

    for tok in tokenize(spans) {
        match tok {
            Tok::Space(st) => {
                if lw > 0 && space.is_none() {
                    space = Some(st);
                }
            }
            Tok::Break => {
                lines.push(take(&mut line));
                lw = 0;
                space = None;
            }
            Tok::Word(word) => {
                let ww = line_width(&word);
                let sp = space.take().filter(|_| lw > 0);
                let gap = sp.is_some() as usize;
                if lw + gap + ww <= max {
                    if let Some(st) = sp {
                        push_merged(&mut line, " ", st);
                        lw += 1;
                    }
                } else if lw > 0 {
                    lines.push(take(&mut line));
                    lw = 0;
                }
                if lw + ww <= max {
                    for s in &word {
                        push_merged(&mut line, &s.text, s.style);
                    }
                    lw += ww;
                } else {
                    // Longer than a whole line: split by characters.
                    for s in &word {
                        for ch in s.text.chars() {
                            let cw = ch.width().unwrap_or(0);
                            if lw + cw > max && lw > 0 {
                                lines.push(take(&mut line));
                                lw = 0;
                            }
                            push_merged(&mut line, ch.encode_utf8(&mut [0; 4]), s.style);
                            lw += cw;
                        }
                    }
                }
            }
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// Split a line at exactly `max` columns, preserving all whitespace (for code).
pub fn hard_wrap(s: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > max && w > 0 {
            out.push(take(&mut cur));
            w = 0;
        }
        cur.push(ch);
        w += cw;
    }
    out.push(cur);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.iter().map(|s| s.text.as_str()).collect()).collect()
    }

    #[test]
    fn wraps_words() {
        let l = wrap(&[Span::plain("the quick brown fox jumps")], 10);
        assert_eq!(text(&l), ["the quick", "brown fox", "jumps"]);
    }

    #[test]
    fn splits_long_words_and_breaks() {
        let l = wrap(&[Span::plain("abcdefghij xy\nz")], 4);
        assert_eq!(text(&l), ["abcd", "efgh", "ij", "xy", "z"]);
    }

    #[test]
    fn wide_chars() {
        let l = wrap(&[Span::plain("日本語テキスト")], 6);
        assert_eq!(text(&l), ["日本語", "テキス", "ト"]);
    }
}
