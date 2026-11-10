//! Live view of the virtual desktop: a small web page on the local network that shows the whole
//! desktop (see `DESKTOP`), to watch, or to take control of, which plays the user's mouse and keys
//! on it with xdotool while the agent's browser waits. The link carries a random token; without it
//! every request is refused.

use std::borrow::Cow;
use std::collections::HashMap;
use std::net::{IpAddr, UdpSocket};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use reqwest::Url;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::OnceCell;

use super::{random, screen, set_control, user_in_control};

/// The viewer's link, starting it on first use; it then serves until mokuzu exits.
// ponytail: never stops once started; stop it with the session if an open port bothers anyone.
pub(super) async fn link(port: u16) -> Result<String> {
    static LINK: OnceCell<String> = OnceCell::const_new();
    LINK.get_or_try_init(|| start(port)).await.cloned()
}

/// Serves the viewer on `port`, or a free one when 0.
async fn start(port: u16) -> Result<String> {
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .context("failed to open the viewer's port; is browser.viewer_port taken?")?;
    let port = listener.local_addr()?.port();
    // ponytail: 128 bits from std's OS-seeded hasher keys; switch to getrandom if this ever
    // guards more than a LAN page.
    let token = format!("{:016x}{:016x}", random(), random());
    let host = lan_ip().map_or_else(|| "localhost".to_owned(), |ip| ip.to_string());
    let expected = token.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let token = expected.clone();
            tokio::spawn(async move {
                if let Err(err) = serve(stream, &token).await {
                    tracing::debug!("viewer request failed: {err:#}");
                }
            });
        }
    });
    tracing::debug!(port, "browser viewer is up");
    Ok(format!("http://{host}:{port}/?token={token}"))
}

/// The address other machines on the network reach this one at: the one the default route
/// leaves from. Connecting a UDP socket sends nothing.
pub(crate) fn lan_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip())
}

/// Answers one HTTP/1.1 request, then closes the connection.
async fn serve(connection: TcpStream, token: &str) -> Result<()> {
    let (read, mut write) = connection.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        bail!("bad request line {line:?}");
    };
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).await? == 0 || header.trim_end().is_empty() {
            break;
        }
    }

    let url = Url::parse(&format!("http://viewer{target}"))?;
    let query: HashMap<_, _> = url.query_pairs().collect();
    let allowed = query
        .get("token")
        .is_some_and(|given| same(given.as_bytes(), token.as_bytes()));
    let display = screen().await;
    let (status, kind, content) = if allowed {
        match (method, url.path(), display) {
            ("GET", "/stream", Some(display)) => {
                let fps = query
                    .get("fps")
                    .and_then(|fps| fps.parse().ok())
                    .unwrap_or(5u8)
                    .clamp(1, 15);
                return stream(&display, fps, &mut write).await;
            }
            ("GET" | "POST", "/stream" | "/input", None) => (
                "503 Service Unavailable",
                "text/plain",
                b"no virtual desktop".to_vec(),
            ),
            ("POST", "/control", _) => {
                set_control(query.get("on").is_some_and(|on| on == "1"));
                ("204 No Content", "text/plain", Vec::new())
            }
            ("POST", "/input", Some(_)) if !user_in_control() => {
                ("409 Conflict", "text/plain", b"take control first".to_vec())
            }
            ("POST", "/input", Some(display)) => {
                set_control(true);
                match input(&display, &query).await {
                    Ok(()) => ("204 No Content", "text/plain", Vec::new()),
                    Err(err) => (
                        "400 Bad Request",
                        "text/plain",
                        format!("{err:#}").into_bytes(),
                    ),
                }
            }
            ("GET", "/", _) => (
                "200 OK",
                "text/html; charset=utf-8",
                PAGE.as_bytes().to_vec(),
            ),
            _ => ("404 Not Found", "text/plain", b"not found".to_vec()),
        }
    } else {
        (
            "403 Forbidden",
            "text/plain",
            b"wrong or missing token".to_vec(),
        )
    };
    let head = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {kind}\r\ncontent-length: {}\r\n\
         cache-control: no-store\r\nreferrer-policy: no-referrer\r\nconnection: close\r\n\r\n",
        content.len()
    );
    write.write_all(head.as_bytes()).await?;
    write.write_all(&content).await?;
    Ok(())
}

