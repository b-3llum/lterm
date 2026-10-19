//! Control API: programs running in a pane (coding agents, scripts) drive the window or
//! session they run in with `lterm pane ...`: split off panes, start commands in them,
//! type into them, read their screens and wait for them.
//!
//! Every pane gets LTERM_SOCKET (where to connect: `127.0.0.1:PORT` for a window, a Unix
//! socket path for a session server), LTERM_TOKEN (a secret every request must carry)
//! and LTERM_PANE (its own pane id).
//!
//! A request is `lterm-control TOKEN\nLEN\n` followed by LEN bytes: the request's words
//! separated by NUL. The reply is `ok\n` or `error\n`, then text, then the connection closes.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::layout::Dir;
use crate::term::{Selection, Term};

const MAGIC: &str = "lterm-control";
const MAX_REQUEST: usize = 16 << 20;

pub type Reply = Result<String, String>;

#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    List,
    /// Split pane `target` (default: the focused pane). `command` empty means the shell;
    /// `run` is typed into the new pane followed by Enter.
    Split { target: Option<usize>, dir: Dir, focus: bool, run: String, command: Vec<String> },
    Tab { focus: bool, run: String, command: Vec<String> },
    /// Type `data` into a pane. With `paste`, it goes in as a paste (one block, newlines
    /// don't submit) when the program asked for bracketed paste.
    Send { id: usize, paste: bool, data: String },
    /// The last `lines` lines of text (0: one screenful), ending at the last non-blank line.
    Read { id: usize, lines: usize },
    Info { id: usize },
    Focus { id: usize },
    Close { id: usize },
    /// An agent in pane `id` reports its state (None: no agent there any more).
    State { id: usize, state: Option<AgentState>, agent: String, message: String },
}

/// What a coding agent in a pane is doing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AgentState {
    /// Generating, running tools.
    Working,
    /// Waiting for you: a permission prompt, a question.
    Blocked,
    /// Finished its turn.
    Done,
    /// Started, or done and looked at.
    Idle,
}

impl AgentState {
    pub fn parse(s: &str) -> Option<AgentState> {
        Some(match s {
            "working" => AgentState::Working,
            "blocked" => AgentState::Blocked,
            "done" => AgentState::Done,
            "idle" => AgentState::Idle,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            AgentState::Working => "working",
            AgentState::Blocked => "blocked",
            AgentState::Done => "done",
            AgentState::Idle => "idle",
        }
    }

    /// Wire code: 0 is "no agent". Sent by session servers (Unix only).
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn code(state: Option<AgentState>) -> u32 {
        match state {
            None => 0,
            Some(AgentState::Working) => 1,
            Some(AgentState::Blocked) => 2,
            Some(AgentState::Done) => 3,
            Some(AgentState::Idle) => 4,
        }
    }

    pub fn from_code(c: u32) -> Option<AgentState> {
        [AgentState::Working, AgentState::Blocked, AgentState::Done, AgentState::Idle].get((c as usize).wrapping_sub(1)).copied()
    }
}

/// A coding agent running in a pane.
#[derive(Clone, Debug, PartialEq)]
pub struct Agent {
    pub name: String,
    pub state: AgentState,
    /// What it said last (a notification, e.g. "Claude needs your permission").
    pub message: String,
    /// It reports `working` itself (hooks), so typing into it doesn't have to imply it.
    pub reports_working: bool,
}

impl Agent {
    /// Apply a `lterm pane state` report. Returns whether anything changed.
    pub fn report(slot: &mut Option<Agent>, state: Option<AgentState>, name: &str, message: &str) -> bool {
        let old = slot.clone();
        match state {
            None => *slot = None,
            Some(state) => {
                let a = slot.get_or_insert_with(|| Agent {
                    name: String::new(),
                    state,
                    message: String::new(),
                    reports_working: false,
                });
                if !name.is_empty() {
                    a.name = name.to_string();
                } else if a.name.is_empty() {
                    a.name = "agent".into();
                }
                a.state = state;
                a.message = message.to_string();
                a.reports_working |= state == AgentState::Working;
            }
        }
        *slot != old
    }

