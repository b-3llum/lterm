//! `lterm install` / `lterm uninstall`: per-user setup, no admin rights needed.
//! Linux/macOS: ~/.local/bin/lterm (+ mdterm link), app-menu entry and icon.
//! Windows: %LOCALAPPDATA%\Programs\lterm (+ ConPTY files), user PATH, Start Menu shortcut.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn same_file(a: &Path, b: &Path) -> bool {
    matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(x), Ok(y)) if x == y)
}

fn copy_exe(from: &Path, to: &Path) -> Result<(), String> {
    if same_file(from, to) {
        return Ok(());
    }
    // Replace via a temp file so a running copy doesn't block the update (on Unix).
    let tmp = to.with_extension("new");
    fs::copy(from, &tmp).map_err(|e| format!("copy to {}: {e}", tmp.display()))?;
    fs::rename(&tmp, to).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("replace {}: {e} (close lterm and try again)", to.display())
    })
}

#[cfg(not(windows))]
fn write_png(path: &Path, size: u32) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    crate::write_rgba_png(path, &crate::logo::render(size), size as usize, size as usize)
}

pub fn main(uninstall: bool) -> ExitCode {
    let result = if uninstall { remove() } else { install() };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lterm: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME").map(PathBuf::from).ok_or("HOME is not set".into())
}

#[cfg(not(windows))]
fn install() -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let home = home()?;
    let bin = home.join(".local/bin");
    fs::create_dir_all(&bin).map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let target = bin.join("lterm");
    copy_exe(&exe, &target)?;
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    let link = bin.join("mdterm");
    let _ = fs::remove_file(&link);
    std::os::unix::fs::symlink("lterm", &link).map_err(|e| format!("{}: {e}", link.display()))?;
    println!("installed {}", target.display());
    println!("installed {} -> lterm", link.display());

    if cfg!(target_os = "linux") {
        let share = home.join(".local/share");
        for size in [48, 256] {
            write_png(&share.join(format!("icons/hicolor/{size}x{size}/apps/lterm.png")), size)?;
        }
        let apps = share.join("applications");
        fs::create_dir_all(&apps).map_err(|e| e.to_string())?;
        let desktop = format!(
            "[Desktop Entry]\nType=Application\nName=lterm\nGenericName=Terminal\n\
             Comment=Lightweight terminal with splits, tabs, sessions and images\n\
             Exec={}\nIcon=lterm\nTerminal=false\nCategories=System;TerminalEmulator;\n\
             StartupWMClass=lterm\nKeywords=shell;terminal;console;\n",
            target.display()
        );
        fs::write(apps.join("lterm.desktop"), desktop).map_err(|e| e.to_string())?;
        println!("added lterm to the application menu");
    }
    let on_path = std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d == bin));
    if !on_path {
        println!("\n{} is not on your PATH. Add this to ~/.bashrc or ~/.zshrc:", bin.display());
        println!("    export PATH=\"$HOME/.local/bin:$PATH\"");
    }
    println!("\nDone. Run `lterm`, or `lterm keys` for the shortcuts.");
    Ok(())
}

#[cfg(not(windows))]
fn remove() -> Result<(), String> {
    let home = home()?;
    let files = [
        ".local/bin/mdterm",
        ".local/bin/lterm",
        ".local/share/applications/lterm.desktop",
        ".local/share/icons/hicolor/48x48/apps/lterm.png",
        ".local/share/icons/hicolor/256x256/apps/lterm.png",
    ];
    for f in files {
        let p = home.join(f);
        if fs::symlink_metadata(&p).is_ok() {
            fs::remove_file(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            println!("removed {}", p.display());
        }
    }
    println!("Sessions that are still running keep running; end them with `lterm kill NAME` first.");
    Ok(())
}

/// Run a PowerShell snippet with arguments passed as data, not spliced into code.
#[cfg(windows)]
fn powershell(script: &str, args: &[&str]) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // Arguments go through an environment variable list, so paths with quotes are safe.
    let joined = args.join("\u{1f}");
    let full = format!("$a = $env:LTERM_ARGS -split [char]0x1f; {script}");
    let status = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &full])
        .env("LTERM_ARGS", joined)
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|e| format!("cannot run PowerShell: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("PowerShell step failed ({status})")) }
}

#[cfg(windows)]
fn dirs() -> Result<(PathBuf, PathBuf), String> {
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).ok_or("LOCALAPPDATA is not set")?;
    let roaming = std::env::var_os("APPDATA").map(PathBuf::from).ok_or("APPDATA is not set")?;
    Ok((local.join("Programs").join("lterm"), roaming.join("Microsoft\\Windows\\Start Menu\\Programs\\lterm.lnk")))
}