/// Streams the whole `display` as MJPEG at `fps` frames a second until the viewer goes.
async fn stream(display: &str, fps: u8, write: &mut OwnedWriteHalf) -> Result<()> {
    let mut ffmpeg = tokio::process::Command::new("ffmpeg")
        .args([
            "-loglevel",
            "error",
            "-f",
            "x11grab",
            "-framerate",
            &fps.to_string(),
            "-i",
            display,
        ])
        .args([
            "-q:v",
            "8",
            "-f",
            "mpjpeg",
            "-boundary_tag",
            "frame",
            "-flush_packets",
            "1",
        ])
        .arg("-")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("failed to run ffmpeg")?;
    let mut frames = ffmpeg.stdout.take().context("ffmpeg has no output")?;
    write
        .write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: multipart/x-mixed-replace; boundary=frame\r\n\
              cache-control: no-store\r\nconnection: close\r\n\r\n",
        )
        .await?;
    // Ends when the viewer goes and a write fails; dropping `ffmpeg` then kills it.
    tokio::io::copy(&mut frames, write).await?;
    Ok(())
}

/// Plays one input event from the viewer on `display`.
async fn input(display: &str, query: &HashMap<Cow<'_, str>, Cow<'_, str>>) -> Result<()> {
    let (args, text) = xdotool_args(query)?;
    let mut xdotool = tokio::process::Command::new("xdotool")
        .env("DISPLAY", display)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("failed to run xdotool; is it installed?")?;
    if let Some(mut stdin) = xdotool.stdin.take() {
        stdin.write_all(text.as_bytes()).await?;
    }
    let status = tokio::time::timeout(Duration::from_secs(10), xdotool.wait())
        .await
        .context("xdotool did not finish")??;
    ensure!(status.success(), "xdotool failed: {status}");
    Ok(())
}

/// Keys and modifiers the viewer may press, besides single letters and digits.
const NAMED_KEYS: [&str; 33] = [
    "ctrl",
    "alt",
    "shift",
    "super",
    "Return",
    "BackSpace",
    "Tab",
    "Escape",
    "Delete",
    "Insert",
    "Home",
    "End",
    "Page_Up",
    "Page_Down",
    "Left",
    "Right",
    "Up",
    "Down",
    "space",
    "F1",
    "F2",
    "F3",
    "F4",
    "F5",
    "F6",
    "F7",
    "F8",
    "F9",
    "F10",
    "F11",
    "F12",
    "minus",
    "equal",
];

/// The xdotool arguments for the viewer's event in `query`, and the text it reads from stdin.
/// Everything is checked here: xdotool chains commands, one of which runs programs, so nothing
/// from the viewer reaches its arguments unchecked, and typed text goes through stdin.
fn xdotool_args(query: &HashMap<Cow<'_, str>, Cow<'_, str>>) -> Result<(Vec<String>, String)> {
    let get = |key: &str| query.get(key).map(AsRef::as_ref).unwrap_or_default();
    let number = |key: &str| -> Result<String> {
        let value: u16 = get(key).parse().with_context(|| format!("bad {key}"))?;
        Ok(value.to_string())
    };
    let pointer = |then: &[&str]| -> Result<Vec<String>> {
        let button: u8 = get("button").parse().unwrap_or(1);
        ensure!((1..=7).contains(&button), "bad button");
        let mut args = vec!["mousemove".to_owned(), number("x")?, number("y")?];
        args.extend(then.iter().map(|arg| (*arg).to_owned()));
        if !then.is_empty() {
            args.push(button.to_string());
        }
        Ok(args)
    };
    let args = match get("action") {
        "move" => pointer(&[])?,
        "down" => pointer(&["mousedown"])?,
        "up" => pointer(&["mouseup"])?,
        "click" => pointer(&["click"])?,
        "key" => {
            let keys = get("keys");
            ensure!(
                !keys.is_empty()
                    && keys.split('+').all(|key| {
                        NAMED_KEYS.contains(&key)
                            || (key.len() == 1 && key.bytes().all(|b| b.is_ascii_alphanumeric()))
                    }),
                "bad keys {keys:?}"
            );
            vec!["key".to_owned(), "--".to_owned(), keys.to_owned()]
        }
        "type" => {
            let text = get("text");
            ensure!(!text.is_empty() && text.chars().count() <= 2000, "bad text");
            return Ok((
                ["type", "--file", "-"].map(str::to_owned).to_vec(),
                text.to_owned(),
            ));
        }
        other => bail!("unknown action {other:?}"),
    };
    Ok((args, String::new()))
}

