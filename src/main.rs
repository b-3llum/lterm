// A console-subsystem program on purpose, so `lterm md` and `lterm -h` can print to the
// shell they were run from. In window mode it detaches from that console (see `detach_console`).

mod app;
mod askpass;
mod copymode;
mod graphics;
mod input;
mod install;
mod keymap;
mod layout;
mod logo;
mod pty;
mod push;
mod render;
mod session;
mod term;

use std::collections::HashMap;
use std::mem::take;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use winit::event_loop::EventLoop;

use layout::{Dir, Node, Rect, Tab};
use pty::PtyMsg;
use render::{Fonts, PaneView, Renderer, Ui};
use session::{ConnEvent, Connection, Frame, Target};
use term::Term;

const USAGE: &str = "\
lterm - a lightweight terminal emulator

USAGE:
    lterm [OPTIONS] [-e COMMAND [ARGS]...]   Open a window running your shell (or COMMAND)
    lterm --attach TARGET [--session NAME]   Open a window attached to a persistent session
    lterm md [OPTIONS] [FILE]...             Render Markdown or images here (lterm md --help)
    lterm keys                               Print the keyboard shortcuts
    lterm push HOST                          Install lterm on a server over your SSH login
    lterm install                            Install for this user (PATH, app menu / Start menu)
    lterm uninstall                          Undo `lterm install`
    lterm ls                                 List sessions on this machine (Linux/macOS)
    lterm kill NAME                          End a session and everything running in it

    The shell is PowerShell on Windows and $SHELL elsewhere. New panes and tabs run the
    same COMMAND. A copy or link of lterm named `mdterm` behaves like `lterm md`.

SESSIONS (like tmux, but images work):
    TARGET is where the shells run and keep running when the window closes:
        wsl              inside WSL (Windows); wsl:DISTRO picks a distro
        local            this machine (Linux/macOS)
        user@host        a server over SSH (install lterm there first: lterm push user@host).
                         Passwords and host-key questions are asked in a small window.
    Close the window or press the leader then d to detach; run the same command to
    come back to every tab, split and scrollback.

OPTIONS:
    -e, --                Run COMMAND instead of the shell (all remaining arguments)
    -a, --attach <TARGET> Attach to a session (see SESSIONS)
    -s, --session <NAME>  Session name [default: main]
        --remote-cmd <C>  Command that starts lterm on the server [default: lterm via a login shell]
        --leader <KEYS>   Leader key, e.g. ctrl+space, ctrl+a, alt+space, f12
                          [default: ctrl+space, or $LTERM_LEADER]
        --font <FILE>     Monospace .ttf/.otf font
        --font-size <PX>  Font size in logical pixels [default: 15]
        --size <CxR>      Initial size in columns x rows [default: 100x30]
        --scrollback <N>  Lines of history per pane [default: 10000]
        --snapshot <PNG>  Headless: run COMMAND (or attach), then save the screen as a PNG
        --snapshot-panes <N>  Snapshot: run COMMAND in N split panes
        --wait <SECS>     Snapshot: how long to wait for output [default: 5]
        --logo <PNG>      Save the lterm logo as a PNG and exit (--logo-size <PX>)
    -h, --help            Print this help
    -V, --version         Print version
";

pub struct Options {
    pub command: Vec<String>,
    pub font: Option<PathBuf>,
    pub font_size: f32,
    pub size: (usize, usize),
    pub scrollback: usize,
    pub attach: Option<String>,
    pub session: String,
    pub remote_cmd: Option<String>,
    pub leader: String,
    snapshot: Option<PathBuf>,
    snapshot_panes: usize,
    wait: f32,
    logo: Option<PathBuf>,
    logo_size: u32,
}

