//! The window: tabs of split panes, keyboard and mouse handling, and the connection
//! to a session server when attached to one.

use std::collections::{HashMap, HashSet};
use std::mem::take;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey, PhysicalKey};
use winit::window::{Icon, UserAttentionType, Window, WindowId};

use crate::copymode::{CopyMode, Outcome};
use crate::keymap::{self, Cmd, Leader};
use crate::layout::{self, Dir, Node, Rect, Tab};
use crate::pty::{self, Pty, PtyMsg};
use crate::render::{self, CopyView, Fonts, PaneView, Renderer, TabLabel, Ui};
use crate::session::{self, ConnEvent, Connection, Frame, Target};
use crate::term::{MouseMode, Selection, Term};
use crate::{fatal, input, logo, Options};

pub enum AppEvent {
    Pty(usize, PtyMsg),
    /// From session connection number `.0`; events of older connections are ignored.
    Conn(u64, ConnEvent),
}

struct Gui {
    window: Rc<Window>,
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
    _context: softbuffer::Context<Rc<Window>>,
    size: (u32, u32),
}

/// One terminal in the window.
struct Pane {
    term: Term,
    parser: vte::Parser,
    /// The local child process; None for panes that live in a session server.
    pty: Option<Pty>,
    remote: bool,
    selection: Option<Selection>,
    title: String,
    copy: Option<CopyMode>,
}

enum LeaderState {
    Idle,
    /// The leader was pressed; the next key is a command.
    Pending,
    /// A repeatable command just ran; more of them work without the leader until then.
    Repeat(Instant),
}

enum Backend {
    /// Shells are children of this window.
    Local,
    /// Shells live in a session server; this window shows them.
    Remote { target: Target, conn: Option<Connection>, attached: bool, generation: u64 },
}

pub struct App {
    opts: Options,
    proxy: EventLoopProxy<AppEvent>,
    gui: Option<Gui>,
    renderer: Option<Renderer>,
    panes: HashMap<usize, Pane>,
    tabs: Vec<Tab>,
    active: usize,
    next_id: usize,
    /// On-screen rectangles of the active tab's panes, its dividers, and the tab bar.
    rects: Vec<(usize, Rect)>,
    dividers: Vec<Rect>,
    bar: Option<Rect>,
    tab_labels: Vec<TabLabel>,
    backend: Backend,
    leader: Leader,
    leader_state: LeaderState,
    help: bool,
    /// A message box that any key closes.
    notice: Option<Vec<String>>,
    status: String,
    clipboard: Option<arboard::Clipboard>,
    mods: ModifiersState,
    mouse: PhysicalPosition<f64>,
    selecting: bool,
    pressed: Option<u8>,
    last_cell: (usize, usize),
    wheel: f64,
    focused: bool,
    font_size: f32,
    scale: f32,
}

impl App {
    pub fn new(opts: Options, proxy: EventLoopProxy<AppEvent>) -> App {
        let leader = Leader::parse(&opts.leader).unwrap_or_else(|e| fatal(&e));
        let backend = match &opts.attach {
            Some(t) => Backend::Remote { target: Target::parse(t), conn: None, attached: false, generation: 0 },
            None => Backend::Local,
        };
        App {
            font_size: opts.font_size,
            opts,
            proxy,
            gui: None,
            renderer: None,
            panes: HashMap::new(),
            tabs: Vec::new(),
            active: 0,
            next_id: 0,
            rects: Vec::new(),
            dividers: Vec::new(),
            bar: None,
            tab_labels: Vec::new(),
            backend,
            leader,
            leader_state: LeaderState::Idle,
            help: false,
            notice: None,
            status: String::new(),
            clipboard: None,
            mods: ModifiersState::empty(),
            mouse: PhysicalPosition::new(0.0, 0.0),
            selecting: false,
            pressed: None,
            last_cell: (usize::MAX, usize::MAX),
            wheel: 0.0,
            focused: true,
            scale: 1.0,
        }
    }
}