/// Constant-time comparison, so response timing gives the token away a byte at a time.
fn same(given: &[u8], expected: &[u8]) -> bool {
    given.len() == expected.len()
        && given
            .iter()
            .zip(expected)
            .fold(0, |diff, (a, b)| diff | (a ^ b))
            == 0
}

const PAGE: &str = r#"<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<meta name="referrer" content="no-referrer">
<title>mokuzu browser</title>
<style>
  /* Tokens from ui-ux-pro-max's design system for a developer tool: Swiss minimal, slate, dense,
     dark first with a light theme, green for a live connection. */
  :root {
    color-scheme: dark light;
    --background: #0f172a; --card: #1b2336; --muted: #272f42; --border: #475569;
    --foreground: #f8fafc; --muted-foreground: #94a3b8; --ring: #f8fafc;
    --accent: #22c55e; --on-accent: #0f172a; --warning: #f59e0b; --destructive: #ef4444;
    --shadow: 0 1px 2px rgb(0 0 0 / .4), 0 12px 32px rgb(0 0 0 / .35);
    --font: "IBM Plex Sans", system-ui, -apple-system, "Segoe UI", sans-serif;
    --mono: "JetBrains Mono", ui-monospace, SFMono-Regular, Menlo, monospace;
    --space-1: 4px; --space-2: 8px; --space-3: 12px; --space-4: 16px;
    --radius: 8px; --control: 40px; --fast: 150ms;
  }
  @media (prefers-color-scheme: light) {
    :root { --background: #f1f5f9; --card: #ffffff; --muted: #e2e8f0; --border: #94a3b8;
      --foreground: #0f172a; --muted-foreground: #475569; --ring: #0f172a; --accent: #15803d;
      --on-accent: #ffffff; --warning: #b45309; --destructive: #b91c1c;
      --shadow: 0 1px 2px rgb(15 23 42 / .08), 0 12px 32px rgb(15 23 42 / .12); }
  }
  * { box-sizing: border-box; }
  html, body { height: 100%; }
  body { margin: 0; display: flex; flex-direction: column; background: var(--background);
    color: var(--foreground); font: 14px/1.5 var(--font); }
  header { display: flex; align-items: center; gap: var(--space-2); background: var(--card);
    padding: var(--space-2) max(var(--space-4), env(safe-area-inset-right))
      var(--space-2) max(var(--space-4), env(safe-area-inset-left)); }
  header { border-bottom: 1px solid var(--muted); padding-top: max(var(--space-2), env(safe-area-inset-top)); }
  .brand { display: flex; align-items: center; gap: var(--space-2); font-weight: 600;
    letter-spacing: -.01em; white-space: nowrap; padding-right: var(--space-2); }
  svg { width: 18px; height: 18px; fill: none; stroke: currentColor; stroke-width: 2;
    stroke-linecap: round; stroke-linejoin: round; flex: none; }
  :disabled { opacity: .4; cursor: not-allowed; }
  .grow { flex: 1; }
  main { flex: 1; min-height: 0; position: relative; }
  /* Pinned to main's box so the picture shrinks to fit it whatever its shape. */
  .stage { position: absolute; inset: var(--space-4); display: flex; align-items: center;
    justify-content: center; padding-bottom: env(safe-area-inset-bottom); }
  #screen { display: block; max-width: 100%; max-height: 100%; border-radius: var(--radius);
    border: 1px solid var(--muted); box-shadow: var(--shadow); background: var(--card); }
  #screen[hidden], #empty[hidden] { display: none; }
  #empty { text-align: center; color: var(--muted-foreground); max-width: 340px; }
  #empty svg { width: 32px; height: 32px; stroke-width: 1.5; margin-bottom: var(--space-3); }
  #empty strong { display: block; color: var(--foreground); font-size: 16px; margin-bottom: var(--space-1); }
  .pill { display: inline-flex; align-items: center; gap: var(--space-2); height: 28px;
    padding: 0 var(--space-3); border-radius: 999px; background: var(--muted); font-size: 12px;
    font-weight: 500; white-space: nowrap; }
  .pill::before { content: ""; width: 8px; height: 8px; border-radius: 50%;
    background: var(--state, var(--muted-foreground)); }
  .pill[data-state="live"] { --state: var(--accent); }
  .pill[data-state="wait"] { --state: var(--warning); }
  .pill[data-state="error"] { --state: var(--destructive); }
  .pill[data-state="control"] { --state: var(--warning); }
  button, input { font: inherit; color: inherit; }
  button { display: inline-flex; align-items: center; justify-content: center; gap: var(--space-2);
    min-height: var(--control); min-width: var(--control); padding: 0 var(--space-3);
    border-radius: var(--radius); border: 1px solid var(--border); background: var(--muted);
    cursor: pointer; white-space: nowrap; transition: background var(--fast); }
  button:hover { background: var(--card); }
  button:focus-visible, input:focus-visible { outline: 2px solid var(--ring); outline-offset: 2px; }
  #control[aria-pressed="true"] { background: var(--warning); border-color: var(--warning);
    color: var(--on-accent); }
  body.controlling #screen { cursor: crosshair; touch-action: none; user-select: none;
    -webkit-user-select: none; outline: 2px solid var(--warning); outline-offset: 2px; }
  #typing { display: flex; gap: var(--space-2); background: var(--card);
    border-top: 1px solid var(--muted); padding: var(--space-2)
      max(var(--space-4), env(safe-area-inset-right)) max(var(--space-2), env(safe-area-inset-bottom))
      max(var(--space-4), env(safe-area-inset-left)); }
  #typing[hidden] { display: none; }
  #typing input { flex: 1; min-width: 0; min-height: var(--control); padding: 0 var(--space-3);
    border-radius: var(--radius); border: 1px solid var(--border); background: var(--background);
    font-size: 16px; }
  @media (prefers-reduced-motion: reduce) { * { transition: none !important; } }
  /* Fingers need bigger targets, and iOS zooms into fields with text under 16px. */
  @media (max-width: 600px) {
    header { flex-wrap: wrap; }
    .brand { display: none; }
    .pill { order: 1; }
    .stage { inset: var(--space-2); }
  }
