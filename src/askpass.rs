//! SSH_ASKPASS helper. `lterm --attach user@host` runs ssh without a terminal, so ssh
//! can't prompt in one; instead it runs this program (with LTERM_ASKPASS set), which
//! shows the prompt in a small window and prints the answer for ssh. Nothing is stored.

use std::io::Write;
use std::num::NonZeroU32;
use std::process::ExitCode;
use std::rc::Rc;

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Icon, Window, WindowId, WindowLevel};

use crate::layout::Rect;
use crate::render::{Fonts, PaneView, Renderer, Ui};
use crate::term::Term;

const COLS: usize = 72;
const MAX_INPUT: usize = 1024;

struct Gui {
    window: Rc<Window>,
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
    _context: softbuffer::Context<Rc<Window>>,
}

struct Ask {
    prompt: String,
    /// Hide what's typed (passwords); yes/no host-key questions are shown.
    secret: bool,
    input: String,
    answered: bool,
    rows: usize,
    gui: Option<Gui>,
    renderer: Option<Renderer>,
    mods: ModifiersState,
}

/// Rows needed to show the prompt wrapped at COLS, plus the input and hint lines.
fn rows_for(prompt: &str) -> usize {
    let text: usize = prompt.lines().map(|l| l.chars().count().max(1).div_ceil(COLS)).sum();
    (text + 6).clamp(7, 30)
}

pub fn main(prompt: String) -> ExitCode {
    let prompt = prompt.trim().to_string();
    let lower = prompt.to_ascii_lowercase();
    let secret = !lower.contains("(yes/no");
    let mut ask = Ask {
        rows: rows_for(&prompt),
        prompt,
        secret,
        input: String::new(),
        answered: false,
        gui: None,
        renderer: None,
        mods: ModifiersState::empty(),
    };
    let Ok(event_loop) = EventLoop::new() else { return ExitCode::FAILURE };
    if event_loop.run_app(&mut ask).is_err() || !ask.answered {
        return ExitCode::FAILURE;
    }
    let mut out = std::io::stdout().lock();
    let ok = writeln!(out, "{}", ask.input).and_then(|_| out.flush()).is_ok();
    if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

impl Ask {
    /// The window's contents, drawn by the terminal renderer.
    fn screen(&self) -> Term {
        let mut t = Term::new(COLS, self.rows, 0);
        let mut text = String::from("\x1b[1;34m lterm \x1b[0;2m- SSH needs an answer\x1b[0m\r\n\r\n");
        for line in self.prompt.lines() {
            text.push_str(line);
            text.push_str("\r\n");
        }
        let shown: String = if self.secret { "\u{2022}".repeat(self.input.chars().count()) } else { self.input.clone() };
        text.push_str(&format!("\r\n\x1b[1m>\x1b[0m {shown}\x1b7"));
        text.push_str(&format!("\x1b[{};1H\x1b[2mEnter to send, Esc to cancel. Nothing is saved.\x1b[0m\x1b8", self.rows));
        t.feed(&mut vte::Parser::new(), text.as_bytes());
        t
    }

    fn finish(&mut self, el: &ActiveEventLoop, answered: bool) {
        self.answered = answered;
        el.exit();
    }
}

impl ApplicationHandler for Ask {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.gui.is_some() {
            return;
        }
        let Ok(fonts) = Fonts::load(None, 15.0) else { return el.exit() };
        let mut renderer = Renderer::new(fonts);
        let (w, h) = renderer.frame_size(COLS, self.rows);
        let attrs = Window::default_attributes()
            .with_title("lterm - SSH")
            .with_window_icon(Icon::from_rgba(crate::logo::render(64), 64, 64).ok())
            .with_inner_size(LogicalSize::new(w as f64, h as f64))
            .with_resizable(false)
            .with_window_level(WindowLevel::AlwaysOnTop);
        let Ok(window) = el.create_window(attrs) else { return el.exit() };
        let window = Rc::new(window);
        renderer.set_scale(15.0, window.scale_factor() as f32);
        let Ok(context) = softbuffer::Context::new(window.clone()) else { return el.exit() };
        let Ok(surface) = softbuffer::Surface::new(&context, window.clone()) else { return el.exit() };
        window.focus_window();
        window.request_redraw();
        self.renderer = Some(renderer);
        self.gui = Some(Gui { window, surface, _context: context });
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => self.finish(el, false),
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let ctrl = self.mods.control_key();
                match &event.logical_key {
                    Key::Named(NamedKey::Enter) => return self.finish(el, true),
                    Key::Named(NamedKey::Escape) => return self.finish(el, false),
                    Key::Named(NamedKey::Backspace) => {
                        self.input.pop();
                    }
                    Key::Character(c) if ctrl && c.eq_ignore_ascii_case("v") => {
                        let pasted = arboard::Clipboard::new().ok().and_then(|mut cb| cb.get_text().ok());
                        let clean: String = pasted.unwrap_or_default().chars().filter(|c| !c.is_control()).collect();
                        self.input.push_str(&clean);
                    }
                    Key::Character(c) if ctrl && c.eq_ignore_ascii_case("u") => self.input.clear(),
                    _ if !ctrl => {
                        if let Some(text) = &event.text {
                            self.input.extend(text.chars().filter(|c| !c.is_control()));
                        }
                    }
                    _ => {}
                }
                self.input = self.input.chars().take(MAX_INPUT).collect();
                if let Some(g) = &self.gui {
                    g.window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                let term = self.screen();
                let (Some(g), Some(r)) = (&mut self.gui, &mut self.renderer) else { return };
                let size = g.window.inner_size();
                let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else { return };
                if g.surface.resize(w, h).is_err() {
                    return;
                }
                let Ok(mut buf) = g.surface.buffer_mut() else { return };
                let (w, h) = (size.width as usize, size.height as usize);
                let view = PaneView { term: &term, rect: Rect { x: 0, y: 0, w, h }, selection: None, focused: true, copy: None };
                r.draw(&mut buf[..], w, h, &[view], &[], None, &Ui::default());
                let _ = buf.present();
            }
            _ => {}
        }
    }
}
