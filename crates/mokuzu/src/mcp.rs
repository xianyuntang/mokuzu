//! MCP client: connects to the configured servers at startup and offers their tools to the model
//! as `<server>__<tool>`. Each call asks the user first unless the server sets `approve = false`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use rig_core::completion::ToolDefinition;
use rig_core::message::{DocumentSourceKind, Image, ImageMediaType, MimeType as _};
use rmcp::ServiceExt as _;
use rmcp::model::{CallToolRequestParams, ContentBlock, Tool};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use serde_json::Value;
use tokio::io::AsyncBufReadExt as _;

use crate::agent::Io;
use crate::config::{BROWSER_SERVER, McpServer, Secret};

/// How long a server gets to start and list its tools; `npx` may download the server first.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long one tool call may run; a server that never answers would otherwise hang the turn.
const CALL_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Longest tool name providers accept.
const MAX_NAME_CHARS: usize = 64;
// ponytail: hard cap on tool output fed back to the model; page it if servers return more.
const MAX_OUTPUT_CHARS: usize = 30_000;

/// The connected servers; ones that failed to start are left out.
#[derive(Default)]
pub struct Mcp {
    servers: Vec<Server>,
    /// Every server's tools, as sent to the model.
    definitions: Vec<ToolDefinition>,
    /// Tools of servers with `approve = false`, which swarm crabs may call too.
    unapproved: Vec<ToolDefinition>,
    /// The built-in browser's config and `DISPLAY` when crabs may use it, to start each crab
    /// its own copy (see `OwnBrowser`).
    own_browser: Option<(McpServer, Option<String>)>,
}

struct Server {
    name: String,
    approve: bool,
    client: RunningService<RoleClient, ()>,
    /// Name the model uses, and the server's own tool name.
    tools: Vec<(String, String)>,
}

impl std::fmt::Debug for Mcp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.servers.iter().map(|s| s.name.as_str()).collect();
        f.debug_struct("Mcp").field("servers", &names).finish()
    }
}

impl Mcp {
    /// Starts every server at once; a server that fails is logged and skipped.
    /// Local servers get `display` as their `DISPLAY` unless their `env` sets one, so a browser
    /// they open headed shows up on mokuzu's virtual desktop.
    pub async fn connect(servers: &BTreeMap<String, McpServer>, display: Option<&str>) -> Self {
        let started = futures::future::join_all(servers.iter().map(|(name, config)| async move {
            let started = tokio::time::timeout(CONNECT_TIMEOUT, start(name, config, display)).await;
            match started {
                Ok(Ok(server)) => Some(server),
                Ok(Err(err)) => {
                    tracing::warn!("MCP server {name} failed to start: {err:#}");
                    None
                }
                Err(_) => {
                    tracing::warn!("MCP server {name} did not start within {CONNECT_TIMEOUT:?}");
                    None
                }
            }
        }))
        .await;
        let mut mcp = Self {
            own_browser: servers
                .get(BROWSER_SERVER)
                .filter(|config| !config.approve && config.command.is_some())
                .map(|config| (config.clone(), display.map(str::to_owned))),
            ..Self::default()
        };
        for (server, definitions) in started.into_iter().flatten() {
            tracing::info!(
                "MCP server {} offers {} tools",
                server.name,
                server.tools.len()
            );
            if !server.approve {
                mcp.unapproved.extend(definitions.iter().cloned());
            }
            mcp.definitions.extend(definitions);
            mcp.servers.push(server);
        }
        mcp
    }

    pub fn tools(&self) -> &[ToolDefinition] {
        &self.definitions
    }

    /// Runs the MCP tool the model called `name`, or returns `None` if no server offers it.
    /// Failures of the tool itself come back as text for the model, and images it returns
    /// alongside.
    pub async fn call(
        &self,
        name: &str,
        args: &Value,
        io: &mut impl Io,
    ) -> Option<Result<(String, Vec<Image>)>> {
        let (server, tool) = self.servers.iter().find_map(|server| {
            let (_, tool) = server.tools.iter().find(|(exposed, _)| exposed == name)?;
            Some((server, tool))
        })?;
        Some(server.call(tool, args, io).await)
    }

    /// Tools that run without asking: those of servers with `approve = false`.
    pub fn unapproved_tools(&self) -> &[ToolDefinition] {
        &self.unapproved
    }