</style>
<header>
  <span class="brand"><svg viewBox="0 0 24 24" aria-hidden="true"><rect x="3" y="4" width="18" height="16" rx="2"/><path d="M3 9h18M7 6.5h.01M10 6.5h.01"/></svg>mokuzu</span>
  <span class="grow"></span>
  <button id="control" type="button" aria-pressed="false" hidden><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M4 4l7 16 2-7 7-2z"/></svg><span id="control-label">Take control</span></button>
  <span id="status" class="pill" data-state="wait" role="status" aria-atomic="true">Connecting</span>
</header>
<main><div class="stage">
  <img id="screen" alt="Live view of mokuzu's desktop" draggable="false" hidden>
  <div id="empty">
    <svg viewBox="0 0 24 24" aria-hidden="true"><rect x="2" y="3" width="20" height="14" rx="2"/><path d="M8 21h8M12 17v4"/></svg>
    <strong id="empty-title">Connecting to mokuzu…</strong>
    <span id="empty-text"></span>
  </div>
</div></main>
<form id="typing" hidden>
  <input id="text" placeholder="Type here and send; keys also go straight through" autocomplete="off" autocapitalize="off" enterkeyhint="send" aria-label="Text to type">
  <button type="submit">Send</button>
  <button type="button" data-key="Return" aria-label="Enter">↵</button>
  <button type="button" data-key="BackSpace" aria-label="Backspace">⌫</button>
