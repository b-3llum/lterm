//! Terminal state: screen grid, scrollback, cursor, modes, and images.
//! Escape sequences are parsed by `vte`; kitty graphics (APC) is pre-filtered here
//! because `vte` discards APC strings.

use std::collections::{HashMap, VecDeque};
use std::mem::take;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use unicode_width::UnicodeWidthChar;
use vte::{Params, Parser, Perform};

use crate::graphics::{self, Image, KittyCmd, Sixel};

pub const BOLD: u16 = 1 << 0;
pub const DIM: u16 = 1 << 1;
pub const ITALIC: u16 = 1 << 2;
pub const UNDERLINE: u16 = 1 << 3;
pub const INVERSE: u16 = 1 << 4;
pub const HIDDEN: u16 = 1 << 5;
pub const STRIKE: u16 = 1 << 6;
/// First half of a double-width character.
pub const WIDE: u16 = 1 << 7;
/// Second half of a double-width character (drawn by the WIDE cell).
pub const SPACER: u16 = 1 << 8;

const MAX_PLACEMENTS: usize = 256;
const MAX_STORED_IMAGES: usize = 64;
const MAX_APC: usize = 128 << 20;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Color {
    Default,
    Idx(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: u16,
}

impl Default for Cell {
    fn default() -> Self {
        Cell { ch: ' ', fg: Color::Default, bg: Color::Default, flags: 0 }
    }
}

pub type Row = Vec<Cell>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseMode {
    Off,
    Click,
    Drag,
    Motion,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorShape {
    Block,
    Underline,
    Bar,
}

pub struct Modes {
    pub app_cursor: bool,
    pub autowrap: bool,
    pub cursor_visible: bool,
    pub bracketed_paste: bool,
    pub mouse: MouseMode,
    pub mouse_sgr: bool,
    pub focus_events: bool,
    pub insert: bool,
    pub newline: bool,
}

impl Default for Modes {
    fn default() -> Self {
        Modes {
            app_cursor: false,
            autowrap: true,
            cursor_visible: true,
            bracketed_paste: false,
            mouse: MouseMode::Off,
            mouse_sgr: false,
            focus_events: false,
            insert: false,
            newline: false,
        }
    }
}

/// An image shown on screen, anchored to a line so it scrolls with the text.
pub struct Placement {
    pub image: Arc<Image>,
    pub id: u32,
    /// Absolute line of the top edge (see `Term::abs_row`).
    pub row: i64,
    pub col: usize,
    /// Size in cells; fractional so it follows font zoom.
    pub w_cells: f32,
    pub h_cells: f32,
}

/// A text selection in absolute line coordinates.
#[derive(Clone, Copy, Debug)]
pub struct Selection {
    pub anchor: (i64, usize),
    pub head: (i64, usize),
}

impl Selection {
    pub fn range(&self) -> ((i64, usize), (i64, usize)) {
        if self.anchor <= self.head { (self.anchor, self.head) } else { (self.head, self.anchor) }
    }

    pub fn contains(&self, row: i64, col: usize) -> bool {
        let (s, e) = self.range();
        (row, col) >= s && (row, col) <= e
    }
}

#[derive(Clone, Copy, Default)]
struct Cursor {
    x: usize,
    y: usize,
    pending_wrap: bool,
    pen: Cell,
    origin: bool,
    /// G0/G1 designated as DEC special graphics.
    charsets: [bool; 2],
    shift_out: bool,
}

struct Screen {
    lines: Vec<Row>,
    images: Vec<Placement>,
    saved: Cursor,
}

impl Screen {
    fn new(cols: usize, rows: usize) -> Screen {
        Screen { lines: vec![vec![Cell::default(); cols]; rows], images: Vec::new(), saved: Cursor::default() }
    }
}

#[derive(Default)]
enum Dcs {
    #[default]
    None,
    Sixel(Box<Sixel>),
    Decrqss,
    Ignore,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Apc {
    Normal,
    Esc,
    Body,
    BodyEsc,
}

pub struct Term {
    pub cols: usize,
    pub rows: usize,
    screen: Screen,
    /// The inactive screen (primary while the alternate screen is shown).
    other: Screen,
    pub alt: bool,
    scrollback: VecDeque<Row>,
    scrollback_max: usize,
    /// Lines that have ever scrolled off the top of the primary screen.
    scrolled: i64,
    /// How many lines the view is scrolled back into history.
    pub display_offset: usize,
    cursor: Cursor,
    top: usize,
    bot: usize,
    tabs: Vec<bool>,
    pub modes: Modes,
    pub cursor_shape: CursorShape,
    /// New window title, taken by the front end.
    pub title: Option<String>,
    /// The last title set, kept for snapshots.
    current_title: String,
    pub bell: bool,
    /// Replies to the program (device reports etc.), written back to the pty.
    pub responses: Vec<u8>,
    /// Cell size in pixels, used for image sizing and pixel reports.
    pub cell_px: (usize, usize),
    /// Default foreground/background, reported for OSC 10/11 queries.
    pub default_colors: ([u8; 3], [u8; 3]),
    last_char: Option<char>,
    dcs: Dcs,
    apc: Apc,
    apc_buf: Vec<u8>,
    kitty_images: HashMap<u32, Arc<Image>>,
    kitty_pending: Option<(KittyCmd, Vec<u8>)>,
}

fn default_tabs(cols: usize) -> Vec<bool> {
    (0..cols).map(|i| i % 8 == 0 && i > 0).collect()
}

fn dec_graphics(c: char) -> char {
    match c {
        '`' => '◆', 'a' => '▒', 'f' => '°', 'g' => '±', 'j' => '┘', 'k' => '┐', 'l' => '┌',
        'm' => '└', 'n' => '┼', 'o' => '⎺', 'p' => '⎻', 'q' => '─', 'r' => '⎼', 's' => '⎽',
        't' => '├', 'u' => '┤', 'v' => '┴', 'w' => '┬', 'x' => '│', 'y' => '≤', 'z' => '≥',
        '{' => 'π', '|' => '≠', '}' => '£', '~' => '·',
        _ => c,
    }
}

impl Term {
    pub fn new(cols: usize, rows: usize, scrollback_max: usize) -> Term {
        let (cols, rows) = (cols.max(2), rows.max(1));
        Term {
            cols,
            rows,
            screen: Screen::new(cols, rows),
            other: Screen::new(cols, rows),
            alt: false,
            scrollback: VecDeque::new(),
            scrollback_max,
            scrolled: 0,
            display_offset: 0,
            cursor: Cursor::default(),
            top: 0,
            bot: rows - 1,
            tabs: default_tabs(cols),
            modes: Modes::default(),
            cursor_shape: CursorShape::Block,
            title: None,
            current_title: String::new(),
            bell: false,
            responses: Vec::new(),
            cell_px: (8, 16),
            default_colors: ([0xc0, 0xca, 0xf5], [0x1a, 0x1b, 0x26]),
            last_char: None,
            dcs: Dcs::None,
            apc: Apc::Normal,
            apc_buf: Vec::new(),
            kitty_images: HashMap::new(),
            kitty_pending: None,
        }
    }

    /// Process program output.
    pub fn feed(&mut self, parser: &mut Parser, bytes: &[u8]) {
        let mut normal = Vec::with_capacity(bytes.len());
        for &b in bytes {
            match self.apc {
                Apc::Normal => {
                    if b == 0x1b {
                        self.apc = Apc::Esc;
                    } else {
                        normal.push(b);
                    }
                }
                Apc::Esc => {
                    if b == b'_' {
                        self.apc = Apc::Body;
                        self.apc_buf.clear();
                    } else {
                        normal.push(0x1b);
                        if b != 0x1b {
                            normal.push(b);
                            self.apc = Apc::Normal;
                        }
                    }
                }
                Apc::Body | Apc::BodyEsc if b == b'\\' && self.apc == Apc::BodyEsc || b == 0x07 => {
                    // Text before the APC must be processed before it.
                    parser.advance(self, &normal);
                    normal.clear();
                    let buf = take(&mut self.apc_buf);
                    self.apc_dispatch(&buf);
                    self.apc = Apc::Normal;
                }
                Apc::Body => {
                    if b == 0x1b {
                        self.apc = Apc::BodyEsc;
                    } else if self.apc_buf.len() < MAX_APC {
                        self.apc_buf.push(b);
                    }
                }
                Apc::BodyEsc => {
                    self.apc_buf.extend_from_slice(&[0x1b, b]);
                    self.apc = Apc::Body;
                }
            }
        }
        parser.advance(self, &normal);
    }

    // ------------------------------------------------------------ queries for the front end

    /// Absolute line number of screen row `y`. Primary-screen lines keep their
    /// number as they scroll into history.
    pub fn abs_row(&self, y: usize) -> i64 {
        if self.alt { y as i64 } else { self.scrolled + y as i64 }
    }

    /// Absolute line shown at the top of the view.
    pub fn view_top(&self) -> i64 {
        self.abs_row(0) - if self.alt { 0 } else { self.display_offset as i64 }
    }

    pub fn line_abs(&self, abs: i64) -> Option<&Row> {
        if self.alt {
            return self.screen.lines.get(usize::try_from(abs).ok()?);
        }
        if abs >= self.scrolled {
            self.screen.lines.get((abs - self.scrolled) as usize)
        } else {
            let first = self.scrolled - self.scrollback.len() as i64;
            if abs < first { None } else { self.scrollback.get((abs - first) as usize) }
        }
    }

    pub fn cursor_pos(&self) -> (usize, usize) {
        (self.cursor.x, self.cursor.y)
    }

    pub fn images(&self) -> &[Placement] {
        &self.screen.images
    }

    /// Oldest line still in history.
    pub fn first_abs(&self) -> i64 {
        if self.alt { 0 } else { self.scrolled - self.scrollback.len() as i64 }
    }

    /// Bottom line of the screen.
    pub fn last_abs(&self) -> i64 {
        self.abs_row(self.rows - 1)
    }

    /// Scroll the view just enough to show line `abs`.
    pub fn ensure_visible(&mut self, abs: i64) {
        if self.alt {
            return;
        }
        let top = self.view_top();
        let bottom = top + self.rows as i64 - 1;
        let offset = self.display_offset as i64
            + if abs < top {
                top - abs
            } else if abs > bottom {
                bottom - abs
            } else {
                return;
            };
        self.display_offset = offset.clamp(0, self.scrollback.len() as i64) as usize;
    }

    /// The characters of a line, each with the column it starts at.
    pub fn line_chars(&self, abs: i64) -> Vec<(char, usize)> {
        self.line_abs(abs)
            .map(|l| l.iter().enumerate().filter(|(_, c)| c.flags & SPACER == 0).map(|(i, c)| (c.ch, i)).collect())
            .unwrap_or_default()
    }

    /// Escape sequences that recreate this terminal's state (up to `history` lines of
    /// scrollback, screen, modes, cursor) when fed into a fresh terminal of the same size.
    /// Used by the session server (Unix only).
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn snapshot(&self, history: usize) -> Vec<u8> {
        use std::fmt::Write as _;
        let mut s = String::from("\x1b[0m\x1b[H\x1b[2J");
        let primary = if self.alt { &self.other } else { &self.screen };
        let skip = self.scrollback.len().saturating_sub(history);
        for (i, line) in self.scrollback.iter().skip(skip).chain(primary.lines.iter()).enumerate() {
            if i > 0 {
                // Reset first, or new lines scrolled in would take the last cell's background.
                s.push_str("\x1b[0m\r\n");
            }
            write_row(&mut s, line, self.cols);
        }
        s.push_str("\x1b[0m");
        if self.alt {
            let saved = self.other.saved;
            let _ = write!(s, "\x1b[{};{}H\x1b[?1049h", saved.y + 1, saved.x + 1);
            for (y, line) in self.screen.lines.iter().enumerate() {
                let _ = write!(s, "\x1b[{};1H", y + 1);
                write_row(&mut s, line, self.cols);
                s.push_str("\x1b[0m");
            }
        }
        // Images on the visible screen, re-sent with the kitty protocol (C=1: cursor stays).
        let mut budget: usize = 32 << 20;
        for p in &self.screen.images {
            let y = p.row - self.abs_row(0);
            let size = p.image.rgba.len() * 4 / 3;
            if !(0..self.rows as i64).contains(&y) || size > budget {
                continue;
            }
            budget -= size;
            let _ = write!(s, "\x1b[{};{}H", y + 1, p.col + 1);
            let cols = (p.w_cells.round() as u32).max(1);
            let rows = (p.h_cells.round() as u32).max(1);
            let b64 = B64.encode(&p.image.rgba);
            let chunks: Vec<&[u8]> = b64.as_bytes().chunks(4096).collect();
            for (i, chunk) in chunks.iter().enumerate() {
                let more = (i + 1 < chunks.len()) as u8;
                let chunk = std::str::from_utf8(chunk).unwrap_or_default();
                if i == 0 {
                    let (w, h) = (p.image.w, p.image.h);
                    let _ = write!(s, "\x1b_Ga=T,f=32,s={w},v={h},c={cols},r={rows},C=1,q=2,m={more};{chunk}\x1b\\");
                } else {
                    let _ = write!(s, "\x1b_Gm={more};{chunk}\x1b\\");
                }
            }
        }
        if self.top != 0 || self.bot != self.rows - 1 {
            let _ = write!(s, "\x1b[{};{}r", self.top + 1, self.bot + 1);
        }
        let m = &self.modes;
        for (on, mode) in [(m.app_cursor, 1), (m.bracketed_paste, 2004), (m.focus_events, 1004), (m.mouse_sgr, 1006)] {
            if on {
                let _ = write!(s, "\x1b[?{mode}h");
            }
        }
        match m.mouse {
            MouseMode::Click => s.push_str("\x1b[?1000h"),
            MouseMode::Drag => s.push_str("\x1b[?1002h"),
            MouseMode::Motion => s.push_str("\x1b[?1003h"),
            MouseMode::Off => {}
        }
        if !m.autowrap {
            s.push_str("\x1b[?7l");
        }
        if !m.cursor_visible {
            s.push_str("\x1b[?25l");
        }
        if m.insert {
            s.push_str("\x1b[4h");
        }
        match self.cursor_shape {
            CursorShape::Underline => s.push_str("\x1b[4 q"),
            CursorShape::Bar => s.push_str("\x1b[6 q"),
            CursorShape::Block => {}
        }
        if !self.current_title.is_empty() {
            let _ = write!(s, "\x1b]2;{}\x1b\\", self.current_title);
        }
        s.push_str(&sgr_of(&self.cursor.pen));
        let _ = write!(s, "\x1b[{};{}H", self.cursor.y + 1, self.cursor.x + 1);
        s.into_bytes()
    }

    pub fn scroll_view(&mut self, delta: isize) {
        if !self.alt {
            let max = self.scrollback.len() as isize;
            self.display_offset = (self.display_offset as isize + delta).clamp(0, max) as usize;
        }
    }

    pub fn selection_text(&self, sel: &Selection) -> String {
        let (s, e) = sel.range();
        let mut out = String::new();
        for row in s.0..=e.0 {
            if let Some(line) = self.line_abs(row) {
                let from = if row == s.0 { s.1 } else { 0 };
                let to = if row == e.0 { (e.1 + 1).min(line.len()) } else { line.len() };
                let text: String = line
                    .get(from..to)
                    .unwrap_or(&[])
                    .iter()
                    .filter(|c| c.flags & SPACER == 0)
                    .map(|c| c.ch)
                    .collect();
                out.push_str(text.trim_end());
            }
            if row != e.0 {
                out.push('\n');
            }
        }
        out
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let (cols, rows) = (cols.max(2), rows.max(1));
        if cols == self.cols && rows == self.rows {
            return;
        }
        let prim_y = if self.alt { self.other.saved.y } else { self.cursor.y };
        let alt_y = if self.alt { self.cursor.y } else { 0 };
        let (prim, alts) = if self.alt { (&mut self.other, &mut self.screen) } else { (&mut self.screen, &mut self.other) };
        let moved = fit_rows(&mut prim.lines, rows, prim_y, cols);
        let alt_moved = fit_rows(&mut alts.lines, rows, alt_y, cols).len();
        for line in prim.lines.iter_mut().chain(alts.lines.iter_mut()) {
            line.resize(cols, Cell::default());
            if let Some(last) = line.last_mut() {
                if last.flags & WIDE != 0 {
                    *last = Cell::default();
                }
            }
        }
        for p in &mut alts.images {
            p.row -= alt_moved as i64;
        }
        let n = moved.len();
        for line in moved {
            self.push_scrollback(line);
        }
        if self.alt {
            self.other.saved.y = self.other.saved.y.saturating_sub(n);
            self.cursor.y = self.cursor.y.saturating_sub(alt_moved);
        } else {
            self.cursor.y = self.cursor.y.saturating_sub(n);
        }
        self.cols = cols;
        self.rows = rows;
        self.top = 0;
        self.bot = rows - 1;
        self.tabs = default_tabs(cols);
        for c in [&mut self.cursor, &mut self.screen.saved, &mut self.other.saved] {
            c.x = c.x.min(cols - 1);
            c.y = c.y.min(rows - 1);
            c.pending_wrap = false;
        }
        self.display_offset = 0;
    }

    // ------------------------------------------------------------ primitives

    fn blank(&self) -> Cell {
        Cell { bg: self.cursor.pen.bg, ..Cell::default() }
    }

    fn blank_row(&self) -> Row {
        vec![self.blank(); self.cols]
    }

    fn push_scrollback(&mut self, row: Row) {
        self.scrolled += 1;
        if self.scrollback_max == 0 {
            return;
        }
        self.scrollback.push_back(row);
        if self.scrollback.len() > self.scrollback_max {
            self.scrollback.pop_front();
        }
        if self.display_offset > 0 {
            self.display_offset = (self.display_offset + 1).min(self.scrollback.len());
        }
    }

    fn scroll_up(&mut self, n: usize) {
        let (top, bot) = (self.top, self.bot);
        let n = n.min(bot + 1 - top);
        if n == 0 {
            return;
        }
        let blank = self.blank_row();
        let removed: Vec<Row> = self.screen.lines.drain(top..top + n).collect();
        self.screen.lines.splice(bot + 1 - n..bot + 1 - n, std::iter::repeat_n(blank, n));
        if top != 0 {
            return;
        }
        if self.alt {
            self.screen.images.iter_mut().for_each(|p| p.row -= n as i64);
            self.screen.images.retain(|p| p.row + p.h_cells.ceil() as i64 > 0);
        } else {
            for row in removed {
                self.push_scrollback(row);
            }
            let first = self.scrolled - self.scrollback.len() as i64;
            self.screen.images.retain(|p| p.row + p.h_cells.ceil() as i64 > first);
        }
    }

    fn scroll_down(&mut self, n: usize) {
        let (top, bot) = (self.top, self.bot);
        let n = n.min(bot + 1 - top);
        let blank = self.blank_row();
        self.screen.lines.drain(bot + 1 - n..bot + 1);
        self.screen.lines.splice(top..top, std::iter::repeat_n(blank, n));
    }

    fn linefeed(&mut self) {
        self.cursor.pending_wrap = false;
        if self.cursor.y == self.bot {
            self.scroll_up(1);
        } else if self.cursor.y + 1 < self.rows {
            self.cursor.y += 1;
        }
    }

    fn reverse_index(&mut self) {
        self.cursor.pending_wrap = false;
        if self.cursor.y == self.top {
            self.scroll_down(1);
        } else if self.cursor.y > 0 {
            self.cursor.y -= 1;
        }
    }

    /// Move to (x, y); y is relative to the scroll region in origin mode.
    fn goto(&mut self, x: usize, y: usize) {
        let (min_y, max_y) = if self.cursor.origin { (self.top, self.bot) } else { (0, self.rows - 1) };
        self.cursor.x = x.min(self.cols - 1);
        self.cursor.y = (min_y + y).min(max_y);
        self.cursor.pending_wrap = false;
    }

    fn put_char(&mut self, c: char) {
        let c = if self.cursor.charsets[self.cursor.shift_out as usize] { dec_graphics(c) } else { c };
        let w = match c.width() {
            Some(w) if w > 0 => w,
            _ => return, // combining marks and other zero-width characters
        };
        if self.cursor.pending_wrap {
            if self.modes.autowrap {
                self.cursor.x = 0;
                self.linefeed();
            }
            self.cursor.pending_wrap = false;
        }
        if w == 2 && self.cursor.x + 1 >= self.cols {
            if !self.modes.autowrap {
                return;
            }
            let b = self.blank();
            self.screen.lines[self.cursor.y][self.cursor.x] = b;
            self.cursor.x = 0;
            self.linefeed();
        }
        let (x, y) = (self.cursor.x, self.cursor.y);
        let blank = self.blank();
        if self.modes.insert {
            let row = &mut self.screen.lines[y];
            row.splice(x..x, std::iter::repeat_n(blank, w));
            row.truncate(self.cols);
        }
        self.fix_wide(y, x);
        if w == 2 {
            self.fix_wide(y, x + 1);
        }
        let pen = self.cursor.pen;
        let row = &mut self.screen.lines[y];
        row[x] = Cell { ch: c, flags: pen.flags | if w == 2 { WIDE } else { 0 }, ..pen };
        if w == 2 {
            row[x + 1] = Cell { ch: ' ', flags: SPACER, ..pen };
        }
        if x + w >= self.cols {
            self.cursor.x = self.cols - 1;
            self.cursor.pending_wrap = true;
        } else {
            self.cursor.x = x + w;
        }
        self.last_char = Some(c);
    }

    /// Before overwriting cell x, blank the other half of any wide char it belongs to.
    fn fix_wide(&mut self, y: usize, x: usize) {
        let b = self.blank();
        let row = &mut self.screen.lines[y];
        if row[x].flags & WIDE != 0 && x + 1 < row.len() {
            row[x + 1] = b;
        }
        if row[x].flags & SPACER != 0 && x > 0 {
            row[x - 1] = b;
        }
    }

    fn clear_cells(&mut self, y: usize, x0: usize, x1: usize) {
        let b = self.blank();
        let row = &mut self.screen.lines[y];
        let x1 = x1.min(row.len());
        if x0 < x1 {
            row[x0..x1].fill(b);
        }
    }

    fn erase_display(&mut self, mode: u16) {
        let (x, y) = (self.cursor.x, self.cursor.y);
        match mode {
            0 => {
                self.clear_cells(y, x, self.cols);
                for r in y + 1..self.rows {
                    self.clear_cells(r, 0, self.cols);
                }
            }
            1 => {
                for r in 0..y {
                    self.clear_cells(r, 0, self.cols);
                }
                self.clear_cells(y, 0, x + 1);
            }
            2 => {
                for r in 0..self.rows {
                    self.clear_cells(r, 0, self.cols);
                }
                self.screen.images.clear();
            }
            3 => {
                self.scrollback.clear();
                self.display_offset = 0;
                let first = self.abs_row(0);
                self.screen.images.retain(|p| p.row + p.h_cells.ceil() as i64 > first);
            }
            _ => {}
        }
    }

    fn erase_line(&mut self, mode: u16) {
        let (x, y) = (self.cursor.x, self.cursor.y);
        match mode {
            0 => self.clear_cells(y, x, self.cols),
            1 => self.clear_cells(y, 0, x + 1),
            2 => self.clear_cells(y, 0, self.cols),
            _ => {}
        }
    }

    fn insert_lines(&mut self, n: usize) {
        let (y, top, bot) = (self.cursor.y, self.top, self.bot);
        if y < top || y > bot {
            return;
        }
        let n = n.min(bot + 1 - y);
        let blank = self.blank_row();
        self.screen.lines.drain(bot + 1 - n..bot + 1);
        self.screen.lines.splice(y..y, std::iter::repeat_n(blank, n));
        self.cursor.x = 0;
        self.cursor.pending_wrap = false;
    }

    fn delete_lines(&mut self, n: usize) {
        let (y, top, bot) = (self.cursor.y, self.top, self.bot);
        if y < top || y > bot {
            return;
        }
        let n = n.min(bot + 1 - y);
        let blank = self.blank_row();
        self.screen.lines.drain(y..y + n);
        self.screen.lines.splice(bot + 1 - n..bot + 1 - n, std::iter::repeat_n(blank, n));
        self.cursor.x = 0;
        self.cursor.pending_wrap = false;
    }

    fn insert_chars(&mut self, n: usize) {
        let (x, y) = (self.cursor.x, self.cursor.y);
        let b = self.blank();
        let n = n.min(self.cols - x);
        let row = &mut self.screen.lines[y];
        row.splice(x..x, std::iter::repeat_n(b, n));
        row.truncate(self.cols);
    }

    fn delete_chars(&mut self, n: usize) {
        let (x, y) = (self.cursor.x, self.cursor.y);
        let b = self.blank();
        let cols = self.cols;
        let row = &mut self.screen.lines[y];
        row.drain(x..(x + n).min(cols));
        row.resize(cols, b);
    }

    fn tab(&mut self, n: usize) {
        for _ in 0..n {
            let mut x = self.cursor.x + 1;
            while x < self.cols - 1 && !self.tabs[x] {
                x += 1;
            }
            self.cursor.x = x.min(self.cols - 1);
        }
        self.cursor.pending_wrap = false;
    }

    fn back_tab(&mut self, n: usize) {
        for _ in 0..n {
            let mut x = self.cursor.x.saturating_sub(1);
            while x > 0 && !self.tabs[x] {
                x -= 1;
            }
            self.cursor.x = x;
        }
        self.cursor.pending_wrap = false;
    }

    fn save_cursor(&mut self) {
        self.screen.saved = self.cursor;
    }

    fn restore_cursor(&mut self) {
        self.cursor = self.screen.saved;
        self.cursor.x = self.cursor.x.min(self.cols - 1);
        self.cursor.y = self.cursor.y.min(self.rows - 1);
    }

    fn set_alt(&mut self, on: bool, clear: bool) {
        if on == self.alt {
            return;
        }
        std::mem::swap(&mut self.screen, &mut self.other);
        self.alt = on;
        if on && clear {
            self.screen.lines = vec![vec![Cell::default(); self.cols]; self.rows];
            self.screen.images.clear();
        }
        self.display_offset = 0;
        self.cursor.pending_wrap = false;
    }

    fn reset(&mut self) {
        let mut t = Term::new(self.cols, self.rows, self.scrollback_max);
        t.cell_px = self.cell_px;
        t.default_colors = self.default_colors;
        *self = t;
    }

    fn reply(&mut self, s: &str) {
        self.responses.extend_from_slice(s.as_bytes());
    }

    // ------------------------------------------------------------ modes and attributes

    fn dec_mode(&mut self, mode: u16, on: bool) {
        match mode {
            1 => self.modes.app_cursor = on,
            6 => {
                self.cursor.origin = on;
                self.goto(0, 0);
            }
            7 => self.modes.autowrap = on,
            25 => self.modes.cursor_visible = on,
            47 => self.set_alt(on, false),
            1047 => self.set_alt(on, true),
            1048 => {
                if on { self.save_cursor() } else { self.restore_cursor() }
            }
            1049 => {
                if on {
                    self.save_cursor();
                    self.set_alt(true, true);
                } else {
                    self.set_alt(false, false);
                    self.restore_cursor();
                }
            }
            1000 | 1002 | 1003 => {
                self.modes.mouse = match (on, mode) {
                    (false, _) => MouseMode::Off,
                    (_, 1000) => MouseMode::Click,
                    (_, 1002) => MouseMode::Drag,
                    _ => MouseMode::Motion,
                }
            }
            1004 => self.modes.focus_events = on,
            1006 => self.modes.mouse_sgr = on,
            2004 => self.modes.bracketed_paste = on,
            _ => {}
        }
    }

    fn dec_mode_state(&self, mode: u16) -> u8 {
        let m = &self.modes;
        let v = match mode {
            1 => m.app_cursor,
            6 => self.cursor.origin,
            7 => m.autowrap,
            25 => m.cursor_visible,
            47 | 1047 | 1049 => self.alt,
            1000 => m.mouse == MouseMode::Click,
            1002 => m.mouse == MouseMode::Drag,
            1003 => m.mouse == MouseMode::Motion,
            1004 => m.focus_events,
            1006 => m.mouse_sgr,
            2004 => m.bracketed_paste,
            _ => return 0,
        };
        if v { 1 } else { 2 }
    }

    fn sgr(&mut self, params: &Params) {
        let pen = &mut self.cursor.pen;
        if params.is_empty() {
            *pen = Cell::default();
            return;
        }
        let mut it = params.iter();
        while let Some(p) = it.next() {
            match p[0] {
                0 => *pen = Cell::default(),
                1 => pen.flags |= BOLD,
                2 => pen.flags |= DIM,
                3 => pen.flags |= ITALIC,
                4 if p.get(1) == Some(&0) => pen.flags &= !UNDERLINE,
                4 | 21 => pen.flags |= UNDERLINE,
                7 => pen.flags |= INVERSE,
                8 => pen.flags |= HIDDEN,
                9 => pen.flags |= STRIKE,
                22 => pen.flags &= !(BOLD | DIM),
                23 => pen.flags &= !ITALIC,
                24 => pen.flags &= !UNDERLINE,
                27 => pen.flags &= !INVERSE,
                28 => pen.flags &= !HIDDEN,
                29 => pen.flags &= !STRIKE,
                n @ 30..=37 => pen.fg = Color::Idx((n - 30) as u8),
                38 => {
                    if let Some(c) = ext_color(p, &mut it) {
                        pen.fg = c;
                    }
                }
                39 => pen.fg = Color::Default,
                n @ 40..=47 => pen.bg = Color::Idx((n - 40) as u8),
                48 => {
                    if let Some(c) = ext_color(p, &mut it) {
                        pen.bg = c;
                    }
                }
                49 => pen.bg = Color::Default,
                58 => {
                    ext_color(p, &mut it); // underline color: consume, unsupported
                }
                n @ 90..=97 => pen.fg = Color::Idx((n - 90 + 8) as u8),
                n @ 100..=107 => pen.bg = Color::Idx((n - 100 + 8) as u8),
                _ => {}
            }
        }
    }

    // ------------------------------------------------------------ images

    fn place(&mut self, image: Arc<Image>, id: u32, w_cells: f32, h_cells: f32, advance: bool, no_move: bool) {
        let row = self.abs_row(self.cursor.y);
        let col = self.cursor.x;
        self.screen.images.push(Placement { image, id, row, col, w_cells, h_cells });
        if self.screen.images.len() > MAX_PLACEMENTS {
            self.screen.images.remove(0);
        }
        if no_move {
            return;
        }
        let rows = (h_cells.ceil() as usize).max(1);
        for _ in 1..rows {
            self.linefeed();
        }
        if advance {
            let x = col + (w_cells.ceil() as usize).max(1);
            if x >= self.cols {
                self.cursor.x = self.cols - 1;
                self.cursor.pending_wrap = true;
            } else {
                self.cursor.x = x;
            }
        }
    }

    fn apc_dispatch(&mut self, data: &[u8]) {
        let Some(rest) = data.strip_prefix(b"G") else { return };
        let (ctrl, payload) = match rest.iter().position(|&b| b == b';') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, &[][..]),
        };
        let cmd = KittyCmd::parse(&String::from_utf8_lossy(ctrl));
        // Continuation chunks carry only `m=`; the first chunk's keys apply.
        let (cmd, payload) = match self.kitty_pending.take() {
            Some((first, mut buf)) => {
                if buf.len() + payload.len() > MAX_APC {
                    return;
                }
                buf.extend_from_slice(payload);
                if cmd.more {
                    self.kitty_pending = Some((first, buf));
                    return;
                }
                (first, buf)
            }
            None if cmd.more => {
                self.kitty_pending = Some((cmd, payload.to_vec()));
                return;
            }
            None => (cmd, payload.to_vec()),
        };
        let result = self.kitty_exec(&cmd, &payload);
        if cmd.id == 0 || cmd.action == b'd' {
            return;
        }
        match result {
            Ok(()) if cmd.quiet == 0 => self.reply(&format!("\x1b_Gi={};OK\x1b\\", cmd.id)),
            Err(e) if cmd.quiet < 2 => self.reply(&format!("\x1b_Gi={};{e}\x1b\\", cmd.id)),
            _ => {}
        }
    }

    fn kitty_exec(&mut self, cmd: &KittyCmd, payload: &[u8]) -> Result<(), String> {
        match cmd.action {
            b'd' => {
                match cmd.delete {
                    b'i' | b'I' => {
                        self.screen.images.retain(|p| p.id != cmd.id);
                        if cmd.delete == b'I' {
                            self.kitty_images.remove(&cmd.id);
                        }
                    }
                    b'A' => {
                        self.screen.images.clear();
                        self.kitty_images.clear();
                    }
                    _ => self.screen.images.clear(),
                }
                Ok(())
            }
            b'p' => {
                let img = self.kitty_images.get(&cmd.id).cloned().ok_or("ENOENT:no such image")?;
                self.kitty_place(img, cmd);
                Ok(())
            }
            b't' | b'T' | b'q' => {
                let img = Arc::new(graphics::kitty_decode(cmd, payload)?);
                if cmd.action == b'q' {
                    return Ok(());
                }
                if cmd.id != 0 {
                    if self.kitty_images.len() >= MAX_STORED_IMAGES {
                        if let Some(&k) = self.kitty_images.keys().min() {
                            self.kitty_images.remove(&k);
                        }
                    }
                    self.kitty_images.insert(cmd.id, img.clone());
                }
                if cmd.action == b'T' {
                    self.kitty_place(img, cmd);
                }
                Ok(())
            }
            _ => Err("EINVAL:unsupported action".into()),
        }
    }

    fn kitty_place(&mut self, img: Arc<Image>, cmd: &KittyCmd) {
        let (cw, ch) = (self.cell_px.0 as f32, self.cell_px.1 as f32);
        let (iw, ih) = (img.w as f32, img.h as f32);
        let (wc, hc) = match (cmd.cols, cmd.rows) {
            (0, 0) => (iw / cw, ih / ch),
            (c, 0) => (c as f32, c as f32 * cw * ih / iw / ch),
            (0, r) => (r as f32 * ch * iw / ih / cw, r as f32),
            (c, r) => (c as f32, r as f32),
        };
        self.place(img, cmd.id, wc, hc, true, cmd.no_move);
    }
}

/// SGR sequence that sets exactly this cell's attributes.
#[cfg_attr(not(unix), allow(dead_code))]
fn sgr_of(c: &Cell) -> String {
    let mut codes = vec!["0".to_string()];
    for (flag, code) in [(BOLD, "1"), (DIM, "2"), (ITALIC, "3"), (UNDERLINE, "4"), (INVERSE, "7"), (HIDDEN, "8"), (STRIKE, "9")] {
        if c.flags & flag != 0 {
            codes.push(code.into());
        }
    }
    for (color, base) in [(c.fg, 38), (c.bg, 48)] {
        match color {
            Color::Default => {}
            Color::Idx(i) => codes.push(format!("{base};5;{i}")),
            Color::Rgb(r, g, b) => codes.push(format!("{base};2;{r};{g};{b}")),
        }
    }
    format!("\x1b[{}m", codes.join(";"))
}

#[cfg_attr(not(unix), allow(dead_code))]
fn write_row(s: &mut String, line: &[Cell], cols: usize) {
    let end = line.iter().rposition(|c| *c != Cell::default()).map_or(0, |i| i + 1).min(cols);
    let mut last = None;
    for cell in &line[..end] {
        if cell.flags & SPACER != 0 {
            continue;
        }
        let key = (cell.fg, cell.bg, cell.flags & !(WIDE | SPACER));
        if last != Some(key) {
            s.push_str(&sgr_of(cell));
            last = Some(key);
        }
        s.push(cell.ch);
    }
}

fn fit_rows(lines: &mut Vec<Row>, rows: usize, cursor_y: usize, cols: usize) -> Vec<Row> {
    let mut cursor_y = cursor_y;
    let mut moved = Vec::new();
    while lines.len() > rows {
        let blank_tail = lines.last().is_some_and(|l| l.iter().all(|c| c.ch == ' ' && c.bg == Color::Default));
        if lines.len() - 1 > cursor_y && blank_tail {
            lines.pop();
        } else {
            moved.push(lines.remove(0));
            cursor_y = cursor_y.saturating_sub(1);
        }
    }
    while lines.len() < rows {
        lines.push(vec![Cell::default(); cols]);
    }
    moved
}

/// Parse `38;5;n` / `38;2;r;g;b` and their colon forms.
fn ext_color<'a>(p: &[u16], it: &mut impl Iterator<Item = &'a [u16]>) -> Option<Color> {
    if p.len() > 1 {
        return match p[1] {
            5 => p.get(2).map(|&i| Color::Idx(i as u8)),
            2 => {
                let v = &p[2..];
                let v = if v.len() >= 4 { &v[1..4] } else { v };
                (v.len() >= 3).then(|| Color::Rgb(v[0] as u8, v[1] as u8, v[2] as u8))
            }
            _ => None,
        };
    }
    match it.next()?[0] {
        5 => Some(Color::Idx(it.next()?[0] as u8)),
        2 => {
            let (r, g, b) = (it.next()?[0], it.next()?[0], it.next()?[0]);
            Some(Color::Rgb(r as u8, g as u8, b as u8))
        }
        _ => None,
    }
}

