use std::fmt::Write as _;
use std::io::Cursor;
use std::path::Path;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, ImageFormat, RgbaImage};

/// How images are drawn in the terminal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Protocol {
    /// Kitty graphics protocol (kitty, Ghostty, WezTerm, lterm).
    Kitty,
    /// iTerm2 inline images (iTerm2, WezTerm, mintty).
    Iterm,
    /// DEC sixel (Windows Terminal 1.22+, foot, mlterm, xterm -ti vt340, Konsole).
    Sixel,
    /// Unicode half blocks in 24-bit color: works in any modern terminal, over SSH, in tmux.
    Blocks,
    /// Plain ASCII luminance art, for terminals without color.
    Ascii,
    /// Show alt text only.
    None,
}

impl Protocol {
    /// Parse a protocol name; `Ok(None)` means "auto".
    pub fn parse(s: &str) -> Result<Option<Protocol>, String> {
        Ok(Some(match s.to_ascii_lowercase().as_str() {
            "auto" => return Ok(None),
            "kitty" => Protocol::Kitty,
            "iterm" | "iterm2" => Protocol::Iterm,
            "sixel" => Protocol::Sixel,
            "blocks" | "halfblocks" => Protocol::Blocks,
            "ascii" => Protocol::Ascii,
            "none" | "off" => Protocol::None,
            _ => return Err(format!("unknown image mode '{s}'")),
        }))
    }

    /// Guess the best protocol from environment variables.
    pub fn detect() -> Protocol {
        let env = |k: &str| std::env::var(k).unwrap_or_default();
        let term = env("TERM");
        let program = env("TERM_PROGRAM");
        if term == "dumb" {
            Protocol::Ascii
        } else if !env("TMUX").is_empty() || term.starts_with("screen") {
            // Multiplexers swallow graphics escapes unless specially configured.
            Protocol::Blocks
        } else if !env("KITTY_WINDOW_ID").is_empty()
            || term.contains("kitty")
            || term.contains("ghostty")
            || program == "ghostty"
            || program == "lterm"
        {
            Protocol::Kitty
        } else if program == "iTerm.app" || program == "WezTerm" || env("LC_TERMINAL") == "iTerm2" {
            Protocol::Iterm
        } else if program == "mintty"
            || term.contains("foot")
            || term.contains("mlterm")
            || term.contains("contour")
            || !env("KONSOLE_VERSION").is_empty()
        {
            Protocol::Sixel
        } else {
            // Includes Windows Terminal: sixel only works from 1.22, so it's opt-in.
            Protocol::Blocks
        }
    }
}

pub struct ImageOpts {
    pub protocol: Protocol,
    /// Maximum image height in terminal rows (0 = unlimited).
    pub max_rows: usize,
    /// Assumed size of one terminal cell in pixels.
    pub cell_w: u32,
    pub cell_h: u32,
}

pub enum Rendered {
    /// One string per terminal row.
    Rows(Vec<String>),
    /// A single escape sequence that draws the whole image at the cursor.
    Raw(String),
}

/// Load an image from a local path (relative to `base`), a `data:` URI or,
/// with the `remote` feature, an http(s) URL.
pub fn load(src: &str, base: &Path) -> Result<DynamicImage, String> {
    let src = src.trim();
    let bytes = if let Some(rest) = src.strip_prefix("data:") {
        let (meta, data) = rest.split_once(',').ok_or("malformed data URI")?;
        if meta.ends_with(";base64") {
            B64.decode(data.trim()).map_err(|e| e.to_string())?
        } else {
            percent_decode(data)
        }
    } else if src.starts_with("http://") || src.starts_with("https://") {
        fetch(src)?
    } else {
        let raw = src.strip_prefix("file://").unwrap_or(src);
        let decoded = String::from_utf8_lossy(&percent_decode(raw)).into_owned();
        let path = Path::new(&decoded);
        let full = if path.is_absolute() { path.to_path_buf() } else { base.join(path) };
        std::fs::read(&full).map_err(|e| format!("{}: {e}", full.display()))?
    };
    decode(&bytes)
}

pub fn decode(bytes: &[u8]) -> Result<DynamicImage, String> {
    image::load_from_memory(bytes).map_err(|e| e.to_string())
}

