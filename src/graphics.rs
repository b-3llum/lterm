//! Terminal image protocols: kitty graphics (APC `G`) and DEC sixel (DCS `q`).

use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::Engine;

/// Largest image we accept, in pixels per side and total bytes.
const MAX_SIDE: u32 = 10_000;
const MAX_BYTES: usize = 256 << 20;

const B64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

pub struct Image {
    pub w: u32,
    pub h: u32,
    /// Straight (non-premultiplied) RGBA, row-major.
    pub rgba: Vec<u8>,
}

fn check_size(w: u32, h: u32) -> Result<(), String> {
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE || (w as usize * h as usize * 4) > MAX_BYTES {
        Err(format!("EINVAL:bad image size {w}x{h}"))
    } else {
        Ok(())
    }
}

pub fn decode_png(data: &[u8]) -> Result<Image, String> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(data));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().map_err(|e| format!("EBADPNG:{e}"))?;
    let (w, h) = (reader.info().width, reader.info().height);
    check_size(w, h)?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(|e| format!("EBADPNG:{e}"))?;
    let buf = &buf[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf.to_vec(),
        png::ColorType::Rgb => buf.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf.as_chunks::<2>().0.iter().flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return Err("EBADPNG:unexpanded palette".into()),
    };
    Ok(Image { w, h, rgba })
}

/// Resample to `tw`x`th` with a box filter (averages when shrinking, nearest when growing).
pub fn resize(img: &Image, tw: u32, th: u32) -> Vec<u8> {
    let (w, h) = (img.w as usize, img.h as usize);
    let (tw, th) = (tw.max(1) as usize, th.max(1) as usize);
    let mut out = vec![0u8; tw * th * 4];
    for ty in 0..th {
        let y0 = ty * h / th;
        let y1 = ((ty + 1) * h / th).clamp(y0 + 1, h);
        for tx in 0..tw {
            let x0 = tx * w / tw;
            let x1 = ((tx + 1) * w / tw).clamp(x0 + 1, w);
            let (mut r, mut g, mut b, mut a, mut n) = (0u64, 0u64, 0u64, 0u64, 0u64);
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = &img.rgba[(y * w + x) * 4..][..4];
                    let pa = p[3] as u64;
                    r += p[0] as u64 * pa;
                    g += p[1] as u64 * pa;
                    b += p[2] as u64 * pa;
                    a += pa;
                    n += 1;
                }
            }
            let o = &mut out[(ty * tw + tx) * 4..][..4];
            if a > 0 {
                o.copy_from_slice(&[(r / a) as u8, (g / a) as u8, (b / a) as u8, (a / n) as u8]);
            }
        }
    }
    out
}

// ---------------------------------------------------------------- kitty

/// Parsed control data of a kitty graphics command (`key=value,...`).
#[derive(Clone, Debug)]
pub struct KittyCmd {
    pub action: u8,
    pub format: u32,
    pub medium: u8,
    pub more: bool,
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub cols: u32,
    pub rows: u32,
    pub quiet: u32,
    pub compressed: bool,
    pub no_move: bool,
    pub delete: u8,
}

impl KittyCmd {
    pub fn parse(control: &str) -> KittyCmd {
        let mut c = KittyCmd {
            action: b't',
            format: 32,
            medium: b'd',
            more: false,
            id: 0,
            width: 0,
            height: 0,
            cols: 0,
            rows: 0,
            quiet: 0,
            compressed: false,
            no_move: false,
            delete: b'a',
        };
        for kv in control.split(',') {
            let Some((k, v)) = kv.split_once('=') else { continue };
            let num = || v.parse::<u32>().unwrap_or(0);
            let byte = || v.bytes().next().unwrap_or(0);
            match k {
                "a" => c.action = byte(),
                "f" => c.format = num(),
                "t" => c.medium = byte(),
                "m" => c.more = v == "1",
                "i" => c.id = num(),
                "s" => c.width = num(),
                "v" => c.height = num(),
                "c" => c.cols = num(),
                "r" => c.rows = num(),
                "q" => c.quiet = num(),
                "o" => c.compressed = v == "z",
                "C" => c.no_move = v == "1",
                "d" => c.delete = byte(),
                _ => {}
            }
        }
        c
    }
}

