//! The virtual desktop browsers draw on when no screen is plugged in, the `/browser` live view
//! of it, and the `desktop` tool that lets the model see and use it. Mokuzu starts Xvfb once and
//! hands its `DISPLAY` to local MCP servers, so a browser one opens headed
//! (invisible-playwright-mcp with `STEALTHFOX_HEADLESS=0`) shows up there.

use std::borrow::Cow;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use rig_core::completion::ToolDefinition;
use rig_core::message::{Image, ImageMediaType};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::OnceCell;

use crate::agent::Io;

pub(crate) mod viewer;

/// The `/browser` command: a link to a live view of the desktop, to watch it.
pub const BROWSER_COMMAND: &str = "/browser";

/// `port` is the viewer's port, taken on first use; 0 picks a free one.
pub async fn view_link(port: u16) -> Result<String> {
    viewer::link(port).await
}

/// How long the user keeps control of the desktop without the viewer renewing it, so a closed
/// viewer hands the browser back.
const CONTROL_LEASE: Duration = Duration::from_secs(30);

/// When the user's control of the desktop, taken in the viewer, runs out; `None` when the agent
/// has it.
static CONTROL: Mutex<Option<Instant>> = Mutex::new(None);

/// Gives the desktop to the user, renewing their lease, or back to the agent.
pub(crate) fn set_control(user: bool) {
    *CONTROL.lock().unwrap_or_else(PoisonError::into_inner) =
        user.then(|| Instant::now() + CONTROL_LEASE);
}

/// Whether the user is controlling the desktop, so the agent's browser waits.
pub fn user_in_control() -> bool {
    CONTROL
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_some_and(|until| until > Instant::now())
}