    /// Runs `name` if it comes from a server with `approve = false`, with no one to ask or tell,
    /// as swarm crabs do; images it returns are left out. Returns `None` for any other name.
    pub async fn call_unapproved(&self, name: &str, args: &Value) -> Option<String> {
        let (server, tool) = self
            .servers
            .iter()
            .filter(|s| !s.approve)
            .find_map(|server| {
                let (_, tool) = server.tools.iter().find(|(exposed, _)| exposed == name)?;
                Some((server, tool))
            })?;
        Some(server.invoke_text(tool, args).await)
    }

    /// A browser of the caller's own, for a swarm crab; see `OwnBrowser`.
    pub fn own_browser(&self) -> OwnBrowser<'_> {
        OwnBrowser {
            mcp: self,
            running: None,
        }
    }
}

/// Firefox profile entries a copy leaves out: the lock of the browser using the original, and
/// caches.
const NOT_COPIED: [&str; 5] = [
    "lock",
    ".parentlock",
    "parent.lock",
    "cache2",
    "startupCache",
];

/// A swarm crab's own copy of the built-in browser. The server drives one page, so crabs sharing
/// it would steer each other's page; each gets its own instead. It starts on first use from a copy
/// of the main profile, so it has the user's sign-ins, and is shut down and its copy deleted when
/// this is dropped; nothing it does reaches the main profile.
pub struct OwnBrowser<'a> {
    mcp: &'a Mcp,
    /// The running server and the directory its copy lives in.
    running: Option<(Server, PathBuf)>,
}

impl std::fmt::Debug for OwnBrowser<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnBrowser")
            .field("running", &self.running.as_ref().map(|(_, dir)| dir))
            .finish()
    }
}

impl OwnBrowser<'_> {
    /// Runs the browser tool the model called `name`, starting the browser first if needed, with
    /// no one to ask or tell; images it returns are left out. Returns `None` when `name` is not a
    /// tool of a built-in browser crabs may use.
    pub async fn call(&mut self, name: &str, args: &Value) -> Option<String> {
        let (config, display) = self.mcp.own_browser.as_ref()?;
        let shared = self.mcp.servers.iter().find(|s| s.name == BROWSER_SERVER)?;
        let (_, tool) = shared.tools.iter().find(|(exposed, _)| exposed == name)?;
        if self.running.is_none() {
            match start_copy(config, display.as_deref()).await {
                Ok(running) => self.running = Some(running),
                Err(err) => return Some(format!("error: the browser failed to start: {err:#}")),
            }
        }
        let (server, _) = self.running.as_ref()?;
        Some(server.invoke_text(tool, args).await)
    }
}

impl Drop for OwnBrowser<'_> {
    fn drop(&mut self) {
        if let Some((server, dir)) = self.running.take() {
            // ponytail: cleanup runs on the runtime, so a copy is left in the temp dir if mokuzu
            // exits first, or if Firefox still holds files 3 s after its server is told to stop.
            tokio::spawn(async move {
                // Closes the server's stdin, which ends it and its Firefox.
                let _ = server.client.cancel().await;
                if let Err(err) = tokio::fs::remove_dir_all(&dir).await {
                    tracing::debug!("failed to delete {}: {err}", dir.display());
                }
            });
        }
    }
}

/// Starts a copy of the browser `config` describes, on a copy of its profile in a new temporary
/// directory, which is returned and deleted again if the start fails.
async fn start_copy(config: &McpServer, display: Option<&str>) -> Result<(Server, PathBuf)> {
    const PROFILE: &str = "STEALTHFOX_PROFILE_DIR";
    let dir =
        std::env::temp_dir().join(format!("mokuzu-browser-{:016x}", crate::browser::random()));
    let profile = dir.join("profile");
    let mut config = config.clone();
    let original = config.env.get(PROFILE).map(|p| PathBuf::from(p.as_str()));
    // The server keeps a file per session under its home; this one's goes with the copy.
    let home = dir.join("home").to_string_lossy().into_owned();
    config
        .env
        .insert("INVISIBLE_MCP_HOME".to_owned(), Secret::new(home));
    config.env.insert(
        PROFILE.to_owned(),
        Secret::new(profile.to_string_lossy().into_owned()),
    );
    let started = async {
        tokio::task::spawn_blocking(move || match original {
            Some(original) => copy_profile(&original, &profile),
            None => std::fs::create_dir_all(&profile),
        })
        .await?
        .context("failed to copy the browser profile")?;
        tokio::time::timeout(CONNECT_TIMEOUT, start(BROWSER_SERVER, &config, display))
            .await
            .with_context(|| format!("did not start within {CONNECT_TIMEOUT:?}"))?
    }
    .await;
    match started {
        Ok((server, _)) => Ok((server, dir)),
        Err(err) => {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            Err(err)
        }
    }
}