#[cfg(feature = "remote")]
fn fetch(url: &str) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let resp = ureq::get(url)
        .timeout(std::time::Duration::from_secs(10))
        .call()
        .map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    resp.into_reader()
        .take(32 << 20)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    Ok(buf)
}

#[cfg(not(feature = "remote"))]
fn fetch(_url: &str) -> Result<Vec<u8>, String> {
    Err("remote images need mdterm built with --features remote".into())
}

fn percent_decode(s: &str) -> Vec<u8> {
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(hi), Some(lo)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// Size in cells: natural size (one cell per `cell_w` pixels), shrunk to fit
/// `max_cols` and `max_rows` while keeping the aspect ratio.
fn fit(w: u32, h: u32, max_cols: usize, o: &ImageOpts) -> (usize, usize) {
    let (w, h) = (w as f64, h as f64);
    let (cw, ch) = (o.cell_w as f64, o.cell_h as f64);
    let rows_for = |cols: usize| ((cols as f64 * cw * h) / (w * ch)).ceil().max(1.0) as usize;
    let mut cols = ((w / cw).ceil() as usize).clamp(1, max_cols.max(1));
    if o.max_rows > 0 && rows_for(cols) > o.max_rows {
        cols = ((o.max_rows as f64 * ch * w) / (cw * h)).floor().max(1.0) as usize;
    }
    (cols, rows_for(cols))
}

fn scale(img: &DynamicImage, w: u32, h: u32) -> RgbaImage {
    let (w, h) = (w.max(1), h.max(1));
    if w < img.width() && h < img.height() {
        img.thumbnail_exact(w, h).to_rgba8()
    } else {
        img.resize_exact(w, h, FilterType::Triangle).to_rgba8()
    }
}

pub fn render(img: &DynamicImage, max_cols: usize, o: &ImageOpts) -> Rendered {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 || o.protocol == Protocol::None {
        return Rendered::Rows(Vec::new());
    }
    let (cols, rows) = fit(w, h, max_cols, o);
    match o.protocol {
        Protocol::Blocks => Rendered::Rows(blocks(&scale(img, cols as u32, rows as u32 * 2))),
        Protocol::Ascii => Rendered::Rows(ascii(&scale(img, cols as u32, rows as u32))),
        Protocol::Kitty | Protocol::Iterm => {
            // Downscale before encoding; the terminal scales to `cols` itself.
            let max_px = cols as u32 * o.cell_w * 2;
            let img = if w > max_px {
                DynamicImage::ImageRgba8(scale(img, max_px, (h as u64 * max_px as u64 / w as u64) as u32))
            } else {
                DynamicImage::ImageRgba8(img.to_rgba8())
            };
            let mut png = Cursor::new(Vec::new());
            if img.write_to(&mut png, ImageFormat::Png).is_err() {
                return Rendered::Rows(Vec::new());
            }
            let png = png.into_inner();
            Rendered::Raw(if o.protocol == Protocol::Kitty { kitty(&png, cols) } else { iterm(&png, cols) })
        }
        Protocol::Sixel => {
            let pw = cols as u32 * o.cell_w;
            let ph = ((pw as f64 * h as f64 / w as f64).round() as u32).clamp(1, rows as u32 * o.cell_h);
            Rendered::Raw(sixel(&scale(img, pw, ph)))
        }
        Protocol::None => unreachable!(),
    }
}

/// Two pixels per cell using the upper half block: fg = top pixel, bg = bottom.
fn blocks(px: &RgbaImage) -> Vec<String> {
    let (w, h) = px.dimensions();
    (0..h / 2)
        .map(|y| {
            let mut s = String::new();
            for x in 0..w {
                let t = px.get_pixel(x, 2 * y).0;
                let b = px.get_pixel(x, 2 * y + 1).0;
                let _ = match (t[3] >= 128, b[3] >= 128) {
                    (true, true) => write!(
                        s,
                        "\x1b[38;2;{};{};{};48;2;{};{};{}m\u{2580}",
                        t[0], t[1], t[2], b[0], b[1], b[2]
                    ),
                    (true, false) => write!(s, "\x1b[0;38;2;{};{};{}m\u{2580}", t[0], t[1], t[2]),
                    (false, true) => write!(s, "\x1b[0;38;2;{};{};{}m\u{2584}", b[0], b[1], b[2]),
                    (false, false) => write!(s, "\x1b[0m "),
                };
            }
            s.push_str("\x1b[0m");
            s
        })
        .collect()
}

fn ascii(px: &RgbaImage) -> Vec<String> {
    const RAMP: &[u8] = b" .:-=+*#%@";
    px.rows()
        .map(|row| {
            row.map(|p| {
                let [r, g, b, a] = p.0;
                if a < 128 {
                    ' '
                } else {
                    let lum = (0.299 * r as f64 + 0.587 * g as f64 + 0.114 * b as f64) / 255.0;
                    RAMP[(lum * (RAMP.len() - 1) as f64).round() as usize] as char
                }
            })
            .collect()
        })
        .collect()
}

fn kitty(png: &[u8], cols: usize) -> String {
    let b64 = B64.encode(png);
    let chunks: Vec<&[u8]> = b64.as_bytes().chunks(4096).collect();
    let mut s = String::new();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = (i + 1 < chunks.len()) as u8;
        let chunk = std::str::from_utf8(chunk).unwrap_or_default();
        // q=2 suppresses the terminal's reply so nothing leaks into stdin.
        let _ = if i == 0 {
            write!(s, "\x1b_Ga=T,f=100,q=2,c={cols},m={more};{chunk}\x1b\\")
        } else {
            write!(s, "\x1b_Gm={more};{chunk}\x1b\\")
        };
    }
    s
}