impl ApplicationHandler<AppEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.gui.is_some() {
            return;
        }
        let fonts = Fonts::load(self.opts.font.as_deref(), self.font_size).unwrap_or_else(|e| fatal(&e));
        let mut renderer = Renderer::new(fonts);
        let (w, h) = renderer.frame_size(self.opts.size.0, self.opts.size.1);
        let icon = Icon::from_rgba(logo::render(64), 64, 64).ok();
        let attrs = Window::default_attributes()
            .with_title("lterm")
            .with_window_icon(icon)
            .with_inner_size(LogicalSize::new(w as f64, h as f64));
        // App id / WM_CLASS, so desktops match the window to lterm.desktop and its icon.
        #[cfg(all(unix, not(target_os = "macos")))]
        let attrs = winit::platform::wayland::WindowAttributesExtWayland::with_name(attrs, "lterm", "lterm");
        let window = Rc::new(el.create_window(attrs).unwrap_or_else(|e| fatal(&e.to_string())));
        self.scale = window.scale_factor() as f32;
        renderer.set_scale(self.font_size, self.scale);
        let context = softbuffer::Context::new(window.clone()).unwrap_or_else(|e| fatal(&e.to_string()));
        let surface = softbuffer::Surface::new(&context, window.clone()).unwrap_or_else(|e| fatal(&e.to_string()));
        self.clipboard = arboard::Clipboard::new().ok();
        self.renderer = Some(renderer);
        self.gui = Some(Gui { window, surface, _context: context, size: (0, 0) });
        match self.backend {
            Backend::Local => self.new_tab(),
            Backend::Remote { .. } => self.connect(),
        }
        self.relayout();
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: AppEvent) {
        match event {
            AppEvent::Pty(id, PtyMsg::Data(data)) => self.output(id, &data),
            AppEvent::Pty(id, PtyMsg::Exit) => self.close_pane(id, el, false),
            AppEvent::Conn(generation, ev) => {
                if matches!(self.backend, Backend::Remote { generation: g, .. } if g == generation) {
                    self.on_conn(ev, el);
                }
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            // In a session, closing the window only detaches; the session keeps running.
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(_) => self.relayout(),
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale = scale_factor as f32;
                if let Some(r) = &mut self.renderer {
                    r.set_scale(self.font_size, self.scale);
                }
                self.relayout();
            }
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::Focused(focused) => {
                self.focused = focused;
                self.focus_event(self.focus(), focused);
                self.request_redraw();
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::KeyboardInput { event, .. } => self.key(event, el),
            WindowEvent::CursorMoved { position, .. } => self.mouse_moved(position),
            WindowEvent::MouseInput { state, button, .. } => self.mouse_button(state, button),
            WindowEvent::MouseWheel { delta, .. } => self.wheel(delta),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------- panes and tabs

impl App {
    fn request_redraw(&self) {
        if let Some(gui) = &self.gui {
            gui.window.request_redraw();
        }
    }

    fn gap(&self) -> usize {
        (2.0 * self.scale).round().max(1.0) as usize
    }

    fn remote(&self) -> bool {
        matches!(self.backend, Backend::Remote { .. })
    }

    /// The focused pane of the active tab (usize::MAX before there is one).
    fn focus(&self) -> usize {
        self.tabs.get(self.active).map_or(usize::MAX, |t| t.focus)
    }

    fn new_pane(&mut self, cols: usize, rows: usize) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        let renderer = self.renderer.as_ref().expect("renderer exists before panes");
        let cell = renderer.cell();
        let mut term = Term::new(cols, rows, self.opts.scrollback);
        term.cell_px = cell;
        term.default_colors = (render::rgb_bytes(renderer.theme.fg), render::rgb_bytes(renderer.theme.bg));
        let mut parser = vte::Parser::new();
        let pty = match &mut self.backend {
            Backend::Local => {
                let proxy = self.proxy.clone();
                match pty::spawn(&self.opts.command, cols, rows, cell, move |m| {
                    let _ = proxy.send_event(AppEvent::Pty(id, m));
                }) {
                    Ok(p) => Some(p),
                    Err(e) => {
                        let what = self.opts.command.first().map_or("shell", |s| s.as_str());
                        let msg = format!("\x1b[31mlterm: cannot start {what}: {e}\x1b[0m\r\n");
                        term.feed(&mut parser, msg.as_bytes());
                        None
                    }
                }
            }
            Backend::Remote { conn, .. } => {
                if let Some(c) = conn {
                    c.send(&Frame::Spawn {
                        id: id as u32,
                        cols: cols as u16,
                        rows: rows as u16,
                        cw: cell.0 as u16,
                        ch: cell.1 as u16,
                        command: self.opts.command.clone(),
                    });
                }
                None
            }
        };
        let remote = self.remote();
        let pane = Pane { term, parser, pty, remote, selection: None, title: String::new(), copy: None };
        self.panes.insert(id, pane);
        id
    }

    /// The area panes get: the window minus the tab bar.
    fn pane_area(&self) -> Rect {
        let Some(gui) = &self.gui else { return Rect::default() };
        let size = gui.window.inner_size();
        let bar = self.bar.map_or(0, |b| b.h);
        Rect { x: 0, y: bar, w: size.width as usize, h: (size.height as usize).saturating_sub(bar) }
    }

    fn new_tab(&mut self) {
        let Some(r) = &self.renderer else { return };
        let area = self.pane_area();
        let (cols, rows) = r.grid_size(area.w, area.h);
        let id = self.new_pane(cols, rows);
        self.focus_event(self.focus(), false);
        self.tabs.push(Tab { tree: Node::Leaf(id), focus: id, zoomed: false });
        self.active = self.tabs.len() - 1;
        self.relayout();
        self.update_title();
    }

    fn switch_tab(&mut self, i: usize) {
        if i >= self.tabs.len() || i == self.active {
            return;
        }
        self.focus_event(self.focus(), false);
        self.active = i;
        self.focus_event(self.focus(), self.focused);
        self.selecting = false;
        self.relayout();
        self.update_title();
    }

    fn close_tab(&mut self, i: usize, el: &ActiveEventLoop) {
        if let Some(tab) = self.tabs.get(i) {
            for id in tab.tree.leaves() {
                self.close_pane(id, el, true);
            }
        }
    }

    fn split(&mut self, dir: Dir) {
        let focus = self.focus();
        let rect = self.rects.iter().find(|(id, _)| *id == focus).map(|(_, r)| *r);
        let (Some(rect), Some(r)) = (rect, &self.renderer) else { return };
        let (_, half, _) = layout::split_rect(rect, dir, 0.5, self.gap());
        let (cols, rows) = r.grid_size(half.w, half.h);
        let id = self.new_pane(cols, rows);
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.tree.split(focus, dir, id);
            tab.zoomed = false;
        }
        self.set_focus(id);
        self.relayout();
    }

    /// Remove a pane. With `notify`, a session server is told to end it too (not needed
    /// when the server reported that it exited). The window closes with the last pane.
    fn close_pane(&mut self, id: usize, el: &ActiveEventLoop, notify: bool) {
        if self.panes.remove(&id).is_none() {
            return;
        }
        if let (true, Backend::Remote { conn: Some(c), .. }) = (notify, &mut self.backend) {
            c.send(&Frame::Kill { id: id as u32 });
        }
        let Some(ti) = self.tabs.iter().position(|t| t.tree.contains(id)) else { return };
        let tab = self.tabs.remove(ti);
        let was_focus = tab.focus == id;
        match tab.tree.remove(id) {
            Some(tree) => {
                let focus = if !was_focus {
                    tab.focus
                } else {
                    [(-1, 0), (1, 0), (0, -1), (0, 1)]
                        .iter()
                        .find_map(|&(dx, dy)| layout::neighbor(&self.rects, id, dx, dy))
                        .filter(|n| tree.contains(*n))
                        .unwrap_or_else(|| tree.first_leaf())
                };
                self.tabs.insert(ti, Tab { tree, focus, zoomed: tab.zoomed && !was_focus });
                if was_focus && ti == self.active {
                    self.focus_event(focus, self.focused);
                }
            }
            None => {
                if ti < self.active {
                    self.active -= 1;
                }
                self.active = self.active.min(self.tabs.len().saturating_sub(1));
            }
        }
        if self.tabs.is_empty() {
            el.exit();
            return;
        }
        self.selecting = false;
        self.relayout();
        self.update_title();
    }

    fn set_focus(&mut self, id: usize) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        if id == tab.focus || !tab.tree.contains(id) {
            return;
        }
        let old = tab.focus;
        self.focus_event(old, false);
        self.tabs[self.active].focus = id;
        self.focus_event(id, self.focused);
        self.selecting = false;
        self.pressed = None;
        if self.tabs[self.active].zoomed {
            self.relayout();
        }
        self.update_title();
        self.save_layout();
        self.request_redraw();
    }

    /// Tell a pane's program it gained or lost focus, if it asked to know (DECSET 1004).
    fn focus_event(&mut self, id: usize, gained: bool) {
        if self.panes.get(&id).is_some_and(|p| p.term.modes.focus_events) {
            self.write_pane(id, if gained { b"\x1b[I" } else { b"\x1b[O" });
        }
    }

    fn update_title(&self) {
        let Some(gui) = &self.gui else { return };
        let pane_title = self.panes.get(&self.focus()).map(|p| p.title.as_str()).filter(|t| !t.is_empty());
        let title = pane_title.unwrap_or("lterm");
        match &self.backend {
            Backend::Remote { target, .. } => {
                gui.window.set_title(&format!("{title} - {}:{}", target.label(), self.opts.session));
            }
            Backend::Local => gui.window.set_title(title),
        }
    }

    fn write_pane(&mut self, id: usize, bytes: &[u8]) {
        match &mut self.backend {
            Backend::Local => {
                if let Some(pty) = self.panes.get_mut(&id).and_then(|p| p.pty.as_mut()) {
                    pty.write(bytes);
                }
            }
            Backend::Remote { conn: Some(c), .. } => {
                c.send(&Frame::Input { id: id as u32, data: bytes.to_vec() });
            }
            Backend::Remote { .. } => {}
        }
    }

    /// Program output for a pane, from a local child or from the session server.
    fn output(&mut self, id: usize, data: &[u8]) {
        let Some(p) = self.panes.get_mut(&id) else { return };
        p.term.feed(&mut p.parser, data);
        let replies = take(&mut p.term.responses);
        // In a session the server answers terminal queries, so drop ours.
        if !p.remote {
            if let Some(pty) = &mut p.pty {
                pty.write(&replies);
            }
        }
        let new_title = p.term.title.take().map(|t| p.title = t).is_some();
        let bell = take(&mut p.term.bell);
        if new_title && id == self.focus() {
            self.update_title();
        }
        if let Some(gui) = &self.gui {
            if bell && !self.focused {
                gui.window.request_user_attention(Some(UserAttentionType::Informational));
            }
            gui.window.request_redraw();
        }
    }
}

