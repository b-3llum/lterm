//! A small Chrome DevTools Protocol client: JSON commands and events over a WebSocket to a
//! browser running on this machine (`lterm browse` starts it).

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::{Message, WebSocket};

pub struct Cdp {
    ws: WebSocket<TcpStream>,
    next_id: u64,
    /// Events that arrived while waiting for a command's answer.
    events: VecDeque<Value>,
}

/// Seconds a command may take before it's given up on.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

impl Cdp {
    /// Connect to a DevTools WebSocket URL like `ws://127.0.0.1:PORT/devtools/page/ID`.
    pub fn connect(url: &str) -> Result<Cdp, String> {
        let host = url
            .strip_prefix("ws://")
            .and_then(|r| r.split('/').next())
            .ok_or_else(|| format!("not a local DevTools URL: {url}"))?;
        let stream = TcpStream::connect(host).map_err(|e| format!("cannot reach the browser at {host}: {e}"))?;
        let _ = stream.set_nodelay(true);
        let (ws, _) = tungstenite::client(url, stream).map_err(|e| format!("DevTools handshake failed: {e}"))?;
        Ok(Cdp { ws, next_id: 1, events: VecDeque::new() })
    }

    /// Send a command without waiting for its answer (it is dropped when it arrives).
    pub fn send(&mut self, method: &str, params: Value) -> Result<u64, String> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({ "id": id, "method": method, "params": params }).to_string();
        self.ws.send(Message::text(msg)).map_err(|e| format!("browser connection lost: {e}"))?;
        Ok(id)
    }

    /// Run a command and return its result.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.send(method, params)?;
        let deadline = Instant::now() + CALL_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!("{method}: the browser didn't answer"));
            }
            let Some(msg) = self.read(left)? else { continue };
            if msg.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(err) = msg.get("error") {
                    let text = err.get("message").and_then(Value::as_str).unwrap_or("error");
                    return Err(format!("{method}: {text}"));
                }
                return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
            }
            if msg.get("method").is_some() {
                self.events.push_back(msg);
            }
        }
    }

    /// The next event, waiting at most `wait`. Answers to `send` are skipped.
    pub fn event(&mut self, wait: Duration) -> Result<Option<Value>, String> {
        if let Some(ev) = self.events.pop_front() {
            return Ok(Some(ev));
        }
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.read(left)? {
                Some(msg) if msg.get("method").is_some() => return Ok(Some(msg)),
                Some(_) if !left.is_zero() => continue,
                _ => return Ok(None),
            }
        }
    }

    /// One message, or None if nothing came within `wait`.
    fn read(&mut self, wait: Duration) -> Result<Option<Value>, String> {
        let _ = self.ws.get_ref().set_read_timeout(Some(wait.max(Duration::from_millis(1))));
        match self.ws.read() {
            Ok(Message::Text(t)) => Ok(serde_json::from_str(t.as_str()).ok()),
            Ok(Message::Close(_)) => Err("the browser closed the connection".into()),
            Ok(_) => Ok(None),
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => Ok(None),
            Err(e) => Err(format!("browser connection lost: {e}")),
        }
    }
}

/// `Runtime.evaluate` with the value returned as JSON; script exceptions become errors.
pub fn eval(cdp: &mut Cdp, expression: &str) -> Result<Value, String> {
    let r = cdp.call(
        "Runtime.evaluate",
        json!({ "expression": expression, "returnByValue": true, "awaitPromise": true, "userGesture": true }),
    )?;
    if let Some(ex) = r.get("exceptionDetails") {
        let text = ex
            .pointer("/exception/description")
            .or_else(|| ex.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("script error");
        return Err(text.lines().next().unwrap_or(text).to_string());
    }
    Ok(r.pointer("/result/value").cloned().unwrap_or(Value::Null))
}

/// DevTools key name, code and Windows key code for a named key (enter, tab, up, ...).
pub fn key_info(name: &str) -> Option<(&'static str, &'static str, u32)> {
    Some(match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => ("Enter", "Enter", 13),
        "tab" => ("Tab", "Tab", 9),
        "esc" | "escape" => ("Escape", "Escape", 27),
        "backspace" | "bs" => ("Backspace", "Backspace", 8),
        "delete" | "del" => ("Delete", "Delete", 46),
        "up" | "arrowup" => ("ArrowUp", "ArrowUp", 38),
        "down" | "arrowdown" => ("ArrowDown", "ArrowDown", 40),
        "left" | "arrowleft" => ("ArrowLeft", "ArrowLeft", 37),
        "right" | "arrowright" => ("ArrowRight", "ArrowRight", 39),
        "home" => ("Home", "Home", 36),
        "end" => ("End", "End", 35),
        "pgup" | "pageup" => ("PageUp", "PageUp", 33),
        "pgdn" | "pagedown" => ("PageDown", "PageDown", 34),
        "space" => (" ", "Space", 32),
        _ => return None,
    })
}

/// Press and release a named key. `modifiers`: 1 Alt, 2 Ctrl, 4 Meta, 8 Shift.
pub fn press_key(cdp: &mut Cdp, name: &str, modifiers: u32) -> Result<(), String> {
    let (key, code, vk) = key_info(name).ok_or_else(|| format!("unknown key '{name}'"))?;
    // Enter needs its text on keyDown, or forms don't submit.
    let text = match key {
        "Enter" => "\r",
        " " => " ",
        _ => "",
    };
    let down = json!({ "type": if text.is_empty() { "rawKeyDown" } else { "keyDown" }, "key": key, "code": code,
        "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk, "text": text, "unmodifiedText": text, "modifiers": modifiers });
    cdp.call("Input.dispatchKeyEvent", down)?;
    cdp.call("Input.dispatchKeyEvent", json!({ "type": "keyUp", "key": key, "code": code,
        "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk, "modifiers": modifiers }))?;
    Ok(())
}

/// A trusted left click at page coordinates (CSS pixels). If the click opens an alert,
/// confirm or prompt, returns its message: the browser holds the click until it's answered.
pub fn click_at(cdp: &mut Cdp, x: f64, y: f64) -> Result<Option<String>, String> {
    cdp.call("Page.enable", json!({}))?;
    cdp.call("Input.dispatchMouseEvent", json!({ "type": "mouseMoved", "x": x, "y": y }))?;
    cdp.call("Input.dispatchMouseEvent", json!({ "type": "mousePressed", "x": x, "y": y, "button": "left", "clickCount": 1 }))?;
    let id = cdp.send("Input.dispatchMouseEvent", json!({ "type": "mouseReleased", "x": x, "y": y, "button": "left", "clickCount": 1 }))?;
    let deadline = Instant::now() + CALL_TIMEOUT;
    while Instant::now() < deadline {
        let Some(msg) = cdp.read(Duration::from_millis(200))? else { continue };
        if msg.get("id").and_then(Value::as_u64) == Some(id) {
            return Ok(None);
        }
        if msg.get("method").and_then(Value::as_str) == Some("Page.javascriptDialogOpening") {
            let p = &msg["params"];
            return Ok(Some(format!("{}: {}", p["type"].as_str().unwrap_or("dialog"), p["message"].as_str().unwrap_or(""))));
        }
    }
    Err("click: the browser didn't answer".into())
}
