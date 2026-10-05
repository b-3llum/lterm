//! Key bindings: a leader key followed by one mnemonic key, plus single-chord
//! shortcuts for the most common actions.

use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cmd {
    SplitRight,
    SplitDown,
    ClosePane,
    Zoom,
    Focus(i32, i32),
    NextPane,
    Resize(i32, i32),
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    GoTab(usize),
    CopyMode,
    Search,
    Detach,
    Reconnect,
    Help,
    Copy,
    Paste,
    FontBigger,
    FontSmaller,
    FontReset,
    ScrollPage(i32),
}

impl Cmd {
    /// Commands that can be repeated after one leader press (`l l l`, `L L L`, `n n`).
    pub fn repeatable(self) -> bool {
        matches!(self, Cmd::Focus(..) | Cmd::Resize(..) | Cmd::NextPane | Cmd::NextTab | Cmd::PrevTab)
    }
}

pub struct Leader {
    ctrl: bool,
    alt: bool,
    shift: bool,
    key: KeyCode,
    label: String,
}

impl Leader {
    /// Parse e.g. `ctrl+space`, `ctrl+a`, `alt+space`, `ctrl+shift+b`, `f12`.
    pub fn parse(spec: &str) -> Result<Leader, String> {
        let mut l = Leader { ctrl: false, alt: false, shift: false, key: KeyCode::Space, label: String::new() };
        let mut key = None;
        let mut label = Vec::new();
        for part in spec.split('+').map(|p| p.trim().to_ascii_lowercase()) {
            match part.as_str() {
                "ctrl" | "control" => l.ctrl = true,
                "alt" | "option" => l.alt = true,
                "shift" => l.shift = true,
                k => key = Some(key_code(k).ok_or(format!("unknown key '{k}' in leader '{spec}'"))?),
            }
            let mut c = part.chars();
            label.push(c.next().map(|f| f.to_uppercase().chain(c).collect::<String>()).unwrap_or_default());
        }
        l.key = key.ok_or(format!("leader '{spec}' has no key"))?;
        l.label = label.join("+");
        Ok(l)
    }

    pub fn matches(&self, code: KeyCode, m: ModifiersState) -> bool {
        code == self.key && m.control_key() == self.ctrl && m.alt_key() == self.alt && m.shift_key() == self.shift
    }

    pub fn label(&self) -> &str {
        &self.label
    }
}

fn key_code(k: &str) -> Option<KeyCode> {
    use KeyCode::*;
    let letters = [
        KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN, KeyO, KeyP, KeyQ, KeyR,
        KeyS, KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ,
    ];
    let fkeys = [F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12];
    Some(match k {
        "space" => Space,
        "backquote" | "grave" | "`" => Backquote,
        "backslash" | "\\" => Backslash,
        "enter" | "return" => Enter,
        "tab" => Tab,
        _ if k.len() == 1 && k.as_bytes()[0].is_ascii_lowercase() => letters[(k.as_bytes()[0] - b'a') as usize],
        _ if k.starts_with('f') => *fkeys.get(k[1..].parse::<usize>().ok()?.checked_sub(1)?)?,
        _ => return None,
    })
}

/// The command for the key pressed after the leader.
pub fn leader_cmd(key: &Key, shift: bool) -> Option<Cmd> {
    let c = match key {
        Key::Named(n) => {
            let c = match n {
                NamedKey::ArrowLeft => 'h',
                NamedKey::ArrowDown => 'j',
                NamedKey::ArrowUp => 'k',
                NamedKey::ArrowRight => 'l',
                NamedKey::Tab => return Some(if shift { Cmd::PrevTab } else { Cmd::NextTab }),
                _ => return None,
            };
            if shift { c.to_ascii_uppercase() } else { c }
        }
        Key::Character(s) => s.chars().next()?,
        _ => return None,
    };
    Some(match c {
        'v' | '|' | '\\' | '%' => Cmd::SplitRight,
        's' | '-' | '"' => Cmd::SplitDown,
        'h' => Cmd::Focus(-1, 0),
        'j' => Cmd::Focus(0, 1),
        'k' => Cmd::Focus(0, -1),
        'l' => Cmd::Focus(1, 0),
        'H' => Cmd::Resize(-1, 0),
        'J' => Cmd::Resize(0, 1),
        'K' => Cmd::Resize(0, -1),
        'L' => Cmd::Resize(1, 0),
        'o' => Cmd::NextPane,
        'z' => Cmd::Zoom,
        'x' => Cmd::ClosePane,
        't' => Cmd::NewTab,
        'w' => Cmd::CloseTab,
        'n' => Cmd::NextTab,
        'p' => Cmd::PrevTab,
        '1'..='9' => Cmd::GoTab(c as usize - '1' as usize),
        'c' | '[' => Cmd::CopyMode,
        '/' => Cmd::Search,
        'd' => Cmd::Detach,
        'r' => Cmd::Reconnect,
        '?' => Cmd::Help,
        _ => return None,
    })
}