fn parse_args(args: Vec<String>) -> Result<Options, String> {
    let mut o = Options {
        command: Vec::new(),
        font: None,
        font_size: 15.0,
        size: (100, 30),
        scrollback: 10_000,
        attach: None,
        session: "main".into(),
        remote_cmd: None,
        leader: std::env::var("LTERM_LEADER").unwrap_or_else(|_| "ctrl+space".into()),
        snapshot: None,
        snapshot_panes: 1,
        wait: 5.0,
        logo: None,
        logo_size: 256,
    };
    let mut it = args.into_iter();
    fn value(it: &mut impl Iterator<Item = String>, name: &str) -> Result<String, String> {
        it.next().ok_or(format!("{name} needs a value"))
    }
    fn num<T: std::str::FromStr>(v: String, name: &str) -> Result<T, String> {
        v.parse().map_err(|_| format!("invalid {name} '{v}'"))
    }
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                console();
                say(&format!("{USAGE}\nKEYS:\n"));
                print_keys(&o.leader);
                std::process::exit(0);
            }
            "-V" | "--version" => {
                console();
                say(&format!("lterm {}\n", env!("CARGO_PKG_VERSION")));
                std::process::exit(0);
            }
            "-e" | "--" => {
                o.command = it.by_ref().collect();
                break;
            }
            "-a" | "--attach" => o.attach = Some(value(&mut it, &arg)?),
            "-s" | "--session" => {
                o.session = value(&mut it, &arg)?;
                if !session::valid_name(&o.session) {
                    return Err(format!("invalid session name '{}' (letters, digits, - _ .)", o.session));
                }
            }
            "--remote-cmd" => o.remote_cmd = Some(value(&mut it, &arg)?),
            "--leader" => o.leader = value(&mut it, &arg)?,
            "--font" => o.font = Some(value(&mut it, &arg)?.into()),
            "--font-size" => o.font_size = num::<f32>(value(&mut it, &arg)?, &arg)?.clamp(4.0, 200.0),
            "--scrollback" => o.scrollback = num::<usize>(value(&mut it, &arg)?, &arg)?.min(1_000_000),
            "--snapshot" => o.snapshot = Some(value(&mut it, &arg)?.into()),
            "--snapshot-panes" => o.snapshot_panes = num::<usize>(value(&mut it, &arg)?, &arg)?.clamp(1, 16),
            "--wait" => o.wait = num(value(&mut it, &arg)?, &arg)?,
            "--logo" => o.logo = Some(value(&mut it, &arg)?.into()),
            "--logo-size" => o.logo_size = num::<u32>(value(&mut it, &arg)?, &arg)?.clamp(8, 1024),
            "--size" => {
                let v = value(&mut it, &arg)?;
                o.size = v
                    .split_once(['x', 'X'])
                    .and_then(|(c, r)| Some((c.parse().ok()?, r.parse().ok()?)))
                    .filter(|&(c, r): &(usize, usize)| (2..=1000).contains(&c) && (1..=500).contains(&r))
                    .ok_or(format!("invalid --size '{v}', expected e.g. 100x30"))?;
            }
            _ => return Err(format!("unknown option '{arg}'")),
        }
    }
    keymap::Leader::parse(&o.leader)?;
    Ok(o)
}

/// Print to stdout, ignoring errors (e.g. `lterm -h | head` closing the pipe early).
fn say(text: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes()).and_then(|_| out.flush());
}

fn print_keys(leader: &str) {
    let label = keymap::Leader::parse(leader).map(|l| l.label().to_string()).unwrap_or_else(|_| leader.to_string());
    let mut text = String::new();
    for line in keymap::help(&label) {
        if line != "Press any key to close" {
            text.push_str(&format!("    {line}\n"));
        }
    }
    text.push_str(&format!("    In the window, F1 or {label} then ? shows this list.\n"));
    say(&text);
}

/// Attach to the launching console, so messages show up even after detaching from it.
fn console() {
    #[cfg(windows)]
    {
        extern "system" {
            fn AttachConsole(pid: u32) -> i32;
        }
        unsafe {
            AttachConsole(u32::MAX);
        }
    }
}

