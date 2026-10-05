//! CPU renderer: draws the grid, images and cursor into a 0RGB pixel buffer.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use fontdue::{Font, FontSettings};
use unicode_width::UnicodeWidthChar;

use crate::graphics::{self, Image};
pub use crate::layout::Rect;
use crate::term::{self, Cell, Color, CursorShape, Selection, Term};

// ---------------------------------------------------------------- theme

pub struct Theme {
    pub fg: u32,
    pub bg: u32,
    pub cursor: u32,
    pub selection: u32,
    /// Lines between split panes.
    pub divider: u32,
    /// Outline of the focused pane.
    pub accent: u32,
    pub palette: [u32; 256],
}

const BASE16: [u32; 16] = [
    0x15161e, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xa9b1d6,
    0x414868, 0xff899d, 0xb9f27c, 0xffc777, 0x8db0ff, 0xc7a9ff, 0xa4daff, 0xc0caf5,
];

impl Default for Theme {
    fn default() -> Self {
        let mut palette = [0u32; 256];
        palette[..16].copy_from_slice(&BASE16);
        let level = |v: usize| if v == 0 { 0 } else { 55 + 40 * v as u32 };
        for i in 16..232 {
            let n = i - 16;
            palette[i] = level(n / 36) << 16 | level(n / 6 % 6) << 8 | level(n % 6);
        }
        for i in 232..256 {
            let g = 8 + 10 * (i as u32 - 232);
            palette[i] = g << 16 | g << 8 | g;
        }
        Theme {
            fg: 0xc0caf5,
            bg: 0x1a1b26,
            cursor: 0xc0caf5,
            selection: 0x364a82,
            divider: 0x2f334d,
            accent: 0x7aa2f7,
            palette,
        }
    }
}

pub fn rgb_bytes(c: u32) -> [u8; 3] {
    [(c >> 16) as u8, (c >> 8) as u8, c as u8]
}

// ---------------------------------------------------------------- fonts

pub struct Glyph {
    w: usize,
    h: usize,
    left: i32,
    /// Offset of the bitmap's top edge from the cell's top edge.
    top: i32,
    alpha: Vec<u8>,
}

type FontPath = (PathBuf, u32);

fn load_font(path: &Path, index: u32) -> Option<Font> {
    let data = std::fs::read(path).ok()?;
    Font::from_bytes(data, FontSettings { collection_index: index, ..FontSettings::default() }).ok()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn fc_match(pattern: &str) -> Option<PathBuf> {
    let out = std::process::Command::new("fc-match").args(["-f", "%{file}", pattern]).output().ok()?;
    let path = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    path.is_file().then_some(path)
}

/// Candidate files for regular, bold, italic, bold-italic, and fallback fonts.
#[cfg(windows)]
fn system_fonts() -> ([Vec<FontPath>; 4], Vec<FontPath>) {
    let dir = PathBuf::from(std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into())).join("Fonts");
    let f = |names: &[&str]| names.iter().map(|n| (dir.join(n), 0)).collect::<Vec<_>>();
    (
        [
            f(&["consola.ttf", "CascadiaMono.ttf", "lucon.ttf", "cour.ttf"]),
            f(&["consolab.ttf"]),
            f(&["consolai.ttf"]),
            f(&["consolaz.ttf"]),
        ],
        f(&["seguisym.ttf", "msgothic.ttc", "malgun.ttf", "msyh.ttc", "segoeui.ttf", "arial.ttf"]),
    )
}

#[cfg(target_os = "macos")]
fn system_fonts() -> ([Vec<FontPath>; 4], Vec<FontPath>) {
    let menlo = |i| vec![(PathBuf::from("/System/Library/Fonts/Menlo.ttc"), i)];
    (
        [menlo(0), menlo(1), menlo(2), menlo(3)],
        ["/System/Library/Fonts/Apple Symbols.ttf", "/System/Library/Fonts/Supplemental/Arial Unicode.ttf"]
            .iter()
            .map(|p| (PathBuf::from(p), 0))
            .collect(),
    )
}