// ---------------------------------------------------------------- layout and drawing

impl App {
    /// Recompute the tab bar and pane rectangles, and resize terminals to fit them.
    fn relayout(&mut self) {
        let gap = self.gap();
        let remote = self.remote();
        let (Some(gui), Some(r)) = (&self.gui, &self.renderer) else { return };
        let size = gui.window.inner_size();
        if size.width == 0 || size.height == 0 {
            return;
        }
        let (w, h) = (size.width as usize, size.height as usize);
        let show_bar = self.tabs.len() > 1 || remote;
        let bar_h = if show_bar { r.cell().1 + r.pad } else { 0 };
        self.bar = show_bar.then_some(Rect { x: 0, y: 0, w, h: bar_h });
        let area = Rect { x: 0, y: bar_h, w, h: h.saturating_sub(bar_h) };
        self.rects.clear();
        self.dividers.clear();
        if let Some(tab) = self.tabs.get(self.active) {
            if tab.zoomed {
                self.rects.push((tab.focus, area));
            } else {
                tab.tree.layout(area, gap, &mut self.rects, &mut self.dividers);
            }
        }
        let cell = r.cell();
        let mut resized = Vec::new();
        for &(id, rect) in &self.rects {
            let Some(p) = self.panes.get_mut(&id) else { continue };
            let (cols, rows) = r.grid_size(rect.w, rect.h);
            p.term.cell_px = cell;
            if (cols, rows) != (p.term.cols, p.term.rows) {
                p.term.resize(cols, rows);
                p.selection = None;
                if let Some(pty) = &p.pty {
                    pty.resize(cols, rows, cell);
                }
                resized.push((id, cols, rows));
            }
        }
        gui.window.request_redraw();
        if let Backend::Remote { conn: Some(c), .. } = &mut self.backend {
            let (cw, ch) = (cell.0 as u16, cell.1 as u16);
            for (id, cols, rows) in resized {
                c.send(&Frame::Resize { id: id as u32, cols: cols as u16, rows: rows as u16, cw, ch });
            }
        }
        self.save_layout();
    }