pub fn fatal(msg: &str) -> ! {
    console();
    eprintln!("lterm: {msg}");
    std::process::exit(1);
}

/// On Windows, leave the console before opening the window. Returns true if a
/// detached copy was started and this process should exit.
#[cfg(windows)]
fn detach_console() -> bool {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    extern "system" {
        fn GetConsoleWindow() -> isize;
        fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
        fn FreeConsole() -> i32;
        fn GetStdHandle(which: u32) -> isize;
        fn SetHandleInformation(handle: isize, mask: u32, flags: u32) -> i32;
    }
    let relaunched = std::env::var_os("LTERM_DETACHED").is_some();
    std::env::remove_var("LTERM_DETACHED"); // don't leak into the shells we start
    unsafe {
        if GetConsoleWindow() == 0 {
            return false; // started without a console (Explorer, shortcut, detached)
        }
        let mut pids = [0u32; 4];
        if relaunched || GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) <= 1 {
            // The console exists only for us (older Windows ignoring the manifest): drop it.
            FreeConsole();
            return false;
        }
    }
    // Started from a shell: relaunch without the console so the prompt comes back.
    // The copy must not inherit our std handles: if the shell captured our output
    // through a pipe, it would otherwise wait for the window to close.
    const HANDLE_FLAG_INHERIT: u32 = 1;
    for which in [-10i32, -11, -12] {
        unsafe {
            SetHandleInformation(GetStdHandle(which as u32), HANDLE_FLAG_INHERIT, 0);
        }
    }
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let Ok(exe) = std::env::current_exe() else { return false };
    Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env("LTERM_DETACHED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .is_ok()
}

