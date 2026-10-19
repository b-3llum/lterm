//! `lterm browse`: a web browser in a pane, which agents can drive and you can watch.
//!
//! The viewer (`lterm browse [URL]`) runs in a pane like any program. It starts a headless
//! Chrome or Edge with a throwaway profile, streams the page into the pane as images (kitty
//! graphics), and passes clicks, scrolling and typing to the page. It needs nothing from the
//! window beyond images, so it also works in sessions and over SSH.
//!
//! The other subcommands (`open`, `goto`, `text`, `click`, ...) are for agents and scripts:
//! they find the browser of a pane from a small state file the viewer writes and talk to the
//! same page over the Chrome DevTools Protocol, so you see what they do as they do it.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{json, Value};
use unicode_width::UnicodeWidthChar;

use crate::cdp::{self, Cdp};
use crate::control::{self, Request};
use crate::layout::Dir;

pub const USAGE: &str = "\
lterm browse - a web browser in a pane, for you and for agents

IN A PANE:
    lterm browse [URL]                 Show a browser here (headless Chrome or Edge)
        Click, scroll and type as usual. Ctrl+L address, Ctrl+B / Ctrl+F back / forward,
        Ctrl+R reload, Ctrl+Y / Ctrl+N answer a page dialog, Ctrl+Q quit.

FOR AGENTS AND SCRIPTS (inside lterm; -p ID picks the browser pane, default: the only one):
    lterm browse open [URL] [--down|--tab]   Open a browser pane; prints its id
    lterm browse goto URL [--no-wait]        Go to URL and wait for it to load
    lterm browse back | forward | reload
    lterm browse info                        URL, title and load state
    lterm browse text [SELECTOR]             The page's (or an element's) visible text
    lterm browse html [SELECTOR]             HTML of the page or an element
    lterm browse links                       Links: text, then URL
    lterm browse click SELECTOR              Click an element (CSS selector)
        --text TEXT                          ...or the first link/button showing TEXT
        --at X Y                             ...or a point, in CSS pixels
    lterm browse type [--selector S] [--clear] [--enter] TEXT   Type into a field
    lterm browse key NAME...                 enter tab esc backspace up down pgdn ctrl+a ...
    lterm browse scroll up|down|top|bottom|PIXELS
    lterm browse eval JS                     Run JavaScript; prints the result
    lterm browse shot FILE [--full]          Save a PNG screenshot
    lterm browse wait --load | --text T | --selector S | --url PART   [--timeout SECS]
    lterm browse dialog accept|dismiss [TEXT]   Answer an alert/confirm/prompt
    lterm browse list                        Browser panes in this window
    lterm browse close                       Close the browser and its pane

    The browser is a fresh profile (no logins, cookies or extensions) and is deleted when it
    closes. Set LTERM_BROWSER to use a particular Chrome/Edge/Chromium executable.
";

pub fn main(args: &[String]) -> ExitCode {
    let result = match args.first().map(String::as_str) {
        Some("-h" | "--help" | "help") => {
            print!("{USAGE}");
            Ok(())
        }
        Some(sub) if CLIENT_COMMANDS.contains(&sub) => client(sub, args[1..].to_vec()),
        Some(url) if url.starts_with('-') => Err(format!("unknown option '{url}' (see lterm browse --help)")),
        url => viewer(url.map(String::from)),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lterm browse: {e}");
            ExitCode::FAILURE
        }
    }
}

const CLIENT_COMMANDS: &[&str] = &[
    "open", "goto", "back", "forward", "reload", "info", "text", "html", "links", "click", "type", "key", "scroll",
    "eval", "shot", "wait", "dialog", "list", "close",
];

// ---------------------------------------------------------------- where things live

/// Private per-user directory for state files.
fn state_dir() -> Result<PathBuf, String> {
    #[cfg(windows)]
    let dir = PathBuf::from(std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA isn't set")?).join("lterm").join("browse");
    #[cfg(unix)]
    let dir = {
        use std::os::unix::fs::DirBuilderExt;
        let uid = unsafe { libc::getuid() };
        let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|p| p.is_dir());
        let dir = match base {
            Some(b) => b.join("lterm").join("browse"),
            None => PathBuf::from(format!("/tmp/lterm-{uid}")).join("browse"),
        };
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        dir
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

/// This window (or session): browsers are found per window, by pane id.
fn window_key() -> Option<String> {
    use std::hash::{Hash, Hasher};
    let socket = std::env::var("LTERM_SOCKET").ok()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    socket.hash(&mut h);
    Some(format!("{:016x}", h.finish()))
}

fn state_path(pane: usize) -> Result<PathBuf, String> {
    pane_file(pane, "json")
}

/// Files next to the state: `dialog` (the open alert/confirm/prompt, written by the viewer)
/// and `answer` (how to answer it, written by `lterm browse dialog`). Chrome won't show a
/// dialog to a connection that starts following the page while it's open, so the viewer,
/// which follows it all along, answers for agents.
fn pane_file(pane: usize, ext: &str) -> Result<PathBuf, String> {
    let key = window_key().ok_or("not running inside lterm (LTERM_SOCKET isn't set)")?;
    Ok(state_dir()?.join(format!("{key}-{pane}.{ext}")))
}

fn own_pane() -> Option<usize> {
    std::env::var("LTERM_PANE").ok()?.parse().ok()
}

/// Whether process `pid` is running.
fn is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    unsafe {
        const SYNCHRONIZE: u32 = 0x0010_0000;
        let h = win::OpenProcess(SYNCHRONIZE, 0, pid);
        if h == 0 {
            return false;
        }
        let running = win::WaitForSingleObject(h, 0) == 0x102; // WAIT_TIMEOUT
        win::CloseHandle(h);
        running
    }
}

// ---------------------------------------------------------------- the browser process

fn find_browser() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("LTERM_BROWSER").map(PathBuf::from) {
        return if p.is_file() { Ok(p) } else { Err(format!("LTERM_BROWSER={} doesn't exist", p.display())) };
    }
    #[cfg(windows)]
    let candidates: Vec<PathBuf> = {
        let mut v = Vec::new();
        for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(base) = std::env::var_os(var).map(PathBuf::from) {
                v.push(base.join(r"Google\Chrome\Application\chrome.exe"));
                v.push(base.join(r"Microsoft\Edge\Application\msedge.exe"));
                v.push(base.join(r"Chromium\Application\chrome.exe"));
            }
        }
        v
    };
    #[cfg(unix)]
    let candidates: Vec<PathBuf> = {
        let names = ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser", "microsoft-edge", "brave-browser"];
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut v: Vec<PathBuf> = names.iter().flat_map(|n| std::env::split_paths(&path).map(move |d| d.join(n))).collect();
        v.push("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into());
        v.push("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge".into());
        v.push("/Applications/Chromium.app/Contents/MacOS/Chromium".into());
        v
    };
    candidates.into_iter().find(|p| p.is_file()).ok_or_else(|| {
        "no Chrome, Edge or Chromium found; install one, or set LTERM_BROWSER to its executable".to_string()
    })
}

/// A headless browser with a throwaway profile; it ends when this process does.
struct Browser {
    child: Child,
    profile: PathBuf,
    port: u16,
    /// DevTools URL of the browser itself.
    ws: String,
    #[cfg(windows)]
    _job: isize,
}

