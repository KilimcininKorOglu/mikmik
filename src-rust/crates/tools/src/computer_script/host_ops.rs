//! What a host call actually does.
//!
//! Every op the runner can ask for is answered here, against `enigo` and
//! `xcap`. The whole module is feature-gated: without `computer-use` there is
//! no backend to answer with, and the tool is not registered either.

use serde_json::{json, Value};

/// The ops that change the machine rather than reading it.
///
/// `read_only` is enforced in the runner, which is where the flag lives for
/// the duration of one call. This list is the host's own copy of the same
/// rule, so a runner that lies about the flag still cannot write.
pub const WRITING_OPS: &[&str] = &[
    "move",
    "click",
    "double_click",
    "drag",
    "type",
    "key",
    "scroll",
    "clipboard_write",
];

/// Whether `op` writes.
pub fn writes(op: &str) -> bool {
    WRITING_OPS.contains(&op)
}

/// Read an integer argument, or say which one was missing.
fn number(args: &Value, name: &str) -> Result<i32, String> {
    args.get(name)
        .and_then(Value::as_i64)
        .map(|n| n as i32)
        .ok_or_else(|| format!("`{name}` is required and must be a number"))
}

/// Read a string argument, or say which one was missing.
fn text(args: &Value, name: &str) -> Result<String, String> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("`{name}` is required and must be a string"))
}

#[cfg(not(feature = "computer-use"))]
pub async fn run(_op: &str, _args: &Value) -> Result<Value, String> {
    Err("this build carries no desktop backend; rebuild with the computer-use feature".to_string())
}

#[cfg(feature = "computer-use")]
pub async fn run(op: &str, args: &Value) -> Result<Value, String> {
    // Every op is a blocking platform call, so it runs off the async runtime's
    // worker rather than holding one for the length of a screen capture.
    let op = op.to_string();
    let args = args.clone();
    tokio::task::spawn_blocking(move || run_blocking(&op, &args))
        .await
        .map_err(|error| format!("the desktop call did not finish: {error}"))?
}

