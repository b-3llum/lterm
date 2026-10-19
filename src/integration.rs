//! `lterm integration install|uninstall AGENT`: make a coding agent report what it's
//! doing (`lterm pane state ...`), so its pane shows working / blocked / done.
//!
//! Claude Code: hooks in ~/.claude/settings.json. Codex: `notify` in ~/.codex/config.toml
//! (Codex only reports finished turns; pressing Enter in its pane counts as "working").

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{json, Map, Value};

const USAGE: &str = "\
lterm integration - let coding agents show their state on lterm panes

USAGE:
    lterm integration                    Show what is installed
    lterm integration install AGENT      AGENT: claude (Claude Code) or codex
    lterm integration uninstall AGENT

    Claude Code gets hooks in ~/.claude/settings.json (a backup is kept next to it);
    Codex gets `notify` in ~/.codex/config.toml. Restart the agent afterwards.
    Outside lterm the hooks do nothing.
";

/// (Claude Code hook event, matcher, state it reports)
const CLAUDE_HOOKS: [(&str, Option<&str>, &str); 7] = [
    ("SessionStart", None, "idle"),
    ("UserPromptSubmit", None, "working"),
    // After a tool ran, including one you just approved.
    ("PostToolUse", None, "working"),
    ("Notification", Some("permission_prompt|elicitation_dialog"), "blocked"),
    ("Stop", None, "done"),
    ("StopFailure", None, "done"),
    ("SessionEnd", None, "none"),
];

pub fn main(args: &[String]) -> ExitCode {
    let result = match (args.first().map(String::as_str), args.get(1).map(String::as_str)) {
        (None, _) => {
            print!("{USAGE}\nINSTALLED:\n{}", status());
            Ok(String::new())
        }
        (Some("-h" | "--help" | "help"), _) => {
            print!("{USAGE}");
            Ok(String::new())
        }
        (Some("install"), Some("claude")) => install_claude(),
        (Some("uninstall"), Some("claude")) => uninstall_claude(),
        (Some("install"), Some("codex")) => install_codex(),
        (Some("uninstall"), Some("codex")) => uninstall_codex(),
        (Some("install" | "uninstall"), Some(other)) => Err(format!("unknown agent '{other}' (claude or codex)")),
        (Some("install" | "uninstall"), None) => Err("which agent? claude or codex".into()),
        (Some(other), _) => Err(format!("unknown command '{other}' (see lterm integration --help)")),
    };
    match result {
        Ok(msg) => {
            print!("{msg}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("lterm integration: {e}");
            ExitCode::FAILURE
        }
    }
}

fn home() -> Result<PathBuf, String> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .ok_or_else(|| "can't find your home directory".to_string())
}

fn claude_settings() -> Result<PathBuf, String> {
    let dir = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(d) => PathBuf::from(d),
        None => home()?.join(".claude"),
    };
    Ok(dir.join("settings.json"))
}

fn codex_config() -> Result<PathBuf, String> {
    let dir = match std::env::var_os("CODEX_HOME") {
        Some(d) => PathBuf::from(d),
        None => home()?.join(".codex"),
    };
    Ok(dir.join("config.toml"))
}

/// lterm on PATH (`lterm install` puts it there).
fn on_path() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "lterm.exe" } else { "lterm" };
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(exe)).find(|f| f.is_file())
}

/// lterm's full path: the one on PATH if there is one (it survives updates made with
/// `lterm install`), else this executable.
fn lterm_path() -> Result<String, String> {
    let path = match on_path() {
        Some(p) => p,
        None => std::env::current_exe().map_err(|e| e.to_string())?,
    };
    // Forward slashes work everywhere on Windows, and survive shells and TOML.
    Ok(path.to_string_lossy().replace('\\', "/"))
}

/// The hook command line. Claude Code runs it in sh, Git Bash or PowerShell, so it must
/// read the same in all of them: no quotes, so no spaces.
fn hook_command(state: &str) -> Result<String, String> {
    let exe = lterm_path()?;
    let lterm = if on_path().is_some() {
        "lterm".to_string()
    } else if exe.contains(' ') {
        return Err(format!("lterm is at a path with spaces ({exe}); run `lterm install` first so it's on PATH"));
    } else {
        exe
    };
    Ok(format!("{lterm} pane state {state} --agent claude"))
}

