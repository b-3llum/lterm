//! Translate key presses into the byte sequences xterm-compatible programs expect.

use winit::keyboard::{Key, ModifiersState, NamedKey};

pub fn encode(key: &Key, text: Option<&str>, mods: ModifiersState, app_cursor: bool) -> Option<Vec<u8>> {
    let (shift, alt, ctrl) = (mods.shift_key(), mods.alt_key(), mods.control_key());
    // xterm modifier parameter: 1 + shift + 2*alt + 4*ctrl
    let m = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;
    let s = match key {
        Key::Named(named) => {
            let cursor = |c: char| {
                if m > 1 {
                    format!("\x1b[1;{m}{c}")
                } else if app_cursor {
                    format!("\x1bO{c}")
                } else {
                    format!("\x1b[{c}")
                }
            };
            let tilde = |n: u8| if m > 1 { format!("\x1b[{n};{m}~") } else { format!("\x1b[{n}~") };
            let ss3 = |c: char| if m > 1 { format!("\x1b[1;{m}{c}") } else { format!("\x1bO{c}") };
            match named {
                NamedKey::ArrowUp => cursor('A'),
                NamedKey::ArrowDown => cursor('B'),
                NamedKey::ArrowRight => cursor('C'),
                NamedKey::ArrowLeft => cursor('D'),
                NamedKey::Home => cursor('H'),
                NamedKey::End => cursor('F'),
                NamedKey::Insert => tilde(2),
                NamedKey::Delete => tilde(3),
                NamedKey::PageUp => tilde(5),
                NamedKey::PageDown => tilde(6),
                NamedKey::F1 => ss3('P'),
                NamedKey::F2 => ss3('Q'),
                NamedKey::F3 => ss3('R'),
                NamedKey::F4 => ss3('S'),
                NamedKey::F5 => tilde(15),
                NamedKey::F6 => tilde(17),
                NamedKey::F7 => tilde(18),
                NamedKey::F8 => tilde(19),
                NamedKey::F9 => tilde(20),
                NamedKey::F10 => tilde(21),
                NamedKey::F11 => tilde(23),
                NamedKey::F12 => tilde(24),
                NamedKey::Enter => if alt { "\x1b\r" } else { "\r" }.into(),
                NamedKey::Backspace => if ctrl { "\x08" } else if alt { "\x1b\x7f" } else { "\x7f" }.into(),
                NamedKey::Tab => if shift { "\x1b[Z" } else { "\t" }.into(),
                NamedKey::Escape => "\x1b".into(),
                NamedKey::Space => if ctrl { "\0" } else if alt { "\x1b " } else { " " }.into(),
                _ => return None,
            }
        }
        Key::Character(chars) => {
            let typed = text.unwrap_or(chars);
            // AltGr arrives as Ctrl+Alt; if it produced printable text, send that.
            let altgr = ctrl && alt && typed.chars().all(|c| !c.is_control());
            if ctrl && !altgr {
                let c = chars.chars().next()?.to_ascii_lowercase();
                let code = match c {
                    'a'..='z' => c as u8 & 0x1f,
                    '@' | '2' | ' ' => 0,
                    '[' | '3' => 0x1b,
                    '\\' | '4' => 0x1c,
                    ']' | '5' => 0x1d,
                    '^' | '6' => 0x1e,
                    '_' | '-' | '7' | '/' => 0x1f,
                    '8' | '?' => 0x7f,
                    _ => return Some(typed.as_bytes().to_vec()),
                };
                let mut v = if alt { vec![0x1b] } else { Vec::new() };
                v.push(code);
                return Some(v);
            }
            if alt && !altgr {
                format!("\x1b{typed}")
            } else {
                typed.to_string()
            }
        }
        _ => text?.to_string(),
    };
    (!s.is_empty()).then(|| s.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(s: &str) -> Key {
        Key::Character(s.into())
    }

    #[test]
    fn control_and_alt() {
        assert_eq!(encode(&k("c"), Some("\x03"), ModifiersState::CONTROL, false), Some(vec![3]));
        assert_eq!(encode(&k("b"), Some("b"), ModifiersState::ALT, false), Some(b"\x1bb".to_vec()));
        let altgr = ModifiersState::CONTROL | ModifiersState::ALT;
        assert_eq!(encode(&k("q"), Some("@"), altgr, false), Some(b"@".to_vec()));
    }

    #[test]
    fn cursor_keys() {
        let up = Key::Named(NamedKey::ArrowUp);
        assert_eq!(encode(&up, None, ModifiersState::empty(), false), Some(b"\x1b[A".to_vec()));
        assert_eq!(encode(&up, None, ModifiersState::empty(), true), Some(b"\x1bOA".to_vec()));
        assert_eq!(encode(&up, None, ModifiersState::CONTROL, true), Some(b"\x1b[1;5A".to_vec()));
    }
}