/// `lterm ls|kill|session-server|session-proxy`; None if `args` isn't one of them.
fn session_command(args: &[String]) -> Option<ExitCode> {
    let name = || args.get(1).map(String::as_str).unwrap_or("main");
    #[cfg(unix)]
    {
        match args.first().map(String::as_str)? {
            "ls" => Some(session::list_main()),
            "kill" => Some(session::kill_main(name())),
            "session-server" => Some(session::server_main(name())),
            "session-proxy" => Some(session::proxy_main(name())),
            _ => None,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = name;
        match args.first().map(String::as_str)? {
            "ls" | "kill" | "session-server" | "session-proxy" => {
                eprintln!("lterm: sessions run on Linux/macOS. On Windows use `lterm --attach wsl`,");
                eprintln!("       and `wsl lterm ls` / `wsl lterm kill NAME` to manage them.");
                Some(ExitCode::FAILURE)
            }
            _ => None,
        }
    }
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    // Started by ssh as SSH_ASKPASS (lterm --attach sets this): the prompt is the argument.
    if std::env::var_os("LTERM_ASKPASS").is_some() {
        return askpass::main(argv[1..].join(" "));
    }
    let name = argv.first().and_then(|a| Path::new(a).file_stem()).map(|s| s.to_string_lossy().to_lowercase());
    if name.as_deref() == Some("mdterm") {
        return mdterm::run("mdterm", argv[1..].to_vec());
    }
    let args = argv[1..].to_vec();
    match args.first().map(String::as_str) {
        Some("md") => return mdterm::run("lterm md", args[1..].to_vec()),
        Some("keys") => {
            print_keys(&std::env::var("LTERM_LEADER").unwrap_or_else(|_| "ctrl+space".into()));
            return ExitCode::SUCCESS;
        }
        Some("push") => return push::main(&args[1..]),
        Some("install") => return install::main(false),
        Some("uninstall") => return install::main(true),
        _ => {}
    }
    if let Some(code) = session_command(&args) {
        return code;
    }

    let opts = parse_args(args).unwrap_or_else(|e| fatal(&format!("{e}\nTry 'lterm --help'.")));
    if let Some(path) = &opts.logo {
        let s = opts.logo_size;
        if let Err(e) = write_rgba_png(path, &logo::render(s), s as usize, s as usize) {
            fatal(&e);
        }
        return ExitCode::SUCCESS;
    }
    if let Some(path) = opts.snapshot.clone() {
        let result = if opts.attach.is_some() { snapshot_session(&opts, &path) } else { snapshot(&opts, &path) };
        if let Err(e) = result {
            fatal(&e);
        }
        return ExitCode::SUCCESS;
    }
    #[cfg(windows)]
    if detach_console() {
        return ExitCode::SUCCESS;
    }
    let event_loop = EventLoop::<app::AppEvent>::with_user_event().build().unwrap_or_else(|e| fatal(&e.to_string()));
    let mut app = app::App::new(opts, event_loop.create_proxy());
    if let Err(e) = event_loop.run_app(&mut app) {
        fatal(&e.to_string());
    }
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------- headless snapshots

/// Render panes laid out by `tree` over the whole frame and save a PNG.
fn render_png(
    renderer: &mut Renderer,
    path: &Path,
    (w, h): (usize, usize),
    tree: &Node,
    terms: &HashMap<usize, Term>,
) -> Result<(), String> {
    let (mut rects, mut dividers) = (Vec::new(), Vec::new());
    tree.layout(Rect { x: 0, y: 0, w, h }, 2, &mut rects, &mut dividers);
    let first = rects.first().map(|r| r.0);
    let views: Vec<PaneView> = rects
        .iter()
        .filter_map(|&(id, rect)| {
            let term = terms.get(&id)?;
            Some(PaneView { term, rect, selection: None, focused: Some(id) == first, copy: None })
        })
        .collect();
    let outline = (rects.len() > 1).then(|| rects[0].1);
    let mut buf = vec![0u32; w * h];
    renderer.draw(&mut buf, w, h, &views, &dividers, outline, &Ui::default());
    let rgba: Vec<u8> = buf
        .iter()
        .flat_map(|&p| {
            let [r, g, b] = render::rgb_bytes(p);
            [r, g, b, 255]
        })
        .collect();
    write_rgba_png(path, &rgba, w, h)
}

/// Run COMMAND headless in `snapshot_panes` split panes (alternating right/down splits,
/// as the window lays them out) and save the screen as a PNG.
fn snapshot(opts: &Options, path: &Path) -> Result<(), String> {
    let mut renderer = Renderer::new(Fonts::load(opts.font.as_deref(), opts.font_size)?);
    let (w, h) = renderer.frame_size(opts.size.0, opts.size.1);
    let mut tree = Node::Leaf(0);
    for id in 1..opts.snapshot_panes {
        tree.split(id - 1, if id % 2 == 1 { Dir::Right } else { Dir::Down }, id);
    }
    let (mut rects, mut dividers) = (Vec::new(), Vec::new());
    tree.layout(Rect { x: 0, y: 0, w, h }, 2, &mut rects, &mut dividers);

    let (tx, rx) = mpsc::channel();
    let mut terms = HashMap::new();
    let mut ptys = HashMap::new();
    let mut parsers = HashMap::new();
    for &(id, rect) in &rects {
        let (cols, rows) = renderer.grid_size(rect.w, rect.h);
        let mut term = Term::new(cols, rows, opts.scrollback);
        term.cell_px = renderer.cell();
        let tx = tx.clone();
        let pty = pty::spawn(&opts.command, cols, rows, renderer.cell(), move |m| {
            let _ = tx.send((id, m));
        })?;
        terms.insert(id, term);
        ptys.insert(id, pty);
        parsers.insert(id, vte::Parser::new());
    }
    drop(tx);
    let deadline = Instant::now() + Duration::from_secs_f32(opts.wait.max(0.1));
    let mut running = ptys.len();
    loop {
        // After all children exit, keep reading until the output goes quiet.
        let timeout = if running == 0 { Duration::from_millis(300) } else { deadline.saturating_duration_since(Instant::now()) };
        match rx.recv_timeout(timeout) {
            Ok((id, PtyMsg::Data(d))) => {
                if let (Some(term), Some(parser), Some(pty)) = (terms.get_mut(&id), parsers.get_mut(&id), ptys.get_mut(&id)) {
                    term.feed(parser, &d);
                    pty.write(&take(&mut term.responses));
                }
            }
            Ok((_, PtyMsg::Exit)) => running -= 1,
            Err(_) => break,
        }
        if running > 0 && Instant::now() >= deadline {
            break;
        }
    }
    drop(ptys);
    render_png(&mut renderer, path, (w, h), &tree, &terms)
}

/// Attach to a session headless: start it with COMMAND if it's new, collect output for
/// `--wait` seconds, save the active tab as a PNG, then detach (the session keeps running).
fn snapshot_session(opts: &Options, path: &Path) -> Result<(), String> {
    let mut renderer = Renderer::new(Fonts::load(opts.font.as_deref(), opts.font_size)?);
    let (w, h) = renderer.frame_size(opts.size.0, opts.size.1);
    let target = Target::parse(opts.attach.as_deref().unwrap_or("local"));
    let (tx, rx) = mpsc::channel();
    let mut conn = Connection::open(&target, &opts.session, opts.remote_cmd.as_deref(), move |e| {
        let _ = tx.send(e);
    })?;
    let mut terms: HashMap<usize, Term> = HashMap::new();
    let mut parsers: HashMap<usize, vte::Parser> = HashMap::new();
    let mut layout_text = String::new();
    let mut welcomed = false;
    let deadline = Instant::now() + Duration::from_secs_f32(opts.wait.max(0.1));
    while let Ok(ev) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        match ev {
            ConnEvent::Frame(Frame::Welcome { panes, layout, .. }) => {
                welcomed = true;
                for p in panes {
                    terms.insert(p.id as usize, Term::new(p.cols as usize, p.rows as usize, opts.scrollback));
                    parsers.insert(p.id as usize, vte::Parser::new());
                }
                layout_text = layout;
                if terms.is_empty() {
                    let (cols, rows) = renderer.grid_size(w, h);
                    let (cw, ch) = renderer.cell();
                    let command = opts.command.clone();
                    conn.send(&Frame::Spawn { id: 0, cols: cols as u16, rows: rows as u16, cw: cw as u16, ch: ch as u16, command });
                    terms.insert(0, Term::new(cols, rows, opts.scrollback));
                    parsers.insert(0, vte::Parser::new());
                    layout_text = layout::encode_tabs(&[Tab { tree: Node::Leaf(0), focus: 0, zoomed: false }], 0);
                    conn.send(&Frame::Layout { data: layout_text.clone() });
                }
            }
            ConnEvent::Frame(Frame::Output { id, data }) => {
                if let (Some(t), Some(p)) = (terms.get_mut(&(id as usize)), parsers.get_mut(&(id as usize))) {
                    t.feed(p, &data);
                }
            }
            ConnEvent::Frame(Frame::Exited { id }) => {
                terms.remove(&(id as usize));
            }
            ConnEvent::Frame(_) => {}
            ConnEvent::Closed(msg) if !welcomed => return Err(msg),
            ConnEvent::Closed(_) => break,
        }
    }
    if !welcomed {
        return Err("no answer from the session server".into());
    }
    let (tabs, active) = layout::decode_tabs(&layout_text);
    let tree = tabs
        .into_iter()
        .nth(active)
        .and_then(|t| t.tree.retain(&|id| terms.contains_key(&id)))
        .or_else(|| terms.keys().min().map(|&id| Node::Leaf(id)))
        .ok_or("the session has no panes")?;
    drop(conn); // detach
    render_png(&mut renderer, path, (w, h), &tree, &terms)
}

pub(crate) fn write_rgba_png(path: &Path, rgba: &[u8], w: usize, h: usize) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .and_then(|mut wr| wr.write_image_data(rgba))
        .map_err(|e| e.to_string())
}