    /// A terminal notification (OSC 9/777/99) from the pane: the program wants attention.
    /// It marks the pane blocked unless its agent already said it's done.
    pub fn notified(slot: &mut Option<Agent>, message: &str) -> bool {
        match slot {
            Some(a) if matches!(a.state, AgentState::Done | AgentState::Idle) => false,
            _ => {
                let name = slot.as_ref().map(|a| a.name.clone()).unwrap_or_default();
                Agent::report(slot, Some(AgentState::Blocked), &name, message)
            }
        }
    }

    /// Input typed into the pane. Pressing Enter in an agent that doesn't report its own
    /// progress (e.g. Codex, which only says when it's done) starts a new turn.
    pub fn typed(slot: &mut Option<Agent>, data: &[u8]) -> bool {
        match slot {
            Some(a) if !a.reports_working && a.state != AgentState::Working && data.contains(&b'\r') => {
                a.state = AgentState::Working;
                a.message.clear();
                true
            }
            _ => false,
        }
    }
}

impl Request {
    /// The request's words on the wire.
    pub fn encode(&self) -> Vec<String> {
        let flag = |v: bool| if v { "1" } else { "0" }.to_string();
        let dir = |d: Dir| if d == Dir::Right { "right" } else { "down" }.to_string();
        let mut w: Vec<String> = match self {
            Request::List => vec!["list".into()],
            Request::Split { target, dir: d, focus, run, command } => {
                let target = target.map(|t| t.to_string()).unwrap_or_default();
                let mut w = vec!["split".into(), target, dir(*d), flag(*focus), run.clone()];
                w.extend(command.iter().cloned());
                w
            }
            Request::Tab { focus, run, command } => {
                let mut w = vec!["tab".into(), flag(*focus), run.clone()];
                w.extend(command.iter().cloned());
                w
            }
            Request::Send { id, paste, data } => vec!["send".into(), id.to_string(), flag(*paste), data.clone()],
            Request::Read { id, lines } => vec!["read".into(), id.to_string(), lines.to_string()],
            Request::Info { id } => vec!["info".into(), id.to_string()],
            Request::Focus { id } => vec!["focus".into(), id.to_string()],
            Request::Close { id } => vec!["close".into(), id.to_string()],
            Request::State { id, state, agent, message } => {
                let s = state.map_or("none", |s| s.name());
                vec!["state".into(), id.to_string(), s.into(), agent.clone(), message.clone()]
            }
        };
        w.shrink_to_fit();
        w
    }

