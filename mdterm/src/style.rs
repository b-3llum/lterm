#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Color {
    /// 256-color palette index (0-15 map to the basic ANSI colors).
    Idx(u8),
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Style {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub fg: Option<Color>,
    pub bg: Option<Color>,
}

impl Style {
    pub fn color(c: Color) -> Style {
        Style { fg: Some(c), ..Default::default() }
    }

    fn sgr(&self) -> String {
        let mut codes: Vec<String> = Vec::new();
        for (on, code) in [
            (self.bold, "1"),
            (self.dim, "2"),
            (self.italic, "3"),
            (self.underline, "4"),
            (self.strike, "9"),
        ] {
            if on {
                codes.push(code.into());
            }
        }
        if let Some(Color::Idx(n)) = self.fg {
            codes.push(match n {
                0..=7 => format!("{}", 30 + n),
                8..=15 => format!("{}", 90 + n - 8),
                _ => format!("38;5;{n}"),
            });
        }
        if let Some(Color::Idx(n)) = self.bg {
            codes.push(match n {
                0..=7 => format!("{}", 40 + n),
                8..=15 => format!("{}", 100 + n - 8),
                _ => format!("48;5;{n}"),
            });
        }
        format!("\x1b[{}m", codes.join(";"))
    }
}

/// Append `text` to `out` with `style`. Control characters from the document
/// are replaced so untrusted Markdown cannot inject terminal escape sequences.
pub fn paint(out: &mut String, text: &str, style: Style, color: bool) {
    let styled = color && style != Style::default();
    if styled {
        out.push_str(&style.sgr());
    }
    for ch in text.chars() {
        if ch.is_control() && ch != '\n' && ch != '\t' {
            out.push('\u{FFFD}');
        } else {
            out.push(ch);
        }
    }
    if styled {
        out.push_str("\x1b[0m");
    }
}