#[cfg(all(unix, not(target_os = "macos")))]
fn system_fonts() -> ([Vec<FontPath>; 4], Vec<FontPath>) {
    let candidates = |pattern: &str, files: &[&str]| {
        let mut v: Vec<FontPath> = fc_match(pattern).into_iter().map(|p| (p, 0)).collect();
        for f in files {
            for dir in ["/usr/share/fonts/truetype/dejavu", "/usr/share/fonts/TTF", "/usr/share/fonts/dejavu"] {
                v.push((Path::new(dir).join(f), 0));
            }
        }
        v
    };
    (
        [
            candidates("monospace:style=Regular", &["DejaVuSansMono.ttf"]),
            candidates("monospace:style=Bold", &["DejaVuSansMono-Bold.ttf"]),
            candidates("monospace:style=Oblique", &["DejaVuSansMono-Oblique.ttf"]),
            candidates("monospace:style=Bold Oblique", &["DejaVuSansMono-BoldOblique.ttf"]),
        ],
        Vec::new(), // fallbacks come from `fc-match :charset=` per character
    )
}

enum Pick {
    Styled(usize),
    Regular,
    Fallback(usize),
}

pub struct Fonts {
    regular: Font,
    /// bold, italic, bold-italic
    styled: [Option<Font>; 3],
    fallbacks: Vec<Font>,
    fallback_queue: Vec<FontPath>,
    loaded: HashSet<PathBuf>,
    missing: HashSet<char>,
    px: f32,
    pub cell_w: usize,
    pub cell_h: usize,
    baseline: i32,
    underline: usize,
    thickness: usize,
    cache: HashMap<(char, u8), Rc<Glyph>>,
}

impl Fonts {
    pub fn load(custom: Option<&Path>, px: f32) -> Result<Fonts, String> {
        let (styles, fallbacks) = system_fonts();
        let [regular, bold, italic, bold_italic] = styles;
        let first = |list: &[FontPath]| list.iter().find_map(|(p, i)| Some((load_font(p, *i)?, (p.clone(), *i))));
        let (regular, styled) = match custom {
            Some(p) => (load_font(p, 0).ok_or(format!("cannot load font {}", p.display()))?, [None, None, None]),
            None => {
                let (font, path) = first(&regular).ok_or("no monospace font found; use --font <file.ttf>")?;
                // fontconfig answers "bold" with the regular file when no bold face exists;
                // drop those so bold is synthesized instead.
                let styled = [bold, italic, bold_italic]
                    .map(|list| first(&list).filter(|(_, p)| *p != path).map(|(f, _)| f));
                (font, styled)
            }
        };
        let mut fonts = Fonts {
            regular,
            styled,
            fallbacks: Vec::new(),
            fallback_queue: fallbacks.into_iter().rev().collect(),
            loaded: HashSet::new(),
            missing: HashSet::new(),
            px,
            cell_w: 1,
            cell_h: 1,
            baseline: 0,
            underline: 0,
            thickness: 1,
            cache: HashMap::new(),
        };
        fonts.set_px(px);
        Ok(fonts)
    }

    pub fn set_px(&mut self, px: f32) {
        self.px = px.max(4.0);
        let (ascent, descent, gap) = match self.regular.horizontal_line_metrics(self.px) {
            Some(m) => (m.ascent, m.descent, m.line_gap),
            None => (self.px * 0.8, -self.px * 0.2, 0.0),
        };
        self.cell_h = (ascent - descent + gap).ceil().max(1.0) as usize;
        self.cell_w = self.regular.metrics('M', self.px).advance_width.round().max(1.0) as usize;
        self.baseline = (ascent + gap / 2.0).round() as i32;
        self.thickness = (self.px / 14.0).round().max(1.0) as usize;
        self.underline = ((self.baseline as usize) + self.thickness + 1).min(self.cell_h - self.thickness);
        self.cache.clear();
    }

    pub fn glyph(&mut self, ch: char, style: u8) -> Rc<Glyph> {
        if let Some(g) = self.cache.get(&(ch, style)) {
            return g.clone();
        }
        let g = Rc::new(self.rasterize(ch, style));
        self.cache.insert((ch, style), g.clone());
        g
    }