impl Browser {
    fn launch(exe: &Path) -> Result<Browser, String> {
        let profiles = std::env::temp_dir().join("lterm-browse");
        clean_profiles(&profiles);
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let profile = profiles.join(format!("{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&profile).map_err(|e| format!("{}: {e}", profile.display()))?;
        let mut cmd = Command::new(exe);
        cmd.arg("--headless=new")
            .arg("--remote-debugging-port=0")
            .arg(format!("--user-data-dir={}", profile.display()))
            .args(["--no-first-run", "--no-default-browser-check", "--disable-extensions", "--mute-audio"])
            // Don't wait on the desktop keyring (Linux) or keychain (macOS): it can hang forever.
            .args(["--password-store=basic", "--use-mock-keychain"])
            .args(["--disable-background-timer-throttling", "--disable-renderer-backgrounding"])
            .args(["--disable-backgrounding-occluded-windows", "--window-size=1280,800", "about:blank"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            if unsafe { libc::getuid() } == 0 {
                cmd.arg("--no-sandbox"); // Chrome refuses to run as root otherwise
            }
            // End with us, even if we're killed (Linux).
            #[cfg(target_os = "linux")]
            unsafe {
                cmd.pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                    Ok(())
                });
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let child = cmd.spawn().map_err(|e| format!("cannot start {}: {e}", exe.display()))?;
        #[cfg(windows)]
        let job = win::kill_on_close_job(&child);
        let mut b = Browser {
            child,
            profile,
            port: 0,
            ws: String::new(),
            #[cfg(windows)]
            _job: job,
        };
        // Chrome writes its port and browser path here once it listens.
        let file = b.profile.join("DevToolsActivePort");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(text) = std::fs::read_to_string(&file) {
                let mut lines = text.lines();
                if let (Some(port), Some(path)) = (lines.next().and_then(|p| p.trim().parse().ok()), lines.next()) {
                    b.port = port;
                    b.ws = format!("ws://127.0.0.1:{port}{}", path.trim());
                    return Ok(b);
                }
            }
            if let Ok(Some(status)) = b.child.try_wait() {
                return Err(format!("{} exited at startup ({status})", exe.display()));
            }
            if Instant::now() > deadline {
                return Err(format!("{} didn't start its DevTools server", exe.display()));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn page_url(&self, target: &str) -> String {
        format!("ws://127.0.0.1:{}/devtools/page/{target}", self.port)
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // The browser's helpers may hold files for a moment after it exits.
        for _ in 0..20 {
            if std::fs::remove_dir_all(&self.profile).is_ok() || !self.profile.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Remove profiles left by viewers that were killed (named after their process id).
fn clean_profiles(dir: &Path) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let pid = name.split('-').next().and_then(|p| p.parse::<u32>().ok());
        if pid.is_some_and(|p| p != std::process::id() && !is_alive(p)) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

// ---------------------------------------------------------------- the terminal

#[cfg(windows)]
mod win {
    pub type Handle = isize;
    #[repr(C)]
    #[derive(Default)]
    pub struct Coord {
        pub x: i16,
        pub y: i16,
    }
    #[repr(C)]
    #[derive(Default)]
    pub struct SmallRect {
        pub left: i16,
        pub top: i16,
        pub right: i16,
        pub bottom: i16,
    }
    #[repr(C)]
    #[derive(Default)]
    pub struct ScreenBufferInfo {
        pub size: Coord,
        pub cursor: Coord,
        pub attributes: u16,
        pub window: SmallRect,
        pub max_size: Coord,
    }
    #[repr(C)]
    #[derive(Default)]
    struct BasicLimits {
        per_process_user_time: i64,
        per_job_user_time: i64,
        limit_flags: u32,
        min_working_set: usize,
        max_working_set: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct ExtendedLimits {
        basic: BasicLimits,
        io: [u64; 6],
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory: usize,
        peak_job_memory: usize,
    }
    extern "system" {
        pub fn GetStdHandle(which: u32) -> Handle;
        pub fn GetConsoleMode(h: Handle, mode: *mut u32) -> i32;
        pub fn SetConsoleMode(h: Handle, mode: u32) -> i32;
        pub fn GetConsoleScreenBufferInfo(h: Handle, info: *mut ScreenBufferInfo) -> i32;
        pub fn SetConsoleCP(cp: u32) -> i32;
        pub fn SetConsoleOutputCP(cp: u32) -> i32;
        pub fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        pub fn WaitForSingleObject(h: Handle, ms: u32) -> u32;
        pub fn CloseHandle(h: Handle) -> i32;
        fn CreateJobObjectW(attrs: *const u8, name: *const u16) -> Handle;
        fn SetInformationJobObject(job: Handle, class: i32, info: *const ExtendedLimits, len: u32) -> i32;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
    }
    pub const STD_INPUT: u32 = -10i32 as u32;
    pub const STD_OUTPUT: u32 = -11i32 as u32;

    /// Put `child` in a job that kills it (and the processes it starts) when we exit.
    pub fn kill_on_close_job(child: &std::process::Child) -> Handle {
        use std::os::windows::io::AsRawHandle;
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job == 0 {
                return 0;
            }
            let mut info = ExtendedLimits::default();
            info.basic.limit_flags = 0x2000; // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            SetInformationJobObject(job, 9, &info, std::mem::size_of::<ExtendedLimits>() as u32);
            AssignProcessToJobObject(job, child.as_raw_handle() as Handle);
            job // deliberately kept open: closing it (at exit) ends the browser
        }
    }
}

/// Raw keyboard and mouse input while it's alive; the terminal's previous mode after.
struct Raw {
    #[cfg(unix)]
    saved: libc::termios,
    #[cfg(windows)]
    saved: (u32, u32),
}

impl Raw {
    fn enter() -> Result<Raw, String> {
        #[cfg(unix)]
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return Err("not a terminal".into());
            }
            let saved = t;
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(0, libc::TCSANOW, &t);
            Ok(Raw { saved })
        }
        #[cfg(windows)]
        unsafe {
            let (i, o) = (win::GetStdHandle(win::STD_INPUT), win::GetStdHandle(win::STD_OUTPUT));
            let (mut im, mut om) = (0u32, 0u32);
            if win::GetConsoleMode(i, &mut im) == 0 || win::GetConsoleMode(o, &mut om) == 0 {
                return Err("not a terminal".into());
            }
            // VT input (keys and mouse arrive as escape sequences), no line editing or echo.
            win::SetConsoleMode(i, 0x0200 | 0x0080);
            win::SetConsoleMode(o, om | 0x0001 | 0x0004 | 0x0008);
            win::SetConsoleCP(65001);
            win::SetConsoleOutputCP(65001);
            Ok(Raw { saved: (im, om) })
        }
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.saved);
        }
        #[cfg(windows)]
        unsafe {
            win::SetConsoleMode(win::GetStdHandle(win::STD_INPUT), self.saved.0);
            win::SetConsoleMode(win::GetStdHandle(win::STD_OUTPUT), self.saved.1);
        }
    }
}

/// Columns and rows of the terminal.
fn term_size() -> (usize, usize) {
    #[cfg(unix)]
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            return (ws.ws_col as usize, ws.ws_row as usize);
        }
    }
    #[cfg(windows)]
    unsafe {
        let mut info = win::ScreenBufferInfo::default();
        if win::GetConsoleScreenBufferInfo(win::GetStdHandle(win::STD_OUTPUT), &mut info) != 0 {
            let w = &info.window;
            return ((w.right - w.left + 1).max(1) as usize, (w.bottom - w.top + 1).max(1) as usize);
        }
    }
    (80, 24)
}

fn out(s: &str) {
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(s.as_bytes()).and_then(|_| o.flush());
}

/// Input from the terminal.
#[derive(Debug, PartialEq)]
enum In {
    Text(String),
    /// A named key (cdp::key_info names) with DevTools modifiers.
    Key(&'static str, u32),
    Ctrl(char),
    /// SGR mouse report: button code, 1-based column and row, pressed.
    Mouse(u32, usize, usize, bool),
    Paste(String),
    /// Reply to CSI 16 t: cell height and width in pixels.
    CellSize(usize, usize),
}

/// Parse terminal input. Incomplete sequences stay in `buf` for the next read.
fn parse_input(buf: &mut Vec<u8>, events: &mut Vec<In>) {
    let mut i = 0;
    while i < buf.len() {
        let b = buf[i];
        if b == 0x1b {
            let Some(&next) = buf.get(i + 1) else {
                events.push(In::Key("escape", 0));
                i += 1;
                continue;
            };
            if next == b'[' {
                let Some(end) = buf[i + 2..].iter().position(|c| (0x40..=0x7e).contains(c)).map(|p| i + 2 + p) else { break };
                let params = String::from_utf8_lossy(&buf[i + 2..end]).into_owned();
                let fin = buf[end];
                if params == "200" && fin == b'~' {
                    let body = &buf[end + 1..];
                    let Some(stop) = body.windows(6).position(|w| w == b"\x1b[201~") else { break };
                    events.push(In::Paste(String::from_utf8_lossy(&body[..stop]).into_owned()));
                    i = end + 1 + stop + 6;
                    continue;
                }
                i = end + 1;
                if let Some(ev) = csi(&params, fin) {
                    events.push(ev);
                }
            } else if next == b'O' {
                let Some(&fin) = buf.get(i + 2) else { break };
                if let Some(ev) = csi("", fin) {
                    events.push(ev);
                }
                i += 3;
            } else {
                i += 2; // Alt+key: not passed on
            }
            continue;
        }
        match b {
            b'\r' | b'\n' => events.push(In::Key("enter", 0)),
            0x7f | 0x08 => events.push(In::Key("backspace", 0)),
            b'\t' => events.push(In::Key("tab", 0)),
            0x00 => {}
            0x01..=0x1a => events.push(In::Ctrl((b - 1 + b'a') as char)),
            0x1c..=0x1f => {}
            _ => {
                let end = buf[i..].iter().position(|&c| c < 0x20 || c == 0x7f).map_or(buf.len(), |p| i + p);
                let chunk = &buf[i..end];
                // Keep a character split across reads for next time.
                let valid = match std::str::from_utf8(chunk) {
                    Ok(_) => chunk.len(),
                    Err(e) if e.error_len().is_none() && end == buf.len() => e.valid_up_to(),
                    Err(_) => chunk.len(),
                };
                if valid == 0 {
                    break;
                }
                events.push(In::Text(String::from_utf8_lossy(&chunk[..valid]).into_owned()));
                i += valid;
                continue;
            }
        }
        i += 1;
    }
    buf.drain(..i.min(buf.len()));
}

fn csi(params: &str, fin: u8) -> Option<In> {
    if let Some(m) = params.strip_prefix('<') {
        let n: Vec<usize> = m.split(';').filter_map(|p| p.parse().ok()).collect();
        return match n[..] {
            [b, x, y] if fin == b'M' || fin == b'm' => Some(In::Mouse(b as u32, x, y, fin == b'M')),
            _ => None,
        };
    }
    if fin == b't' {
        let n: Vec<usize> = params.split(';').filter_map(|p| p.parse().ok()).collect();
        return match n[..] {
            [6, h, w] => Some(In::CellSize(h, w)),
            _ => None,
        };
    }
    // xterm modifiers: 2 Shift, 3 Alt, 5 Ctrl (+ combinations) -> DevTools bits.
    let m = params.split(';').nth(1).and_then(|m| m.parse::<u32>().ok()).unwrap_or(1).saturating_sub(1);
    let mods = ((m & 1) << 3) | ((m & 2) >> 1) | ((m & 4) >> 1);
    let key = match (fin, params.split(';').next().unwrap_or("")) {
        (b'A', _) => "up",
        (b'B', _) => "down",
        (b'C', _) => "right",
        (b'D', _) => "left",
        (b'H', _) | (b'~', "1" | "7") => "home",
        (b'F', _) | (b'~', "4" | "8") => "end",
        (b'Z', _) => return Some(In::Key("tab", 8)),
        (b'~', "3") => "delete",
        (b'~', "5") => "pgup",
        (b'~', "6") => "pgdn",
        _ => return None,
    };
    Some(In::Key(key, mods))
}

// ---------------------------------------------------------------- the viewer

/// Text-mode colors for the address bar.
const BAR: &str = "\x1b[0;48;2;36;40;59;38;2;192;202;245m";
const BAR_DIM: &str = "\x1b[38;2;122;132;168m";
/// Columns of the back, forward and reload buttons in the bar.
const BUTTONS: [(usize, &str); 3] = [(2, "back"), (4, "forward"), (6, "reload")];

struct View {
    page: Cdp,
    browser: Cdp,
    target: String,
    cols: usize,
    rows: usize,
    /// Cell size in pixels, and device pixels per CSS pixel.
    cell: (usize, usize),
    scale: f64,
    url: String,
    title: String,
    loading: bool,
    dialog: Option<(String, String)>,
    editing: Option<String>,
    /// Image id of the frame on screen (frames alternate between 1 and 2).
    image: u32,
    /// Mouse button held down, and the last press (for double clicks).
    held: Option<&'static str>,
    last_press: (Instant, usize, usize, u32),
    /// New tabs the page opened, to be shown here instead.
    popups: Vec<String>,
    /// Where agents see the open dialog; its `answer` file is next to it.
    dialog_file: Option<PathBuf>,
}

fn viewer(url: Option<String>) -> Result<(), String> {
    let exe = find_browser()?;
    let browser = Browser::launch(&exe)?;
    let mut bconn = Cdp::connect(&browser.ws)?;
    let targets = bconn.call("Target.getTargets", json!({}))?;
    let target = targets["targetInfos"]
        .as_array()
        .and_then(|t| t.iter().find(|t| t["type"] == "page"))
        .and_then(|t| t["targetId"].as_str())
        .map(String::from);
    let target = match target {
        Some(t) => t,
        None => bconn.call("Target.createTarget", json!({ "url": "about:blank" }))?["targetId"].as_str().unwrap_or_default().to_string(),
    };
    let page = Cdp::connect(&browser.page_url(&target))?;
    bconn.call("Target.setDiscoverTargets", json!({ "discover": true }))?;

    // Tell agents in this window where this browser is.
    let state = own_pane().and_then(|p| state_path(p).ok());
    if let Some(path) = &state {
        let info = json!({ "page": browser.page_url(&target), "browser": browser.ws, "pid": std::process::id() });
        std::fs::write(path, info.to_string()).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    let _cleanup = Cleanup(state.clone());
    let dialog_file = state.as_ref().map(|p| p.with_extension("dialog"));

    let raw = Raw::enter()?;
    // Alternate screen, no cursor, mouse (press, release, drag; SGR), bracketed paste.
    out("\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?2004h\x1b[2J");
    let result = run_view(page, bconn, target, url, &browser, dialog_file);
    out("\x1b_Ga=d,d=A,q=2\x1b\\\x1b[?2004l\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?25h\x1b[?1049l");
    drop(raw);
    drop(browser);
    result
}

/// Removes the state files when the viewer ends.
struct Cleanup(Option<PathBuf>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(p) = &self.0 {
            for ext in ["json", "dialog", "answer"] {
                let _ = std::fs::remove_file(p.with_extension(ext));
            }
        }
    }
}

fn run_view(page: Cdp, browser_conn: Cdp, target: String, url: Option<String>, browser: &Browser, dialog_file: Option<PathBuf>) -> Result<(), String> {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) if cfg!(unix) => break,
                // Windows reports Ctrl+Z as end of input; keep reading.
                Ok(0) | Err(_) => thread::sleep(Duration::from_millis(50)),
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let (cols, rows) = term_size();
    let mut v = View {
        page,
        browser: browser_conn,
        target,
        cols,
        rows,
        cell: (8, 16),
        scale: 1.0,
        url: String::new(),
        title: String::new(),
        loading: false,
        dialog: None,
        editing: None,
        image: 2,
        held: None,
        last_press: (Instant::now() - Duration::from_secs(9), 0, 0, 0),
        popups: Vec::new(),
        dialog_file,
    };
    v.page.call("Page.enable", json!({}))?;
    v.query_cell_size(&rx);
    v.configure()?;
    if let Some(u) = url {
        v.navigate(&u)?;
    }
    v.draw_bar();

    let mut pending = Vec::new();
    let mut last_size_check = Instant::now();
    loop {
        // Keyboard and mouse.
        let mut events = Vec::new();
        while let Ok(bytes) = rx.try_recv() {
            pending.extend_from_slice(&bytes);
            parse_input(&mut pending, &mut events);
        }
        for ev in events {
            match v.input(ev) {
                Ok(true) => {}
                Ok(false) => return Ok(()),
                Err(e) if e.contains("connection") => return Err(e),
                // A command the page refused (e.g. no history to go back to): say so, carry on.
                Err(e) => {
                    v.title = e;
                    v.draw_bar();
                }
            }
        }
        // The page.
        while let Some(ev) = v.page.event(Duration::from_millis(12))? {
            v.page_event(&ev)?;
            if ev["method"] == "Inspector.detached" {
                return Ok(());
            }
        }
        while let Some(ev) = v.browser.event(Duration::ZERO)? {
            if !v.browser_event(&ev)? {
                return Ok(());
            }
        }
        if last_size_check.elapsed() > Duration::from_millis(250) {
            last_size_check = Instant::now();
            if term_size() != (v.cols, v.rows) {
                (v.cols, v.rows) = term_size();
                v.query_cell_size(&rx);
                v.configure()?;
            }
            v.answer_from_agent()?;
            if !is_alive(browser.child.id()) {
                return Err("the browser exited".into());
            }
        }
    }
}

impl View {
    /// Ask the terminal for its cell size in pixels (CSI 16 t); keep the old one if it
    /// doesn't answer. Input that arrives meanwhile is dropped.
    fn query_cell_size(&mut self, rx: &mpsc::Receiver<Vec<u8>>) {
        out("\x1b[16t");
        let deadline = Instant::now() + Duration::from_millis(800);
        let mut buf = Vec::new();
        while let Ok(bytes) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            buf.extend_from_slice(&bytes);
            let mut events = Vec::new();
            parse_input(&mut buf, &mut events);
            if let Some(In::CellSize(h, w)) = events.into_iter().find(|e| matches!(e, In::CellSize(..))) {
                if h > 0 && w > 0 {
                    self.cell = (w, h);
                }
                return;
            }
        }
    }

    /// Size the page to the pane and (re)start streaming it.
    fn configure(&mut self) -> Result<(), String> {
        let (cw, ch) = self.cell;
        let (w, h) = (self.cols * cw, self.rows.saturating_sub(1).max(1) * ch);
        // Page zoom follows the terminal's font size (about 19 px per row at 100%).
        self.scale = (ch as f64 / 19.0).clamp(0.5, 4.0);
        let (css_w, css_h) = ((w as f64 / self.scale).round(), (h as f64 / self.scale).round());
        self.page.call(
            "Emulation.setDeviceMetricsOverride",
            json!({ "width": css_w as u32, "height": css_h as u32, "deviceScaleFactor": self.scale, "mobile": false }),
        )?;
        let _ = self.page.call("Page.stopScreencast", json!({}));
        self.page.call("Page.startScreencast", json!({ "format": "png", "maxWidth": w, "maxHeight": h, "everyNthFrame": 1 }))?;
        out("\x1b[2J");
        self.draw_bar();
        Ok(())
    }

    /// Start loading a page; a slow site doesn't hold up the viewer (errors show as pages).
    fn navigate(&mut self, input: &str) -> Result<(), String> {
        let url = normalize_url(input);
        self.page.send("Page.navigate", json!({ "url": url }))?;
        self.url = url;
        self.title.clear();
        self.draw_bar();
        Ok(())
    }

    /// Bar text: buttons, then the address (or the one being typed), then status.
    fn draw_bar(&self) {
        let left = match &self.editing {
            Some(text) => format!(" ← → ↻  Go to: {text}▏"),
            None if self.title.is_empty() => format!(" ← → ↻  {}", self.url),
            None => format!(" ← → ↻  {}  {BAR_DIM}{}{BAR}", self.title, self.url),
        };
        let right = match (&self.dialog, self.loading, self.editing.is_some()) {
            (Some((kind, msg)), _, _) => format!("{kind}: {msg}  ^Y ok  ^N cancel "),
            (None, _, true) => "Enter go  Esc cancel ".into(),
            (None, true, _) => "loading… ".into(),
            (None, false, _) => "^L address  ^Q quit ".into(),
        };
        let right_w = text_width(&right);
        let left = fit(&left, self.cols.saturating_sub(right_w + 1));
        let pad = self.cols.saturating_sub(text_width(&left) + right_w);
        out(&format!("\x1b[1;1H{BAR}{left}{}{BAR_DIM}{right}\x1b[0m", " ".repeat(pad)));
    }

    /// Show a screencast frame (base64 PNG) below the bar.
    fn draw_frame(&mut self, png_b64: &str) {
        let (id, old) = if self.image == 1 { (2, 1) } else { (1, 2) };
        let rows = self.rows.saturating_sub(1).max(1);
        let mut s = String::with_capacity(png_b64.len() + png_b64.len() / 4096 * 16 + 128);
        s.push_str("\x1b[2;1H");
        let chunks: Vec<&[u8]> = png_b64.as_bytes().chunks(4096).collect();
        for (n, chunk) in chunks.iter().enumerate() {
            let more = (n + 1 < chunks.len()) as u8;
            if n == 0 {
                s.push_str(&format!("\x1b_Ga=T,f=100,q=2,C=1,i={id},c={},r={rows},m={more};", self.cols));
            } else {
                s.push_str(&format!("\x1b_Gm={more};"));
            }
            s.push_str(std::str::from_utf8(chunk).unwrap_or(""));
            s.push_str("\x1b\\");
        }
        // Remove the previous frame only now that the new one covers it.
        s.push_str(&format!("\x1b_Ga=d,d=I,i={old},q=2\x1b\\"));
        out(&s);
        self.image = id;
    }

    fn page_event(&mut self, ev: &Value) -> Result<(), String> {
        let p = &ev["params"];
        match ev["method"].as_str().unwrap_or("") {
            "Page.screencastFrame" => {
                if let Some(data) = p["data"].as_str() {
                    self.draw_frame(data);
                }
                self.page.send("Page.screencastFrameAck", json!({ "sessionId": p["sessionId"] }))?;
            }
            "Page.frameNavigated" if p["frame"]["parentId"].is_null() => {
                self.url = p["frame"]["url"].as_str().unwrap_or_default().to_string();
                self.draw_bar();
            }
            "Page.navigatedWithinDocument" | "Page.loadEventFired" | "Page.domContentEventFired" => self.refresh_title(),
            "Page.frameStartedLoading" | "Page.frameStoppedLoading" if p["frameId"] == self.target.as_str() => {
                self.loading = ev["method"] == "Page.frameStartedLoading";
                if !self.loading {
                    self.refresh_title();
                }
                self.draw_bar();
            }
            "Page.javascriptDialogOpening" => {
                let kind = p["type"].as_str().unwrap_or("dialog").to_string();
                let message = p["message"].as_str().unwrap_or_default().to_string();
                if let Some(f) = &self.dialog_file {
                    let _ = std::fs::write(f, format!("{kind}: {message}"));
                }
                self.dialog = Some((kind, message));
                self.draw_bar();
            }
            "Page.javascriptDialogClosed" => {
                if let Some(f) = &self.dialog_file {
                    let _ = std::fs::remove_file(f);
                }
                self.dialog = None;
                self.draw_bar();
            }
            _ => {}
        }
        Ok(())
    }

    /// Show tabs the page opens here instead. Returns false when our page is gone.
    fn browser_event(&mut self, ev: &Value) -> Result<bool, String> {
        let info = &ev["params"]["targetInfo"];
        match ev["method"].as_str().unwrap_or("") {
            "Target.targetDestroyed" if ev["params"]["targetId"] == self.target.as_str() => return Ok(false),
            "Target.targetCreated" if info["type"] == "page" && info["openerId"] == self.target.as_str() => {
                self.popups.push(info["targetId"].as_str().unwrap_or_default().to_string());
                self.adopt_popup(info)?;
            }
            "Target.targetInfoChanged" => self.adopt_popup(info)?,
            _ => {}
        }
        Ok(true)
    }

    fn adopt_popup(&mut self, info: &Value) -> Result<(), String> {
        let id = info["targetId"].as_str().unwrap_or_default();
        let url = info["url"].as_str().unwrap_or_default();
        if !self.popups.iter().any(|p| p == id) || url.is_empty() || url == "about:blank" {
            return Ok(());
        }
        self.popups.retain(|p| p != id);
        let _ = self.browser.send("Target.closeTarget", json!({ "targetId": id }));
        self.navigate(url)
    }

    /// Answer the open dialog the way `lterm browse dialog` asked.
    fn answer_from_agent(&mut self) -> Result<(), String> {
        let Some(file) = self.dialog_file.as_ref().map(|f| f.with_extension("answer")) else { return Ok(()) };
        let Ok(text) = std::fs::read_to_string(&file) else { return Ok(()) };
        let _ = std::fs::remove_file(&file);
        let (how, prompt) = text.split_once('\n').unwrap_or((text.as_str(), ""));
        let mut params = json!({ "accept": how.trim() == "accept" });
        if !prompt.is_empty() {
            params["promptText"] = json!(prompt);
        }
        self.page.send("Page.handleJavaScriptDialog", params)?;
        Ok(())
    }

    fn refresh_title(&mut self) {
        if let Ok(v) = cdp::eval(&mut self.page, "[document.title, location.href]") {
            self.title = v[0].as_str().unwrap_or_default().to_string();
            self.url = v[1].as_str().unwrap_or_default().to_string();
            self.draw_bar();
        }
    }

    fn history(&mut self, step: i64) -> Result<(), String> {
        let h = self.page.call("Page.getNavigationHistory", json!({}))?;
        let i = h["currentIndex"].as_i64().unwrap_or(0) + step;
        if let Some(entry) = h["entries"].as_array().and_then(|e| e.get(usize::try_from(i).ok()?)) {
            self.page.call("Page.navigateToHistoryEntry", json!({ "entryId": entry["id"] }))?;
        }
        Ok(())
    }

    /// Handle one input event. Returns false to quit.
    fn input(&mut self, ev: In) -> Result<bool, String> {
        if let Some(text) = &mut self.editing {
            match ev {
                In::Text(t) | In::Paste(t) => text.push_str(&t),
                In::Key("backspace", _) => {
                    text.pop();
                }
                In::Key("enter", _) => {
                    let target = std::mem::take(text);
                    self.editing = None;
                    if !target.trim().is_empty() {
                        self.navigate(target.trim())?;
                    }
                }
                In::Key("escape", _) | In::Ctrl('l') => self.editing = None,
                In::Ctrl('q') => return Ok(false),
                _ => {}
            }
            self.draw_bar();
            return Ok(true);
        }
        match ev {
            In::Ctrl('q') => return Ok(false),
            In::Ctrl('l') => {
                self.editing = Some(String::new());
                self.draw_bar();
            }
            In::Ctrl('r') => {
                self.page.call("Page.reload", json!({}))?;
            }
            In::Ctrl('b') => self.history(-1)?,
            In::Ctrl('f') => self.history(1)?,
            In::Ctrl(c @ ('y' | 'n')) if self.dialog.is_some() => {
                self.page.call("Page.handleJavaScriptDialog", json!({ "accept": c == 'y' }))?;
            }
            In::Ctrl(c) => {
                // Other Ctrl+letter combinations go to the page (select all, copy, ...).
                let key = c.to_string();
                let code = format!("Key{}", c.to_ascii_uppercase());
                let vk = c.to_ascii_uppercase() as u32;
                for kind in ["rawKeyDown", "keyUp"] {
                    self.page.send(
                        "Input.dispatchKeyEvent",
                        json!({ "type": kind, "key": key, "code": code, "windowsVirtualKeyCode": vk, "modifiers": 2 }),
                    )?;
                }
            }
            In::Text(t) => {
                for ch in t.chars() {
                    let s = ch.to_string();
                    self.page.send("Input.dispatchKeyEvent", json!({ "type": "keyDown", "key": s, "text": s, "unmodifiedText": s }))?;
                    self.page.send("Input.dispatchKeyEvent", json!({ "type": "keyUp", "key": s }))?;
                }
            }
            In::Paste(t) => {
                self.page.send("Input.insertText", json!({ "text": t }))?;
            }
            In::Key(name, mods) => cdp::press_key(&mut self.page, name, mods)?,
            In::Mouse(b, col, row, pressed) => self.mouse(b, col, row, pressed)?,
            In::CellSize(..) => {}
        }
        Ok(true)
    }

    fn mouse(&mut self, b: u32, col: usize, row: usize, pressed: bool) -> Result<(), String> {
        if row <= 1 {
            // The bar: buttons, or click the address to type one.
            if pressed && b == 0 {
                match BUTTONS.iter().find(|(c, _)| col == *c).map(|(_, what)| *what) {
                    Some("back") => self.history(-1)?,
                    Some("forward") => self.history(1)?,
                    Some("reload") => {
                        self.page.call("Page.reload", json!({}))?;
                    }
                    _ => {
                        self.editing = Some(String::new());
                        self.draw_bar();
                    }
                }
            }
            return Ok(());
        }
        let (cw, ch) = self.cell;
        let x = ((col - 1) as f64 + 0.5) * cw as f64 / self.scale;
        let y = ((row - 2) as f64 + 0.5) * ch as f64 / self.scale;
        if b & 64 != 0 {
            let dy = if b & 1 == 0 { -120 } else { 120 };
            self.page.send("Input.dispatchMouseEvent", json!({ "type": "mouseWheel", "x": x, "y": y, "deltaX": 0, "deltaY": dy }))?;
            return Ok(());
        }
        let button = match b & 3 {
            0 => "left",
            1 => "middle",
            2 => "right",
            _ => self.held.unwrap_or("left"),
        };
        if b & 32 != 0 {
            let buttons = if self.held.is_some() { 1 } else { 0 };
            self.page.send(
                "Input.dispatchMouseEvent",
                json!({ "type": "mouseMoved", "x": x, "y": y, "button": self.held.unwrap_or("none"), "buttons": buttons }),
            )?;
            return Ok(());
        }
        let clicks = if pressed {
            let (t, c, r, n) = self.last_press;
            let n = if t.elapsed() < Duration::from_millis(400) && (c, r) == (col, row) { n + 1 } else { 1 };
            self.last_press = (Instant::now(), col, row, n);
            n
        } else {
            self.last_press.3.max(1)
        };
        self.held = pressed.then_some(button);
        let kind = if pressed { "mousePressed" } else { "mouseReleased" };
        self.page.send("Input.dispatchMouseEvent", json!({ "type": kind, "x": x, "y": y, "button": button, "clickCount": clicks }))?;
        Ok(())
    }
}

fn text_width(s: &str) -> usize {
    let mut w = 0;
    let mut in_esc = false;
    for c in s.chars() {
        match (in_esc, c) {
            (false, '\x1b') => in_esc = true,
            (true, 'm') => in_esc = false,
            (true, _) => {}
            (false, c) => w += c.width().unwrap_or(0),
        }
    }
    w
}

/// Cut `s` (which may contain SGR sequences) to `width` columns, with an ellipsis.
fn fit(s: &str, width: usize) -> String {
    if text_width(s) <= width {
        return s.to_string();
    }
    let (mut out, mut w, mut in_esc) = (String::new(), 0, false);
    for c in s.chars() {
        if in_esc || c == '\x1b' {
            in_esc = c != 'm';
            out.push(c);
            continue;
        }
        let cw = c.width().unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// What the user typed in the address bar, as a URL: add a scheme, or search the web.
fn normalize_url(input: &str) -> String {
    let s = input.trim();
    let has_scheme = s.split_once("://").is_some_and(|(scheme, _)| scheme.chars().all(|c| c.is_ascii_alphanumeric()))
        || s.starts_with("about:")
        || s.starts_with("data:")
        || s.starts_with("file:");
    if has_scheme {
        s.to_string()
    } else if s.starts_with("localhost") || s.starts_with("127.") || s.starts_with("[::1]") {
        format!("http://{s}")
    } else if !s.contains(' ') && s.contains('.') {
        format!("https://{s}")
    } else {
        let q: String = s.bytes().map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            b' ' => "+".into(),
            _ => format!("%{b:02X}"),
        }).collect();
        format!("https://duckduckgo.com/?q={q}")
    }
}

// ---------------------------------------------------------------- commands for agents

/// Take `--name VALUE` (any of `names`) out of `args`.
fn take_value(args: &mut Vec<String>, names: &[&str]) -> Result<Option<String>, String> {
    let Some(i) = args.iter().position(|a| names.contains(&a.as_str())) else { return Ok(None) };
    if i + 1 >= args.len() {
        return Err(format!("{} needs a value", args[i]));
    }
    let v = args.remove(i + 1);
    args.remove(i);
    Ok(Some(v))
}

fn take_flag(args: &mut Vec<String>, names: &[&str]) -> bool {
    let before = args.len();
    args.retain(|a| !names.contains(&a.as_str()));
    args.len() != before
}

/// Browser panes of this window: (pane, state).
fn browsers() -> Result<Vec<(usize, Value)>, String> {
    let key = window_key().ok_or("not running inside lterm (LTERM_SOCKET isn't set)")?;
    let mut found = Vec::new();
    for entry in std::fs::read_dir(state_dir()?).map_err(|e| e.to_string())?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(pane) = name.strip_prefix(&format!("{key}-")).and_then(|r| r.strip_suffix(".json")).and_then(|p| p.parse().ok()) else {
            continue;
        };
        let Some(state) = std::fs::read_to_string(entry.path()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { continue };
        // A viewer that was killed leaves its file behind.
        if state["pid"].as_u64().is_some_and(|p| !is_alive(p as u32)) {
            let _ = std::fs::remove_file(entry.path());
            continue;
        }
        found.push((pane, state));
    }
    found.sort_by_key(|(p, _)| *p);
    Ok(found)
}

/// The browser pane `-p ID` names, or the only one.
fn pick(pane: Option<usize>) -> Result<(usize, Value), String> {
    let all = browsers()?;
    match pane {
        Some(id) => all.into_iter().find(|(p, _)| *p == id).ok_or(format!("pane {id} has no browser (start one with `lterm browse open`)")),
        None => match all.len() {
            0 => Err("no browser in this window; start one with `lterm browse open URL`".into()),
            1 => Ok(all.into_iter().next().expect("one")),
            _ => {
                let ids: Vec<String> = all.iter().map(|(p, _)| p.to_string()).collect();
                Err(format!("several browser panes ({}); pick one with -p ID", ids.join(", ")))
            }
        },
    }
}

fn connect(pane: Option<usize>) -> Result<Cdp, String> {
    let (id, state) = pick(pane)?;
    // The page can't run anything while a dialog is open; say so instead of waiting.
    if let Ok(dialog) = std::fs::read_to_string(pane_file(id, "dialog")?) {
        return Err(format!("a dialog is open in pane {id}: {dialog}\nanswer it with: lterm browse dialog accept|dismiss [TEXT]"));
    }
    Cdp::connect(state["page"].as_str().unwrap_or_default()).map_err(|e| format!("pane {id}: {e}"))
}

/// Wait for the page to finish loading (or for a same-page navigation).
fn wait_load(cdp: &mut Cdp, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(ev) = cdp.event(Duration::from_millis(200))? {
            if matches!(ev["method"].as_str(), Some("Page.loadEventFired" | "Page.navigatedWithinDocument")) {
                return Ok(());
            }
        }
    }
    Err("timed out waiting for the page to load".into())
}