    /// Hand the layout to the session server, so the next window restores it.
    fn save_layout(&mut self) {
        if let Backend::Remote { conn: Some(c), attached: true, .. } = &mut self.backend {
            c.send(&Frame::Layout { data: layout::encode_tabs(&self.tabs, self.active) });
        }
    }

    /// The box shown over the panes, if any.
    fn popup(&self) -> Option<Vec<String>> {
        if self.help {
            return Some(keymap::help(self.leader.label()));
        }
        if let Some(n) = &self.notice {
            return Some(n.clone());
        }
        if self.remote() && self.tabs.is_empty() {
            let status = if self.status.is_empty() { "Connecting..." } else { self.status.as_str() };
            return Some(vec![status.to_string(), String::new(), "R  reconnect      Q  quit".into()]);
        }
        if matches!(self.leader_state, LeaderState::Pending) {
            return Some(vec![
                format!("{} ...", self.leader.label()),
                "v split  s split down  h j k l focus  t new tab  c copy  / search  ? all keys".into(),
            ]);
        }
        None
    }

    fn bar_text(&self) -> String {
        match &self.backend {
            Backend::Remote { target, attached: true, .. } => format!("session {} @ {}", self.opts.session, target.label()),
            Backend::Remote { .. } => self.status.clone(),
            Backend::Local => String::new(),
        }
    }

    fn redraw(&mut self) {
        let popup = self.popup();
        let right = self.bar_text();
        let (Some(gui), Some(r)) = (&mut self.gui, &mut self.renderer) else { return };
        let size = gui.window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else { return };
        if gui.size != (size.width, size.height) {
            if gui.surface.resize(w, h).is_err() {
                return;
            }
            gui.size = (size.width, size.height);
        }

        // Tab labels; kept for mouse clicks.
        self.tab_labels.clear();
        if let Some(bar) = self.bar {
            let mut x = 0;
            for (i, tab) in self.tabs.iter().enumerate() {
                let title = self.panes.get(&tab.focus).map(|p| p.title.as_str()).filter(|t| !t.is_empty());
                let title: String = title.unwrap_or("shell").chars().take(20).collect();
                let text = format!("{} {title}", i + 1);
                let tw = r.text_width(&text) + r.fonts.cell_w;
                self.tab_labels.push(TabLabel { rect: Rect { x, y: bar.y, w: tw, h: bar.h }, text, active: i == self.active });
                x += tw + 1;
            }
        }

        let focus = self.tabs.get(self.active).map_or(usize::MAX, |t| t.focus);
        let views: Vec<PaneView> = self
            .rects
            .iter()
            .filter_map(|&(id, rect)| {
                let p = self.panes.get(&id)?;
                let copy = p.copy.as_ref().map(|c| CopyView { cursor: c.cursor, search: &c.search, status: c.status() });
                let focused = self.focused && id == focus;
                Some(PaneView { term: &p.term, rect, selection: p.selection.as_ref(), focused, copy })
            })
            .collect();
        let outline = if self.rects.len() > 1 { self.rects.iter().find(|(id, _)| *id == focus).map(|(_, r)| *r) } else { None };
        let ui = Ui { bar: self.bar.map(|b| (b, &self.tab_labels[..], right.as_str())), popup: popup.as_deref() };
        let Ok(mut buf) = gui.surface.buffer_mut() else { return };
        r.draw(&mut buf[..], size.width as usize, size.height as usize, &views, &self.dividers, outline, &ui);
        let _ = buf.present();
    }