#[cfg(windows)]
const ADD_PATH: &str = "$d = $a[0]; $p = [Environment]::GetEnvironmentVariable('Path', 'User'); \
    $parts = @($p -split ';' | Where-Object { $_ }); \
    if ($parts -notcontains $d) { [Environment]::SetEnvironmentVariable('Path', (($parts + $d) -join ';'), 'User') }";

#[cfg(windows)]
const REMOVE_PATH: &str = "$d = $a[0]; $p = [Environment]::GetEnvironmentVariable('Path', 'User'); \
    $parts = @($p -split ';' | Where-Object { $_ -and $_ -ne $d }); \
    [Environment]::SetEnvironmentVariable('Path', ($parts -join ';'), 'User')";

#[cfg(windows)]
const SHORTCUT: &str = "$s = (New-Object -ComObject WScript.Shell).CreateShortcut($a[0]); \
    $s.TargetPath = $a[1]; $s.WorkingDirectory = $env:USERPROFILE; \
    $s.Description = 'lterm terminal'; $s.Save()";

#[cfg(windows)]
fn install() -> Result<(), String> {
    let (dir, lnk) = dirs()?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let target = dir.join("lterm.exe");
    copy_exe(&exe, &target)?;
    println!("installed {}", target.display());
    let src = exe.parent().unwrap_or(Path::new("."));
    let mut have_conpty = true;
    for f in ["conpty.dll", "OpenConsole.exe", "THIRD_PARTY.txt"] {
        let from = src.join(f);
        if from.is_file() {
            if !same_file(&from, &dir.join(f)) {
                fs::copy(&from, dir.join(f)).map_err(|e| format!("copy {f}: {e}"))?;
            }
        } else if f != "THIRD_PARTY.txt" && !dir.join(f).is_file() {
            have_conpty = false;
        }
    }
    // Keep the Linux build alongside, for `lterm push` to servers.
    if let Ok(linux) = crate::push::find_linux_build() {
        let dest = dir.join("lterm-linux-x86_64");
        if !same_file(&linux, &dest) {
            fs::copy(&linux, &dest).map_err(|e| format!("copy Linux build: {e}"))?;
        }
        println!("included the Linux build, for `lterm push user@server`");
    }
    let alias = dir.join("mdterm.exe");
    let _ = fs::remove_file(&alias);
    if fs::hard_link(&target, &alias).is_err() {
        fs::copy(&target, &alias).map_err(|e| e.to_string())?;
    }
    println!("installed {} (same program)", alias.display());
    powershell(ADD_PATH, &[&dir.to_string_lossy()])?;
    println!("added {} to your PATH (open a new terminal to use it)", dir.display());
    powershell(SHORTCUT, &[&lnk.to_string_lossy(), &target.to_string_lossy()])?;
    println!("added lterm to the Start menu");
    if !have_conpty {
        println!("\nNote: conpty.dll and OpenConsole.exe weren't next to lterm.exe, so images won't");
        println!("show inside lterm. Keep them in the same folder as lterm.exe and run install again.");
    }
    println!("\nDone. Start lterm from the Start menu or by typing `lterm`; `lterm keys` lists the shortcuts.");
    Ok(())
}

#[cfg(windows)]
fn remove() -> Result<(), String> {
    let (dir, lnk) = dirs()?;
    powershell(REMOVE_PATH, &[&dir.to_string_lossy()])?;
    println!("removed {} from your PATH", dir.display());
    if lnk.is_file() {
        fs::remove_file(&lnk).map_err(|e| e.to_string())?;
        println!("removed the Start menu shortcut");
    }
    let running_from_dir = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| same_file(p, &dir))).unwrap_or(false);
    for f in ["mdterm.exe", "conpty.dll", "OpenConsole.exe", "THIRD_PARTY.txt", "lterm-linux-x86_64", "lterm.exe"] {
        let p = dir.join(f);
        if p.is_file() && fs::remove_file(&p).is_ok() {
            println!("removed {}", p.display());
        }
    }
    if fs::remove_dir(&dir).is_err() && dir.exists() {
        if running_from_dir {
            println!("{} is in use by this lterm; delete the folder after it exits.", dir.display());
        } else {
            println!("Some files in {} are in use; close lterm and delete the folder.", dir.display());
        }
    }
    Ok(())
}