impl Perform for Term {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, b: u8) {
        match b {
            0x07 => self.bell = true,
            0x08 => {
                self.cursor.x = self.cursor.x.saturating_sub(1);
                self.cursor.pending_wrap = false;
            }
            0x09 => self.tab(1),
            0x0a..=0x0c => {
                self.linefeed();
                if self.modes.newline {
                    self.cursor.x = 0;
                }
            }
            0x0d => {
                self.cursor.x = 0;
                self.cursor.pending_wrap = false;
            }
            0x0e => self.cursor.shift_out = true,
            0x0f => self.cursor.shift_out = false,
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, inter: &[u8], _ignore: bool, action: char) {
        let p: Vec<u16> = params.iter().map(|s| s[0]).collect();
        let arg = |i: usize, d: usize| p.get(i).copied().filter(|&v| v != 0).map_or(d, |v| v as usize);
        let raw = |i: usize| p.get(i).copied().unwrap_or(0);
        let (x, y) = (self.cursor.x, self.cursor.y);
        match (action, inter) {
            ('m', []) => self.sgr(params),
            ('@', []) => self.insert_chars(arg(0, 1)),
            ('A', []) => {
                let min = if y >= self.top { self.top } else { 0 };
                self.cursor.y = y.saturating_sub(arg(0, 1)).max(min);
                self.cursor.pending_wrap = false;
            }
            ('B' | 'e', []) => {
                let max = if y <= self.bot { self.bot } else { self.rows - 1 };
                self.cursor.y = (y + arg(0, 1)).min(max);
                self.cursor.pending_wrap = false;
            }
            ('C' | 'a', []) => {
                self.cursor.x = (x + arg(0, 1)).min(self.cols - 1);
                self.cursor.pending_wrap = false;
            }
            ('D', []) => {
                self.cursor.x = x.saturating_sub(arg(0, 1));
                self.cursor.pending_wrap = false;
            }
            ('E', []) => {
                let max = if y <= self.bot { self.bot } else { self.rows - 1 };
                self.cursor.y = (y + arg(0, 1)).min(max);
                self.cursor.x = 0;
                self.cursor.pending_wrap = false;
            }
            ('F', []) => {
                let min = if y >= self.top { self.top } else { 0 };
                self.cursor.y = y.saturating_sub(arg(0, 1)).max(min);
                self.cursor.x = 0;
                self.cursor.pending_wrap = false;
            }
            ('G' | '`', []) => {
                self.cursor.x = (arg(0, 1) - 1).min(self.cols - 1);
                self.cursor.pending_wrap = false;
            }
            ('H' | 'f', []) => self.goto(arg(1, 1) - 1, arg(0, 1) - 1),
            ('I', []) => self.tab(arg(0, 1)),
            ('J', [] | [b'?']) => self.erase_display(raw(0)),
            ('K', [] | [b'?']) => self.erase_line(raw(0)),
            ('L', []) => self.insert_lines(arg(0, 1)),
            ('M', []) => self.delete_lines(arg(0, 1)),
            ('P', []) => self.delete_chars(arg(0, 1)),
            ('S', []) => self.scroll_up(arg(0, 1)),
            ('T', []) if p.len() <= 1 => self.scroll_down(arg(0, 1)),
            ('X', []) => self.clear_cells(y, x, x + arg(0, 1)),
            ('Z', []) => self.back_tab(arg(0, 1)),
            ('b', []) => {
                if let Some(c) = self.last_char {
                    for _ in 0..arg(0, 1).min(self.cols * self.rows) {
                        self.put_char(c);
                    }
                }
            }
            ('c', []) if raw(0) == 0 => self.reply("\x1b[?62;4;22c"), // VT220 with sixel
            ('c', [b'>']) => self.reply("\x1b[>1;10;0c"),
            ('d', []) => {
                let x = self.cursor.x;
                self.goto(x, arg(0, 1) - 1);
            }
            ('g', []) => match raw(0) {
                0 => self.tabs[x] = false,
                3 => self.tabs.iter_mut().for_each(|t| *t = false),
                _ => {}
            },
            ('h' | 'l', _) => {
                let on = action == 'h';
                for &m in &p {
                    match inter {
                        [b'?'] => self.dec_mode(m, on),
                        [] if m == 4 => self.modes.insert = on,
                        [] if m == 20 => self.modes.newline = on,
                        _ => {}
                    }
                }
            }
            ('n', []) => match raw(0) {
                5 => self.reply("\x1b[0n"),
                6 => {
                    let row = if self.cursor.origin { y - self.top } else { y };
                    self.reply(&format!("\x1b[{};{}R", row + 1, x + 1));
                }
                _ => {}
            },
            ('r', []) => {
                let top = arg(0, 1) - 1;
                let bot = arg(1, self.rows).min(self.rows) - 1;
                if top < bot {
                    self.top = top;
                    self.bot = bot;
                    self.goto(0, 0);
                }
            }
            ('s', []) => self.save_cursor(),
            ('u', []) => self.restore_cursor(),
            ('t', []) => {
                let (cw, ch) = self.cell_px;
                match raw(0) {
                    14 => self.reply(&format!("\x1b[4;{};{}t", self.rows * ch, self.cols * cw)),
                    16 => self.reply(&format!("\x1b[6;{ch};{cw}t")),
                    18 => self.reply(&format!("\x1b[8;{};{}t", self.rows, self.cols)),
                    _ => {}
                }
            }
            ('q', [b' ']) => {
                self.cursor_shape = match raw(0) {
                    3 | 4 => CursorShape::Underline,
                    5 | 6 => CursorShape::Bar,
                    _ => CursorShape::Block,
                }
            }
            ('p', [b'!']) => {
                self.modes = Modes::default();
                self.cursor.pen = Cell::default();
                self.cursor.origin = false;
                self.top = 0;
                self.bot = self.rows - 1;
            }
            ('p', [b'?', b'$']) => {
                let m = raw(0);
                let state = self.dec_mode_state(m);
                self.reply(&format!("\x1b[?{m};{state}$y"));
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, inter: &[u8], _ignore: bool, b: u8) {
        match (inter, b) {
            ([], b'7') => self.save_cursor(),
            ([], b'8') => self.restore_cursor(),
            ([], b'D') => self.linefeed(),
            ([], b'E') => {
                self.linefeed();
                self.cursor.x = 0;
            }
            ([], b'H') => {
                let x = self.cursor.x;
                self.tabs[x] = true;
            }
            ([], b'M') => self.reverse_index(),
            ([], b'c') => self.reset(),
            ([b'('], c) => self.cursor.charsets[0] = c == b'0',
            ([b')'], c) => self.cursor.charsets[1] = c == b'0',
            ([b'#'], b'8') => {
                for line in &mut self.screen.lines {
                    line.fill(Cell { ch: 'E', ..Cell::default() });
                }
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        let Some(&cmd) = params.first() else { return };
        match cmd {
            b"0" | b"2" => {
                let title: String = params[1..]
                    .iter()
                    .map(|p| String::from_utf8_lossy(p))
                    .collect::<Vec<_>>()
                    .join(";")
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(256)
                    .collect();
                self.current_title = title.clone();
                self.title = Some(title);
            }
            b"10" | b"11" | b"12" if params.get(1) == Some(&&b"?"[..]) => {
                let c = if cmd == b"11" { self.default_colors.1 } else { self.default_colors.0 };
                let end = if bell_terminated { "\x07" } else { "\x1b\\" };
                let name = String::from_utf8_lossy(cmd);
                self.reply(&format!(
                    "\x1b]{name};rgb:{0:02x}{0:02x}/{1:02x}{1:02x}/{2:02x}{2:02x}{end}",
                    c[0], c[1], c[2]
                ));
            }
            _ => {}
        }
    }

    fn hook(&mut self, _params: &Params, inter: &[u8], _ignore: bool, action: char) {
        self.dcs = match (inter, action) {
            ([], 'q') => Dcs::Sixel(Box::new(Sixel::new())),
            ([b'$'], 'q') => Dcs::Decrqss,
            _ => Dcs::Ignore,
        };
    }

    fn put(&mut self, b: u8) {
        if let Dcs::Sixel(s) = &mut self.dcs {
            s.put(b);
        }
    }

    fn unhook(&mut self) {
        match take(&mut self.dcs) {
            Dcs::Sixel(s) => {
                if let Some(img) = s.finish() {
                    let (cw, ch) = (self.cell_px.0 as f32, self.cell_px.1 as f32);
                    let (w, h) = (img.w as f32 / cw, img.h as f32 / ch);
                    self.place(Arc::new(img), 0, w, h, false, false);
                }
            }
            Dcs::Decrqss => self.reply("\x1bP0$r\x1b\\"),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(cols: usize, rows: usize, input: &str) -> Term {
        let mut t = Term::new(cols, rows, 100);
        let mut p = Parser::new();
        t.feed(&mut p, input.as_bytes());
        t
    }

    fn text(t: &Term, y: usize) -> String {
        t.screen.lines[y].iter().filter(|c| c.flags & SPACER == 0).map(|c| c.ch).collect::<String>().trim_end().into()
    }

    #[test]
    fn wraps_and_scrolls_into_history() {
        let t = run(4, 2, "abcdefghij");
        assert_eq!((text(&t, 0).as_str(), text(&t, 1).as_str()), ("efgh", "ij"));
        assert_eq!(t.scrollback.len(), 1);
        assert_eq!(t.line_abs(0).unwrap()[0].ch, 'a');
    }

    #[test]
    fn cursor_movement_and_erase() {
        let t = run(10, 3, "hello\x1b[1;3H\x1b[KX\x1b[3;1Hwide界");
        assert_eq!(text(&t, 0), "heX");
        assert_eq!(text(&t, 2), "wide界");
        assert_eq!(t.screen.lines[2][4].flags & WIDE, WIDE);
    }

    #[test]
    fn sgr_colors() {
        let t = run(10, 1, "\x1b[1;38;2;1;2;3;48;5;200mA\x1b[0mB");
        let a = t.screen.lines[0][0];
        assert_eq!((a.fg, a.bg, a.flags & BOLD), (Color::Rgb(1, 2, 3), Color::Idx(200), BOLD));
        assert_eq!(t.screen.lines[0][1].fg, Color::Default);
    }

    #[test]
    fn alt_screen_restores_primary() {
        let t = run(10, 2, "main\x1b[?1049hfull\x1b[?1049l");
        assert!(!t.alt);
        assert_eq!(text(&t, 0), "main");
    }

    #[test]
    fn kitty_image_split_across_reads_and_chunks() {
        let mut t = Term::new(20, 5, 100);
        t.cell_px = (1, 1);
        let mut p = Parser::new();
        // 1x1 RGB pixel "AQID", sent in two chunks, with the stream split mid-escape.
        t.feed(&mut p, b"hi\x1b_Ga=T,f=24,s=1,v=1,i=3,m=1;AQ\x1b");
        t.feed(&mut p, b"\\\x1b_Gm=0;ID\x1b\\after");
        assert_eq!(t.images().len(), 1);
        assert_eq!(t.images()[0].image.rgba, vec![1, 2, 3, 255]);
        assert_eq!(t.responses, b"\x1b_Gi=3;OK\x1b\\");
        assert_eq!(text(&t, 0), "hi after");
    }

    #[test]
    fn snapshot_recreates_screen_history_and_modes() {
        let mut a = Term::new(12, 3, 100);
        let mut p = Parser::new();
        a.feed(&mut p, "one\r\n\x1b[1;31mtwo\x1b[0m\r\nthree\r\nfour 界\r\n\x1b[?2004h\x1b]2;title\x07\x1b[44mx".as_bytes());
        a.feed(&mut p, b"\x1b[?1049h\x1b[2;3Halt!");
        let mut b = Term::new(12, 3, 100);
        b.feed(&mut Parser::new(), &a.snapshot(1000));
        assert_eq!(b.screen.lines, a.screen.lines);
        assert_eq!(b.other.lines, a.other.lines);
        assert_eq!(b.scrollback, a.scrollback);
        assert_eq!(b.cursor_pos(), a.cursor_pos());
        assert!(b.alt && b.modes.bracketed_paste);
        assert_eq!(b.cursor.pen, a.cursor.pen);
        assert_eq!(b.current_title, "title");
    }

    #[test]
    fn snapshot_restores_images() {
        let mut a = Term::new(20, 5, 100);
        a.cell_px = (1, 1);
        a.feed(&mut Parser::new(), b"x\r\n  \x1b_Ga=T,f=24,s=2,v=1;AQIDBAUG\x1b\\");
        let mut b = Term::new(20, 5, 100);
        b.cell_px = (1, 1);
        b.feed(&mut Parser::new(), &a.snapshot(100));
        assert_eq!(b.images().len(), 1);
        let (pa, pb) = (&a.images()[0], &b.images()[0]);
        assert_eq!((pb.row, pb.col, pb.w_cells, pb.h_cells), (pa.row, pa.col, pa.w_cells, pa.h_cells));
        assert_eq!(pb.image.rgba, pa.image.rgba);
        assert_eq!(b.cursor_pos(), a.cursor_pos());
    }

    #[test]
    fn sixel_is_placed_and_reports() {
        let mut t = Term::new(20, 5, 100);
        t.cell_px = (2, 6);
        let mut p = Parser::new();
        t.feed(&mut p, b"\x1b[c\x1bPq#1;2;0;100;0#1!4~\x1b\\");
        assert_eq!(t.responses, b"\x1b[?62;4;22c");
        let img = &t.images()[0];
        assert_eq!((img.image.w, img.image.h, img.w_cells, img.h_cells), (4, 6, 2.0, 1.0));
    }
}