    pub fn decode(w: &[String]) -> Result<Request, String> {
        let bad = || format!("bad request {w:?}");
        let num = |i: usize| w.get(i).and_then(|s| s.parse::<usize>().ok()).ok_or_else(bad);
        let flag = |i: usize| w.get(i).map(|s| s == "1").ok_or_else(bad);
        let text = |i: usize| w.get(i).cloned().ok_or_else(bad);
        let rest = |i: usize| w.get(i..).unwrap_or(&[]).to_vec();
        Ok(match w.first().map(String::as_str) {
            Some("list") => Request::List,
            Some("split") => Request::Split {
                target: w.get(1).filter(|s| !s.is_empty()).map(|s| s.parse()).transpose().map_err(|_| bad())?,
                dir: if text(2)? == "down" { Dir::Down } else { Dir::Right },
                focus: flag(3)?,
                run: text(4)?,
                command: rest(5),
            },
            Some("tab") => Request::Tab { focus: flag(1)?, run: text(2)?, command: rest(3) },
            Some("send") => Request::Send { id: num(1)?, paste: flag(2)?, data: text(3)? },
            Some("read") => Request::Read { id: num(1)?, lines: num(2)? },
            Some("info") => Request::Info { id: num(1)? },
            Some("focus") => Request::Focus { id: num(1)? },
            Some("close") => Request::Close { id: num(1)? },
            Some("state") => Request::State {
                id: num(1)?,
                state: match text(2)?.as_str() {
                    "none" => None,
                    s => Some(AgentState::parse(s).ok_or_else(bad)?),
                },
                agent: text(3)?,
                message: text(4)?,
            },
            _ => return Err(bad()),
        })
    }
}

// ---------------------------------------------------------------- helpers for both hosts

pub fn no_pane(id: usize) -> String {
    format!("no pane {id}")
}

/// One line of `lterm pane list`.
pub fn list_line(id: usize, tab: Option<usize>, term: &Term, focused: bool, agent: Option<&Agent>) -> String {
    let tab = tab.map_or("-".to_string(), |t| (t + 1).to_string());
    let mark = if focused { "*" } else { "-" };
    let agent = agent.map_or("-".to_string(), |a| format!("{}:{}", a.name, a.state.name()));
    format!("{id}\t{tab}\t{}x{}\t{mark}\t{agent}\t{}\n", term.cols, term.rows, term.current_title())
}

pub const LIST_HEADER: &str = "pane\ttab\tsize\tfocus\tagent\ttitle\n";

/// The last `lines` lines of a pane's text (0: a screenful), ending at the last
/// non-blank line so an idle prompt halfway down the screen isn't followed by blanks.
pub fn read_text(term: &Term, lines: usize) -> String {
    let cursor = term.abs_row(term.cursor_pos().1);
    let mut end = term.last_abs();
    while end > cursor && term.line_chars(end).iter().all(|(c, _)| *c == ' ') {
        end -= 1;
    }
    let n = if lines == 0 { term.rows } else { lines } as i64;
    let start = (end - n + 1).max(term.first_abs());
    let mut text = term.selection_text(&Selection { anchor: (start, 0), head: (end, term.cols - 1) });
    text.push('\n');
    text
}

pub fn info_text(term: &Term, last_output: Instant, agent: Option<&Agent>) -> String {
    let mut s = format!("idle_ms {}\nsize {}x{}\ntitle {}\n", last_output.elapsed().as_millis(), term.cols, term.rows, term.current_title());
    match agent {
        Some(a) => s.push_str(&format!("agent {}\nstate {}\nmessage {}\n", a.name, a.state.name(), a.message)),
        None => s.push_str("agent -\nstate -\n"),
    }
    s
}

/// Bytes to write for a Send request to a pane showing `term`.
pub fn input_bytes(term: &Term, paste: bool, data: &str) -> Vec<u8> {
    if !paste {
        return data.as_bytes().to_vec();
    }
    let text = data.replace("\r\n", "\r").replace('\n', "\r");
    if term.modes.bracketed_paste {
        // Strip ESC so the text can't end the paste early and inject keys.
        let clean: String = text.chars().filter(|&c| c != '\x1b').collect();
        format!("\x1b[200~{clean}\x1b[201~").into_bytes()
    } else {
        text.into_bytes()
    }
}

// ---------------------------------------------------------------- serving

/// Where a window or session server takes requests, and the secret they must carry.
#[derive(Clone)]
pub struct Endpoint {
    pub addr: String,
    pub token: String,
}

impl Endpoint {
    /// Environment for the program in pane `id`.
    pub fn env(&self, id: usize) -> Vec<(String, String)> {
        vec![
            ("LTERM_SOCKET".into(), self.addr.clone()),
            ("LTERM_TOKEN".into(), self.token.clone()),
            ("LTERM_PANE".into(), id.to_string()),
        ]
    }
}

/// 128 unpredictable bits, from the OS-seeded keys of std's hasher.
fn new_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    (0..2u64)
        .map(|i| {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u64(i);
            h.write_u128(nanos);
            h.write_u32(std::process::id());
            format!("{:016x}", h.finish())
        })
        .collect()
}

/// Read one request, hand it to `post` with a channel for the answer, write the answer.
fn handle(mut s: impl Read + Write, token: &str, post: &dyn Fn(Request, mpsc::Sender<Reply>)) {
    let request = {
        let mut r = BufReader::new(&mut s);
        read_request(&mut r, token)
    };
    let reply = request.and_then(|req| {
        let (tx, rx) = mpsc::channel();
        post(req, tx);
        rx.recv_timeout(Duration::from_secs(30)).unwrap_or_else(|_| Err("no answer".into()))
    });
    let out = match reply {
        Ok(text) => format!("ok\n{text}"),
        Err(e) => format!("error\n{e}"),
    };
    let _ = s.write_all(out.as_bytes()).and_then(|_| s.flush());
}