#[cfg(feature = "computer-use")]
fn run_blocking(op: &str, args: &Value) -> Result<Value, String> {
    use enigo::{Button, Coordinate, Direction, Enigo, Keyboard, Mouse, Settings};

    // A fresh connection per call. Holding one across calls would keep a
    // display connection open for the life of the session, and the cost of
    // opening one is far below the cost of the input it sends.
    let enigo =
        || Enigo::new(&Settings::default()).map_err(|error| format!("no input device: {error}"));

    match op {
        "screenshot" => screenshot(args),
        "displays" => displays(),
        "windows" => windows(),
        "cursor" => {
            let enigo = enigo()?;
            let (x, y) = enigo
                .location()
                .map_err(|error| format!("cursor position unavailable: {error}"))?;
            Ok(json!({ "x": x, "y": y }))
        }
        "move" => {
            let (x, y) = (number(args, "x")?, number(args, "y")?);
            let mut enigo = enigo()?;
            enigo
                .move_mouse(x, y, Coordinate::Abs)
                .map_err(|error| format!("move failed: {error}"))?;
            Ok(json!({ "x": x, "y": y }))
        }
        "click" => {
            let (x, y) = (number(args, "x")?, number(args, "y")?);
            let button = match args.get("button").and_then(Value::as_str) {
                Some("right") => Button::Right,
                Some("middle") => Button::Middle,
                _ => Button::Left,
            };
            let mut enigo = enigo()?;
            enigo
                .move_mouse(x, y, Coordinate::Abs)
                .map_err(|error| format!("move failed: {error}"))?;
            enigo
                .button(button, Direction::Click)
                .map_err(|error| format!("click failed: {error}"))?;
            Ok(json!({ "clicked": [x, y] }))
        }
        "double_click" => {
            let (x, y) = (number(args, "x")?, number(args, "y")?);
            let mut enigo = enigo()?;
            enigo
                .move_mouse(x, y, Coordinate::Abs)
                .map_err(|error| format!("move failed: {error}"))?;
            for _ in 0..2 {
                enigo
                    .button(Button::Left, Direction::Click)
                    .map_err(|error| format!("click failed: {error}"))?;
            }
            Ok(json!({ "clicked": [x, y] }))
        }
        "drag" => {
            let (x1, y1) = (number(args, "x1")?, number(args, "y1")?);
            let (x2, y2) = (number(args, "x2")?, number(args, "y2")?);
            let mut enigo = enigo()?;
            enigo
                .move_mouse(x1, y1, Coordinate::Abs)
                .map_err(|error| format!("move failed: {error}"))?;
            enigo
                .button(Button::Left, Direction::Press)
                .map_err(|error| format!("press failed: {error}"))?;
            let moved = enigo.move_mouse(x2, y2, Coordinate::Abs);
            // Release whatever happened, so a failed move does not leave the
            // pointer held down over the user's desktop.
            let released = enigo.button(Button::Left, Direction::Release);
            moved.map_err(|error| format!("drag failed: {error}"))?;
            released.map_err(|error| format!("release failed: {error}"))?;
            Ok(json!({ "from": [x1, y1], "to": [x2, y2] }))
        }
        "type" => {
            let body = text(args, "text")?;
            let mut enigo = enigo()?;
            enigo
                .text(&body)
                .map_err(|error| format!("typing failed: {error}"))?;
            Ok(json!({ "typed": body.chars().count() }))
        }
        "key" => {
            let combo = text(args, "combo")?;
            let mut enigo = enigo()?;
            crate::computer_use::press_key_sequence(&mut enigo, &combo)
                .map_err(|error| format!("key failed: {error}"))?;
            Ok(json!({ "pressed": combo }))
        }
        "scroll" => {
            let direction = args
                .get("direction")
                .and_then(Value::as_str)
                .unwrap_or("down");
            let amount = args.get("amount").and_then(Value::as_i64).unwrap_or(3) as i32;
            let axis = match direction {
                "up" | "down" => enigo::Axis::Vertical,
                "left" | "right" => enigo::Axis::Horizontal,
                other => return Err(format!("unknown scroll direction: {other}")),
            };
            let length = match direction {
                "up" | "left" => -amount,
                _ => amount,
            };
            let mut enigo = enigo()?;
            enigo
                .scroll(length, axis)
                .map_err(|error| format!("scroll failed: {error}"))?;
            Ok(json!({ "scrolled": direction, "amount": amount }))
        }
        "clipboard_read" => clipboard_read(),
        "clipboard_write" => clipboard_write(&text(args, "text")?),
        other => Err(format!("unknown host call: {other}")),
    }
}

#[cfg(feature = "computer-use")]
fn screenshot(args: &Value) -> Result<Value, String> {
    use base64::Engine as _;

    let wanted = args.get("display").and_then(Value::as_u64).unwrap_or(0) as usize;
    let monitors = xcap::Monitor::all().map_err(|error| format!("no monitor list: {error}"))?;
    let monitor = monitors
        .into_iter()
        .nth(wanted)
        .ok_or_else(|| format!("there is no display {wanted}"))?;
    let image = monitor
        .capture_image()
        .map_err(|error| format!("capture failed: {error}"))?;

    let (width, height) = (image.width(), image.height());
    let mut png = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|error| format!("encoding failed: {error}"))?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(png.into_inner());

    Ok(json!({
        "width": width,
        "height": height,
        "mime_type": "image/png",
        "base64": encoded,
    }))
}

#[cfg(feature = "computer-use")]
fn displays() -> Result<Value, String> {
    let monitors = xcap::Monitor::all().map_err(|error| format!("no monitor list: {error}"))?;
    let rows: Vec<Value> = monitors
        .iter()
        .enumerate()
        .map(|(index, monitor)| {
            json!({
                "index": index,
                "name": monitor.name().unwrap_or_default(),
                "x": monitor.x().unwrap_or_default(),
                "y": monitor.y().unwrap_or_default(),
                "width": monitor.width().unwrap_or_default(),
                "height": monitor.height().unwrap_or_default(),
                "scale": monitor.scale_factor().unwrap_or(1.0),
                "primary": monitor.is_primary().unwrap_or(false),
            })
        })
        .collect();
    Ok(Value::Array(rows))
}

