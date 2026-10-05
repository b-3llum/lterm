//! The lterm logo, drawn procedurally so the window icon, the Windows .exe icon
//! (via build.rs) and assets/logo.png all come from one definition.
//! Design: a dark rounded tile split into three panes; the main pane holds a
//! prompt chevron and a cursor. Small sizes drop the panes and keep the prompt.

type Rgb = [f32; 3];

fn hex(c: u32) -> Rgb {
    [((c >> 16) & 255) as f32 / 255.0, ((c >> 8) & 255) as f32 / 255.0, (c & 255) as f32 / 255.0]
}

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

/// Inside a rounded rectangle (x0, y0)-(x1, y1) with corner radius r?
fn rrect(x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> bool {
    let dx = (x0 + r - x).max(x - (x1 - r)).max(0.0);
    let dy = (y0 + r - y).max(y - (y1 - r)).max(0.0);
    x >= x0 && x <= x1 && y >= y0 && y <= y1 && dx * dx + dy * dy <= r * r
}

/// Within distance `w/2` of the segment (ax, ay)-(bx, by) (round caps)?
fn stroke(x: f32, y: f32, a: (f32, f32), b: (f32, f32), w: f32) -> bool {
    let (vx, vy) = (b.0 - a.0, b.1 - a.1);
    let t = (((x - a.0) * vx + (y - a.1) * vy) / (vx * vx + vy * vy)).clamp(0.0, 1.0);
    let (dx, dy) = (x - (a.0 + t * vx), y - (a.1 + t * vy));
    dx * dx + dy * dy <= w * w / 4.0
}

/// Color at normalized coordinates (0..1); None is transparent.
fn sample(x: f32, y: f32, detailed: bool) -> Option<Rgb> {
    if !rrect(x, y, 0.03, 0.03, 0.97, 0.97, 0.21) {
        return None;
    }
    let chevron = mix(hex(0x7aa2f7), hex(0xbb9af7), (y - 0.3) / 0.4);
    let cursor = hex(0xf7768e);
    if !detailed {
        if stroke(x, y, (0.24, 0.28), (0.48, 0.5), 0.11) || stroke(x, y, (0.48, 0.5), (0.24, 0.72), 0.11) {
            return Some(chevron);
        }
        if rrect(x, y, 0.54, 0.62, 0.80, 0.73, 0.02) {
            return Some(cursor);
        }
        return Some(mix(hex(0x24283b), hex(0x16161e), y));
    }
    let pane = hex(0x24283b);
    let panes = [(0.11, 0.12, 0.61, 0.88), (0.65, 0.12, 0.89, 0.48), (0.65, 0.52, 0.89, 0.88)];
    if !panes.iter().any(|&(x0, y0, x1, y1)| rrect(x, y, x0, y0, x1, y1, 0.06)) {
        return Some(mix(hex(0x1f2335), hex(0x111219), y));
    }
    if stroke(x, y, (0.22, 0.34), (0.38, 0.5), 0.07) || stroke(x, y, (0.38, 0.5), (0.22, 0.66), 0.07) {
        return Some(chevron);
    }
    if rrect(x, y, 0.42, 0.6, 0.54, 0.665, 0.012) {
        return Some(cursor);
    }
    let lines = [
        ((0.71, 0.24), (0.83, 0.24), 0x9ece6a),
        ((0.71, 0.34), (0.78, 0.34), 0xe0af68),
        ((0.71, 0.64), (0.83, 0.64), 0x7dcfff),
        ((0.71, 0.74), (0.76, 0.74), 0xbb9af7),
    ];
    for (a, b, c) in lines {
        if stroke(x, y, a, b, 0.045) {
            return Some(hex(c));
        }
    }
    Some(pane)
}

/// Render the logo as straight RGBA at `size`x`size`, 4x4 supersampled.
pub fn render(size: u32) -> Vec<u8> {
    const SS: u32 = 4;
    let detailed = size >= 40;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let (mut acc, mut cover) = ([0.0f32; 3], 0.0f32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32;
                    if let Some(c) = sample(x, y, detailed) {
                        for i in 0..3 {
                            acc[i] += c[i];
                        }
                        cover += 1.0;
                    }
                }
            }
            if cover == 0.0 {
                out.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                let a = cover / (SS * SS) as f32;
                let ch = |v: f32| (v / cover * 255.0).round() as u8;
                out.extend_from_slice(&[ch(acc[0]), ch(acc[1]), ch(acc[2]), (a * 255.0).round() as u8]);
            }
        }
    }
    out
}