/// Decode the (possibly chunk-joined) base64 payload of a transmit command.
pub fn kitty_decode(cmd: &KittyCmd, payload: &[u8]) -> Result<Image, String> {
    if cmd.medium != b'd' {
        // File and shared-memory transfers would let remote programs probe local files.
        return Err("EINVAL:only direct transmission (t=d) is supported".into());
    }
    let clean: Vec<u8> = payload.iter().copied().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut data = B64.decode(&clean).map_err(|_| "EINVAL:bad base64".to_string())?;
    if cmd.compressed {
        data = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&data, MAX_BYTES)
            .map_err(|_| "EINVAL:bad zlib data".to_string())?;
    }
    match cmd.format {
        100 => decode_png(&data),
        24 | 32 => {
            let (w, h) = (cmd.width, cmd.height);
            check_size(w, h)?;
            let bpp = if cmd.format == 24 { 3 } else { 4 };
            let need = w as usize * h as usize * bpp;
            if data.len() < need {
                return Err("ENODATA:not enough pixel data".into());
            }
            let rgba = if bpp == 4 {
                data.truncate(need);
                data
            } else {
                data[..need].as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect()
            };
            Ok(Image { w, h, rgba })
        }
        f => Err(format!("EINVAL:unsupported format {f}")),
    }
}

// ---------------------------------------------------------------- sixel

/// Incremental sixel decoder; fed one byte at a time from the DCS payload.
pub struct Sixel {
    palette: Vec<[u8; 3]>,
    color: usize,
    x: usize,
    y: usize,
    repeat: usize,
    cmd: u8,
    params: Vec<u32>,
    /// Canvas, grown on demand.
    data: Vec<u8>,
    cap_w: usize,
    cap_h: usize,
    /// Extent actually painted (or declared by raster attributes).
    w: usize,
    h: usize,
}

const VT340: [[u8; 3]; 16] = [
    [0, 0, 0], [20, 20, 80], [80, 13, 13], [20, 80, 20], [80, 20, 80], [20, 80, 80], [80, 80, 20], [53, 53, 53],
    [26, 26, 26], [33, 33, 60], [60, 26, 26], [33, 60, 33], [60, 33, 60], [33, 60, 60], [60, 60, 33], [80, 80, 80],
];

fn pct(v: u32) -> u8 {
    (v.min(100) * 255 / 100) as u8
}

/// Sixel HLS: hue 0 = blue, 120 = red, 240 = green.
fn hls(h: u32, l: u32, s: u32) -> [u8; 3] {
    let h = ((h + 240) % 360) as f32;
    let (l, s) = (l.min(100) as f32 / 100.0, s.min(100) as f32 / 100.0);
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let f = |v: f32| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    [f(r), f(g), f(b)]
}

impl Sixel {
    pub fn new() -> Sixel {
        let mut palette = vec![[0u8; 3]; 256];
        for (i, c) in VT340.iter().enumerate() {
            palette[i] = [pct(c[0] as u32), pct(c[1] as u32), pct(c[2] as u32)];
        }
        Sixel {
            palette,
            color: 0,
            x: 0,
            y: 0,
            repeat: 0,
            cmd: 0,
            params: Vec::new(),
            data: Vec::new(),
            cap_w: 0,
            cap_h: 0,
            w: 0,
            h: 0,
        }
    }

    pub fn put(&mut self, b: u8) {
        if self.cmd != 0 {
            match b {
                b'0'..=b'9' => {
                    if let Some(p) = self.params.last_mut() {
                        *p = p.saturating_mul(10).saturating_add((b - b'0') as u32);
                    }
                    return;
                }
                b';' => {
                    if self.params.len() < 16 {
                        self.params.push(0);
                    }
                    return;
                }
                _ => self.finish_cmd(),
            }
        }
        match b {
            b'#' | b'!' | b'"' => {
                self.cmd = b;
                self.params = vec![0];
            }
            b'$' => self.x = 0,
            b'-' => {
                self.x = 0;
                self.y += 6;
            }
            b'?'..=b'~' => {
                let n = std::mem::take(&mut self.repeat).max(1);
                self.paint(b - b'?', n);
            }
            _ => {}
        }
    }