    fn zoom_font(&mut self, delta: f32) {
        self.font_size = if delta == 0.0 { self.opts.font_size } else { (self.font_size + delta).clamp(6.0, 72.0) };
        if let Some(r) = &mut self.renderer {
            r.set_scale(self.font_size, self.scale);
        }
        self.relayout();
    }
}

// ---------------------------------------------------------------- keyboard

const REPEAT_WINDOW: Duration = Duration::from_millis(900);

impl App {
    fn key(&mut self, ev: KeyEvent, el: &ActiveEventLoop) {
        if ev.state != ElementState::Pressed {
            return;
        }
        // A modifier pressed on its own (Shift before H) must not count as the key after
        // the leader, or end the repeat window.
        if let Key::Named(
            NamedKey::Shift
            | NamedKey::Control
            | NamedKey::Alt
            | NamedKey::AltGraph
            | NamedKey::Super
            | NamedKey::Meta
            | NamedKey::Hyper
            | NamedKey::CapsLock
            | NamedKey::NumLock,
        ) = ev.logical_key
        {
            return;
        }
        let m = self.mods;
        if self.help || self.notice.is_some() {
            self.help = false;
            self.notice = None;
            return self.request_redraw();
        }
        if self.tabs.is_empty() {
            // Not attached to a session (yet): offer reconnect or quit.
            match &ev.logical_key {
                Key::Character(c) if c.eq_ignore_ascii_case("r") => self.connect(),
                Key::Character(c) if c.eq_ignore_ascii_case("q") => el.exit(),
                Key::Named(NamedKey::Escape) => el.exit(),
                _ => {}
            }
            return self.request_redraw();
        }
        let code = match ev.physical_key {
            PhysicalKey::Code(c) => Some(c),
            _ => None,
        };
        let is_leader = code.is_some_and(|c| self.leader.matches(c, m));
        match self.leader_state {
            LeaderState::Pending => {
                self.leader_state = LeaderState::Idle;
                if is_leader {
                    // Pressed twice: the program gets the key itself.
                    self.send_key(&ev, m);
                } else if let Some(cmd) = keymap::leader_cmd(&ev.logical_key, m.shift_key()) {
                    self.run(cmd, el);
                    if cmd.repeatable() {
                        self.leader_state = LeaderState::Repeat(Instant::now() + REPEAT_WINDOW);
                    }
                }
                return self.request_redraw();
            }
            LeaderState::Repeat(until) => {
                self.leader_state = LeaderState::Idle;
                let cmd = keymap::leader_cmd(&ev.logical_key, m.shift_key()).filter(|c| c.repeatable());
                if let (Some(cmd), true) = (cmd, Instant::now() < until) {
                    self.run(cmd, el);
                    self.leader_state = LeaderState::Repeat(Instant::now() + REPEAT_WINDOW);
                    return;
                }
            }
            LeaderState::Idle => {}
        }
        if is_leader {
            self.leader_state = LeaderState::Pending;
            return self.request_redraw();
        }
        if matches!(ev.logical_key, Key::Named(NamedKey::F1)) && m.is_empty() {
            self.help = true;
            return self.request_redraw();
        }
        if let Some(cmd) = code.and_then(|c| keymap::direct(c, m)) {
            if self.run(cmd, el) {
                return;
            }
        }
        if self.copy_key(&ev, m) {
            return;
        }
        self.send_key(&ev, m);
    }

    /// Encode a key for the focused pane's program and send it.
    fn send_key(&mut self, ev: &KeyEvent, m: ModifiersState) {
        let focus = self.focus();
        let Some(p) = self.panes.get(&focus) else { return };
        if let Some(bytes) = input::encode(&ev.logical_key, ev.text.as_deref(), m, p.term.modes.app_cursor) {
            self.send(&bytes);
        }
    }

    /// User input for the focused pane; also jumps back to its live screen.
    fn send(&mut self, bytes: &[u8]) {
        let focus = self.focus();
        if let Some(p) = self.panes.get_mut(&focus) {
            p.term.display_offset = 0;
            p.selection = None;
        }
        self.write_pane(focus, bytes);
        self.request_redraw();
    }

