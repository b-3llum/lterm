//! `lterm push HOST`: copy the Linux build of lterm to a server over your own SSH login
//! and install it there for that user (~/.local/bin), so `lterm --attach HOST` works.
//! It asks before copying, sends the file over one ssh connection, and prints what it did.

use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};

use crate::session;

const USAGE: &str = "\
lterm push - install lterm on a server, over your SSH login

USAGE:
    lterm push [OPTIONS] HOST        HOST is user@host or a Host from ~/.ssh/config

OPTIONS:
    -y, --yes          Don't ask for confirmation
        --binary FILE  Linux x86_64 lterm to send [default: this program on Linux,
                       or the Linux build next to lterm.exe on Windows]

The file goes to a temporary file on the server, which runs `lterm install` (your
home directory only, no root) and then deletes the temporary file. Set LTERM_SSH to
change the ssh command, e.g. LTERM_SSH=\"ssh -p 2222\".
";

/// Runs on the server: check the platform, receive the file on stdin, install, clean up.
const REMOTE: &str = "sh -c 'case \"$(uname -sm)\" in \"Linux x86_64\") ;; \
*) echo \"lterm push: this server is $(uname -sm); only Linux x86_64 is supported\" >&2; exit 3;; esac; \
f=$(mktemp \"${TMPDIR:-/tmp}/lterm.XXXXXX\") || exit 4; \
cat > \"$f\" && chmod 700 \"$f\" && \"$f\" install; rc=$?; rm -f \"$f\"; exit $rc'";

fn is_linux_x86_64(data: &[u8]) -> bool {
    data.len() > 20 && &data[..4] == b"\x7fELF" && u16::from_le_bytes([data[18], data[19]]) == 0x3e
}

/// This program on Linux x86_64; otherwise a Linux build kept next to it.
pub fn find_linux_build() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return Ok(exe);
    }
    let dir = exe.parent().unwrap_or(Path::new("."));
    for name in ["lterm-linux-x86_64", "lterm"] {
        let p = dir.join(name);
        if fs::read(&p).is_ok_and(|d| is_linux_x86_64(&d)) {
            return Ok(p);
        }
    }
    Err(format!(
        "no Linux build of lterm next to {} (expected lterm-linux-x86_64); pass --binary FILE",
        exe.display()
    ))
}

pub fn main(args: &[String]) -> ExitCode {
    let mut host = None;
    let mut binary = None;
    let mut yes = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-y" | "--yes" => yes = true,
            "--binary" => binary = it.next().map(PathBuf::from),
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            s if s.starts_with('-') => {
                eprintln!("lterm push: unknown option '{s}'\n\n{USAGE}");
                return ExitCode::from(2);
            }
            s => host = Some(s.to_string()),
        }
    }
    let Some(host) = host else {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    };
    match push(&host, binary, yes) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("lterm push: {e}");
            ExitCode::FAILURE
        }
    }
}

fn push(host: &str, binary: Option<PathBuf>, yes: bool) -> Result<bool, String> {
    let path = match binary {
        Some(p) => p,
        None => find_linux_build()?,
    };
    let data = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !is_linux_x86_64(&data) {
        return Err(format!("{} is not a Linux x86_64 build of lterm", path.display()));
    }
    println!("lterm push: {} ({:.1} MB) -> {host}:~/.local/bin/lterm", path.display(), data.len() as f64 / 1e6);
    if !yes {
        print!("Install lterm on {host} for that user? [y/N] ");
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        let _ = std::io::stdin().lock().read_line(&mut answer);
        // Ignore stray bytes around the answer (e.g. a BOM from a PowerShell pipe).
        let answer = answer.trim_matches(|c: char| !c.is_alphanumeric()).to_ascii_lowercase();
        if !matches!(answer.as_str(), "y" | "yes") {
            println!("Cancelled.");
            return Ok(false);
        }
    }
    let mut child = session::ssh_command()
        .args([host, REMOTE])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run ssh: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        // The server may stop reading early (unsupported platform); its exit status says why.
        let _ = stdin.write_all(&data);
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if status.success() {
        println!("\nlterm is installed on {host}. Connect with:  lterm --attach {host}");
        Ok(true)
    } else {
        eprintln!("\nlterm push: installing on {host} failed ({status}).");
        eprintln!("If the error mentions GLIBC, the server's Linux is older than this build supports.");
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_linux_builds() {
        let mut elf = vec![0u8; 64];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[18] = 0x3e;
        assert!(is_linux_x86_64(&elf));
        elf[18] = 0xb7; // aarch64
        assert!(!is_linux_x86_64(&elf));
        assert!(!is_linux_x86_64(b"MZ\x90\x00 not elf at all......"));
    }
}