/// Shortcuts that work without the leader.
pub fn direct(code: KeyCode, m: ModifiersState) -> Option<Cmd> {
    use KeyCode::*;
    let (c, s, a) = (m.control_key(), m.shift_key(), m.alt_key());
    let digit = [Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9].iter().position(|&d| d == code);
    let arrow = match code {
        ArrowLeft => Some((-1, 0)),
        ArrowRight => Some((1, 0)),
        ArrowUp => Some((0, -1)),
        ArrowDown => Some((0, 1)),
        _ => None,
    };
    if let (Some((dx, dy)), true, false) = (arrow, a, c) {
        return Some(if s { Cmd::Resize(dx, dy) } else { Cmd::Focus(dx, dy) });
    }
    if let (Some(n), true, false, false) = (digit, a, c, s) {
        return Some(Cmd::GoTab(n));
    }
    Some(match code {
        KeyE if c && s => Cmd::SplitRight,
        KeyO if c && s => Cmd::SplitDown,
        Equal if a && s && !c => Cmd::SplitRight,
        Minus if a && s && !c => Cmd::SplitDown,
        KeyW if c && s => Cmd::ClosePane,
        KeyZ if c && s => Cmd::Zoom,
        KeyT if c && s => Cmd::NewTab,
        KeyX if c && s => Cmd::CopyMode,
        KeyF if c && s => Cmd::Search,
        Tab if c => if s { Cmd::PrevTab } else { Cmd::NextTab },
        PageUp if c && !s => Cmd::PrevTab,
        PageDown if c && !s => Cmd::NextTab,
        KeyC if c && s => Cmd::Copy,
        KeyV if c && s => Cmd::Paste,
        Insert if s && !c => Cmd::Paste,
        Insert if c && !s => Cmd::Copy,
        Equal | NumpadAdd if c && s => Cmd::FontBigger,
        Minus | NumpadSubtract if c && s => Cmd::FontSmaller,
        Digit0 | Numpad0 if c && s => Cmd::FontReset,
        PageUp if s && !c => Cmd::ScrollPage(1),
        PageDown if s && !c => Cmd::ScrollPage(-1),
        _ => return None,
    })
}

/// Lines for the `?` cheat sheet.
pub fn help(leader: &str) -> Vec<String> {
    [
        format!("Press {leader}, then:"),
        "  v  split right          s  split down          x  close pane       z  zoom".into(),
        "  h j k l / arrows  focus (repeat: l l l)    H J K L  resize (repeat)".into(),
        "  o  next pane            t  new tab             w  close tab".into(),
        "  n / p  next / prev tab  1-9  go to tab         Tab  next tab".into(),
        "  c  copy mode            /  search scrollback   ?  this help".into(),
        "  d  detach session       r  reconnect session".into(),
        format!("  {leader} twice sends it to the program;  Esc cancels"),
        String::new(),
        "Without the leader:".into(),
        "  Ctrl+Shift+E / O   split right / down       Ctrl+Shift+W  close pane".into(),
        "  Alt+Arrows         focus                    Alt+Shift+Arrows  resize".into(),
        "  Ctrl+Shift+T       new tab                  Ctrl+Tab / Ctrl+Shift+Tab  switch tab".into(),
        "  Alt+1..9           go to tab                Ctrl+Shift+Z  zoom".into(),
        "  Ctrl+Shift+X       copy mode                Ctrl+Shift+F  search".into(),
        "  Ctrl+Shift+C / V   copy / paste             Ctrl+Shift+= / - / 0  font size".into(),
        String::new(),
        "Copy mode:".into(),
        "  h j k l  w b  0 $  g G  Ctrl+U/D   move      v / V  select chars / lines".into(),
        "  y or Enter  copy and exit    / ?  search forward / back    n N  next / prev".into(),
        "  q or Esc  exit".into(),
        String::new(),
        "Press any key to close".into(),
    ]
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leader_parsing() {
        let l = Leader::parse("ctrl+space").unwrap();
        assert!(l.matches(KeyCode::Space, ModifiersState::CONTROL));
        assert!(!l.matches(KeyCode::Space, ModifiersState::CONTROL | ModifiersState::SHIFT));
        assert_eq!(l.label(), "Ctrl+Space");
        assert!(Leader::parse("ctrl+a").unwrap().matches(KeyCode::KeyA, ModifiersState::CONTROL));
        assert!(Leader::parse("f12").unwrap().matches(KeyCode::F12, ModifiersState::empty()));
        assert!(Leader::parse("ctrl+nope").is_err());
    }

    #[test]
    fn leader_keys() {
        let ch = |s: &str| Key::Character(s.into());
        assert_eq!(leader_cmd(&ch("v"), false), Some(Cmd::SplitRight));
        assert_eq!(leader_cmd(&ch("L"), true), Some(Cmd::Resize(1, 0)));
        assert_eq!(leader_cmd(&Key::Named(NamedKey::ArrowUp), true), Some(Cmd::Resize(0, -1)));
        assert_eq!(leader_cmd(&ch("3"), false), Some(Cmd::GoTab(2)));
        assert_eq!(leader_cmd(&ch("q"), false), None);
    }

    #[test]
    fn direct_keys() {
        let cs = ModifiersState::CONTROL | ModifiersState::SHIFT;
        assert_eq!(direct(KeyCode::KeyT, cs), Some(Cmd::NewTab));
        assert_eq!(direct(KeyCode::Equal, cs), Some(Cmd::FontBigger));
        assert_eq!(direct(KeyCode::Equal, ModifiersState::ALT | ModifiersState::SHIFT), Some(Cmd::SplitRight));
        assert_eq!(direct(KeyCode::Digit2, ModifiersState::ALT), Some(Cmd::GoTab(1)));
        assert_eq!(direct(KeyCode::KeyT, ModifiersState::CONTROL), None);
    }
}