fn is_ours(hook: &Value) -> bool {
    hook.get("command").and_then(Value::as_str).is_some_and(|c| c.contains(" pane state ") && c.ends_with("--agent claude"))
}

/// Remove lterm's hooks, and any groups and events left empty. Returns how many it removed.
fn remove_claude_hooks(root: &mut Map<String, Value>) -> usize {
    let Some(Value::Object(hooks)) = root.get_mut("hooks") else { return 0 };
    let mut removed = 0;
    for groups in hooks.values_mut() {
        let Value::Array(groups) = groups else { continue };
        for group in groups.iter_mut() {
            if let Some(Value::Array(list)) = group.get_mut("hooks") {
                let before = list.len();
                list.retain(|h| !is_ours(h));
                removed += before - list.len();
            }
        }
        groups.retain(|g| g.get("hooks").and_then(Value::as_array).is_none_or(|l| !l.is_empty()));
    }
    hooks.retain(|_, v| v.as_array().is_none_or(|a| !a.is_empty()));
    if hooks.is_empty() {
        root.remove("hooks");
    }
    removed
}

fn add_claude_hooks(root: &mut Map<String, Value>) -> Result<(), String> {
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks.as_object_mut().ok_or("\"hooks\" in settings.json isn't an object")?;
    for (event, matcher, state) in CLAUDE_HOOKS {
        let mut group = Map::new();
        if let Some(m) = matcher {
            group.insert("matcher".into(), json!(m));
        }
        group.insert("hooks".into(), json!([{ "type": "command", "command": hook_command(state)?, "timeout": 10 }]));
        let list = hooks.entry(event).or_insert_with(|| json!([]));
        list.as_array_mut().ok_or(format!("\"hooks.{event}\" in settings.json isn't a list"))?.push(Value::Object(group));
    }
    Ok(())
}

fn read_json(path: &Path) -> Result<Map<String, Value>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str(&text) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(format!("{} isn't a JSON object; not touching it", path.display())),
        Err(e) => Err(format!("{} isn't valid JSON ({e}); not touching it", path.display())),
    }
}

/// Write `text` to `path`, keeping the first version lterm ever replaced as a backup.
fn write_file(path: &Path, text: &str) -> Result<(), String> {
    let fail = |e: std::io::Error| format!("{}: {e}", path.display());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(fail)?;
    }
    let backup = path.with_extension(format!("{}.lterm-backup", path.extension().and_then(|e| e.to_str()).unwrap_or("")));
    if path.is_file() && !backup.exists() {
        std::fs::copy(path, &backup).map_err(fail)?;
    }
    let tmp = path.with_extension("lterm-tmp");
    std::fs::write(&tmp, text).map_err(fail)?;
    std::fs::rename(&tmp, path).map_err(fail)
}

fn write_json(path: &Path, root: Map<String, Value>) -> Result<(), String> {
    let mut text = serde_json::to_string_pretty(&Value::Object(root)).map_err(|e| e.to_string())?;
    text.push('\n');
    write_file(path, &text)
}

fn install_claude() -> Result<String, String> {
    let path = claude_settings()?;
    let mut root = read_json(&path)?;
    remove_claude_hooks(&mut root);
    add_claude_hooks(&mut root)?;
    write_json(&path, root)?;
    Ok(format!(
        "Added lterm hooks to {}.\nRestart Claude Code; its panes now show working, blocked (needs you) and done.\n",
        path.display()
    ))
}

fn uninstall_claude() -> Result<String, String> {
    let path = claude_settings()?;
    let mut root = read_json(&path)?;
    if remove_claude_hooks(&mut root) == 0 {
        return Ok(format!("No lterm hooks in {}.\n", path.display()));
    }
    write_json(&path, root)?;
    Ok(format!("Removed lterm hooks from {}.\n", path.display()))
}

// ---------------------------------------------------------------- Codex (TOML, edited by line)

/// Lines before the first [table]: where top-level keys like `notify` live.
fn top_level(lines: &[String]) -> usize {
    lines.iter().position(|l| l.trim_start().starts_with('[')).unwrap_or(lines.len())
}