/// Returns once the user hands the desktop back, or their lease runs out.
// ponytail: polls twice a second; a watch channel if the wait ever needs to be instant.
pub async fn until_user_done() {
    while user_in_control() {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// A running Xvfb, its `DISPLAY`, and its size.
type Screen = (Child, String, (u32, u32));

/// The Xvfb screen browsers draw on when no real one is plugged in; started once, then kept
/// until mokuzu exits.
static DESKTOP: OnceCell<Option<Screen>> = OnceCell::const_new();

/// The virtual desktop's `DISPLAY`, starting it on a `size` screen on first use; `None` with a
/// real screen plugged in, or without Xvfb.
pub async fn desktop(size: (u32, u32)) -> Option<String> {
    DESKTOP
        .get_or_init(|| async {
            if screen_attached() {
                None
            } else {
                virtual_screen(size).await
            }
        })
        .await
        .as_ref()
        .map(|(_, display, _)| display.clone())
}

/// The virtual desktop's `DISPLAY` and size when it is up and ffmpeg can grab it.
async fn screen() -> Option<(String, (u32, u32))> {
    static FFMPEG: OnceCell<bool> = OnceCell::const_new();
    let (_, display, size) = DESKTOP.get()?.as_ref()?;
    FFMPEG
        .get_or_init(|| installed("ffmpeg"))
        .await
        .then(|| (display.clone(), *size))
}

/// Whether `program` is on the `PATH`.
async fn installed(program: &str) -> bool {
    tokio::process::Command::new(program)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok()
}

/// Whether browsers have a real screen to draw on: always off Linux; on Linux, a display server
/// with a monitor plugged in.
fn screen_attached() -> bool {
    !cfg!(target_os = "linux")
        || ((std::env::var_os("DISPLAY").is_some()
            || std::env::var_os("WAYLAND_DISPLAY").is_some())
            && monitor_connected())
}

/// Starts Xvfb on a free display and returns it with its `DISPLAY` value, or `None` when Xvfb
/// is not installed.
async fn virtual_screen((width, height): (u32, u32)) -> Option<Screen> {
    let spawned = tokio::process::Command::new("Xvfb")
        .args([
            "-displayfd",
            "1",
            "-screen",
            "0",
            &format!("{width}x{height}x24"),
            "-nolisten",
            "tcp",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(err) => {
            tracing::debug!("no Xvfb ({err}); no virtual desktop");
            return None;
        }
    };
    // `-displayfd 1` prints the display number it picked once it is ready.
    let mut line = String::new();
    let mut stdout = BufReader::new(child.stdout.take()?);
    let read = tokio::time::timeout(Duration::from_secs(10), stdout.read_line(&mut line)).await;
    let number = line.trim();
    if !matches!(read, Ok(Ok(_))) || number.is_empty() {
        tracing::warn!("Xvfb did not start; no virtual desktop");
        return None;
    }
    tracing::debug!("virtual desktop on Xvfb display :{number}");
    let display = format!(":{number}");
    window_manager(&display);
    Some((child, display, (width, height)))
}

/// Starts matchbox-window-manager on `display`, which fills the screen with each window, so
/// popups (a site's sign-in window) are not left at the small size the page asked for and cut
/// off. Optional: without it windows keep the size they open at. It exits with Xvfb.
fn window_manager(display: &str) {
    let spawned = tokio::process::Command::new("matchbox-window-manager")
        .args(["-use_titlebar", "no"])
        .env("DISPLAY", display)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Err(err) = spawned {
        tracing::debug!("no matchbox-window-manager ({err}); popups keep their own size");
    }
}

/// Whether a screen is plugged in. A desktop session runs on a headless Raspberry Pi too, and a
/// window there is never drawn, so pages never finish rendering.
fn monitor_connected() -> bool {
    std::fs::read_dir("/sys/class/drm").is_ok_and(|ports| {
        ports.flatten().any(|port| {
            std::fs::read_to_string(port.path().join("status"))
                .is_ok_and(|status| status.trim() == "connected")
        })
    })
}

/// A random number from std's randomly keyed hasher.
pub(crate) fn random() -> u64 {
    use std::hash::BuildHasher as _;
    std::collections::hash_map::RandomState::new().hash_one(0u8)
}

/// Largest screenshot the `desktop` tool sends; providers shrink bigger ones on their side, and
/// the model's coordinates would then miss.
const SHOT_MAX: (u32, u32) = (1280, 800);

/// How long the screen gets to redraw after an action before the screenshot.
const SETTLE: Duration = Duration::from_millis(700);

/// The `desktop` tool: the model sees the virtual desktop and uses its mouse and keys.
pub fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "desktop".to_owned(),
        description: "See and use the virtual desktop the built-in browser draws on (the one \
             /browser shows): take a screenshot, click, move the mouse, press keys, type. Every \
             call returns a screenshot taken after it, attached after this message's tool \
             results; x and y are in that screenshot's pixels. Prefer the browser__* tools for \
             web pages; use this for what they can't reach or get wrong, such as a captcha, a \
             canvas, a native dialog, or checking what is really on screen. Drag with down, \
             move, up. Scroll by clicking button 4 (up), 5 (down), 6 (left), or 7 (right) with \
             a count; double-click with count 2."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["screenshot", "click", "move", "down", "up", "key", "type"],
                    "description": "down and up press and release a mouse button.",
                },
                "x": {"type": "integer", "description": "For mouse actions."},
                "y": {"type": "integer", "description": "For mouse actions."},
                "button": {
                    "type": "integer",
                    "description": "1 left (default), 2 middle, 3 right, 4-7 scroll up, down, left, right.",
                },
                "count": {"type": "integer", "description": "Times to repeat the action, 1-10. Default 1."},
                "keys": {
                    "type": "string",
                    "description": "For key: keys joined with +, e.g. ctrl+l or Return. Letters, digits, \
                        ctrl, alt, shift, super, Return, BackSpace, Tab, Escape, Delete, Insert, Home, \
                        End, Page_Up, Page_Down, Left, Right, Up, Down, space, F1-F12, minus, equal.",
                },
                "text": {"type": "string", "description": "For type: text to type, up to 2000 characters."},
            },
            "required": ["action"],
        }),
    }
}

/// Whether the `desktop` tool works here: a virtual desktop is up and ffmpeg can grab it.
pub async fn available() -> bool {
    screen().await.is_some()
}