#[cfg(feature = "computer-use")]
fn windows() -> Result<Value, String> {
    let windows = xcap::Window::all().map_err(|error| format!("no window list: {error}"))?;
    let rows: Vec<Value> = windows
        .iter()
        .map(|window| {
            json!({
                "id": window.id().unwrap_or_default(),
                "title": window.title().unwrap_or_default(),
                "app": window.app_name().unwrap_or_default(),
                "pid": window.pid().unwrap_or_default(),
                "x": window.x().unwrap_or_default(),
                "y": window.y().unwrap_or_default(),
                "width": window.width().unwrap_or_default(),
                "height": window.height().unwrap_or_default(),
                "minimized": window.is_minimized().unwrap_or(false),
                "focused": window.is_focused().unwrap_or(false),
            })
        })
        .collect();
    Ok(Value::Array(rows))
}

/// The clipboard, through the platform's own command.
///
/// No crate is pulled in for it: each platform ships a pair of tools that do
/// exactly this, and a clipboard crate would add a second event loop beside
/// the one `enigo` already opens.
#[cfg(feature = "computer-use")]
fn clipboard_read() -> Result<Value, String> {
    let (program, args) = clipboard_reader()?;
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("could not read the clipboard: {error}"))?;
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(json!(text))
}

#[cfg(feature = "computer-use")]
fn clipboard_write(text: &str) -> Result<Value, String> {
    use std::io::Write as _;

    let (program, args) = clipboard_writer()?;
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not write the clipboard: {error}"))?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| "the clipboard command took no input".to_string())?;
        stdin
            .write_all(text.as_bytes())
            .map_err(|error| format!("could not write the clipboard: {error}"))?;
    }
    child
        .wait()
        .map_err(|error| format!("the clipboard command failed: {error}"))?;
    Ok(json!({ "wrote": text.chars().count() }))
}

#[cfg(feature = "computer-use")]
fn clipboard_reader() -> Result<(&'static str, Vec<&'static str>), String> {
    if cfg!(target_os = "macos") {
        Ok(("pbpaste", vec![]))
    } else if cfg!(target_os = "windows") {
        Ok((
            "powershell",
            vec!["-NoProfile", "-Command", "Get-Clipboard"],
        ))
    } else if which::which("wl-paste").is_ok() {
        Ok(("wl-paste", vec!["--no-newline"]))
    } else if which::which("xclip").is_ok() {
        Ok(("xclip", vec!["-selection", "clipboard", "-o"]))
    } else {
        Err("no clipboard command found; install wl-clipboard or xclip".to_string())
    }
}

#[cfg(feature = "computer-use")]
fn clipboard_writer() -> Result<(&'static str, Vec<&'static str>), String> {
    if cfg!(target_os = "macos") {
        Ok(("pbcopy", vec![]))
    } else if cfg!(target_os = "windows") {
        Ok(("clip", vec![]))
    } else if which::which("wl-copy").is_ok() {
        Ok(("wl-copy", vec![]))
    } else if which::which("xclip").is_ok() {
        Ok(("xclip", vec!["-selection", "clipboard"]))
    } else {
        Err("no clipboard command found; install wl-clipboard or xclip".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_op_that_moves_the_machine_is_named_as_writing() {
        for op in ["move", "click", "type", "key", "scroll", "clipboard_write"] {
            assert!(writes(op), "{op}");
        }
    }

    #[test]
    fn a_reading_op_is_not_named_as_writing() {
        for op in [
            "screenshot",
            "displays",
            "windows",
            "cursor",
            "clipboard_read",
        ] {
            assert!(!writes(op), "{op}");
        }
    }

    #[test]
    fn a_missing_argument_names_itself() {
        let error = number(&json!({}), "x").expect_err("x is required");

        assert!(error.contains('x'), "{error}");
    }
}