fn read_request(r: &mut impl BufRead, token: &str) -> Result<Request, String> {
    let mut line = String::new();
    let mut header = |line: &mut String| {
        line.clear();
        r.take(256).read_line(line).map_err(|e| e.to_string())?;
        Ok::<_, String>(line.trim_end().to_string())
    };
    if header(&mut line)? != format!("{MAGIC} {token}") {
        return Err("bad token".into());
    }
    let len: usize = header(&mut line)?.parse().map_err(|_| "bad length")?;
    if len > MAX_REQUEST {
        return Err("request too large".into());
    }
    let mut buf = vec![0; len];
    r.read_exact(&mut buf).map_err(|e| e.to_string())?;
    let words: Vec<String> = String::from_utf8_lossy(&buf).split('\0').map(String::from).collect();
    Request::decode(&words)
}

/// Serve requests on a loopback TCP port (windows: works the same on every platform).
pub fn serve_tcp(post: impl Fn(Request, mpsc::Sender<Reply>) + Send + Clone + 'static) -> io::Result<Endpoint> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let ep = Endpoint { addr: listener.local_addr()?.to_string(), token: new_token() };
    let token = ep.token.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (token, post) = (token.clone(), post.clone());
            thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                handle(stream, &token, &post);
            });
        }
    });
    Ok(ep)
}

/// Serve requests on a Unix socket (session servers).
#[cfg(unix)]
pub fn serve_unix(
    listener: std::os::unix::net::UnixListener,
    addr: String,
    post: impl Fn(Request, mpsc::Sender<Reply>) + Send + Clone + 'static,
) -> Endpoint {
    let ep = Endpoint { addr, token: new_token() };
    let token = ep.token.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (token, post) = (token.clone(), post.clone());
            thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                handle(stream, &token, &post);
            });
        }
    });
    ep
}

// ---------------------------------------------------------------- client: `lterm pane ...`

pub const USAGE: &str = "\
lterm pane - control the lterm window (or session) this shell runs in

USAGE:
    lterm pane list                          Panes: id, tab, size, focus (*), title
    lterm pane split [OPTIONS] [-- CMD...]   Split a pane; prints the new pane's id
        -d, --down        Stack below instead of side by side
        -t, --pane ID     Pane to split [default: this one]
        -f, --focus       Move the focus to the new pane
        -r, --run TEXT    Type TEXT and Enter in the new shell (it stays open after)
        -- CMD...         Run CMD instead of a shell (the pane closes when it exits)
    lterm pane tab [-f] [-r TEXT] [-- CMD...]   Open a tab; prints the new pane's id
    lterm pane send ID TEXT...               Type TEXT into a pane, then Enter
        -n, --no-enter    Don't press Enter after the text
    lterm pane keys ID KEY...                Press keys: enter esc tab space backspace up
                                             down left right home end pgup pgdn delete
                                             shift-tab ctrl-a..ctrl-z, or one character
    lterm pane read ID [-l N]                Print a pane's last N lines [default: a screen]
    lterm pane info ID                       Agent, state, message, size, ms since output
    lterm pane wait ID [CONDITION] [--timeout SECS]
        --idle SECS       No output for SECS seconds [default: 3]
        --text TEXT       TEXT appears on the screen
        --exit            The pane has closed
        --state LIST      The pane's agent reports one of these states (e.g. done,blocked)
    lterm pane focus ID                      Focus a pane (switching tabs if needed)
    lterm pane close ID                      Close a pane and end its program
    lterm pane state STATE [OPTIONS]         Report what the agent in this pane is doing:
                                             working, blocked, done, idle, or none (gone).
        -a, --agent NAME  Agent name shown on the pane (e.g. claude)
        -m, --message T   What it needs (shown on the pane)
        -t, --pane ID     Another pane [default: this one]
                          `lterm integration install claude` sets this up for Claude Code.

    Works from any program started inside lterm (it uses $LTERM_SOCKET, $LTERM_TOKEN,
    $LTERM_PANE). Exit status: 0 on success, 1 on errors, 2 if `wait` timed out.