    fn rasterize(&mut self, ch: char, style: u8) -> Glyph {
        if let Some(alpha) = builtin(ch, self.cell_w, self.cell_h) {
            return Glyph { w: self.cell_w, h: self.cell_h, left: 0, top: 0, alpha };
        }
        let pick = self.pick(ch, style);
        let font = match pick {
            Pick::Styled(i) => self.styled[i].as_ref().unwrap_or(&self.regular),
            Pick::Regular => &self.regular,
            Pick::Fallback(i) => &self.fallbacks[i],
        };
        let real_bold = matches!(pick, Pick::Styled(0) | Pick::Styled(2));
        let (m, alpha) = font.rasterize(ch, self.px);
        let mut g = Glyph {
            w: m.width,
            h: m.height,
            left: m.xmin,
            top: self.baseline - m.ymin - m.height as i32,
            alpha,
        };
        if style & 1 != 0 && !real_bold {
            embolden(&mut g);
        }
        g
    }

    fn pick(&mut self, ch: char, style: u8) -> Pick {
        let has = |f: &Font| f.lookup_glyph_index(ch) != 0;
        let styled = match style & 3 {
            1 => Some(0),
            2 => Some(1),
            3 => Some(if self.styled[2].is_some() { 2 } else { 0 }),
            _ => None,
        };
        if let Some(i) = styled {
            if self.styled[i].as_ref().is_some_and(has) {
                return Pick::Styled(i);
            }
        }
        if ch.is_ascii() || has(&self.regular) || self.missing.contains(&ch) {
            return Pick::Regular;
        }
        if let Some(i) = self.fallbacks.iter().position(has) {
            return Pick::Fallback(i);
        }
        // Load further fallback fonts lazily, only when a character needs one.
        while let Some((path, index)) = self.next_fallback(ch) {
            if !self.loaded.insert(path.clone()) {
                continue;
            }
            if let Some(f) = load_font(&path, index) {
                self.fallbacks.push(f);
                if has(self.fallbacks.last().unwrap()) {
                    return Pick::Fallback(self.fallbacks.len() - 1);
                }
            }
        }
        self.missing.insert(ch);
        Pick::Regular
    }

    fn next_fallback(&mut self, ch: char) -> Option<FontPath> {
        if let Some(p) = self.fallback_queue.pop() {
            return Some(p);
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let p = fc_match(&format!(":charset={:x}", ch as u32))?;
            if !self.loaded.contains(&p) {
                return Some((p, 0));
            }
        }
        let _ = ch;
        None
    }
}

fn embolden(g: &mut Glyph) {
    let w = g.w + 1;
    let mut a = vec![0u8; w * g.h];
    for y in 0..g.h {
        for x in 0..g.w {
            let v = g.alpha[y * g.w + x];
            let i = y * w + x;
            a[i] = a[i].max(v);
            a[i + 1] = a[i + 1].max(v);
        }
    }
    g.w = w;
    g.alpha = a;
}

// ---------------------------------------------------------------- built-in glyphs

fn fill(a: &mut [u8], w: usize, h: usize, x0: isize, y0: isize, x1: isize, y1: isize, v: u8) {
    let (x0, x1) = (x0.max(0) as usize, (x1.max(0) as usize).min(w));
    let (y0, y1) = (y0.max(0) as usize, (y1.max(0) as usize).min(h));
    for y in y0..y1 {
        a[y * w + x0.min(x1)..y * w + x1].fill(v);
    }
}

