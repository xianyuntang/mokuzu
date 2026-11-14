//! The virtual desktop browsers draw on when no screen is plugged in, and the `/browser` live
//! view of it. Mokuzu starts Xvfb once and hands its `DISPLAY` to local MCP servers, so a browser
//! one opens headed (invisible-playwright-mcp with `STEALTHFOX_HEADLESS=0`) shows up there.

use std::process::Stdio;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::OnceCell;

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

/// The Xvfb screen browsers draw on when no real one is plugged in; started once, then kept
/// until mokuzu exits.
static DESKTOP: OnceCell<Option<(Child, String)>> = OnceCell::const_new();

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
        .map(|(_, display)| display.clone())
}

/// The virtual desktop's `DISPLAY` when it is up and ffmpeg can grab it.
async fn screen() -> Option<String> {
    static FFMPEG: OnceCell<bool> = OnceCell::const_new();
    let (_, display) = DESKTOP.get()?.as_ref()?;
    FFMPEG
        .get_or_init(|| installed("ffmpeg"))
        .await
        .then(|| display.clone())
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
async fn virtual_screen((width, height): (u32, u32)) -> Option<(Child, String)> {
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
    Some((child, display))
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