";

pub fn call(req: &Request) -> Reply {
    let addr = std::env::var("LTERM_SOCKET").map_err(|_| "not running inside lterm (LTERM_SOCKET isn't set)".to_string())?;
    let token = std::env::var("LTERM_TOKEN").unwrap_or_default();
    let payload = req.encode().join("\0");
    let msg = format!("{MAGIC} {token}\n{}\n{payload}", payload.len());
    let fail = |e: io::Error| format!("cannot reach lterm at {addr}: {e}");
    let mut answer = Vec::new();
    #[cfg(unix)]
    if addr.starts_with('/') {
        let mut s = std::os::unix::net::UnixStream::connect(&addr).map_err(fail)?;
        s.write_all(msg.as_bytes()).and_then(|_| s.read_to_end(&mut answer)).map_err(fail)?;
        return parse_reply(&answer);
    }
    let mut s = std::net::TcpStream::connect(&addr).map_err(fail)?;
    s.write_all(msg.as_bytes()).and_then(|_| s.read_to_end(&mut answer)).map_err(fail)?;
    parse_reply(&answer)
}

fn parse_reply(answer: &[u8]) -> Reply {
    let text = String::from_utf8_lossy(answer);
    match text.split_once('\n') {
        Some(("ok", body)) => Ok(body.to_string()),
        Some(("error", msg)) => Err(msg.to_string()),
        _ => Err("lterm closed the connection".into()),
    }
}

fn key_bytes(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    let seq = match lower.as_str() {
        "enter" | "return" => "\r",
        "esc" | "escape" => "\x1b",
        "tab" => "\t",
        "shift-tab" | "backtab" => "\x1b[Z",
        "space" => " ",
        "backspace" | "bs" => "\x7f",
        "up" => "\x1b[A",
        "down" => "\x1b[B",
        "right" => "\x1b[C",
        "left" => "\x1b[D",
        "home" => "\x1b[H",
        "end" => "\x1b[F",
        "pgup" | "pageup" => "\x1b[5~",
        "pgdn" | "pagedown" => "\x1b[6~",
        "delete" | "del" => "\x1b[3~",
        _ => {
            if let Some(c) = lower.strip_prefix("ctrl-").filter(|c| c.len() == 1).and_then(|c| c.chars().next()) {
                if c.is_ascii_lowercase() {
                    return Some(((c as u8 & 0x1f) as char).to_string());
                }
            }
            let mut chars = name.chars();
            return match (chars.next(), chars.next()) {
                (Some(c), None) => Some(c.to_string()),
                _ => None,
            };
        }
    };
    Some(seq.to_string())
}