/// Line weights (up, right, down, left): 1 light, 2 heavy, 3 double.
fn box_weights(ch: char) -> Option<[u8; 4]> {
    Some(match ch {
        '─' => [0, 1, 0, 1], '│' => [1, 0, 1, 0],
        '┌' | '╭' => [0, 1, 1, 0], '┐' | '╮' => [0, 0, 1, 1], '└' | '╰' => [1, 1, 0, 0], '┘' | '╯' => [1, 0, 0, 1],
        '├' => [1, 1, 1, 0], '┤' => [1, 0, 1, 1], '┬' => [0, 1, 1, 1], '┴' => [1, 1, 0, 1], '┼' => [1, 1, 1, 1],
        '╴' => [0, 0, 0, 1], '╵' => [1, 0, 0, 0], '╶' => [0, 1, 0, 0], '╷' => [0, 0, 1, 0],
        '━' => [0, 2, 0, 2], '┃' => [2, 0, 2, 0],
        '┏' => [0, 2, 2, 0], '┓' => [0, 0, 2, 2], '┗' => [2, 2, 0, 0], '┛' => [2, 0, 0, 2],
        '┣' => [2, 2, 2, 0], '┫' => [2, 0, 2, 2], '┳' => [0, 2, 2, 2], '┻' => [2, 2, 0, 2], '╋' => [2, 2, 2, 2],
        '═' => [0, 3, 0, 3], '║' => [3, 0, 3, 0],
        '╔' => [0, 3, 3, 0], '╗' => [0, 0, 3, 3], '╚' => [3, 3, 0, 0], '╝' => [3, 0, 0, 3],
        '╠' => [3, 3, 3, 0], '╣' => [3, 0, 3, 3], '╦' => [0, 3, 3, 3], '╩' => [3, 3, 0, 3], '╬' => [3, 3, 3, 3],
        _ => return None,
    })
}

/// Pixel-exact block elements and box lines, so adjacent cells join seamlessly.
fn builtin(ch: char, w: usize, h: usize) -> Option<Vec<u8>> {
    let mut a = vec![0u8; w * h];
    let (wi, hi) = (w as isize, h as isize);
    let c = ch as u32;
    match c {
        0x2580 => fill(&mut a, w, h, 0, 0, wi, hi / 2, 255),
        0x2581..=0x2587 => {
            let n = (c - 0x2580) as isize;
            fill(&mut a, w, h, 0, hi - (hi * n + 4) / 8, wi, hi, 255);
        }
        0x2588 => fill(&mut a, w, h, 0, 0, wi, hi, 255),
        0x2589..=0x258f => {
            let n = (0x2590 - c) as isize;
            fill(&mut a, w, h, 0, 0, (wi * n + 4) / 8, hi, 255);
        }
        0x2590 => fill(&mut a, w, h, wi / 2, 0, wi, hi, 255),
        0x2591..=0x2593 => a.fill(64 * (c - 0x2590) as u8),
        0x2594 => fill(&mut a, w, h, 0, 0, wi, (hi / 8).max(1), 255),
        0x2595 => fill(&mut a, w, h, wi - (wi / 8).max(1), 0, wi, hi, 255),
        0x2596..=0x259f => {
            // quadrants: upper-left, upper-right, lower-left, lower-right
            let q: [bool; 4] = match c {
                0x2596 => [false, false, true, false],
                0x2597 => [false, false, false, true],
                0x2598 => [true, false, false, false],
                0x2599 => [true, false, true, true],
                0x259a => [true, false, false, true],
                0x259b => [true, true, true, false],
                0x259c => [true, true, false, true],
                0x259d => [false, true, false, false],
                0x259e => [false, true, true, false],
                _ => [false, true, true, true],
            };
            let (mx, my) = (wi / 2, hi / 2);
            for (i, &on) in q.iter().enumerate() {
                if on {
                    let (x0, x1) = if i % 2 == 0 { (0, mx) } else { (mx, wi) };
                    let (y0, y1) = if i < 2 { (0, my) } else { (my, hi) };
                    fill(&mut a, w, h, x0, y0, x1, y1, 255);
                }
            }
        }
        _ => {
            let weights = box_weights(ch)?;
            let t = (wi / 8).max(1);
            for (dir, &wt) in weights.iter().enumerate() {
                if wt == 0 {
                    continue;
                }
                let th = if wt == 2 { t * 2 } else { t };
                let offsets: &[isize] = if wt == 3 { &[-t, t] } else { &[0] };
                let (cx, cy) = ((wi - th) / 2, (hi - th) / 2);
                for &o in offsets {
                    let (vx, hy) = (cx + o, cy + o);
                    match dir {
                        0 => fill(&mut a, w, h, vx, 0, vx + th, cy + th, 255),
                        1 => fill(&mut a, w, h, cx, hy, wi, hy + th, 255),
                        2 => fill(&mut a, w, h, vx, cy, vx + th, hi, 255),
                        _ => fill(&mut a, w, h, 0, hy, cx + th, hy + th, 255),
                    }
                }
            }
        }
    }
    Some(a)
}