    /// Run a command. Returns false if it didn't apply, so the key goes to the program
    /// instead (e.g. Alt+Left with no pane to the left).
    fn run(&mut self, cmd: Cmd, el: &ActiveEventLoop) -> bool {
        let focus = self.focus();
        let (multi, zoomed) = self.tabs.get(self.active).map_or((false, false), |t| (t.tree.leaves().len() > 1, t.zoomed));
        match cmd {
            Cmd::SplitRight => self.split(Dir::Right),
            Cmd::SplitDown => self.split(Dir::Down),
            Cmd::ClosePane => self.close_pane(focus, el, true),
            Cmd::Zoom => {
                if !multi {
                    return false;
                }
                self.tabs[self.active].zoomed = !zoomed;
                self.relayout();
            }
            Cmd::Focus(dx, dy) => match layout::neighbor(&self.rects, focus, dx, dy) {
                Some(next) => self.set_focus(next),
                None => return false,
            },
            Cmd::NextPane => {
                let ids: Vec<usize> = self.rects.iter().map(|(id, _)| *id).collect();
                let Some(i) = ids.iter().position(|&id| id == focus) else { return false };
                self.set_focus(ids[(i + 1) % ids.len()]);
            }
            Cmd::Resize(dx, dy) => {
                if !multi || zoomed {
                    return false;
                }
                let axis = if dx != 0 { Dir::Right } else { Dir::Down };
                self.tabs[self.active].tree.resize(focus, axis, 0.05 * (dx + dy) as f32);
                self.relayout();
            }
            Cmd::NewTab => self.new_tab(),
            Cmd::CloseTab => self.close_tab(self.active, el),
            Cmd::NextTab | Cmd::PrevTab => {
                let n = self.tabs.len();
                if n < 2 {
                    return false;
                }
                let step = if cmd == Cmd::NextTab { 1 } else { n - 1 };
                self.switch_tab((self.active + step) % n);
            }
            Cmd::GoTab(i) => {
                if i >= self.tabs.len() {
                    return false;
                }
                self.switch_tab(i);
            }
            Cmd::CopyMode | Cmd::Search => {
                if let Some(p) = self.panes.get_mut(&focus) {
                    let cm = p.copy.get_or_insert_with(|| CopyMode::new(&p.term));
                    if cmd == Cmd::Search {
                        cm.start_search(false);
                    }
                }
            }
            Cmd::Detach => {
                if self.remote() {
                    el.exit(); // dropping the connection detaches
                } else {
                    self.notice = Some(vec![
                        "This window isn't attached to a session.".into(),
                        String::new(),
                        "Start one with:  lterm --attach wsl      (Windows)".into(),
                        "                 lterm --attach local    (Linux)".into(),
                        "                 lterm --attach user@server".into(),
                    ]);
                }
            }
            Cmd::Reconnect => {
                if !self.remote() {
                    return false;
                }
                self.connect();
            }
            Cmd::Help => self.help = true,
            Cmd::Copy => self.copy(),
            Cmd::Paste => self.paste(),
            Cmd::FontBigger => self.zoom_font(1.0),
            Cmd::FontSmaller => self.zoom_font(-1.0),
            Cmd::FontReset => self.zoom_font(0.0),
            Cmd::ScrollPage(dir) => {
                let Some(p) = self.panes.get_mut(&focus) else { return false };
                if p.term.alt {
                    return false;
                }
                let page = p.term.rows.saturating_sub(1).max(1) as isize;
                p.term.scroll_view(page * dir as isize);
            }
        }
        self.request_redraw();
        true
    }

    /// Keys for a pane in copy mode. Returns false if the focused pane isn't in it.
    fn copy_key(&mut self, ev: &KeyEvent, m: ModifiersState) -> bool {
        let focus = self.focus();
        let Some(p) = self.panes.get_mut(&focus) else { return false };
        let Some(cm) = p.copy.as_mut() else { return false };
        match cm.key(&mut p.term, &ev.logical_key, ev.text.as_deref(), m.control_key()) {
            Outcome::Stay => p.selection = cm.selection(),
            Outcome::Exit => {
                p.copy = None;
                p.selection = None;
                p.term.display_offset = 0;
            }
            Outcome::Copy(text) => {
                p.copy = None;
                p.selection = None;
                p.term.display_offset = 0;
                if let Some(cb) = &mut self.clipboard {
                    let _ = cb.set_text(text);
                }
            }
        }
        self.request_redraw();
        true
    }

    fn copy(&mut self) {
        let Some(p) = self.panes.get(&self.focus()) else { return };
        let Some(sel) = &p.selection else { return };
        let text = p.term.selection_text(sel);
        if let Some(cb) = &mut self.clipboard {
            let _ = cb.set_text(text);
        }
    }

    fn paste(&mut self) {
        let Some(text) = self.clipboard.as_mut().and_then(|c| c.get_text().ok()) else { return };
        let bracketed = self.panes.get(&self.focus()).is_some_and(|p| p.term.modes.bracketed_paste);
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        let data = if bracketed {
            // Strip ESC so pasted text can't end bracketed paste early and inject keys.
            let clean: String = text.chars().filter(|&c| c != '\x1b').collect();
            format!("\x1b[200~{clean}\x1b[201~")
        } else {
            text
        };
        self.send(data.as_bytes());
    }
}

// ---------------------------------------------------------------- mouse

impl App {
    fn rect_of(&self, id: usize) -> Option<Rect> {
        self.rects.iter().find(|(i, _)| *i == id).map(|(_, r)| *r)
    }

    fn pane_at(&self, pos: PhysicalPosition<f64>) -> Option<usize> {
        self.rects.iter().find(|(_, r)| r.contains(pos.x, pos.y)).map(|(id, _)| *id)
    }