fn print(s: &str) {
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(s.as_bytes());
    if !s.ends_with('\n') {
        let _ = o.write_all(b"\n");
    }
    let _ = o.flush();
}

/// JavaScript string literal.
fn js(s: &str) -> String {
    Value::String(s.to_string()).to_string()
}

fn client(sub: &str, mut args: Vec<String>) -> Result<(), String> {
    let pane = take_value(&mut args, &["-p", "--pane"])?
        .map(|p| p.parse::<usize>().map_err(|_| format!("invalid pane id '{p}'")))
        .transpose()?;
    let timeout = take_value(&mut args, &["--timeout"])?
        .map(|t| t.parse::<f64>().map_err(|_| format!("invalid timeout '{t}'")))
        .transpose()?
        .map_or(Duration::from_secs(30), Duration::from_secs_f64);
    match sub {
        "open" => {
            let down = take_flag(&mut args, &["-d", "--down"]);
            let tab = take_flag(&mut args, &["--tab"]);
            let exe = std::env::current_exe().map_err(|e| e.to_string())?.to_string_lossy().into_owned();
            let mut command = vec![exe, "browse".to_string()];
            command.extend(args.first().cloned());
            let req = if tab {
                Request::Tab { focus: false, run: String::new(), command }
            } else {
                let dir = if down { Dir::Down } else { Dir::Right };
                Request::Split { target: own_pane(), dir, focus: false, run: String::new(), command }
            };
            let id: usize = control::call(&req)?.trim().parse().map_err(|_| "unexpected answer from lterm")?;
            // Wait for the viewer to start its browser, then for the first page.
            let path = state_path(id)?;
            let deadline = Instant::now() + timeout;
            while !path.exists() {
                if Instant::now() > deadline {
                    return Err(format!("the browser in pane {id} didn't start"));
                }
                thread::sleep(Duration::from_millis(100));
            }
            if !args.is_empty() {
                let mut cdp = connect(Some(id))?;
                let _ = cdp::eval(&mut cdp, "document.readyState").map(|s| s == "complete");
                let deadline = Instant::now() + timeout;
                while Instant::now() < deadline {
                    let v = cdp::eval(&mut cdp, "[document.readyState, location.href]").unwrap_or_default();
                    if v[0] == "complete" && v[1] != "about:blank" {
                        break;
                    }
                    thread::sleep(Duration::from_millis(200));
                }
            }
            print(&id.to_string());
        }
        "list" => {
            for (id, state) in browsers()? {
                let info = Cdp::connect(state["page"].as_str().unwrap_or_default())
                    .and_then(|mut c| cdp::eval(&mut c, "[location.href, document.title]"))
                    .unwrap_or_default();
                print(&format!("{id}\t{}\t{}", info[0].as_str().unwrap_or("?"), info[1].as_str().unwrap_or("")));
            }
        }
        "dialog" => {
            let (id, _) = pick(pane)?;
            let accept = match args.first().map(String::as_str) {
                Some("accept") => "accept",
                Some("dismiss") => "dismiss",
                _ => return Err("dialog needs accept or dismiss".into()),
            };
            let open = pane_file(id, "dialog")?;
            if !open.exists() {
                return Err(format!("no dialog is open in pane {id}"));
            }
            let answer = format!("{accept}\n{}", args.get(1..).unwrap_or(&[]).join(" "));
            std::fs::write(pane_file(id, "answer")?, answer).map_err(|e| e.to_string())?;
            let deadline = Instant::now() + Duration::from_secs(5);
            while open.exists() {
                if Instant::now() > deadline {
                    return Err("the browser didn't close the dialog".into());
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
        "close" => {
            let (id, state) = pick(pane)?;
            // Closing the browser ends the viewer, and with it the pane `open` made.
            let mut b = Cdp::connect(state["browser"].as_str().unwrap_or_default())?;
            let _ = b.send("Browser.close", json!({}));
            thread::sleep(Duration::from_millis(300));
            print(&format!("closed the browser in pane {id}"));
        }
        "info" if pick(pane).and_then(|(id, _)| pane_file(id, "dialog")).is_ok_and(|f| f.exists()) => {
            let (id, _) = pick(pane)?;
            print(&format!("dialog {}", std::fs::read_to_string(pane_file(id, "dialog")?).unwrap_or_default()));
        }
        _ => command(sub, pane, timeout, args)?,
    }
    Ok(())
}

/// Commands that act on the page.
fn command(sub: &str, pane: Option<usize>, timeout: Duration, mut args: Vec<String>) -> Result<(), String> {
    let mut cdp = connect(pane)?;
    match sub {
        "goto" => {
            let no_wait = take_flag(&mut args, &["--no-wait"]);
            let url = normalize_url(args.first().ok_or("goto needs a URL")?);
            cdp.call("Page.enable", json!({}))?;
            let r = cdp.call("Page.navigate", json!({ "url": url }))?;
            if let Some(err) = r["errorText"].as_str() {
                return Err(format!("{url}: {err}"));
            }
            // A navigation to an anchor on the same page has no loader and no load event.
            if !no_wait && r.get("loaderId").is_some() {
                wait_load(&mut cdp, timeout)?;
            }
        }
        "back" | "forward" => {
            let h = cdp.call("Page.getNavigationHistory", json!({}))?;
            let i = h["currentIndex"].as_i64().unwrap_or(0) + if sub == "back" { -1 } else { 1 };
            let entry = h["entries"].as_array().and_then(|e| e.get(usize::try_from(i).ok()?)).ok_or(format!("nothing to go {sub} to"))?;
            cdp.call("Page.enable", json!({}))?;
            cdp.call("Page.navigateToHistoryEntry", json!({ "entryId": entry["id"] }))?;
            let _ = wait_load(&mut cdp, timeout);
        }
        "reload" => {
            cdp.call("Page.enable", json!({}))?;
            cdp.call("Page.reload", json!({}))?;
            wait_load(&mut cdp, timeout)?;
        }
        "info" => {
            let v = cdp::eval(&mut cdp, "[location.href, document.title, document.readyState]")?;
            print(&format!(
                "url {}\ntitle {}\nstate {}",
                v[0].as_str().unwrap_or(""),
                v[1].as_str().unwrap_or(""),
                v[2].as_str().unwrap_or("")
            ));
        }
        "text" | "html" => {
            let sel = args.first().map(|s| js(s)).unwrap_or_else(|| "null".into());
            let prop = if sub == "text" { "innerText" } else { "outerHTML" };
            let root = if sub == "text" { "document.body" } else { "document.documentElement" };
            let expr = format!(
                "(() => {{ const s = {sel}; const el = s ? document.querySelector(s) : {root};
                   if (!el) throw new Error('no element matches ' + s); return el.{prop}; }})()"
            );
            print(cdp::eval(&mut cdp, &expr)?.as_str().unwrap_or(""));
        }
        "links" => {
            let expr = "[...document.querySelectorAll('a[href]')].filter(a => a.offsetParent !== null)
                .map(a => (a.innerText || a.getAttribute('aria-label') || a.title || '').trim().replace(/\\s+/g, ' ') + '\\t' + a.href)";
            for line in cdp::eval(&mut cdp, expr)?.as_array().into_iter().flatten() {
                print(line.as_str().unwrap_or(""));
            }
        }
        "click" => {
            let at = take_flag(&mut args, &["--at"]);
            let by_text = take_value(&mut args, &["--text"])?;
            let (x, y) = if at {
                let n: Vec<f64> = args.iter().filter_map(|a| a.parse().ok()).collect();
                match n[..] {
                    [x, y] => (x, y),
                    _ => return Err("--at needs X and Y".into()),
                }
            } else {
                let find = match (&by_text, args.first()) {
                    (Some(t), _) => format!(
                        "(() => {{ const t = {}.toLowerCase();
                           const els = [...document.querySelectorAll('a, button, [role=button], input[type=submit], input[type=button], summary, label, [onclick]')];
                           return els.find(e => e.offsetParent !== null && ((e.innerText || e.value || e.getAttribute('aria-label') || '').trim().toLowerCase() === t))
                               || els.find(e => e.offsetParent !== null && (e.innerText || e.value || e.getAttribute('aria-label') || '').toLowerCase().includes(t)); }})()",
                        js(t)
                    ),
                    (None, Some(sel)) => format!("document.querySelector({})", js(sel)),
                    (None, None) => return Err("click needs a CSS selector, --text TEXT or --at X Y".into()),
                };
                let expr = format!(
                    "(() => {{ const el = {find}; if (!el) throw new Error('nothing to click matches that');
                       el.scrollIntoView({{ block: 'center', inline: 'center' }});
                       const r = el.getBoundingClientRect(); return [r.x + r.width / 2, r.y + r.height / 2]; }})()"
                );
                let v = cdp::eval(&mut cdp, &expr)?;
                (v[0].as_f64().unwrap_or(0.0), v[1].as_f64().unwrap_or(0.0))
            };
            if let Some(dialog) = cdp::click_at(&mut cdp, x, y)? {
                print(&format!("dialog open, waiting for an answer (lterm browse dialog accept|dismiss)
{dialog}"));
            }
        }
        "type" => {
            let selector = take_value(&mut args, &["-s", "--selector"])?;
            let clear = take_flag(&mut args, &["--clear"]);
            let enter = take_flag(&mut args, &["--enter"]);
            if let Some(sel) = &selector {
                let expr = format!(
                    "(() => {{ const el = document.querySelector({}); if (!el) throw new Error('no element matches ' + {});
                       el.scrollIntoView({{ block: 'center' }}); el.focus(); }})()",
                    js(sel),
                    js(sel)
                );
                cdp::eval(&mut cdp, &expr)?;
            }
            if clear {
                cdp::eval(&mut cdp, "(() => { const el = document.activeElement; if (el && el.select) el.select(); else document.execCommand('selectAll'); })()")?;
                cdp::press_key(&mut cdp, "backspace", 0)?;
            }
            let text = args.join(" ");
            if !text.is_empty() {
                cdp.call("Input.insertText", json!({ "text": text }))?;
            }
            if enter {
                cdp::press_key(&mut cdp, "enter", 0)?;
            }
        }
        "key" => {
            if args.is_empty() {
                return Err("key needs at least one key name".into());
            }
            for k in &args {
                press(&mut cdp, k)?;
            }
        }
        "scroll" => {
            let how = args.first().map(String::as_str).unwrap_or("down");
            let expr = match how {
                "down" => "window.scrollBy(0, innerHeight * 0.8)".to_string(),
                "up" => "window.scrollBy(0, -innerHeight * 0.8)".to_string(),
                "top" => "window.scrollTo(0, 0)".to_string(),
                "bottom" => "window.scrollTo(0, document.documentElement.scrollHeight)".to_string(),
                n => format!("window.scrollBy(0, {})", n.parse::<f64>().map_err(|_| format!("scroll: '{n}' isn't up, down, top, bottom or a number"))?),
            };
            cdp::eval(&mut cdp, &expr)?;
        }
        "eval" => {
            let v = cdp::eval(&mut cdp, &args.join(" "))?;
            match v {
                Value::String(s) => print(&s),
                Value::Null => {}
                other => print(&serde_json::to_string_pretty(&other).unwrap_or_default()),
            }
        }
        "shot" => {
            let full = take_flag(&mut args, &["--full"]);
            let file = args.first().ok_or("shot needs a file name")?;
            let mut params = json!({ "format": "png" });
            if full {
                let m = cdp.call("Page.getLayoutMetrics", json!({}))?;
                let size = &m["cssContentSize"];
                params["captureBeyondViewport"] = json!(true);
                params["clip"] = json!({ "x": 0, "y": 0, "width": size["width"], "height": size["height"], "scale": 1 });
            }
            let r = cdp.call("Page.captureScreenshot", params)?;
            let png = base64::engine::general_purpose::STANDARD.decode(r["data"].as_str().unwrap_or("")).map_err(|e| e.to_string())?;
            std::fs::write(file, png).map_err(|e| format!("{file}: {e}"))?;
            print(file);
        }
        "wait" => {
            let text = take_value(&mut args, &["--text"])?;
            let selector = take_value(&mut args, &["--selector"])?;
            let url = take_value(&mut args, &["--url"])?;
            let cond = match (text, selector, url) {
                (Some(t), _, _) => format!("(document.body && document.body.innerText.includes({}))", js(&t)),
                (_, Some(s), _) => format!("!!document.querySelector({})", js(&s)),
                (_, _, Some(u)) => format!("location.href.includes({})", js(&u)),
                _ => "document.readyState === 'complete'".to_string(),
            };
            let deadline = Instant::now() + timeout;
            loop {
                if cdp::eval(&mut cdp, &cond).ok() == Some(Value::Bool(true)) {
                    break;
                }
                if Instant::now() > deadline {
                    eprintln!("lterm browse: timed out");
                    std::process::exit(2);
                }
                thread::sleep(Duration::from_millis(250));
            }
        }
        _ => return Err(format!("unknown command '{sub}'")),
    }
    Ok(())
}