// ---------------------------------------------------------------- drawing

fn blend(dst: u32, src: u32, a: u32) -> u32 {
    if a >= 255 {
        return src;
    }
    let ia = 255 - a;
    let mix = |s: u32| (((src >> s) & 0xff) * a + ((dst >> s) & 0xff) * ia) / 255;
    mix(16) << 16 | mix(8) << 8 | mix(0)
}

/// A pixel buffer with a clip rectangle, so a pane can't paint over its neighbors.
struct Canvas<'a> {
    buf: &'a mut [u32],
    stride: usize,
    clip: Rect,
}

impl Canvas<'_> {
    fn rect(&mut self, x: usize, y: usize, rw: usize, rh: usize, c: u32) {
        let (x0, x1) = (x.max(self.clip.x), (x + rw).min(self.clip.x + self.clip.w));
        let (y0, y1) = (y.max(self.clip.y), (y + rh).min(self.clip.y + self.clip.h));
        if x0 >= x1 {
            return;
        }
        for yy in y0..y1 {
            self.buf[yy * self.stride + x0..yy * self.stride + x1].fill(c);
        }
    }

    fn visible(&self, x: i64, y: i64) -> bool {
        let c = &self.clip;
        x >= c.x as i64 && y >= c.y as i64 && x < (c.x + c.w) as i64 && y < (c.y + c.h) as i64
    }

    fn glyph(&mut self, g: &Glyph, x0: usize, y0: usize, fg: u32) {
        for gy in 0..g.h {
            let py = y0 as i64 + g.top as i64 + gy as i64;
            for gx in 0..g.w {
                let a = g.alpha[gy * g.w + gx];
                let px = x0 as i64 + g.left as i64 + gx as i64;
                if a == 0 || !self.visible(px, py) {
                    continue;
                }
                let i = py as usize * self.stride + px as usize;
                self.buf[i] = blend(self.buf[i], fg, a as u32);
            }
        }
    }

    fn image(&mut self, rgba: &[u8], iw: usize, ih: usize, x: i64, y: i64) {
        for iy in 0..ih {
            let py = y + iy as i64;
            for ix in 0..iw {
                let px = x + ix as i64;
                let p = &rgba[(iy * iw + ix) * 4..][..4];
                if p[3] == 0 || !self.visible(px, py) {
                    continue;
                }
                let src = (p[0] as u32) << 16 | (p[1] as u32) << 8 | p[2] as u32;
                let i = py as usize * self.stride + px as usize;
                self.buf[i] = blend(self.buf[i], src, p[3] as u32);
            }
        }
    }
}

/// One pane to draw: its terminal, where it goes, and its UI state.
pub struct PaneView<'a> {
    pub term: &'a Term,
    pub rect: Rect,
    pub selection: Option<&'a Selection>,
    /// Draw a solid cursor (focused pane in a focused window); otherwise hollow.
    pub focused: bool,
    pub copy: Option<CopyView<'a>>,
}

/// Copy-mode overlay for a pane.
pub struct CopyView<'a> {
    pub cursor: (i64, usize),
    pub search: &'a str,
    pub status: String,
}

pub struct TabLabel {
    pub rect: Rect,
    pub text: String,
    pub active: bool,
}

/// Window chrome drawn over the panes.
#[derive(Default)]
pub struct Ui<'a> {
    /// Tab bar: its area, the tabs, and text for the right end (session, status).
    pub bar: Option<(Rect, &'a [TabLabel], &'a str)>,
    /// A centered box of text (help, leader hint, messages).
    pub popup: Option<&'a [String]>,
}

/// Which cells of a line are part of a match for `needle` (smart case).
fn match_cells(term: &Term, abs: i64, needle: &str) -> Vec<bool> {
    let mut out = vec![false; term.cols];
    let smart = needle.chars().any(char::is_uppercase);
    let norm = |c: char| if smart { c } else { c.to_lowercase().next().unwrap_or(c) };
    let n: Vec<char> = needle.chars().map(norm).collect();
    let chars = term.line_chars(abs);
    if n.is_empty() || chars.len() < n.len() {
        return out;
    }
    let hay: Vec<char> = chars.iter().map(|&(c, _)| norm(c)).collect();
    for i in 0..=hay.len() - n.len() {
        if hay[i..i + n.len()] == n[..] {
            for col in chars[i].1..=chars[i + n.len() - 1].1 {
                if let Some(m) = out.get_mut(col) {
                    *m = true;
                }
            }
        }
    }
    out
}