    /// Cell under `pos` in pane `id`, clamped to the pane.
    fn cell_at(&self, id: usize, pos: PhysicalPosition<f64>) -> (usize, usize) {
        let (Some(r), Some(rect), Some(p)) = (&self.renderer, self.rect_of(id), self.panes.get(&id)) else {
            return (0, 0);
        };
        let (cw, ch) = r.cell();
        let x = ((pos.x - (rect.x + r.pad) as f64).max(0.0) / cw as f64) as usize;
        let y = ((pos.y - (rect.y + r.pad) as f64).max(0.0) / ch as f64) as usize;
        (x.min(p.term.cols - 1), y.min(p.term.rows - 1))
    }

    /// Whether the program in pane `id` gets mouse events (Shift overrides, for selecting).
    fn reporting(&self, id: usize) -> bool {
        self.panes.get(&id).is_some_and(|p| p.term.modes.mouse != MouseMode::Off) && !self.mods.shift_key()
    }

    fn report(&mut self, id: usize, button: u8, press: bool, motion: bool, (col, row): (usize, usize)) {
        let Some(p) = self.panes.get(&id) else { return };
        let mut b = button;
        if self.mods.alt_key() {
            b += 8;
        }
        if self.mods.control_key() {
            b += 16;
        }
        if motion {
            b += 32;
        }
        let seq = if p.term.modes.mouse_sgr {
            format!("\x1b[<{b};{};{}{}", col + 1, row + 1, if press { 'M' } else { 'm' }).into_bytes()
        } else {
            if col > 222 || row > 222 {
                return;
            }
            let b = if press { b } else { 3 | (b & !3) };
            vec![0x1b, b'[', b'M', 32 + b, 33 + col as u8, 33 + row as u8]
        };
        self.write_pane(id, &seq);
    }

    fn mouse_button(&mut self, state: ElementState, button: MouseButton) {
        let pressed = state == ElementState::Pressed;
        // Tab bar: click to switch tabs.
        if pressed && self.bar.is_some_and(|b| b.contains(self.mouse.x, self.mouse.y)) {
            let hit = self.tab_labels.iter().position(|t| t.rect.contains(self.mouse.x, self.mouse.y));
            if let Some(i) = hit {
                self.switch_tab(i);
            }
            return;
        }
        let code = match button {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            _ => return,
        };
        if pressed {
            match self.pane_at(self.mouse) {
                Some(id) => self.set_focus(id),
                None => return, // a divider
            }
        }
        let id = self.focus();
        let cell = self.cell_at(id, self.mouse);
        if self.reporting(id) {
            self.report(id, code, pressed, false, cell);
            self.pressed = pressed.then_some(code);
            return;
        }
        let Some(p) = self.panes.get_mut(&id) else { return };
        match (button, pressed) {
            (MouseButton::Left, true) => {
                let at = (p.term.view_top() + cell.1 as i64, cell.0);
                p.selection = Some(Selection { anchor: at, head: at });
                self.selecting = true;
            }
            (MouseButton::Left, false) => {
                self.selecting = false;
                if p.selection.is_some_and(|s| s.anchor == s.head) {
                    p.selection = None;
                }
            }
            (MouseButton::Right, true) => {
                if p.selection.is_some() {
                    self.copy();
                    if let Some(p) = self.panes.get_mut(&id) {
                        p.selection = None;
                    }
                } else {
                    self.paste();
                }
            }
            (MouseButton::Middle, true) => self.paste(),
            _ => {}
        }
        self.request_redraw();
    }

    fn mouse_moved(&mut self, pos: PhysicalPosition<f64>) {
        self.mouse = pos;
        let id = self.focus();
        if self.selecting {
            let cell = self.cell_at(id, pos);
            if let Some(p) = self.panes.get_mut(&id) {
                let head = (p.term.view_top() + cell.1 as i64, cell.0);
                if let Some(s) = &mut p.selection {
                    s.head = head;
                }
            }
            return self.request_redraw();
        }
        let over_focus = self.pane_at(pos) == Some(id);
        if !self.reporting(id) || (!over_focus && self.pressed.is_none()) {
            return;
        }
        let cell = self.cell_at(id, pos);
        if cell == self.last_cell {
            return;
        }
        self.last_cell = cell;
        let mode = self.panes.get(&id).map(|p| p.term.modes.mouse);
        match (mode, self.pressed) {
            (Some(MouseMode::Drag | MouseMode::Motion), Some(b)) => self.report(id, b, true, true, cell),
            (Some(MouseMode::Motion), None) => self.report(id, 3, true, true, cell),
            _ => {}
        }
    }

    fn wheel(&mut self, delta: MouseScrollDelta) {
        let Some(id) = self.pane_at(self.mouse) else { return };
        let ch = self.renderer.as_ref().map_or(16.0, |r| r.cell().1 as f64);
        self.wheel += match delta {
            MouseScrollDelta::LineDelta(_, y) => y as f64 * 3.0,
            MouseScrollDelta::PixelDelta(p) => p.y / ch,
        };
        let lines = self.wheel.trunc() as isize;
        self.wheel -= lines as f64;
        if lines == 0 {
            return;
        }
        if self.reporting(id) {
            let cell = self.cell_at(id, self.mouse);
            for _ in 0..lines.unsigned_abs().min(10) {
                self.report(id, if lines > 0 { 64 } else { 65 }, true, false, cell);
            }
            return;
        }
        let Some(p) = self.panes.get_mut(&id) else { return };
        if p.term.alt {
            // Full-screen programs without mouse support (less, man) get arrow keys.
            let seq: &[u8] = match (lines > 0, p.term.modes.app_cursor) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            let data = seq.repeat(lines.unsigned_abs());
            self.write_pane(id, &data);
        } else {
            p.term.scroll_view(lines);
            self.request_redraw();
        }
    }
}