fn is_notify(line: &str) -> bool {
    line.trim_start().strip_prefix("notify").is_some_and(|r| r.trim_start().starts_with('='))
}

fn codex_line() -> Result<String, String> {
    let exe = lterm_path()?.replace('"', "\\\"");
    Ok(format!("notify = [\"{exe}\", \"pane\", \"state\", \"done\", \"--agent\", \"codex\"]  # added by lterm integration"))
}

fn install_codex() -> Result<String, String> {
    let path = codex_config()?;
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let top = top_level(&lines);
    let line = codex_line()?;
    match lines[..top].iter().position(|l| is_notify(l)) {
        Some(i) if lines[i].contains("added by lterm") => lines[i] = line,
        Some(i) => {
            return Err(format!(
                "{} already runs a notify command:\n    {}\nCodex allows one. Add `lterm pane state done --agent codex` to it instead.",
                path.display(),
                lines[i].trim()
            ))
        }
        None => lines.insert(0, line),
    }
    write_file(&path, &(lines.join("\n") + "\n"))?;
    Ok(format!("Added lterm's notify to {}.\nRestart Codex; its panes now show done when a turn ends.\n", path.display()))
}

fn uninstall_codex() -> Result<String, String> {
    let path = codex_config()?;
    let Ok(text) = std::fs::read_to_string(&path) else { return Ok(format!("No {}.\n", path.display())) };
    let lines: Vec<&str> = text.lines().collect();
    let kept: Vec<&str> = lines.iter().copied().filter(|l| !(is_notify(l) && l.contains("added by lterm"))).collect();
    if kept.len() == lines.len() {
        return Ok(format!("No lterm notify in {}.\n", path.display()));
    }
    write_file(&path, &(kept.join("\n") + "\n"))?;
    Ok(format!("Removed lterm's notify from {}.\n", path.display()))
}

fn status() -> String {
    let claude = claude_settings().ok().map(|p| {
        let n = read_json(&p).ok().map_or(0, |mut root| remove_claude_hooks(&mut root));
        let state = if n > 0 { "installed" } else { "not installed" };
        format!("    claude  {state}  ({})\n", p.display())
    });
    let codex = codex_config().ok().map(|p| {
        let on = std::fs::read_to_string(&p).is_ok_and(|t| t.lines().any(|l| is_notify(l) && l.contains("added by lterm")));
        let state = if on { "installed" } else { "not installed" };
        format!("    codex   {state}  ({})\n", p.display())
    });
    claude.unwrap_or_default() + &codex.unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_hooks_merge_and_remove_cleanly() {
        let mut root: Map<String, Value> = serde_json::from_str(
            r#"{"model":"opus","hooks":{"Stop":[{"hooks":[{"type":"command","command":"notify-send done"}]}]}}"#,
        )
        .unwrap();
        add_claude_hooks(&mut root).unwrap();
        // Installing twice doesn't duplicate.
        remove_claude_hooks(&mut root);
        add_claude_hooks(&mut root).unwrap();
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(stop[0]["hooks"][0]["command"], "notify-send done");
        assert!(stop[1]["hooks"][0]["command"].as_str().unwrap().ends_with("pane state done --agent claude"));
        assert_eq!(root["hooks"]["Notification"][0]["matcher"], "permission_prompt|elicitation_dialog");
        assert_eq!(root.keys().next().map(String::as_str), Some("model"));

        assert_eq!(remove_claude_hooks(&mut root), CLAUDE_HOOKS.len());
        let expected: Value = serde_json::from_str(
            r#"{"model":"opus","hooks":{"Stop":[{"hooks":[{"type":"command","command":"notify-send done"}]}]}}"#,
        )
        .unwrap();
        assert_eq!(Value::Object(root), expected);
    }

    #[test]
    fn codex_notify_lines() {
        let lines: Vec<String> = ["model = \"o3\"", "notify = [\"x\"]", "[tui]", "notify = true"].map(String::from).to_vec();
        assert_eq!(top_level(&lines), 2);
        assert!(is_notify(&lines[1]) && !is_notify("notifyx = 1") && !is_notify(&lines[0]));
    }
}