pub struct Renderer {
    pub fonts: Fonts,
    pub theme: Theme,
    pub pad: usize,
    /// Scaled copies of on-screen images; holding the Arc keeps the key unique.
    scaled: Vec<(Arc<Image>, u32, u32, Rc<Vec<u8>>)>,
}

impl Renderer {
    pub fn new(fonts: Fonts) -> Renderer {
        Renderer { fonts, theme: Theme::default(), pad: 4, scaled: Vec::new() }
    }

    pub fn set_scale(&mut self, font_size: f32, scale: f32) {
        self.fonts.set_px(font_size * scale);
        self.pad = (4.0 * scale).round() as usize;
    }

    pub fn cell(&self) -> (usize, usize) {
        (self.fonts.cell_w, self.fonts.cell_h)
    }

    pub fn grid_size(&self, w: usize, h: usize) -> (usize, usize) {
        let (cw, ch) = self.cell();
        ((w.saturating_sub(2 * self.pad) / cw).max(2), (h.saturating_sub(2 * self.pad) / ch).max(1))
    }

    pub fn frame_size(&self, cols: usize, rows: usize) -> (usize, usize) {
        let (cw, ch) = self.cell();
        (cols * cw + 2 * self.pad, rows * ch + 2 * self.pad)
    }

    fn color(&self, c: Color, default: u32) -> u32 {
        match c {
            Color::Default => default,
            Color::Idx(i) => self.theme.palette[i as usize],
            Color::Rgb(r, g, b) => (r as u32) << 16 | (g as u32) << 8 | b as u32,
        }
    }

    fn colors(&self, cell: &Cell) -> (u32, u32) {
        let mut fg = self.color(cell.fg, self.theme.fg);
        let mut bg = self.color(cell.bg, self.theme.bg);
        if cell.flags & term::INVERSE != 0 {
            std::mem::swap(&mut fg, &mut bg);
        }
        if cell.flags & term::DIM != 0 {
            fg = blend(bg, fg, 150);
        }
        (fg, bg)
    }

    /// Draw a frame: every pane, the dividers between them, and an outline around
    /// the focused pane when there is more than one.
    /// Draw `s` starting at (x, y), within `max_w` pixels. Returns the width used.
    fn text(&mut self, canvas: &mut Canvas, x: usize, y: usize, s: &str, fg: u32, max_w: usize) -> usize {
        let cw = self.fonts.cell_w;
        let mut cx = x;
        for ch in s.chars() {
            let w = ch.width().unwrap_or(0) * cw;
            if w == 0 {
                continue;
            }
            if cx + w > x + max_w {
                break;
            }
            if ch != ' ' {
                let g = self.fonts.glyph(ch, 0);
                canvas.glyph(&g, cx, y, fg);
            }
            cx += w;
        }
        cx - x
    }

    /// Text width in pixels.
    pub fn text_width(&self, s: &str) -> usize {
        s.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>() * self.fonts.cell_w
    }