/// `lterm pane ARGS...`
pub fn main(args: &[String]) -> ExitCode {
    match run(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("lterm pane: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<ExitCode, String> {
    let mut it = args.iter().cloned();
    let sub = it.next().unwrap_or_default();
    let mut rest: Vec<String> = it.collect();
    let print = |text: &str| {
        let mut out = io::stdout().lock();
        let _ = out.write_all(text.as_bytes()).and_then(|_| out.flush());
    };
    let pane_id = |s: Option<&String>| -> Result<usize, String> {
        let s = s.ok_or("missing pane id")?;
        s.parse().map_err(|_| format!("invalid pane id '{s}'"))
    };
    let own_pane = || std::env::var("LTERM_PANE").ok().and_then(|s| s.parse().ok());
    match sub.as_str() {
        "" | "-h" | "--help" | "help" => {
            print(USAGE);
            Ok(ExitCode::SUCCESS)
        }
        "list" | "ls" => {
            print(&format!("{LIST_HEADER}{}", call(&Request::List)?));
            Ok(ExitCode::SUCCESS)
        }
        "split" | "tab" => {
            let (mut dir, mut target, mut focus, mut run, mut command) = (Dir::Right, own_pane(), false, String::new(), vec![]);
            let mut it = rest.into_iter();
            while let Some(a) = it.next() {
                let mut value = |what: &str| it.next().ok_or(format!("{what} needs a value"));
                match a.as_str() {
                    "-d" | "--down" if sub == "split" => dir = Dir::Down,
                    "--right" if sub == "split" => dir = Dir::Right,
                    "-t" | "--pane" if sub == "split" => target = Some(pane_id(Some(&value(&a)?))?),
                    "-f" | "--focus" => focus = true,
                    "-r" | "--run" => run = value(&a)?,
                    "--" | "-e" => {
                        command = it.by_ref().collect();
                        break;
                    }
                    _ => return Err(format!("unknown option '{a}' (see lterm pane --help)")),
                }
            }
            let req = if sub == "split" {
                Request::Split { target, dir, focus, run, command }
            } else {
                Request::Tab { focus, run, command }
            };
            print(&call(&req)?);
            Ok(ExitCode::SUCCESS)
        }
        "send" => {
            let enter = !rest.iter().any(|a| a == "-n" || a == "--no-enter");
            rest.retain(|a| a != "-n" && a != "--no-enter");
            let id = pane_id(rest.first())?;
            let text = rest.get(1..).unwrap_or(&[]).join(" ");
            if !text.is_empty() {
                call(&Request::Send { id, paste: true, data: text })?;
            }
            if enter {
                // A separate keypress: full-screen programs treat Enter inside the same
                // chunk of input as part of a paste rather than as "submit".
                thread::sleep(Duration::from_millis(150));
                call(&Request::Send { id, paste: false, data: "\r".into() })?;
            }
            Ok(ExitCode::SUCCESS)
        }
        "keys" | "key" => {
            let id = pane_id(rest.first())?;
            let keys = rest.get(1..).unwrap_or(&[]);
            if keys.is_empty() {
                return Err("no keys given".into());
            }
            for (i, k) in keys.iter().enumerate() {
                let data = key_bytes(k).ok_or(format!("unknown key '{k}'"))?;
                if i > 0 {
                    thread::sleep(Duration::from_millis(50));
                }
                call(&Request::Send { id, paste: false, data })?;
            }
            Ok(ExitCode::SUCCESS)
        }
        "read" | "cat" => {
            let mut lines = 0;
            let mut id = None;
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "-l" | "--lines" => {
                        let v = it.next().ok_or("--lines needs a value")?;
                        lines = v.parse().map_err(|_| format!("invalid line count '{v}'"))?;
                    }
                    _ if id.is_none() => id = Some(pane_id(Some(a))?),
                    _ => return Err(format!("unexpected '{a}'")),
                }
            }
            let id = id.ok_or("missing pane id")?;
            print(&call(&Request::Read { id, lines })?);
            Ok(ExitCode::SUCCESS)
        }
        "wait" => wait(&rest),
        "info" => {
            print(&call(&Request::Info { id: pane_id(rest.first())? })?);
            Ok(ExitCode::SUCCESS)
        }
        "focus" => {
            call(&Request::Focus { id: pane_id(rest.first())? })?;
            Ok(ExitCode::SUCCESS)
        }
        "close" | "kill" => {
            call(&Request::Close { id: pane_id(rest.first())? })?;
            Ok(ExitCode::SUCCESS)
        }
        "state" => {
            let (mut state, mut agent, mut message, mut id) = (None, String::new(), String::new(), own_pane());
            let mut it = rest.into_iter();
            while let Some(a) = it.next() {
                let mut value = |what: &str| it.next().ok_or(format!("{what} needs a value"));
                match a.as_str() {
                    "-a" | "--agent" => agent = value(&a)?,
                    "-m" | "--message" => message = value(&a)?,
                    "-t" | "--pane" => id = Some(pane_id(Some(&value(&a)?))?),
                    // Codex's `notify` appends a JSON description of the event.
                    _ if a.starts_with('{') => {}
                    _ if state.is_none() => state = Some(a),
                    _ => return Err(format!("unexpected '{a}'")),
                }
            }
            let state = state.ok_or("missing state (working, blocked, done, idle or none)")?;
            let state = match state.as_str() {
                "none" => None,
                s => Some(AgentState::parse(s).ok_or(format!("unknown state '{s}' (working, blocked, done, idle or none)"))?),
            };
            // Agent hooks run this everywhere; outside lterm there is nothing to tell.
            if std::env::var_os("LTERM_SOCKET").is_none() {
                return Ok(ExitCode::SUCCESS);
            }
            let id = id.ok_or("no pane: use --pane ID")?;
            call(&Request::State { id, state, agent, message })?;
            Ok(ExitCode::SUCCESS)
        }
        _ => Err(format!("unknown command '{sub}' (see lterm pane --help)")),
    }
}

