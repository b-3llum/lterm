//! Child process on a pseudo-terminal (ConPTY on Windows, a pty elsewhere).

use std::io::{Read, Write};
use std::thread;

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};

pub enum PtyMsg {
    Data(Vec<u8>),
    Exit,
}

pub struct Pty {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

fn size(cols: usize, rows: usize, px: (usize, usize)) -> PtySize {
    PtySize {
        cols: cols as u16,
        rows: rows as u16,
        pixel_width: (cols * px.0) as u16,
        pixel_height: (rows * px.1) as u16,
    }
}

#[cfg(windows)]
fn default_command() -> CommandBuilder {
    let has = |exe: &str| {
        std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(exe).is_file()))
    };
    let mut cmd = CommandBuilder::new(if has("pwsh.exe") { "pwsh.exe" } else { "powershell.exe" });
    cmd.arg("-NoLogo");
    cmd
}

#[cfg(not(windows))]
fn default_command() -> CommandBuilder {
    CommandBuilder::new_default_prog()
}

/// Start `command` (or the user's shell) and stream its output to `sink` from a
/// background thread. `sink` gets `PtyMsg::Exit` once the child has exited.
pub fn spawn(
    command: &[String],
    cols: usize,
    rows: usize,
    cell_px: (usize, usize),
    sink: impl Fn(PtyMsg) + Send + Clone + 'static,
) -> Result<Pty, String> {
    let pair = native_pty_system().openpty(size(cols, rows, cell_px)).map_err(|e| e.to_string())?;
    let mut cmd = match command.split_first() {
        Some((prog, args)) => {
            let mut c = CommandBuilder::new(prog);
            c.args(args);
            c
        }
        None => default_command(),
    };
    if let Ok(dir) = std::env::current_dir() {
        cmd.cwd(dir);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TERM_PROGRAM", "lterm");
    cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
    // Let `wsl.exe` forward these into Linux.
    let wslenv = std::env::var("WSLENV").unwrap_or_default();
    let sep = if wslenv.is_empty() { "" } else { ":" };
    cmd.env("WSLENV", format!("{wslenv}{sep}TERM_PROGRAM:TERM_PROGRAM_VERSION:COLORTERM"));

    let mut child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
    drop(pair.slave);
    let killer = child.clone_killer();
    let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
    let writer = pair.master.take_writer().map_err(|e| e.to_string())?;

    let out = sink.clone();
    thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => out(PtyMsg::Data(buf[..n].to_vec())),
            }
        }
    });
    // ConPTY doesn't close the output pipe when the child exits, so watch the child itself.
    thread::spawn(move || {
        let _ = child.wait();
        sink(PtyMsg::Exit);
    });
    Ok(Pty { master: pair.master, writer, killer })
}

impl Pty {
    pub fn write(&mut self, data: &[u8]) {
        if !data.is_empty() {
            let _ = self.writer.write_all(data);
            let _ = self.writer.flush();
        }
    }

    pub fn resize(&self, cols: usize, rows: usize, cell_px: (usize, usize)) {
        let _ = self.master.resize(size(cols, rows, cell_px));
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.killer.kill();
    }
}