    fn draw_ui(&mut self, canvas: &mut Canvas, w: usize, h: usize, ui: &Ui) {
        let (cw, ch) = self.cell();
        let t = (self.theme.fg, self.theme.bg, self.theme.accent);
        if let Some((bar, tabs, right)) = ui.bar {
            canvas.rect(bar.x, bar.y, bar.w, bar.h, 0x16161e);
            let ty = bar.y + bar.h.saturating_sub(ch) / 2;
            for tab in tabs {
                let r = tab.rect;
                let (bg, fg) = if tab.active { (t.2, t.1) } else { (0x24283b, 0x9aa5ce) };
                canvas.rect(r.x, r.y, r.w, r.h, bg);
                self.text(canvas, r.x + cw / 2, ty, &tab.text, fg, r.w.saturating_sub(cw / 2));
            }
            let rw = self.text_width(right);
            let rx = (bar.x + bar.w).saturating_sub(rw + cw);
            self.text(canvas, rx, ty, right, 0x9aa5ce, rw);
        }
        if let Some(lines) = ui.popup {
            let inner_w = lines.iter().map(|l| self.text_width(l)).max().unwrap_or(0);
            let (pw, ph) = ((inner_w + 4 * cw).min(w), (lines.len() * ch + 2 * ch).min(h));
            let (px, py) = ((w - pw) / 2, (h - ph) / 2);
            canvas.rect(px, py, pw, ph, 0x1f2335);
            for (x, y, rw, rh) in [(px, py, pw, 1), (px, py + ph - 1, pw, 1), (px, py, 1, ph), (px + pw - 1, py, 1, ph)] {
                canvas.rect(x, y, rw, rh, t.2);
            }
            for (i, line) in lines.iter().enumerate() {
                self.text(canvas, px + 2 * cw, py + ch + i * ch, line, t.0, pw.saturating_sub(4 * cw));
            }
        }
    }

    pub fn draw(&mut self, buf: &mut [u32], w: usize, h: usize, panes: &[PaneView], dividers: &[Rect], outline: Option<Rect>, ui: &Ui) {
        buf[..w * h].fill(self.theme.bg);
        let frame = Rect { x: 0, y: 0, w, h };
        let mut used = Vec::new();
        for view in panes {
            let r = view.rect;
            let clip = Rect { w: r.w.min(w.saturating_sub(r.x)), h: r.h.min(h.saturating_sub(r.y)), ..r };
            let mut canvas = Canvas { buf: &mut *buf, stride: w, clip };
            self.draw_pane(&mut canvas, view, &mut used);
        }
        let mut idx = 0;
        self.scaled.retain(|_| {
            idx += 1;
            used.contains(&(idx - 1))
        });
        let mut canvas = Canvas { buf, stride: w, clip: frame };
        for d in dividers {
            canvas.rect(d.x, d.y, d.w, d.h, self.theme.divider);
        }
        if let Some(r) = outline {
            let c = self.theme.accent;
            canvas.rect(r.x, r.y, r.w, 1, c);
            canvas.rect(r.x, (r.y + r.h).saturating_sub(1), r.w, 1, c);
            canvas.rect(r.x, r.y, 1, r.h, c);
            canvas.rect((r.x + r.w).saturating_sub(1), r.y, 1, r.h, c);
        }
        self.draw_ui(&mut canvas, w, h, ui);
    }