</form>
<script>
const token = new URLSearchParams(location.search).get('token');
const $ = id => document.getElementById(id);
const status = (text, state) => { $('status').textContent = text; $('status').dataset.state = state; };

const img = $('screen');
let controlling = false, renew, chain = Promise.resolve(), pending = 0, lastMove = 0;
const api = (path, params = {}) =>
  fetch(`${path}?${new URLSearchParams({ token, ...params })}`, { method: 'POST' });
const live = () => controlling ? status('You are in control', 'control') : status('Live', 'live');

// One long response the browser shows frame by frame, faster while in control. Shown at once
// rather than on `load`, which browsers fire unevenly for a stream of frames.
function show() {
  img.src = `/stream?token=${token}&fps=${controlling ? 12 : 5}&t=${Date.now()}`;
  img.hidden = false; $('empty').hidden = true; $('control').hidden = false; live();
}
img.onerror = () => {
  img.hidden = true; img.removeAttribute('src'); $('empty').hidden = false;
  $('empty-title').textContent = 'Nothing to show';
  $('empty-text').textContent = 'mokuzu shows its virtual desktop, run on Linux without a screen '
    + 'with Xvfb and ffmpeg installed, or mokuzu stopped. Trying again.';
  status('Waiting', 'wait');
  setTimeout(show, 3000);
};

// Taking control makes the agent's browser wait; the lease is renewed while this page is open,
// so closing it hands the browser back within half a minute.
async function setControl(on) {
  controlling = on;
  await api('/control', { on: on ? 1 : 0 }).catch(() => {});
  $('control').setAttribute('aria-pressed', on);
  $('control-label').textContent = on ? 'Hand back' : 'Take control';
  $('typing').hidden = !on;
  document.body.classList.toggle('controlling', on);
  clearInterval(renew);
  if (on) renew = setInterval(() => api('/control', { on: 1 }).catch(() => {}), 10000);
  if (!img.hidden) show();
}
$('control').onclick = () => setControl(!controlling);
addEventListener('pagehide', () => {
  if (controlling) navigator.sendBeacon(`/control?${new URLSearchParams({ token, on: 0 })}`);
});

// Events go one at a time, in order; moves are dropped while others wait.
function send(params) {
  pending++;
  chain = chain
    .then(() => api('/input', params))
    .then(r => r.ok ? live() : r.text().then(text => status(text, 'error')))
    .catch(() => status('Input failed', 'error'))
    .finally(() => pending--);
}
// The picture is scaled to fit; events land where they would on the desktop itself.
function at(e) {
  const r = img.getBoundingClientRect();
  const clamp = (v, max) => Math.min(Math.max(Math.round(v), 0), max - 1);
  return { x: clamp((e.clientX - r.left) / r.width * img.naturalWidth, img.naturalWidth),
           y: clamp((e.clientY - r.top) / r.height * img.naturalHeight, img.naturalHeight) };
}
const button = e => [1, 2, 3][e.button] ?? 1;
img.addEventListener('pointerdown', e => {
  if (!controlling) return;
  e.preventDefault(); img.setPointerCapture(e.pointerId);
  send({ action: 'down', button: button(e), ...at(e) });
});
img.addEventListener('pointermove', e => {
  if (!controlling || pending > 1 || Date.now() - lastMove < 80) return;
  lastMove = Date.now();
  send({ action: 'move', ...at(e) });
});
img.addEventListener('pointerup', e => {
  if (controlling) send({ action: 'up', button: button(e), ...at(e) });
});
img.addEventListener('contextmenu', e => { if (controlling) e.preventDefault(); });
img.addEventListener('wheel', e => {
  if (!controlling) return;
  e.preventDefault();
  if (pending < 2) send({ action: 'click', button: e.deltaY > 0 ? 5 : 4, ...at(e) });
}, { passive: false });