/// Copies the profile at `from` into `to`, leaving out `NOT_COPIED`; a missing `from`, a profile
/// never used yet, gives an empty one.
fn copy_profile(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    let entries = match std::fs::read_dir(from) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if NOT_COPIED.iter().any(|skip| name == *skip) {
            continue;
        }
        // Symbolic links are left out: Firefox's lock on Linux is one.
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_profile(&entry.path(), &to.join(&name))?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), to.join(&name))?;
        }
    }
    Ok(())
}

impl Server {
    async fn call(
        &self,
        tool: &str,
        args: &Value,
        io: &mut impl Io,
    ) -> Result<(String, Vec<Image>)> {
        let shown = format!(
            "{} → {tool}\n{}",
            self.name,
            serde_json::to_string_pretty(args).unwrap_or_default()
        );
        if self.approve {
            if !io.confirm(&shown).await? {
                return Ok(("error: the user denied this call".to_owned(), Vec::new()));
            }
        } else {
            io.note(&format!("🔌 {} → {tool}", self.name)).await?;
        }
        if self.name == BROWSER_SERVER && crate::browser::user_in_control() {
            io.note("⏸️ waiting for you to hand the browser back in /browser")
                .await?;
        }
        let (output, ok, images) = self.invoke(tool, args).await;
        if self.approve {
            io.ran(&output, ok).await?;
        }
        let output = if ok {
            output
        } else {
            format!("error: {output}")
        };
        Ok((output, images))
    }

    /// Calls `tool` with no one to ask or tell: its output, marked when it failed, without images.
    async fn invoke_text(&self, tool: &str, args: &Value) -> String {
        let (output, ok, _) = self.invoke(tool, args).await;
        if ok {
            output
        } else {
            format!("error: {output}")
        }
    }

    /// Calls `tool` on the server: its output, whether it succeeded, and the images it returned.
    /// The built-in browser's tools wait while the user controls the desktop.
    async fn invoke(&self, tool: &str, args: &Value) -> (String, bool, Vec<Image>) {
        if self.name == BROWSER_SERVER {
            crate::browser::until_user_done().await;
        }
        let mut params = CallToolRequestParams::new(tool.to_owned());
        if let Value::Object(args) = args {
            params = params.with_arguments(args.clone());
        }
        let Ok(called) = tokio::time::timeout(CALL_TIMEOUT, self.client.call_tool(params)).await
        else {
            tracing::warn!(server = %self.name, "{tool} gave no answer in {CALL_TIMEOUT:?}");
            return (
                format!("timed out after {CALL_TIMEOUT:?}"),
                false,
                Vec::new(),
            );
        };
        match called {
            Ok(result) => {
                let mut text: Vec<String> = result.content.iter().map(describe).collect();
                if text.is_empty()
                    && let Some(structured) = &result.structured_content
                {
                    text.push(structured.to_string());
                }
                let images = result.content.iter().filter_map(image).collect();
                (
                    truncate(text.join("\n")),
                    result.is_error != Some(true),
                    images,
                )
            }
            Err(err) => (format!("{err:#}"), false, Vec::new()),
        }
    }
}