    fn draw_pane(&mut self, canvas: &mut Canvas, view: &PaneView, used: &mut Vec<usize>) {
        let (term, sel, focused) = (view.term, view.selection, view.focused);
        let (cw, ch) = self.cell();
        let (ox, oy) = (view.rect.x + self.pad, view.rect.y + self.pad);
        let top = term.view_top();
        let (cx, cy) = term.cursor_pos();
        let cursor_abs = term.abs_row(cy);
        let copy = view.copy.as_ref();
        let show_cursor = term.modes.cursor_visible && term.display_offset == 0 && copy.is_none();
        let (thick, uline) = (self.fonts.thickness, self.fonts.underline);

        for r in 0..term.rows {
            let abs = top + r as i64;
            let Some(line) = term.line_abs(abs) else { continue };
            let y0 = oy + r * ch;
            let matches = copy.filter(|c| !c.search.is_empty()).map(|c| match_cells(term, abs, c.search));
            for (c, cell) in line.iter().enumerate().take(term.cols) {
                if cell.flags & term::SPACER != 0 {
                    continue;
                }
                let span = if cell.flags & term::WIDE != 0 { 2 } else { 1 };
                let x0 = ox + c * cw;
                let (mut fg, mut bg) = self.colors(cell);
                if sel.is_some_and(|s| s.contains(abs, c)) {
                    bg = self.theme.selection;
                }
                if matches.as_ref().is_some_and(|m| m.get(c) == Some(&true)) {
                    (fg, bg) = (self.theme.bg, 0xe0af68);
                }
                if copy.is_some_and(|cv| cv.cursor == (abs, c)) {
                    (fg, bg) = (self.theme.bg, self.theme.accent);
                }
                let on_cursor = show_cursor && abs == cursor_abs && c == cx;
                if on_cursor && focused && term.cursor_shape == CursorShape::Block {
                    bg = self.theme.cursor;
                    fg = self.theme.bg;
                }
                if bg != self.theme.bg {
                    canvas.rect(x0, y0, cw * span, ch, bg);
                }
                if cell.ch != ' ' && cell.flags & term::HIDDEN == 0 {
                    let style = (cell.flags & term::BOLD != 0) as u8 | ((cell.flags & term::ITALIC != 0) as u8) << 1;
                    let g = self.fonts.glyph(cell.ch, style);
                    canvas.glyph(&g, x0, y0, fg);
                }
                if cell.flags & term::UNDERLINE != 0 {
                    canvas.rect(x0, y0 + uline, cw * span, thick, fg);
                }
                if cell.flags & term::STRIKE != 0 {
                    canvas.rect(x0, y0 + ch / 2, cw * span, thick, fg);
                }
            }
        }

        // Images sit above the text, scrolled with the line they were drawn on.
        for p in term.images() {
            let tw = (p.w_cells * cw as f32).round().max(1.0) as u32;
            let th = (p.h_cells * ch as f32).round().max(1.0) as u32;
            let y = oy as i64 + (p.row - top) * ch as i64;
            let clip = canvas.clip;
            if y >= (clip.y + clip.h) as i64 || y + (th as i64) <= clip.y as i64 {
                continue;
            }
            let found = self.scaled.iter().position(|(img, w, h, _)| Arc::ptr_eq(img, &p.image) && (*w, *h) == (tw, th));
            let i = found.unwrap_or_else(|| {
                let px = if (tw, th) == (p.image.w, p.image.h) {
                    p.image.rgba.clone()
                } else {
                    graphics::resize(&p.image, tw, th)
                };
                self.scaled.push((p.image.clone(), tw, th, Rc::new(px)));
                self.scaled.len() - 1
            });
            let px = self.scaled[i].3.clone();
            used.push(i);
            canvas.image(&px, tw as usize, th as usize, (ox + p.col * cw) as i64, y);
        }

        let row = cursor_abs - top;
        if show_cursor && (0..term.rows as i64).contains(&row) {
            let (x0, y0) = (ox + cx * cw, oy + row as usize * ch);
            let c = self.theme.cursor;
            if !focused {
                canvas.rect(x0, y0, cw, 1, c);
                canvas.rect(x0, y0 + ch - 1, cw, 1, c);
                canvas.rect(x0, y0, 1, ch, c);
                canvas.rect(x0 + cw - 1, y0, 1, ch, c);
            } else {
                match term.cursor_shape {
                    CursorShape::Underline => canvas.rect(x0, y0 + ch - 2 * thick, cw, 2 * thick, c),
                    CursorShape::Bar => canvas.rect(x0, y0, 2 * thick, ch, c),
                    CursorShape::Block => {}
                }
            }
        }

        // Copy mode status line across the bottom row of the pane.
        if let Some(cv) = copy {
            let y0 = oy + term.rows.saturating_sub(1) * ch;
            let (x0, w) = (view.rect.x, view.rect.w);
            canvas.rect(x0, y0, w, ch, self.theme.accent);
            let bg = self.theme.bg;
            self.text(canvas, ox, y0, &cv.status, bg, w.saturating_sub(2 * self.pad));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_blocks_tile() {
        let up = builtin('▀', 8, 17).unwrap();
        let down = builtin('▄', 8, 17).unwrap();
        // Every pixel covered by exactly one of the two halves.
        assert!(up.iter().zip(&down).all(|(a, b)| (*a == 255) != (*b == 255)));
    }

    #[test]
    fn box_lines_reach_edges() {
        let g = builtin('┼', 8, 16).unwrap();
        assert_eq!(g[3], 255); // top edge, center column
        assert_eq!(g[15 * 8 + 3], 255); // bottom edge
        assert_eq!(g[7 * 8], 255); // left edge, center row
        assert_eq!(g[7 * 8 + 7], 255); // right edge
    }
}
