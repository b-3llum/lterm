//! Copy mode: move a cursor through the scrollback with vi keys, select, search, copy.

use winit::keyboard::{Key, NamedKey};

use crate::term::{Selection, Term};

pub enum Outcome {
    Stay,
    Exit,
    /// Copy this text, then exit.
    Copy(String),
}

pub struct CopyMode {
    /// (absolute line, column)
    pub cursor: (i64, usize),
    anchor: Option<(i64, usize)>,
    line_mode: bool,
    pub search: String,
    /// The search being typed, if any.
    pub prompt: Option<String>,
    backwards: bool,
    message: String,
}

fn class(c: char) -> u8 {
    if c.is_whitespace() {
        0
    } else if c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

impl CopyMode {
    pub fn new(term: &Term) -> CopyMode {
        let (x, y) = term.cursor_pos();
        let top = term.view_top();
        // Start at the terminal cursor if it's on screen, else at the bottom of the view.
        let cursor = if term.display_offset == 0 { (term.abs_row(y), x) } else { (top + term.rows as i64 - 1, 0) };
        CopyMode { cursor, anchor: None, line_mode: false, search: String::new(), prompt: None, backwards: false, message: String::new() }
    }

    pub fn start_search(&mut self, backwards: bool) {
        self.backwards = backwards;
        self.prompt = Some(String::new());
    }

    /// The selection to highlight and copy.
    pub fn selection(&self) -> Option<Selection> {
        let a = self.anchor?;
        Some(if self.line_mode {
            let (top, bottom) = (a.0.min(self.cursor.0), a.0.max(self.cursor.0));
            Selection { anchor: (top, 0), head: (bottom, 100_000) }
        } else {
            Selection { anchor: a, head: self.cursor }
        })
    }

    pub fn status(&self) -> String {
        if let Some(p) = &self.prompt {
            return format!(" {}{p}\u{2588}", if self.backwards { '?' } else { '/' });
        }
        let mode = match (self.anchor.is_some(), self.line_mode) {
            (false, _) => "COPY",
            (true, false) => "COPY  VISUAL",
            (true, true) => "COPY  VISUAL LINE",
        };
        let hint = if self.message.is_empty() { "v select  y copy  / search  q quit" } else { &self.message };
        format!(" {mode}   {hint}")
    }

    fn at(term: &Term, (row, col): (i64, usize)) -> char {
        term.line_abs(row).and_then(|l| l.get(col)).map_or(' ', |c| c.ch)
    }

    fn next(term: &Term, (r, c): (i64, usize)) -> Option<(i64, usize)> {
        if c + 1 < term.cols {
            Some((r, c + 1))
        } else if r < term.last_abs() {
            Some((r + 1, 0))
        } else {
            None
        }
    }

    fn prev(term: &Term, (r, c): (i64, usize)) -> Option<(i64, usize)> {
        if c > 0 {
            Some((r, c - 1))
        } else if r > term.first_abs() {
            Some((r - 1, term.cols - 1))
        } else {
            None
        }
    }

    fn word_forward(&mut self, term: &Term) {
        let mut p = self.cursor;
        let start = class(Self::at(term, p));
        while let Some(n) = Self::next(term, p) {
            let wrapped = n.0 != p.0;
            p = n;
            if wrapped || class(Self::at(term, p)) != start {
                break;
            }
        }
        while class(Self::at(term, p)) == 0 {
            match Self::next(term, p) {
                Some(n) => p = n,
                None => break,
            }
        }
        self.cursor = p;
    }

    fn word_back(&mut self, term: &Term) {
        let Some(mut p) = Self::prev(term, self.cursor) else { return };
        while class(Self::at(term, p)) == 0 {
            match Self::prev(term, p) {
                Some(n) => p = n,
                None => break,
            }
        }
        let cls = class(Self::at(term, p));
        while let Some(n) = Self::prev(term, p) {
            if n.0 != p.0 || class(Self::at(term, n)) != cls {
                break;
            }
            p = n;
        }
        self.cursor = p;
    }

    fn line_end(term: &Term, row: i64) -> usize {
        term.line_chars(row).iter().rev().find(|(c, _)| !c.is_whitespace()).map_or(0, |&(_, col)| col)
    }

    /// Jump to the next match of `self.search` (wrapping around the history).
    fn find(&mut self, term: &mut Term, forward: bool) {
        if self.search.is_empty() {
            return;
        }
        let smart_case = self.search.chars().any(char::is_uppercase);
        let norm = |c: char| if smart_case { c } else { c.to_lowercase().next().unwrap_or(c) };
        let needle: Vec<char> = self.search.chars().map(norm).collect();
        let (first, last) = (term.first_abs(), term.last_abs());
        let mut row = self.cursor.0.clamp(first, last);
        for step in 0..=(last - first + 1) {
            let chars = term.line_chars(row);
            let hay: Vec<char> = chars.iter().map(|&(c, _)| norm(c)).collect();
            let starts = (0..hay.len().saturating_sub(needle.len() - 1))
                .filter(|&i| hay[i..i + needle.len()] == needle[..])
                .map(|i| chars[i].1);
            let here = |c: usize| step > 0 || if forward { c > self.cursor.1 } else { c < self.cursor.1 };
            let hit = if forward { starts.filter(|&c| here(c)).min() } else { starts.filter(|&c| here(c)).max() };
            if let Some(col) = hit {
                self.cursor = (row, col);
                self.message = String::new();
                return;
            }
            row = match (forward, row == last, row == first) {
                (true, true, _) => first,
                (true, false, _) => row + 1,
                (false, _, true) => last,
                (false, _, false) => row - 1,
            };
        }
        self.message = format!("not found: {}", self.search);
    }

    pub fn key(&mut self, term: &mut Term, key: &Key, text: Option<&str>, ctrl: bool) -> Outcome {
        if let Some(prompt) = &mut self.prompt {
            match key {
                Key::Named(NamedKey::Escape) => self.prompt = None,
                Key::Named(NamedKey::Enter) => {
                    self.search = self.prompt.take().unwrap_or_default();
                    let forward = !self.backwards;
                    self.find(term, forward);
                }
                Key::Named(NamedKey::Backspace) => {
                    prompt.pop();
                }
                _ => {
                    if let Some(t) = text.filter(|t| !t.chars().any(char::is_control)) {
                        prompt.push_str(t);
                    }
                }
            }
            term.ensure_visible(self.cursor.0);
            return Outcome::Stay;
        }
        let c = match key {
            Key::Named(NamedKey::ArrowLeft) => 'h',
            Key::Named(NamedKey::ArrowDown) => 'j',
            Key::Named(NamedKey::ArrowUp) => 'k',
            Key::Named(NamedKey::ArrowRight) => 'l',
            Key::Named(NamedKey::Home) => '0',
            Key::Named(NamedKey::End) => '$',
            Key::Named(NamedKey::PageUp) => 'B',
            Key::Named(NamedKey::PageDown) => 'F',
            Key::Named(NamedKey::Escape) => 'q',
            Key::Named(NamedKey::Enter) => 'y',
            Key::Named(NamedKey::Space) => 'v',
            Key::Character(s) => match s.chars().next() {
                Some(c) => c,
                None => return Outcome::Stay,
            },
            _ => return Outcome::Stay,
        };
        let page = term.rows as i64 - 1;
        let half = (term.rows as i64 / 2).max(1);
        let (first, last) = (term.first_abs(), term.last_abs());
        let c = match (ctrl, c) {
            (true, 'u') => 'U',
            (true, 'd') => 'D',
            (true, 'b') => 'B',
            (true, 'f') => 'F',
            (true, 'c') => 'q',
            _ => c,
        };
        self.message.clear();
        match c {
            'h' => self.cursor.1 = self.cursor.1.saturating_sub(1),
            'l' => self.cursor.1 = (self.cursor.1 + 1).min(term.cols - 1),
            'j' => self.cursor.0 += 1,
            'k' => self.cursor.0 -= 1,
            'U' => self.cursor.0 -= half,
            'D' => self.cursor.0 += half,
            'B' => self.cursor.0 -= page,
            'F' => self.cursor.0 += page,
            'w' => self.word_forward(term),
            'b' => self.word_back(term),
            '0' => self.cursor.1 = 0,
            '^' => {
                let chars = term.line_chars(self.cursor.0);
                self.cursor.1 = chars.iter().find(|(c, _)| !c.is_whitespace()).map_or(0, |&(_, col)| col);
            }
            '$' => self.cursor.1 = Self::line_end(term, self.cursor.0),
            'g' => self.cursor = (first, 0),
            'G' => self.cursor = (last, 0),
            'H' => self.cursor.0 = term.view_top(),
            'M' => self.cursor.0 = term.view_top() + term.rows as i64 / 2,
            'L' => self.cursor.0 = term.view_top() + page,
            'v' | 'V' => {
                let line = c == 'V';
                if self.anchor.is_some() && self.line_mode == line {
                    self.anchor = None;
                } else {
                    self.anchor.get_or_insert(self.cursor);
                    self.line_mode = line;
                }
            }
            'y' => {
                let sel = self.selection().unwrap_or(Selection {
                    anchor: (self.cursor.0, 0),
                    head: (self.cursor.0, 100_000),
                });
                return Outcome::Copy(term.selection_text(&sel));
            }
            '/' => self.start_search(false),
            '?' => self.start_search(true),
            'n' => {
                let forward = !self.backwards;
                self.find(term, forward);
            }
            'N' => {
                let forward = self.backwards;
                self.find(term, forward);
            }
            'q' | 'i' => return Outcome::Exit,
            _ => {}
        }
        self.cursor.0 = self.cursor.0.clamp(first, last);
        term.ensure_visible(self.cursor.0);
        Outcome::Stay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(text: &str) -> Term {
        let mut t = Term::new(40, 3, 100);
        t.feed(&mut vte::Parser::new(), text.as_bytes());
        t
    }

    fn ch(s: &str) -> Key {
        Key::Character(s.into())
    }

    #[test]
    fn search_moves_through_history_and_wraps() {
        let mut t = term("alpha one\r\nbeta two\r\ngamma one\r\ndelta\r\n$ ");
        let mut cm = CopyMode::new(&t);
        cm.key(&mut t, &ch("?"), Some("?"), false);
        for c in "one".chars() {
            cm.key(&mut t, &ch(&c.to_string()), Some(&c.to_string()), false);
        }
        cm.key(&mut t, &Key::Named(NamedKey::Enter), None, false);
        assert_eq!(cm.cursor, (2, 6)); // "gamma one", searching backwards
        cm.key(&mut t, &ch("n"), Some("n"), false);
        assert_eq!(cm.cursor, (0, 6)); // "alpha one", now in scrollback
        assert_eq!(t.view_top(), 0); // view scrolled to show it
        cm.key(&mut t, &ch("n"), Some("n"), false);
        assert_eq!(cm.cursor, (2, 6)); // wrapped around
    }

    #[test]
    fn select_words_and_copy() {
        let mut t = term("cat foo.txt | grep bar");
        let mut cm = CopyMode::new(&t);
        cm.key(&mut t, &ch("0"), None, false);
        cm.key(&mut t, &ch("w"), None, false);
        assert_eq!(cm.cursor, (0, 4));
        cm.key(&mut t, &ch("v"), None, false);
        cm.key(&mut t, &ch("w"), None, false); // "foo" -> "."
        cm.key(&mut t, &ch("w"), None, false); // "." -> "txt"
        cm.key(&mut t, &ch("l"), None, false);
        cm.key(&mut t, &ch("l"), None, false);
        match cm.key(&mut t, &ch("y"), None, false) {
            Outcome::Copy(s) => assert_eq!(s, "foo.txt"),
            _ => panic!("expected copy"),
        }
        cm.key(&mut t, &ch("b"), None, false);
        assert_eq!(cm.cursor, (0, 8)); // start of "txt"
    }
}