// Keys pressed anywhere but the text field go straight to the desktop. Text through an input
// method (Chinese, Japanese) goes through the field instead.
const NAMED = { Enter: 'Return', Backspace: 'BackSpace', Tab: 'Tab', Escape: 'Escape',
  Delete: 'Delete', Insert: 'Insert', Home: 'Home', End: 'End', PageUp: 'Page_Up',
  PageDown: 'Page_Down', ArrowLeft: 'Left', ArrowRight: 'Right', ArrowUp: 'Up', ArrowDown: 'Down',
  ' ': 'space', '-': 'minus', '=': 'equal' };
document.addEventListener('keydown', e => {
  if (!controlling || e.isComposing || e.target.closest('form')) return;
  const mods = [(e.ctrlKey || e.metaKey) && 'ctrl', e.altKey && 'alt'].filter(Boolean);
  const key = NAMED[e.key] || (/^F\d{1,2}$/.test(e.key) ? e.key
    : /^[a-z0-9]$/i.test(e.key) ? e.key.toLowerCase() : null);
  if (e.key.length === 1 && !mods.length) send({ action: 'type', text: e.key });
  else if (key) send({ action: 'key', keys: [...mods, e.shiftKey && 'shift', key].filter(Boolean).join('+') });
  else return;
  e.preventDefault();
});
$('typing').onsubmit = e => {
  e.preventDefault();
  if ($('text').value) send({ action: 'type', text: $('text').value });
  $('text').value = '';
};
for (const b of document.querySelectorAll('[data-key]')) b.onclick = () => send({ action: 'key', keys: b.dataset.key });
show();
</script>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_compare_whole() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abd", b"abc"));
        assert!(!same(b"ab", b"abc"));
    }

    #[test]
    fn viewer_events_become_checked_xdotool_arguments() {
        let args = |pairs: &[(&'static str, &'static str)]| {
            let query = pairs
                .iter()
                .map(|(k, v)| (Cow::Borrowed(*k), Cow::Borrowed(*v)))
                .collect();
            xdotool_args(&query)
        };
        let (click, _) = args(&[
            ("action", "down"),
            ("x", "10"),
            ("y", "20"),
            ("button", "3"),
        ])
        .expect("a press");
        assert_eq!(click, ["mousemove", "10", "20", "mousedown", "3"]);
        let (key, _) = args(&[("action", "key"), ("keys", "ctrl+shift+Tab")]).expect("a key");
        assert_eq!(key, ["key", "--", "ctrl+shift+Tab"]);
        let (typed, text) = args(&[("action", "type"), ("text", "exec rm -rf ~")]).expect("text");
        assert_eq!(typed, ["type", "--file", "-"]);
        assert_eq!(text, "exec rm -rf ~", "text goes through stdin");
        for bad in [
            &[("action", "key"), ("keys", "exec")][..],
            &[("action", "key"), ("keys", "a b")],
            &[("action", "key"), ("keys", "")],
            &[("action", "move"), ("x", "-1"), ("y", "0")],
            &[("action", "click"), ("x", "1"), ("y", "1"), ("button", "9")],
            &[("action", "exec")],
        ] {
            assert!(args(bad).is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn requests_need_the_token() {
        // Its own viewer: `link`'s dies with the first test runtime that started it.
        let link = start(0).await.expect("viewer starts");
        let client = reqwest::Client::new();
        let page = client.get(&link).send().await.expect("page");
        assert_eq!(page.status(), 200);
        assert!(page.text().await.expect("body").contains("mokuzu browser"));
        let stream = link.replace("/?", "/stream?");
        let stream = client.get(&stream).send().await.expect("stream");
        assert_eq!(stream.status(), 503, "no desktop started");
        let base = link.split('?').next().expect("base");
        let refused = client.get(base).send().await.expect("refused");
        assert_eq!(refused.status(), 403);
    }
}