// ---------------------------------------------------------------- sessions

impl App {
    /// (Re)connect to the session server.
    fn connect(&mut self) {
        let Backend::Remote { target, conn, attached, generation } = &mut self.backend else { return };
        *conn = None; // detaches the old connection, if any
        *attached = false;
        *generation += 1;
        let (gen, proxy) = (*generation, self.proxy.clone());
        let sink = move |e| {
            let _ = proxy.send_event(AppEvent::Conn(gen, e));
        };
        match Connection::open(target, &self.opts.session, self.opts.remote_cmd.as_deref(), sink) {
            Ok(c) => {
                *conn = Some(c);
                self.status = format!("Connecting to {}...", target.label());
            }
            Err(e) => self.status = e,
        }
        self.request_redraw();
    }

    fn on_conn(&mut self, ev: ConnEvent, el: &ActiveEventLoop) {
        match ev {
            ConnEvent::Frame(Frame::Welcome { version, layout, panes }) => self.attach(version, &layout, panes),
            ConnEvent::Frame(Frame::Output { id, data }) => self.output(id as usize, &data),
            ConnEvent::Frame(Frame::Exited { id }) => self.close_pane(id as usize, el, false),
            ConnEvent::Frame(Frame::Kicked) => self.disconnected("Detached: the session was opened in another window."),
            ConnEvent::Frame(_) => {}
            ConnEvent::Closed(msg) => self.disconnected(&msg),
        }
    }

    fn disconnected(&mut self, why: &str) {
        if let Backend::Remote { conn, attached, .. } = &mut self.backend {
            *conn = None;
            *attached = false;
        }
        self.status = format!("Disconnected: {why}");
        // The proxy couldn't run: lterm isn't installed on the server.
        if why.contains("not found") || why.contains("No such file") || why.contains("session-proxy") {
            if let Backend::Remote { target: Target::Ssh(host), .. } = &self.backend {
                self.status = format!("lterm isn't installed on {host}. Install it with:  lterm push {host}");
            }
        }
        if !self.tabs.is_empty() {
            let leader = self.leader.label().to_string();
            self.notice = Some(vec![
                self.status.clone(),
                String::new(),
                "Everything keeps running in the session.".into(),
                format!("Reconnect with {leader} then r."),
            ]);
        }
        self.request_redraw();
    }

    /// Build the window from the session server's panes and saved layout.
    fn attach(&mut self, version: u32, layout_text: &str, panes: Vec<session::PaneInfo>) {
        if version != session::VERSION {
            let msg = format!("Version mismatch: this lterm speaks v{}, the server v{version}. Update both.", session::VERSION);
            return self.disconnected(&msg);
        }
        let Some(r) = &self.renderer else { return };
        let cell = r.cell();
        let colors = (render::rgb_bytes(r.theme.fg), render::rgb_bytes(r.theme.bg));
        self.panes.clear();
        self.tabs.clear();
        for info in &panes {
            let mut term = Term::new(info.cols as usize, info.rows as usize, self.opts.scrollback);
            term.cell_px = cell;
            term.default_colors = colors;
            let pane = Pane {
                term,
                parser: vte::Parser::new(),
                pty: None,
                remote: true,
                selection: None,
                title: String::new(),
                copy: None,
            };
            self.panes.insert(info.id as usize, pane);
        }
        self.next_id = self.next_id.max(panes.iter().map(|p| p.id as usize + 1).max().unwrap_or(0));

        // Restore the saved tabs, dropping panes that exited while detached.
        let live: HashSet<usize> = self.panes.keys().copied().collect();
        let (saved, active) = layout::decode_tabs(layout_text);
        let mut tabs: Vec<Tab> = saved
            .into_iter()
            .filter_map(|t| {
                let tree = t.tree.retain(&|id| live.contains(&id))?;
                let focus = if tree.contains(t.focus) { t.focus } else { tree.first_leaf() };
                Some(Tab { tree, focus, zoomed: t.zoomed })
            })
            .collect();
        let mut orphans: Vec<usize> = live.iter().copied().filter(|id| !tabs.iter().any(|t| t.tree.contains(*id))).collect();
        orphans.sort();
        tabs.extend(orphans.into_iter().map(|id| Tab { tree: Node::Leaf(id), focus: id, zoomed: false }));
        self.active = active.min(tabs.len().saturating_sub(1));
        self.tabs = tabs;
        if let Backend::Remote { attached, .. } = &mut self.backend {
            *attached = true;
        }
        self.status.clear();
        self.notice = None;
        if self.tabs.is_empty() {
            self.new_tab(); // a brand-new session
        }
        self.relayout();
        self.update_title();
    }
}