enum Until {
    Idle(Duration),
    Text(String),
    Exit,
    State(Vec<AgentState>),
}

fn wait(args: &[String]) -> Result<ExitCode, String> {
    let mut id = None;
    let mut until = Until::Idle(Duration::from_secs(3));
    let mut timeout: Option<Duration> = None;
    let secs = |v: Option<&String>, what: &str| -> Result<Duration, String> {
        let v = v.ok_or(format!("{what} needs a value"))?;
        v.parse::<f64>().ok().filter(|s| *s >= 0.0).map(Duration::from_secs_f64).ok_or(format!("invalid {what} '{v}'"))
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--idle" => until = Until::Idle(secs(it.next(), a)?),
            "--text" => until = Until::Text(it.next().ok_or("--text needs a value")?.clone()),
            "--exit" => until = Until::Exit,
            "--state" => {
                let v = it.next().ok_or("--state needs a value")?;
                let states = v
                    .split(',')
                    .map(|s| AgentState::parse(s.trim()).ok_or(format!("unknown state '{s}'")))
                    .collect::<Result<Vec<_>, _>>()?;
                until = Until::State(states);
            }
            "--timeout" => timeout = Some(secs(it.next(), a)?),
            _ if id.is_none() => id = Some(a.parse::<usize>().map_err(|_| format!("invalid pane id '{a}'"))?),
            _ => return Err(format!("unexpected '{a}'")),
        }
    }
    let id = id.ok_or("missing pane id")?;
    let deadline = timeout.map(|t| Instant::now() + t);
    loop {
        let info = call(&Request::Info { id });
        let gone = matches!(&info, Err(e) if *e == no_pane(id));
        let done = match &until {
            Until::Exit => gone,
            _ if gone => return Err(format!("pane {id} closed")),
            Until::Idle(d) => {
                let info = info?;
                let ms = info.lines().find_map(|l| l.strip_prefix("idle_ms ")).and_then(|v| v.parse::<u64>().ok());
                ms.is_some_and(|ms| ms >= d.as_millis() as u64)
            }
            Until::Text(t) => {
                info?;
                call(&Request::Read { id, lines: 0 })?.contains(t.as_str())
            }
            Until::State(states) => {
                let info = info?;
                let state = info.lines().find_map(|l| l.strip_prefix("state ")).and_then(AgentState::parse);
                state.is_some_and(|s| states.contains(&s))
            }
        };
        if done {
            return Ok(ExitCode::SUCCESS);
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            eprintln!("lterm pane: timed out waiting for pane {id}");
            return Ok(ExitCode::from(2));
        }
        thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip() {
        let reqs = vec![
            Request::List,
            Request::Split { target: Some(3), dir: Dir::Down, focus: true, run: "claude".into(), command: vec![] },
            Request::Split { target: None, dir: Dir::Right, focus: false, run: String::new(), command: vec!["htop".into(), "-d".into()] },
            Request::Tab { focus: false, run: "make test".into(), command: vec![] },
            Request::Send { id: 2, paste: true, data: "line one\nline two".into() },
            Request::Read { id: 0, lines: 50 },
            Request::Info { id: 7 },
            Request::Focus { id: 1 },
            Request::Close { id: 1 },
            Request::State { id: 3, state: Some(AgentState::Blocked), agent: "claude".into(), message: "needs you".into() },
            Request::State { id: 3, state: None, agent: String::new(), message: String::new() },
        ];
        for r in reqs {
            assert_eq!(Request::decode(&r.encode()).unwrap(), r);
        }
        assert!(Request::decode(&["read".into(), "x".into()]).is_err());
    }

    #[test]
    fn agent_states() {
        for s in [None, Some(AgentState::Working), Some(AgentState::Blocked), Some(AgentState::Done), Some(AgentState::Idle)] {
            assert_eq!(AgentState::from_code(AgentState::code(s)), s);
        }
        let mut a = None;
        // A notification from a plain program marks it as wanting attention.
        assert!(Agent::notified(&mut a, "build finished"));
        assert_eq!(a.as_ref().map(|a| (a.name.as_str(), a.state)), Some(("agent", AgentState::Blocked)));
        // Hooks name it; a finished agent isn't "blocked" by its idle reminder.
        assert!(Agent::report(&mut a, Some(AgentState::Done), "claude", ""));
        assert!(!Agent::notified(&mut a, "Claude is waiting for your input"));
        assert!(!Agent::report(&mut a, Some(AgentState::Done), "", ""));
        // Enter starts a turn only for agents that don't say so themselves.
        assert!(Agent::typed(&mut a, b"next task\r"));
        assert!(Agent::report(&mut a, Some(AgentState::Working), "", ""));
        Agent::report(&mut a, Some(AgentState::Done), "", "");
        assert!(!Agent::typed(&mut a, b"\r"));
        assert_eq!(a.as_ref().unwrap().name, "claude");
        assert!(Agent::report(&mut a, None, "", ""));
        assert!(a.is_none() && !Agent::typed(&mut a, b"\r"));
    }

    #[test]
    fn notifications() {
        let mut t = Term::new(20, 5, 100);
        let mut p = vte::Parser::new();
        t.feed(&mut p, b"\x1b]9;4;3;50\x07");
        assert_eq!(t.notification.take(), None); // a progress bar
        t.feed(&mut p, b"\x1b]9;Claude needs your permission\x07");
        assert_eq!(t.notification.take().as_deref(), Some("Claude needs your permission"));
        t.feed(&mut p, b"\x1b]777;notify;Codex;Turn complete\x1b\\");
        assert_eq!(t.notification.take().as_deref(), Some("Turn complete"));
        t.feed(&mut p, b"\x1b]99;i=1:d=0;Done\x1b\\");
        assert_eq!(t.notification.take().as_deref(), Some("Done"));
    }

    #[test]
    fn wire_format_and_token() {
        let req = Request::Send { id: 4, paste: false, data: "\x1b[A".into() };
        let payload = req.encode().join("\0");
        let msg = format!("{MAGIC} secret\n{}\n{payload}", payload.len());
        assert_eq!(read_request(&mut msg.as_bytes(), "secret").unwrap(), req);
        assert!(read_request(&mut msg.as_bytes(), "other").is_err());
        assert_eq!(parse_reply(b"ok\n3\n"), Ok("3\n".into()));
        assert_eq!(parse_reply(b"error\nno pane 9"), Err("no pane 9".into()));
    }

    #[test]
    fn keys_and_text() {
        assert_eq!(key_bytes("ctrl-c").as_deref(), Some("\x03"));
        assert_eq!(key_bytes("Up").as_deref(), Some("\x1b[A"));
        assert_eq!(key_bytes("y").as_deref(), Some("y"));
        assert_eq!(key_bytes("bogus"), None);

        let mut t = Term::new(20, 5, 100);
        let mut p = vte::Parser::new();
        t.feed(&mut p, b"one\r\ntwo\r\nthree\r\n$ ");
        assert_eq!(read_text(&t, 0), "one\ntwo\nthree\n$\n");
        assert_eq!(read_text(&t, 2), "three\n$\n");
        assert_eq!(input_bytes(&t, true, "a\nb"), b"a\rb");
        t.feed(&mut p, b"\x1b[?2004h");
        assert_eq!(input_bytes(&t, true, "a\x1b"), b"\x1b[200~a\x1b[201~");
        assert_eq!(input_bytes(&t, false, "\x1b[A"), b"\x1b[A");
    }
}
