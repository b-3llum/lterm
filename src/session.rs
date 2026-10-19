//! Persistent sessions, like tmux.
//!
//! A session server (`lterm session-server NAME`, Linux/macOS) owns the shells and keeps a
//! terminal state for each pane, so it can redraw them for a window that attaches later.
//! It listens on a Unix socket private to the user. A window reaches it by running
//! `lterm session-proxy NAME` (locally, through `ssh -T host`, or through `wsl.exe`),
//! which relays the protocol over stdin/stdout and starts the server if needed.

use std::io::{self, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

pub const VERSION: u32 = 3;
/// Written by the proxy before the first frame, so stray output from a remote login
/// shell (motd, profile scripts) can be skipped.
const MAGIC: &[u8] = b"\0LTERM-SESSION\0";
const MAX_FRAME: usize = 64 << 20;

#[derive(Clone, Debug, PartialEq)]
pub struct PaneInfo {
    pub id: u32,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Frame {
    Hello { version: u32 },
    Welcome { version: u32, layout: String, panes: Vec<PaneInfo> },
    Spawn { id: u32, cols: u16, rows: u16, cw: u16, ch: u16, command: Vec<String> },
    Input { id: u32, data: Vec<u8> },
    Resize { id: u32, cols: u16, rows: u16, cw: u16, ch: u16 },
    Kill { id: u32 },
    Layout { data: String },
    Output { id: u32, data: Vec<u8> },
    Exited { id: u32 },
    Detach,
    Kicked,
    Shutdown,
    /// A `lterm pane` request the server passes to the window (it changes the layout).
    Control { req: u32, args: Vec<String> },
    ControlReply { req: u32, ok: bool, text: String },
    /// The server tells the window what the agent in a pane is doing (state 0: none).
    Agent { id: u32, state: u32, name: String, message: String },
}

fn put16(b: &mut Vec<u8>, v: u16) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put_bytes(b: &mut Vec<u8>, v: &[u8]) {
    put32(b, v.len() as u32);
    b.extend_from_slice(v);
}

struct Rd<'a>(&'a [u8]);

impl Rd<'_> {
    fn take(&mut self, n: usize) -> io::Result<&[u8]> {
        if self.0.len() < n {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated frame"));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn bytes(&mut self) -> io::Result<Vec<u8>> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }
    fn string(&mut self) -> io::Result<String> {
        Ok(String::from_utf8_lossy(&self.bytes()?).into_owned())
    }
}

impl Frame {
    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        let mut p = Vec::new();
        let tag = match self {
            Frame::Hello { version } => {
                put32(&mut p, *version);
                1
            }
            Frame::Welcome { version, layout, panes } => {
                put32(&mut p, *version);
                put_bytes(&mut p, layout.as_bytes());
                put32(&mut p, panes.len() as u32);
                for pane in panes {
                    put32(&mut p, pane.id);
                    put16(&mut p, pane.cols);
                    put16(&mut p, pane.rows);
                }
                2
            }
            Frame::Spawn { id, cols, rows, cw, ch, command } => {
                put32(&mut p, *id);
                for v in [*cols, *rows, *cw, *ch] {
                    put16(&mut p, v);
                }
                put32(&mut p, command.len() as u32);
                for arg in command {
                    put_bytes(&mut p, arg.as_bytes());
                }
                3
            }
            Frame::Input { id, data } => {
                put32(&mut p, *id);
                put_bytes(&mut p, data);
                4
            }
            Frame::Resize { id, cols, rows, cw, ch } => {
                put32(&mut p, *id);
                for v in [*cols, *rows, *cw, *ch] {
                    put16(&mut p, v);
                }
                5
            }
            Frame::Kill { id } => {
                put32(&mut p, *id);
                6
            }
            Frame::Layout { data } => {
                put_bytes(&mut p, data.as_bytes());
                7
            }
            Frame::Output { id, data } => {
                put32(&mut p, *id);
                put_bytes(&mut p, data);
                8
            }
            Frame::Exited { id } => {
                put32(&mut p, *id);
                9
            }
            Frame::Detach => 10,
            Frame::Kicked => 11,
            Frame::Shutdown => 12,
            Frame::Control { req, args } => {
                put32(&mut p, *req);
                put32(&mut p, args.len() as u32);
                for a in args {
                    put_bytes(&mut p, a.as_bytes());
                }
                13
            }
            Frame::ControlReply { req, ok, text } => {
                put32(&mut p, *req);
                put32(&mut p, *ok as u32);
                put_bytes(&mut p, text.as_bytes());
                14
            }
            Frame::Agent { id, state, name, message } => {
                put32(&mut p, *id);
                put32(&mut p, *state);
                put_bytes(&mut p, name.as_bytes());
                put_bytes(&mut p, message.as_bytes());
                15
            }
        };
        let mut out = Vec::with_capacity(5 + p.len());
        out.push(tag);
        put32(&mut out, p.len() as u32);
        out.extend_from_slice(&p);
        w.write_all(&out)?;
        w.flush()
    }

    pub fn read_from(r: &mut impl Read) -> io::Result<Frame> {
        let mut hdr = [0u8; 5];
        r.read_exact(&mut hdr)?;
        let len = u32::from_le_bytes(hdr[1..].try_into().unwrap()) as usize;
        if len > MAX_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
        }
        let mut buf = vec![0; len];
        r.read_exact(&mut buf)?;
        let mut d = Rd(&buf);
        Ok(match hdr[0] {
            1 => Frame::Hello { version: d.u32()? },
            2 => {
                let version = d.u32()?;
                let layout = d.string()?;
                let n = d.u32()? as usize;
                let mut panes = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    panes.push(PaneInfo { id: d.u32()?, cols: d.u16()?, rows: d.u16()? });
                }
                Frame::Welcome { version, layout, panes }
            }
            3 => {
                let (id, cols, rows, cw, ch) = (d.u32()?, d.u16()?, d.u16()?, d.u16()?, d.u16()?);
                let n = d.u32()? as usize;
                let mut command = Vec::with_capacity(n.min(256));
                for _ in 0..n {
                    command.push(d.string()?);
                }
                Frame::Spawn { id, cols, rows, cw, ch, command }
            }
            4 => Frame::Input { id: d.u32()?, data: d.bytes()? },
            5 => Frame::Resize { id: d.u32()?, cols: d.u16()?, rows: d.u16()?, cw: d.u16()?, ch: d.u16()? },
            6 => Frame::Kill { id: d.u32()? },
            7 => Frame::Layout { data: d.string()? },
            8 => Frame::Output { id: d.u32()?, data: d.bytes()? },
            9 => Frame::Exited { id: d.u32()? },
            10 => Frame::Detach,
            11 => Frame::Kicked,
            12 => Frame::Shutdown,
            13 => {
                let req = d.u32()?;
                let n = d.u32()? as usize;
                let mut args = Vec::with_capacity(n.min(256));
                for _ in 0..n {
                    args.push(d.string()?);
                }
                Frame::Control { req, args }
            }
            14 => Frame::ControlReply { req: d.u32()?, ok: d.u32()? != 0, text: d.string()? },
            15 => Frame::Agent { id: d.u32()?, state: d.u32()?, name: d.string()?, message: d.string()? },
            t => return Err(io::Error::new(io::ErrorKind::InvalidData, format!("unknown frame type {t}"))),
        })
    }
}