/// Runs one `desktop` call, asking first when `approve` (screenshots never ask): text for the
/// tool result, and the screenshot to show the model. Waits while the user controls the desktop.
pub async fn run(args: &Value, approve: bool, io: &mut impl Io) -> Result<(String, Vec<Image>)> {
    let Some((display, size)) = screen().await else {
        return Ok(("error: no virtual desktop".to_owned(), Vec::new()));
    };
    let shot = shot_size(size);
    let action = args["action"].as_str().unwrap_or_default();
    if action != "screenshot" {
        if approve {
            let shown = format!(
                "desktop\n{}",
                serde_json::to_string_pretty(args).unwrap_or_default()
            );
            if !io.confirm(&shown).await? {
                return Ok(("error: the user denied this call".to_owned(), Vec::new()));
            }
        } else {
            io.note(&format!("🖥️ desktop → {action}")).await?;
        }
        if user_in_control() {
            io.note("⏸️ waiting for you to hand the desktop back in /browser")
                .await?;
        }
        until_user_done().await;
        let acted = act(&display, args, size, shot).await;
        if approve {
            let output = acted
                .as_ref()
                .map_or_else(|err| format!("{err:#}"), |()| "done".to_owned());
            io.ran(&output, acted.is_ok()).await?;
        }
        if let Err(err) = acted {
            return Ok((format!("error: {err:#}"), Vec::new()));
        }
        tokio::time::sleep(SETTLE).await;
    }
    Ok(match screenshot(&display, shot).await {
        Ok(png) => (
            format!(
                "Done. The desktop follows as a {}x{} screenshot, after this message's tool results.",
                shot.0, shot.1
            ),
            vec![crate::document::image(&png, Some(ImageMediaType::PNG))],
        ),
        Err(err) => (format!("error: screenshot failed: {err:#}"), Vec::new()),
    })
}

/// Plays `args` on `display`, `count` times, with coordinates from a `shot`-sized screenshot of a
/// `size` screen.
async fn act(display: &str, args: &Value, size: (u32, u32), shot: (u32, u32)) -> Result<()> {
    let count = args["count"].as_u64().unwrap_or(1);
    ensure!((1..=10).contains(&count), "bad count");
    let query = query(args, size, shot)?;
    for _ in 0..count {
        viewer::input(display, &query).await?;
    }
    Ok(())
}

/// The viewer's input event for `args`, which checks it, with `x` and `y` scaled from a
/// `shot`-sized screenshot to the `size` screen.
fn query(
    args: &Value,
    size: (u32, u32),
    shot: (u32, u32),
) -> Result<HashMap<Cow<'static, str>, Cow<'static, str>>> {
    let mut query = HashMap::new();
    for key in ["action", "button", "keys", "text"] {
        let value = match &args[key] {
            Value::Null => continue,
            Value::String(text) => text.clone(),
            Value::Number(number) => number.to_string(),
            _ => bail!("bad {key}"),
        };
        query.insert(Cow::Borrowed(key), Cow::Owned(value));
    }
    for (key, screen, shot) in [("x", size.0, shot.0), ("y", size.1, shot.1)] {
        if args[key].is_null() {
            continue;
        }
        let at = args[key].as_u64().with_context(|| format!("bad {key}"))?;
        let scaled = (at * u64::from(screen) + u64::from(shot) / 2) / u64::from(shot);
        let scaled = scaled.min(u64::from(screen.saturating_sub(1)));
        query.insert(Cow::Borrowed(key), Cow::Owned(scaled.to_string()));
    }
    Ok(query)
}

/// The screenshot size for a `size` screen: the screen's, shrunk to fit `SHOT_MAX`.
fn shot_size((width, height): (u32, u32)) -> (u32, u32) {
    let (max_width, max_height) = SHOT_MAX;
    if width <= max_width && height <= max_height {
        (width, height)
    } else if u64::from(max_width) * u64::from(height) <= u64::from(max_height) * u64::from(width) {
        (max_width, height * max_width / width)
    } else {
        (width * max_height / height, max_height)
    }
}

/// One PNG frame of `display`, scaled to `shot`.
async fn screenshot(display: &str, (width, height): (u32, u32)) -> Result<Vec<u8>> {
    let grab = tokio::process::Command::new("ffmpeg")
        .args(["-loglevel", "error", "-f", "x11grab", "-i", display])
        .args(["-frames:v", "1", "-vf", &format!("scale={width}:{height}")])
        .args(["-f", "image2pipe", "-c:v", "png", "-"])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(10), grab)
        .await
        .context("ffmpeg did not finish")?
        .context("failed to run ffmpeg")?;
    ensure!(
        out.status.success(),
        "ffmpeg failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinates_scale_from_the_screenshot_to_the_screen() {
        let size = (1920, 1080);
        let shot = shot_size(size);
        assert_eq!(shot, (1280, 720));
        let args = json!({"action": "click", "x": 640, "y": 719, "button": 3});
        let event = query(&args, size, shot).unwrap();
        assert_eq!(event["x"], "960");
        assert_eq!(event["y"], "1079");
        assert_eq!(event["button"], "3");
        assert_eq!(shot_size((1280, 800)), (1280, 800));
        assert!(query(&json!({"action": "click", "x": -1, "y": 0}), size, shot).is_err());
    }
}