/// Connects to one server and lists its tools.
async fn start(
    name: &str,
    config: &McpServer,
    display: Option<&str>,
) -> Result<(Server, Vec<ToolDefinition>)> {
    let client = match (&config.command, &config.url) {
        (Some(command), _) => {
            let mut cmd = tokio::process::Command::new(command);
            if let Some(display) = display {
                cmd.env("DISPLAY", display);
            }
            cmd.args(&config.args)
                .envs(config.env.iter().map(|(k, v)| (k, v.as_str())));
            let (transport, stderr) = TokioChildProcess::builder(cmd)
                .stderr(Stdio::piped())
                .spawn()
                .with_context(|| format!("failed to run {command}"))?;
            // The server's stderr goes to the log, tagged with its name.
            if let Some(stderr) = stderr {
                let name = name.to_owned();
                tokio::spawn(async move {
                    let mut lines = tokio::io::BufReader::new(stderr).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        tracing::info!(server = %name, "{line}");
                    }
                });
            }
            ().serve(transport).await.context("MCP handshake failed")?
        }
        (None, Some(url)) => {
            let mut transport = StreamableHttpClientTransportConfig::with_uri(url.as_str());
            if let Some(token) = &config.token {
                transport = transport.auth_header(token.as_str());
            }
            let transport =
                StreamableHttpClientTransport::with_client(reqwest::Client::new(), transport);
            ().serve(transport).await.context("MCP handshake failed")?
        }
        (None, None) => anyhow::bail!("needs `command` or `url`"),
    };
    let listed = client
        .list_all_tools()
        .await
        .context("failed to list tools")?;
    let mut tools = Vec::new();
    let mut definitions = Vec::new();
    for tool in listed {
        let exposed = exposed_name(name, &tool.name);
        if exposed.chars().count() > MAX_NAME_CHARS {
            tracing::warn!("skipping MCP tool {exposed}: name over {MAX_NAME_CHARS} characters");
            continue;
        }
        definitions.push(definition(&exposed, &tool));
        tools.push((exposed, tool.name.into_owned()));
    }
    let server = Server {
        name: name.to_owned(),
        approve: config.approve,
        client,
        tools,
    };
    Ok((server, definitions))
}

/// `<server>__<tool>`, with characters providers reject in tool names turned into `_`.
fn exposed_name(server: &str, tool: &str) -> String {
    let tool: String = tool
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{server}__{tool}")
}

fn definition(exposed: &str, tool: &Tool) -> ToolDefinition {
    ToolDefinition {
        name: exposed.to_owned(),
        description: tool
            .description
            .as_deref()
            .or(tool.title.as_deref())
            .unwrap_or_default()
            .to_owned(),
        parameters: Value::Object((*tool.input_schema).clone()),
    }
}

/// Text of one result block; other kinds are named, since only text and images (see `image`) go
/// back to the model.
fn describe(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.text.clone(),
        ContentBlock::Image(_) => "[image]".to_owned(),
        ContentBlock::Audio(_) => "[audio omitted]".to_owned(),
        ContentBlock::Resource(resource) => {
            serde_json::to_string(&resource.resource).unwrap_or_default()
        }
        ContentBlock::ResourceLink(link) => format!("[resource: {}]", link.uri),
        _ => "[unsupported content omitted]".to_owned(),
    }
}

/// An image block as an image for the model, already base64 as providers take it.
fn image(block: &ContentBlock) -> Option<Image> {
    let ContentBlock::Image(image) = block else {
        return None;
    };
    Some(Image {
        data: DocumentSourceKind::Base64(image.data.clone()),
        media_type: ImageMediaType::from_mime_type(&image.mime_type),
        ..Image::default()
    })
}