fn iterm(png: &[u8], cols: usize) -> String {
    format!(
        "\x1b]1337;File=inline=1;size={};width={cols};preserveAspectRatio=1:{}\x07",
        png.len(),
        B64.encode(png)
    )
}

/// Encode as sixel with a 256-color NeuQuant palette; transparent pixels stay unpainted.
fn sixel(px: &RgbaImage) -> String {
    let (w, h) = px.dimensions();
    let (wu, hu) = (w as usize, h as usize);
    let opaque: Vec<u8> = px
        .pixels()
        .filter(|p| p.0[3] >= 128)
        .flat_map(|p| [p.0[0], p.0[1], p.0[2], 255])
        .collect();
    if opaque.is_empty() {
        return String::new();
    }
    let nq = color_quant::NeuQuant::new(10, 256, &opaque);
    let palette = nq.color_map_rgb();
    let idx: Vec<i16> = px
        .pixels()
        .map(|p| {
            let [r, g, b, a] = p.0;
            if a < 128 { -1 } else { nq.index_of(&[r, g, b, 255]) as i16 }
        })
        .collect();

    let mut s = String::new();
    let _ = write!(s, "\x1bP0;1;0q\"1;1;{w};{h}");
    for (i, c) in palette.chunks(3).enumerate() {
        let pct = |v: u8| v as u32 * 100 / 255;
        let _ = write!(s, "#{i};2;{};{};{}", pct(c[0]), pct(c[1]), pct(c[2]));
    }
    let ncolors = palette.len() / 3;
    for band in (0..hu).step_by(6) {
        let mut slots: Vec<Option<Vec<u8>>> = vec![None; ncolors];
        for dy in 0..6.min(hu - band) {
            let row = &idx[(band + dy) * wu..(band + dy + 1) * wu];
            for (x, &c) in row.iter().enumerate() {
                if c >= 0 {
                    slots[c as usize].get_or_insert_with(|| vec![0; wu])[x] |= 1 << dy;
                }
            }
        }
        let mut first = true;
        for (c, bits) in slots.iter().enumerate() {
            let Some(bits) = bits else { continue };
            if !first {
                s.push('$');
            }
            first = false;
            let _ = write!(s, "#{c}");
            let end = bits.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
            let mut x = 0;
            while x < end {
                let b = bits[x];
                let run = bits[x..end].iter().take_while(|&&v| v == b).count();
                let ch = (63 + b) as char;
                if run > 3 {
                    let _ = write!(s, "!{run}{ch}");
                } else {
                    s.extend(std::iter::repeat(ch).take(run));
                }
                x += run;
            }
        }
        s.push('-');
    }
    s.push_str("\x1b\\");
    s
}