/// Press a key: a name (enter, tab, ...), a character, or ctrl+/alt+/shift+ with either.
fn press(cdp: &mut Cdp, spec: &str) -> Result<(), String> {
    let mut mods = 0;
    let mut name = spec;
    while let Some((m, rest)) = name.split_once('+').filter(|(_, r)| !r.is_empty()) {
        mods |= match m.to_ascii_lowercase().as_str() {
            "alt" => 1,
            "ctrl" | "control" => 2,
            "meta" | "cmd" => 4,
            "shift" => 8,
            _ => return Err(format!("unknown modifier in '{spec}'")),
        };
        name = rest;
    }
    if cdp::key_info(name).is_some() {
        return cdp::press_key(cdp, name, mods);
    }
    let mut chars = name.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else { return Err(format!("unknown key '{name}'")) };
    let text = if mods & 2 == 0 { c.to_string() } else { String::new() };
    let code = if c.is_ascii_alphabetic() { format!("Key{}", c.to_ascii_uppercase()) } else { String::new() };
    let vk = c.to_ascii_uppercase() as u32;
    cdp.call("Input.dispatchKeyEvent", json!({ "type": if text.is_empty() { "rawKeyDown" } else { "keyDown" },
        "key": c.to_string(), "code": code, "text": text, "windowsVirtualKeyCode": vk, "modifiers": mods }))?;
    cdp.call("Input.dispatchKeyEvent", json!({ "type": "keyUp", "key": c.to_string(), "code": code, "windowsVirtualKeyCode": vk, "modifiers": mods }))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(bytes: &[u8]) -> (Vec<In>, Vec<u8>) {
        let mut buf = bytes.to_vec();
        let mut ev = Vec::new();
        parse_input(&mut buf, &mut ev);
        (ev, buf)
    }

    #[test]
    fn input_parsing() {
        let (ev, rest) = parse(b"hi\r\x1b[A\x1b[1;5C\x7f\x0c");
        assert_eq!(ev, vec![In::Text("hi".into()), In::Key("enter", 0), In::Key("up", 0), In::Key("right", 2), In::Key("backspace", 0), In::Ctrl('l')]);
        assert!(rest.is_empty());
        let (ev, _) = parse(b"\x1b[<0;10;5M\x1b[<0;10;5m\x1b[<65;3;4M\x1b[6;18;9t");
        assert_eq!(ev, vec![In::Mouse(0, 10, 5, true), In::Mouse(0, 10, 5, false), In::Mouse(65, 3, 4, true), In::CellSize(18, 9)]);
        let (ev, _) = parse(b"\x1b[200~line one\nline two\x1b[201~");
        assert_eq!(ev, vec![In::Paste("line one\nline two".into())]);
        // An escape sequence or character cut off at the end of a read waits for the rest.
        let (ev, rest) = parse(b"a\x1b[<0;1");
        assert_eq!((ev, rest), (vec![In::Text("a".into())], b"\x1b[<0;1".to_vec()));
        let (ev, rest) = parse("é".as_bytes()[..1].as_ref());
        assert_eq!((ev.len(), rest.len()), (0, 1));
        // xterm modifier 4 = Alt+Shift, 6 = Ctrl+Shift: DevTools Alt 1, Ctrl 2, Shift 8.
        let (ev, _) = parse(b"\x1b[1;4D\x1b[1;6A");
        assert_eq!(ev, vec![In::Key("left", 9), In::Key("up", 10)]);
        let (ev, _) = parse(b"\x1b");
        assert_eq!(ev, vec![In::Key("escape", 0)]);
    }

    #[test]
    fn urls() {
        assert_eq!(normalize_url("example.com"), "https://example.com");
        assert_eq!(normalize_url("localhost:3000/x"), "http://localhost:3000/x");
        assert_eq!(normalize_url("http://a.b"), "http://a.b");
        assert_eq!(normalize_url("about:blank"), "about:blank");
        assert_eq!(normalize_url("rust borrow checker"), "https://duckduckgo.com/?q=rust+borrow+checker");
    }

    #[test]
    fn bar_text_fits() {
        assert_eq!(text_width(&format!("ab{BAR_DIM}cd{BAR}")), 4);
        let s = fit("hello world", 6);
        assert_eq!(s, "hello…");
        assert_eq!(fit("short", 10), "short");
    }
}