    fn finish_cmd(&mut self) {
        let p = &self.params;
        match self.cmd {
            b'#' => {
                let c = p[0] as usize % 256;
                if p.len() >= 5 {
                    self.palette[c] = if p[1] == 1 { hls(p[2], p[3], p[4]) } else { [pct(p[2]), pct(p[3]), pct(p[4])] };
                }
                self.color = c;
            }
            b'!' => self.repeat = (p[0] as usize).min(MAX_SIDE as usize),
            b'"' if p.len() >= 4 => {
                let (w, h) = ((p[2] as usize).min(MAX_SIDE as usize), (p[3] as usize).min(MAX_SIDE as usize));
                self.grow(w, h);
                self.w = self.w.max(w);
                self.h = self.h.max(h);
            }
            _ => {}
        }
        self.cmd = 0;
    }

    fn grow(&mut self, need_w: usize, need_h: usize) {
        if need_w <= self.cap_w && need_h <= self.cap_h {
            return;
        }
        let nw = need_w.max(self.cap_w).next_power_of_two().min(MAX_SIDE as usize);
        let nh = need_h.max(self.cap_h).next_power_of_two().min(MAX_SIDE as usize);
        let mut data = vec![0u8; nw * nh * 4];
        for y in 0..self.cap_h {
            let src = &self.data[y * self.cap_w * 4..][..self.cap_w * 4];
            data[y * nw * 4..][..self.cap_w * 4].copy_from_slice(src);
        }
        self.data = data;
        self.cap_w = nw;
        self.cap_h = nh;
    }

    fn paint(&mut self, bits: u8, n: usize) {
        let (x, y) = (self.x, self.y);
        self.x = x.saturating_add(n);
        if bits == 0 || x >= MAX_SIDE as usize || y + 6 > MAX_SIDE as usize {
            return;
        }
        let n = n.min(MAX_SIDE as usize - x);
        self.grow(x + n, y + 6);
        let [r, g, b] = self.palette[self.color];
        for i in 0..6 {
            if bits & (1 << i) == 0 {
                continue;
            }
            let row = (y + i) * self.cap_w;
            for px in x..x + n {
                self.data[(row + px) * 4..][..4].copy_from_slice(&[r, g, b, 255]);
            }
            self.h = self.h.max(y + i + 1);
        }
        self.w = self.w.max(x + n);
    }

    pub fn finish(mut self) -> Option<Image> {
        if self.cmd != 0 {
            self.finish_cmd();
        }
        let (w, h) = (self.w.min(self.cap_w), self.h.min(self.cap_h));
        if w == 0 || h == 0 {
            return None;
        }
        let mut rgba = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            rgba.extend_from_slice(&self.data[y * self.cap_w * 4..][..w * 4]);
        }
        Some(Image { w: w as u32, h: h as u32, rgba })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sixel_decodes_bands_and_repeats() {
        let mut s = Sixel::new();
        // red, 3 columns of a full 6-pixel column, then next band one pixel
        for &b in b"\"1;1;3;7#1;2;100;0;0#1!3~-@" {
            s.put(b);
        }
        let img = s.finish().unwrap();
        assert_eq!((img.w, img.h), (3, 7));
        assert_eq!(&img.rgba[..4], &[255, 0, 0, 255]);
        assert_eq!(&img.rgba[(6 * 3) * 4..][..4], &[255, 0, 0, 255]);
        assert_eq!(img.rgba[(6 * 3 + 1) * 4 + 3], 0);
    }

    #[test]
    fn kitty_parse_and_raw_rgb() {
        let cmd = KittyCmd::parse("a=T,f=24,s=1,v=1,i=7,q=2");
        assert_eq!((cmd.action, cmd.format, cmd.id, cmd.quiet), (b'T', 24, 7, 2));
        let img = kitty_decode(&cmd, b"AQID").unwrap();
        assert_eq!(img.rgba, vec![1, 2, 3, 255]);
    }
}