/// The ssh command: `ssh`, or `$LTERM_SSH` split on spaces (e.g. "ssh -p 2222 -i ~/.ssh/k").
pub fn ssh_command() -> Command {
    let spec = std::env::var("LTERM_SSH").unwrap_or_default();
    let mut parts = spec.split_whitespace();
    let mut c = Command::new(parts.next().unwrap_or("ssh"));
    c.args(parts);
    c
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

// ---------------------------------------------------------------- client side (the window)

/// Where the session server runs.
#[derive(Clone, Debug)]
pub enum Target {
    /// This machine (Linux/macOS).
    Local,
    /// Inside WSL, optionally a specific distro (Windows).
    Wsl(Option<String>),
    /// A remote machine over SSH (`user@host`, or a Host from ~/.ssh/config).
    Ssh(String),
}

impl Target {
    pub fn parse(s: &str) -> Target {
        match s {
            "local" => Target::Local,
            "wsl" => Target::Wsl(None),
            _ => match s.strip_prefix("wsl:") {
                Some(distro) => Target::Wsl(Some(distro.to_string())),
                None => Target::Ssh(s.to_string()),
            },
        }
    }

    pub fn label(&self) -> String {
        match self {
            Target::Local => "local".into(),
            Target::Wsl(None) => "wsl".into(),
            Target::Wsl(Some(d)) => format!("wsl:{d}"),
            Target::Ssh(h) => h.clone(),
        }
    }
}

pub enum ConnEvent {
    Frame(Frame),
    Closed(String),
}

/// A window's connection to a session server.
pub struct Connection {
    stdin: ChildStdin,
    child: Child,
}

impl Connection {
    /// Start the proxy (locally, via ssh or via wsl) and say hello. Frames arrive via `sink`.
    pub fn open(
        target: &Target,
        name: &str,
        remote_cmd: Option<&str>,
        sink: impl Fn(ConnEvent) + Send + 'static,
    ) -> Result<Connection, String> {
        if !valid_name(name) {
            return Err(format!("invalid session name '{name}' (use letters, digits, - _ .)"));
        }
        // A login shell, so ~/.local/bin and friends are on PATH remotely.
        let remote = match remote_cmd {
            Some(cmd) => format!("{cmd} session-proxy {name}"),
            None => format!("sh -lc 'exec lterm session-proxy {name}'"),
        };
        let mut cmd = match target {
            Target::Local => {
                let exe = std::env::current_exe().map_err(|e| e.to_string())?;
                let mut c = Command::new(exe);
                c.args(["session-proxy", name]);
                c
            }
            Target::Wsl(distro) => {
                let mut c = Command::new("wsl.exe");
                if let Some(d) = distro {
                    c.args(["-d", d]);
                }
                c.args(["-e", "sh", "-c", &remote]);
                c
            }
            Target::Ssh(host) => {
                let mut c = ssh_command();
                c.args(["-T", "-o", "ServerAliveInterval=15", "-o", "ConnectTimeout=20", host, &remote]);
                // No terminal for ssh to prompt in, so passwords, passphrases and host-key
                // questions go to lterm's prompt window (see askpass.rs).
                if let Ok(exe) = std::env::current_exe() {
                    c.env("SSH_ASKPASS", exe).env("SSH_ASKPASS_REQUIRE", "force").env("LTERM_ASKPASS", "1");
                    #[cfg(unix)]
                    if std::env::var_os("DISPLAY").is_none() {
                        c.env("DISPLAY", ":0"); // older OpenSSH only uses askpass with DISPLAY set
                    }
                }
                c
            }
        };
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd.spawn().map_err(|e| format!("cannot start {:?}: {e}", cmd.get_program()))?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let mut stderr = child.stderr.take().ok_or("no stderr")?;
        let mut stdin = child.stdin.take().ok_or("no stdin")?;

        let errors = Arc::new(Mutex::new(String::new()));
        let errs = errors.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = stderr.read(&mut buf) {
                let mut e = errs.lock().unwrap();
                if e.len() < 8192 {
                    e.push_str(&String::from_utf8_lossy(&buf[..n]));
                }
            }
        });
        thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            let why = match skip_to_magic(&mut r) {
                Err(junk) => junk,
                Ok(()) => loop {
                    match Frame::read_from(&mut r) {
                        Ok(f) => sink(ConnEvent::Frame(f)),
                        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break String::new(),
                        Err(e) => break e.to_string(),
                    }
                },
            };
            thread::sleep(std::time::Duration::from_millis(200)); // let stderr arrive
            let err = errors.lock().unwrap().trim().to_string();
            let msg = [err, why].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(": ");
            sink(ConnEvent::Closed(if msg.is_empty() { "connection closed".into() } else { msg }));
        });
        Frame::Hello { version: VERSION }.write_to(&mut stdin).map_err(|e| e.to_string())?;
        Ok(Connection { stdin, child })
    }

    pub fn send(&mut self, f: &Frame) -> bool {
        f.write_to(&mut self.stdin).is_ok()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = Frame::Detach.write_to(&mut self.stdin);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Discard bytes until MAGIC. On failure, returns what was seen instead (for the error).
fn skip_to_magic(r: &mut impl Read) -> Result<(), String> {
    let mut seen = Vec::new();
    let mut byte = [0u8; 1];
    while seen.len() < 64 * 1024 {
        if r.read(&mut byte).map_err(|e| e.to_string())? == 0 {
            break;
        }
        seen.push(byte[0]);
        if seen.ends_with(MAGIC) {
            return Ok(());
        }
    }
    let text = String::from_utf8_lossy(&seen).trim().to_string();
    Err(if text.is_empty() { String::new() } else { format!("unexpected output: {text}") })
}

// ---------------------------------------------------------------- server side

#[cfg(unix)]
pub use server::{kill_main, list_main, proxy_main, server_main};

#[cfg(unix)]
mod server {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use crate::control::{self, Agent, AgentState, Endpoint, Reply, Request};
    use crate::layout::{self, Dir, Node, Tab};
    use crate::pty::{self, Pty, PtyMsg};
    use crate::term::Term;

    /// A private per-user directory for session sockets.
    fn socket_dir() -> io::Result<PathBuf> {
        let uid = unsafe { libc::getuid() };
        // The same directory whether or not XDG_RUNTIME_DIR is set (it isn't under cron,
        // some SSH setups, or VM tools), so every context sees the same sessions.
        let user_run = PathBuf::from(format!("/run/user/{uid}"));
        let run = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .or_else(|| std::fs::metadata(&user_run).ok().filter(|m| m.is_dir() && m.uid() == uid).map(|_| user_run));
        let dir = match run {
            Some(run) => run.join("lterm"),
            None => PathBuf::from(format!("/tmp/lterm-{uid}")),
        };
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        let meta = std::fs::metadata(&dir)?;
        if meta.uid() != uid || meta.mode() & 0o077 != 0 {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{} is not private to you", dir.display())));
        }
        Ok(dir)
    }

    fn socket_path(name: &str) -> io::Result<PathBuf> {
        if !valid_name(name) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("invalid session name '{name}'")));
        }
        Ok(socket_dir()?.join(format!("{name}.sock")))
    }

    fn fail(msg: impl std::fmt::Display) -> ExitCode {
        eprintln!("lterm: {msg}");
        ExitCode::FAILURE
    }

    /// `lterm session-proxy NAME`: relay stdin/stdout to the session server, starting it if needed.
    pub fn proxy_main(name: &str) -> ExitCode {
        use std::io::IsTerminal;
        if io::stdin().is_terminal() {
            return fail("session-proxy is used by `lterm --attach`; it isn't meant to be run by hand");
        }
        let path = match socket_path(name) {
            Ok(p) => p,
            Err(e) => return fail(e),
        };
        let stream = match UnixStream::connect(&path) {
            Ok(s) => s,
            Err(_) => {
                let exe = match std::env::current_exe() {
                    Ok(e) => e,
                    Err(e) => return fail(e),
                };
                let mut cmd = Command::new(exe);
                cmd.args(["session-server", name]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
                // Own session: the server must outlive this proxy and the SSH connection.
                unsafe {
                    cmd.pre_exec(|| {
                        libc::setsid();
                        Ok(())
                    });
                }
                if let Err(e) = cmd.spawn() {
                    return fail(format!("cannot start session server: {e}"));
                }
                let mut tries = 0;
                loop {
                    thread::sleep(Duration::from_millis(50));
                    match UnixStream::connect(&path) {
                        Ok(s) => break s,
                        Err(e) if tries > 100 => return fail(format!("session server didn't start: {e}")),
                        Err(_) => tries += 1,
                    }
                }
            }
        };
        let mut from_server = match stream.try_clone() {
            Ok(s) => s,
            Err(e) => return fail(e),
        };
        let mut out = io::stdout().lock();
        if out.write_all(MAGIC).and_then(|_| out.flush()).is_err() {
            return ExitCode::FAILURE;
        }
        drop(out);
        thread::spawn(move || {
            let mut out = io::stdout().lock();
            let mut buf = vec![0u8; 64 * 1024];
            while let Ok(n @ 1..) = from_server.read(&mut buf) {
                if out.write_all(&buf[..n]).and_then(|_| out.flush()).is_err() {
                    break;
                }
            }
            std::process::exit(0);
        });
        let mut to_server = stream;
        let _ = io::copy(&mut io::stdin().lock(), &mut to_server);
        let _ = to_server.shutdown(std::net::Shutdown::Write);
        ExitCode::SUCCESS
    }

    /// `lterm ls`: list running sessions.
    pub fn list_main() -> ExitCode {
        let dir = match socket_dir() {
            Ok(d) => d,
            Err(e) => return fail(e),
        };
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.strip_suffix(".sock").map(String::from))
            .collect();
        names.sort();
        let mut any = false;
        for name in names {
            let path = dir.join(format!("{name}.sock"));
            if UnixStream::connect(&path).is_ok() {
                println!("{name}");
                any = true;
            } else {
                let _ = std::fs::remove_file(&path); // stale
            }
        }
        if !any {
            println!("no sessions");
        }
        ExitCode::SUCCESS
    }

    /// `lterm kill NAME`: end a session and everything running in it.
    pub fn kill_main(name: &str) -> ExitCode {
        let path = match socket_path(name) {
            Ok(p) => p,
            Err(e) => return fail(e),
        };
        match UnixStream::connect(&path) {
            Ok(mut s) => {
                let _ = Frame::Shutdown.write_to(&mut s);
                ExitCode::SUCCESS
            }
            Err(_) => fail(format!("no session named '{name}'")),
        }
    }

    enum Msg {
        Conn(UnixStream),
        Frame(u64, Frame),
        Gone(u64),
        Pty(u32, PtyMsg),
        Control(Request, mpsc::Sender<Reply>),
    }

    struct Pane {
        term: Term,
        parser: vte::Parser,
        pty: Pty,
        last_output: Instant,
        agent: Option<Agent>,
    }

    fn agent_frame(id: u32, agent: &Option<Agent>) -> Frame {
        let state = AgentState::code(agent.as_ref().map(|a| a.state));
        let (name, message) = agent.as_ref().map_or((String::new(), String::new()), |a| (a.name.clone(), a.message.clone()));
        Frame::Agent { id, state, name, message }
    }

    type Client = Option<(u64, UnixStream)>;

    fn send(client: &mut Client, f: &Frame) {
        if let Some((_, s)) = client {
            if f.write_to(s).is_err() {
                *client = None;
            }
        }
    }

    /// The shells of a session, and the layout the window last reported.
    struct Shells {
        panes: HashMap<u32, Pane>,
        layout: String,
        tx: mpsc::Sender<Msg>,
        control: Option<Endpoint>,
    }

    impl Shells {
        fn spawn(&mut self, id: u32, cols: usize, rows: usize, cell: (usize, usize), command: &[String]) -> Result<(), String> {
            let (cols, rows) = (cols.max(2), rows.max(1));
            let mut term = Term::new(cols, rows, 10_000);
            term.cell_px = cell;
            let tx = self.tx.clone();
            let env = self.control.as_ref().map(|c| c.env(id as usize)).unwrap_or_default();
            let pty = pty::spawn(command, cols, rows, cell, &env, move |m| {
                let _ = tx.send(Msg::Pty(id, m));
            })?;
            self.panes.insert(id, Pane { term, parser: vte::Parser::new(), pty, last_output: Instant::now(), agent: None });
            Ok(())
        }

        fn pane(&mut self, id: usize) -> Result<&mut Pane, String> {
            u32::try_from(id).ok().and_then(|i| self.panes.get_mut(&i)).ok_or_else(|| control::no_pane(id))
        }

        /// The saved tabs, without panes that have exited.
        fn tabs(&self) -> (Vec<Tab>, usize) {
            let (saved, active) = layout::decode_tabs(&self.layout);
            let tabs: Vec<Tab> = saved
                .into_iter()
                .filter_map(|t| {
                    let tree = t.tree.retain(&|id| self.panes.contains_key(&(id as u32)))?;
                    let focus = if tree.contains(t.focus) { t.focus } else { tree.first_leaf() };
                    Some(Tab { tree, focus, zoomed: t.zoomed })
                })
                .collect();
            let active = active.min(tabs.len().saturating_sub(1));
            (tabs, active)
        }

        /// Answer a `lterm pane` request here. Requests that change the layout come here
        /// only while no window is attached; then the server edits the saved layout, and
        /// the next window shows the result.
        fn request(&mut self, req: Request, client: &mut Client) -> Reply {
            match req {
                Request::List => {
                    let (tabs, active) = self.tabs();
                    let mut out = String::new();
                    let mut listed = HashSet::new();
                    for (ti, tab) in tabs.iter().enumerate() {
                        for id in tab.tree.leaves() {
                            listed.insert(id as u32);
                            let focused = ti == active && id == tab.focus;
                            let p = &self.panes[&(id as u32)];
                            out.push_str(&control::list_line(id, Some(ti), &p.term, focused, p.agent.as_ref()));
                        }
                    }
                    let mut rest: Vec<u32> = self.panes.keys().copied().filter(|id| !listed.contains(id)).collect();
                    rest.sort();
                    for id in rest {
                        let p = &self.panes[&id];
                        out.push_str(&control::list_line(id as usize, None, &p.term, false, p.agent.as_ref()));
                    }
                    Ok(out)
                }
                Request::Send { id, paste, data } => {
                    let p = self.pane(id)?;
                    let bytes = control::input_bytes(&p.term, paste, &data);
                    p.pty.write(&bytes);
                    if Agent::typed(&mut p.agent, &bytes) {
                        send(client, &agent_frame(id as u32, &p.agent));
                    }
                    Ok(String::new())
                }
                Request::Read { id, lines } => Ok(control::read_text(&self.pane(id)?.term, lines)),
                Request::Info { id } => {
                    let p = self.pane(id)?;
                    Ok(control::info_text(&p.term, p.last_output, p.agent.as_ref()))
                }
                Request::Close { id } => {
                    self.pane(id)?;
                    self.panes.remove(&(id as u32));
                    send(client, &Frame::Exited { id: id as u32 });
                    Ok(String::new())
                }
                Request::State { id, state, agent, message } => {
                    let p = self.pane(id)?;
                    if Agent::report(&mut p.agent, state, &agent, &message) {
                        send(client, &agent_frame(id as u32, &p.agent));
                    }
                    Ok(String::new())
                }
                Request::Focus { id } => {
                    self.pane(id)?;
                    let (mut tabs, active) = self.tabs();
                    if let Some(ti) = tabs.iter().position(|t| t.tree.contains(id)) {
                        tabs[ti].focus = id;
                        self.layout = layout::encode_tabs(&tabs, ti);
                    } else {
                        self.layout = layout::encode_tabs(&tabs, active);
                    }
                    Ok(String::new())
                }
                Request::Split { target, dir, focus, run, command } => {
                    let (mut tabs, mut active) = self.tabs();
                    let target = match target {
                        Some(t) => t,
                        None => tabs.get(active).map(|t| t.focus).ok_or("the session has no panes")?,
                    };
                    let t = &self.pane(target)?.term;
                    let (cols, rows) = match dir {
                        Dir::Right => (t.cols / 2, t.rows),
                        Dir::Down => (t.cols, t.rows / 2),
                    };
                    let cell = t.cell_px;
                    let id = self.next_id();
                    self.spawn(id, cols, rows, cell, &command)?;
                    let new = id as usize;
                    let ti = match tabs.iter().position(|t| t.tree.contains(target)) {
                        Some(ti) => {
                            tabs[ti].tree.split(target, dir, new);
                            tabs[ti].zoomed = false;
                            ti
                        }
                        None => {
                            tabs.push(Tab { tree: Node::Leaf(new), focus: new, zoomed: false });
                            tabs.len() - 1
                        }
                    };
                    if focus {
                        tabs[ti].focus = new;
                        active = ti;
                    }
                    self.layout = layout::encode_tabs(&tabs, active);
                    self.started(id, &run)
                }
                Request::Tab { focus, run, command } => {
                    let (mut tabs, active) = self.tabs();
                    let size = self.panes.values().next().map(|p| (p.term.cols, p.term.rows, p.term.cell_px));
                    let (cols, rows, cell) = size.unwrap_or((80, 24, (8, 16)));
                    let id = self.next_id();
                    self.spawn(id, cols, rows, cell, &command)?;
                    tabs.push(Tab { tree: Node::Leaf(id as usize), focus: id as usize, zoomed: false });
                    let active = if focus { tabs.len() - 1 } else { active };
                    self.layout = layout::encode_tabs(&tabs, active);
                    self.started(id, &run)
                }
            }
        }

        fn next_id(&self) -> u32 {
            self.panes.keys().max().map_or(0, |m| m + 1)
        }

        fn started(&mut self, id: u32, run: &str) -> Reply {
            if let (false, Some(p)) = (run.is_empty(), self.panes.get_mut(&id)) {
                p.pty.write(format!("{run}\r").as_bytes());
            }
            Ok(format!("{id}\n"))
        }
    }

    fn fail_waiting(waiting: &mut HashMap<u32, mpsc::Sender<Reply>>) {
        for (_, reply) in waiting.drain() {
            let _ = reply.send(Err("the window detached; try again".into()));
        }
    }

    /// `lterm session-server NAME`: own the shells of one session.
    pub fn server_main(name: &str) -> ExitCode {
        let path = match socket_path(name) {
            Ok(p) => p,
            Err(e) => return fail(e),
        };
        if UnixStream::connect(&path).is_ok() {
            return fail(format!("session '{name}' is already running"));
        }
        let _ = std::fs::remove_file(&path);
        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) => return fail(e),
        };
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        if let Some(home) = std::env::var_os("HOME") {
            let _ = std::env::set_current_dir(home);
        }

        let (tx, rx) = mpsc::channel::<Msg>();
        let accept_tx = tx.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                if accept_tx.send(Msg::Conn(stream)).is_err() {
                    break;
                }
            }
        });

        // `lterm pane` requests from programs in the panes, on a socket next to the session's.
        let ctl_path = path.with_extension("ctl");
        let _ = std::fs::remove_file(&ctl_path);
        let control = UnixListener::bind(&ctl_path).ok().map(|l| {
            let _ = std::fs::set_permissions(&ctl_path, std::fs::Permissions::from_mode(0o600));
            let tx = tx.clone();
            control::serve_unix(l, ctl_path.to_string_lossy().into_owned(), move |req, reply| {
                let _ = tx.send(Msg::Control(req, reply));
            })
        });

        let mut sh = Shells { panes: HashMap::new(), layout: String::new(), tx: tx.clone(), control };
        // The attached window: (connection id, stream).
        let mut client: Client = None;
        // Connections that haven't said Hello yet (`lterm kill` connections never do).
        let mut pending: HashMap<u64, UnixStream> = HashMap::new();
        let mut next_conn = 0u64;
        let mut started = false;
        // Control requests passed to the window, by request number.
        let mut waiting: HashMap<u32, mpsc::Sender<Reply>> = HashMap::new();
        let mut next_req = 0u32;

        while let Ok(msg) = rx.recv() {
            match msg {
                Msg::Conn(stream) => {
                    let id = next_conn;
                    next_conn += 1;
                    let Ok(read) = stream.try_clone() else { continue };
                    let tx = tx.clone();
                    thread::spawn(move || {
                        let mut r = BufReader::new(read);
                        while let Ok(f) = Frame::read_from(&mut r) {
                            if tx.send(Msg::Frame(id, f)).is_err() {
                                return;
                            }
                        }
                        let _ = tx.send(Msg::Gone(id));
                    });
                    pending.insert(id, stream);
                }
                Msg::Frame(conn, Frame::Hello { .. }) => {
                    let Some(stream) = pending.remove(&conn) else { continue };
                    if let Some((_, mut old)) = client.take() {
                        let _ = Frame::Kicked.write_to(&mut old);
                        let _ = old.shutdown(std::net::Shutdown::Both);
                    }
                    fail_waiting(&mut waiting);
                    client = Some((conn, stream));
                    let mut ids: Vec<u32> = sh.panes.keys().copied().collect();
                    ids.sort();
                    let info = ids
                        .iter()
                        .map(|id| {
                            let t = &sh.panes[id].term;
                            PaneInfo { id: *id, cols: t.cols as u16, rows: t.rows as u16 }
                        })
                        .collect();
                    send(&mut client, &Frame::Welcome { version: VERSION, layout: sh.layout.clone(), panes: info });
                    for id in ids {
                        let p = &sh.panes[&id];
                        send(&mut client, &Frame::Output { id, data: p.term.snapshot(5000) });
                        if p.agent.is_some() {
                            send(&mut client, &agent_frame(id, &p.agent));
                        }
                    }
                }
                Msg::Frame(_, Frame::Shutdown) => break,
                Msg::Frame(conn, f) => {
                    if client.as_ref().map(|(c, _)| *c) != Some(conn) {
                        continue;
                    }
                    match f {
                        Frame::Spawn { id, cols, rows, cw, ch, command } => {
                            if sh.panes.contains_key(&id) {
                                continue;
                            }
                            let cell = (cw.max(1) as usize, ch.max(1) as usize);
                            match sh.spawn(id, cols as usize, rows as usize, cell, &command) {
                                Ok(()) => started = true,
                                Err(e) => {
                                    let data = format!("lterm: cannot start shell: {e}\r\n").into_bytes();
                                    send(&mut client, &Frame::Output { id, data });
                                    send(&mut client, &Frame::Exited { id });
                                }
                            }
                        }
                        Frame::Input { id, data } => {
                            if let Some(p) = sh.panes.get_mut(&id) {
                                p.pty.write(&data);
                                if Agent::typed(&mut p.agent, &data) {
                                    send(&mut client, &agent_frame(id, &p.agent));
                                }
                            }
                        }
                        Frame::Resize { id, cols, rows, cw, ch } => {
                            if let Some(p) = sh.panes.get_mut(&id) {
                                let cell = (cw.max(1) as usize, ch.max(1) as usize);
                                p.term.cell_px = cell;
                                p.term.resize(cols.max(2) as usize, rows.max(1) as usize);
                                p.pty.resize(p.term.cols, p.term.rows, cell);
                            }
                        }
                        Frame::Kill { id } => {
                            sh.panes.remove(&id);
                        }
                        Frame::Layout { data } => sh.layout = data,
                        Frame::ControlReply { req, ok, text } => {
                            if let Some(reply) = waiting.remove(&req) {
                                let _ = reply.send(if ok { Ok(text) } else { Err(text) });
                            }
                        }
                        Frame::Detach => {
                            if let Some((_, s)) = client.take() {
                                let _ = s.shutdown(std::net::Shutdown::Both);
                            }
                            fail_waiting(&mut waiting);
                        }
                        _ => {}
                    }
                }
                Msg::Gone(conn) => {
                    pending.remove(&conn);
                    if client.as_ref().map(|(c, _)| *c) == Some(conn) {
                        client = None;
                        fail_waiting(&mut waiting);
                    }
                }
                Msg::Control(req, reply) => {
                    // The window owns the layout while it's attached, so it makes those changes.
                    let layout_change = matches!(req, Request::Split { .. } | Request::Tab { .. } | Request::Focus { .. });
                    if layout_change && client.is_some() {
                        let n = next_req;
                        next_req = next_req.wrapping_add(1);
                        send(&mut client, &Frame::Control { req: n, args: req.encode() });
                        if client.is_some() {
                            waiting.insert(n, reply);
                        } else {
                            let _ = reply.send(Err("the window went away; try again".into()));
                        }
                    } else {
                        let _ = reply.send(sh.request(req, &mut client));
                    }
                }
                Msg::Pty(id, PtyMsg::Data(data)) => {
                    let Some(p) = sh.panes.get_mut(&id) else { continue };
                    // The server answers terminal queries; it's there even while detached.
                    p.term.feed(&mut p.parser, &data);
                    p.last_output = Instant::now();
                    let replies = std::mem::take(&mut p.term.responses);
                    p.pty.write(&replies);
                    let notified = p.term.notification.take().is_some_and(|m| Agent::notified(&mut p.agent, &m));
                    send(&mut client, &Frame::Output { id, data });
                    if notified {
                        send(&mut client, &agent_frame(id, &p.agent));
                    }
                }
                Msg::Pty(id, PtyMsg::Exit) => {
                    if sh.panes.remove(&id).is_some() {
                        send(&mut client, &Frame::Exited { id });
                    }
                }
            }
            if started && sh.panes.is_empty() {
                break; // the last shell exited: the session is over
            }
        }
        drop(sh);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&ctl_path);
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let frames = vec![
            Frame::Hello { version: 1 },
            Frame::Welcome { version: 1, layout: "active 0\n".into(), panes: vec![PaneInfo { id: 3, cols: 80, rows: 24 }] },
            Frame::Spawn { id: 4, cols: 100, rows: 30, cw: 9, ch: 18, command: vec!["htop".into(), "-d".into(), "5".into()] },
            Frame::Input { id: 4, data: b"ls\r".to_vec() },
            Frame::Resize { id: 4, cols: 50, rows: 20, cw: 9, ch: 18 },
            Frame::Kill { id: 4 },
            Frame::Layout { data: "x".into() },
            Frame::Output { id: 3, data: vec![0, 27, 255] },
            Frame::Exited { id: 3 },
            Frame::Detach,
            Frame::Kicked,
            Frame::Shutdown,
            Frame::Control { req: 5, args: vec!["split".into(), String::new(), "down".into()] },
            Frame::ControlReply { req: 5, ok: true, text: "8\n".into() },
            Frame::Agent { id: 2, state: 2, name: "claude".into(), message: "needs you".into() },
        ];
        let mut buf = b"Welcome to Ubuntu!\n".to_vec();
        buf.extend_from_slice(MAGIC);
        for f in &frames {
            f.write_to(&mut buf).unwrap();
        }
        let mut r = &buf[..];
        skip_to_magic(&mut r).unwrap();
        for f in &frames {
            assert_eq!(&Frame::read_from(&mut r).unwrap(), f);
        }
    }

    #[test]
    fn targets() {
        assert!(matches!(Target::parse("wsl"), Target::Wsl(None)));
        assert!(matches!(Target::parse("wsl:kali-linux"), Target::Wsl(Some(d)) if d == "kali-linux"));
        assert!(matches!(Target::parse("me@box"), Target::Ssh(h) if h == "me@box"));
        assert!(valid_name("main") && !valid_name("../x") && !valid_name(""));
    }
}