fn truncate(mut text: String) -> String {
    if let Some((cut, _)) = text.char_indices().nth(MAX_OUTPUT_CHARS) {
        text.truncate(cut);
        text.push_str("\n[output truncated]");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposed_names_are_safe_for_providers() {
        assert_eq!(exposed_name("gh", "create_issue"), "gh__create_issue");
        assert_eq!(exposed_name("fs", "read.file/v2"), "fs__read_file_v2");
    }

    /// Approves everything and records what it was shown.
    #[derive(Default)]
    struct Approver(Vec<String>);

    impl Io for Approver {
        async fn say(&mut self, _text: &str) -> Result<()> {
            Ok(())
        }
        async fn confirm(&mut self, action: &str) -> Result<bool> {
            self.0.push(action.to_owned());
            Ok(true)
        }
    }

    /// Needs node and network: `cargo test -- --ignored everything_server`.
    #[tokio::test]
    #[ignore = "starts a real MCP server through npx"]
    async fn everything_server_round_trip() {
        let config = crate::config::Config::parse(
            "[opencode-go]\napi_key = \"k\"\n[mcp.servers.every]\ncommand = \"npx\"\n\
             args = [\"-y\", \"@modelcontextprotocol/server-everything\"]\n",
        )
        .expect("valid");
        let mcp = Mcp::connect(&config.mcp, None).await;
        assert!(mcp.tools().iter().any(|t| t.name == "every__echo"));
        let mut io = Approver::default();
        let out = mcp
            .call(
                "every__echo",
                &serde_json::json!({"message": "hi"}),
                &mut io,
            )
            .await
            .expect("tool exists")
            .expect("call runs")
            .0;
        assert!(out.contains("hi") && !out.starts_with("error"), "{out}");
        assert_eq!(io.0.len(), 1, "asked for approval once");
        assert!(
            mcp.call("every__nope", &Value::Null, &mut io)
                .await
                .is_none()
        );
    }

    #[test]
    fn image_blocks_reach_the_model() {
        let shot = image(&ContentBlock::image("aGk=", "image/png")).expect("an image");
        assert_eq!(shot.data, DocumentSourceKind::Base64("aGk=".to_owned()));
        assert_eq!(shot.media_type, Some(ImageMediaType::PNG));
        assert!(image(&ContentBlock::text("hi")).is_none());
    }

    /// Needs uv and the browser's Firefox: `cargo test -- --ignored own_browsers`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "starts real browsers through uvx"]
    async fn own_browsers_each_drive_their_own_page() {
        let dir =
            std::env::temp_dir().join(format!("mokuzu-own-test-{:016x}", crate::browser::random()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "[opencode-go]\napi_key = \"k\"\n[browser]\napprove = false\n",
        )
        .unwrap();
        let mut config = crate::config::Config::load(&path).unwrap();
        config.database_path = dir.join("mokuzu.db");
        let mcp = Mcp::connect(&config.mcp_servers(), None).await;
        let (mut a, mut b) = (mcp.own_browser(), mcp.own_browser());
        let open = serde_json::json!({});
        let (one, two) = tokio::join!(
            a.call("browser__browser_open", &open),
            b.call("browser__browser_open", &open)
        );
        let (one, two) = (one.unwrap(), two.unwrap());
        assert!(one.contains("identity:"), "{one}");
        assert!(two.contains("identity:"), "{two}");
        let go = |url: &str| serde_json::json!({ "url": url });
        a.call(
            "browser__browser_navigate",
            &go("data:text/html,<title>one</title>"),
        )
        .await
        .unwrap();
        b.call(
            "browser__browser_navigate",
            &go("data:text/html,<title>two</title>"),
        )
        .await
        .unwrap();
        let title = serde_json::json!({ "expression": "document.title" });
        let one = a.call("browser__browser_evaluate", &title).await.unwrap();
        let two = b.call("browser__browser_evaluate", &title).await.unwrap();
        assert!(one.contains("one") && !one.contains("two"), "{one}");
        assert!(two.contains("two") && !two.contains("one"), "{two}");
        let copies = [
            a.running.as_ref().unwrap().1.clone(),
            b.running.as_ref().unwrap().1.clone(),
        ];
        drop((a, b));
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert!(copies.iter().all(|copy| !copy.exists()), "copies deleted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn profile_copies_leave_out_locks_and_caches() {
        let root = std::env::temp_dir().join(format!(
            "mokuzu-copy-test-{:016x}",
            crate::browser::random()
        ));
        let (from, to) = (root.join("from"), root.join("to"));
        std::fs::create_dir_all(from.join("cache2")).unwrap();
        std::fs::create_dir_all(from.join("storage/default")).unwrap();
        std::fs::write(from.join("cookies.sqlite"), "c").unwrap();
        std::fs::write(from.join("storage/default/x"), "x").unwrap();
        std::fs::write(from.join(".parentlock"), "").unwrap();
        std::fs::write(from.join("cache2/entry"), "").unwrap();
        copy_profile(&from, &to).unwrap();
        assert_eq!(
            std::fs::read_to_string(to.join("cookies.sqlite")).unwrap(),
            "c"
        );
        assert!(to.join("storage/default/x").is_file());
        assert!(!to.join(".parentlock").exists() && !to.join("cache2").exists());
        copy_profile(&root.join("missing"), &root.join("empty")).unwrap();
        assert!(root.join("empty").is_dir());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn truncate_caps_output() {
        assert_eq!(truncate("ok".to_owned()), "ok");
        assert!(truncate("é".repeat(MAX_OUTPUT_CHARS + 1)).ends_with("[output truncated]"));
    }
}
