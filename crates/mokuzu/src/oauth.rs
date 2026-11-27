//! Subscription providers use Rig's OAuth clients and Mokuzu-owned credential files.
//! Only explicit CLI or TUI sign-in may start a device flow; background requests fail promptly.
//! xAI, for Grok Voice, has no Rig client, so its device flow and refresh live here.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rig_core::client::CompletionClient;
use rig_core::completion::{
    CompletionError, CompletionModel, CompletionRequest, CompletionResponse,
};
use rig_core::providers::{chatgpt, copilot};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{Api, Config, Provider};

enum Client {
    Copilot(copilot::Client),
    Chatgpt(chatgpt::Client),
}

// Clones share Rig's authentication mutex. Reviewers, conversation titles, and swarm
// workers must not each refresh the same rotating ChatGPT token independently.
struct CachedClient {
    client: Client,
    // Rig serializes refreshes within a client, but not between processes. Keep one
    // process per credential directory without serializing inference requests.
    _lease: fs::File,
}

type Clients = BTreeMap<(PathBuf, Provider), CachedClient>;
static CLIENTS: OnceLock<Mutex<Clients>> = OnceLock::new();

pub(crate) enum Model {
    Copilot {
        model: <copilot::Client as CompletionClient>::CompletionModel,
        name: String,
    },
    Chatgpt(<chatgpt::Client as CompletionClient>::CompletionModel),
}

impl Model {
    pub fn new(config: &Config) -> Result<Self> {
        let dir = credentials_dir(&config.path, config.provider)?;
        let mut clients = CLIENTS
            .get_or_init(Mutex::default)
            .lock()
            .map_err(|_| anyhow::anyhow!("OAuth client cache is unavailable"))?;
        let cached = match clients.entry((dir.clone(), config.provider)) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => entry.insert(CachedClient {
                _lease: lease(&dir)?,
                client: client(config.provider, &dir, false)?,
            }),
        };
        Ok(match &cached.client {
            Client::Copilot(client) => copilot_model(client, &config.model, config.api),
            Client::Chatgpt(client) => Self::Chatgpt(client.completion_model(&config.model)),
        })
    }

    pub async fn complete(
        &self,
        mut request: CompletionRequest,
    ) -> Result<CompletionResponse, CompletionError> {
        // Responses backends cannot replay another provider's assistant-message ID.
        for message in &mut request.chat_history {
            if let rig_core::message::Message::Assistant { id, .. } = message
                && id.as_deref().is_some_and(|id| !id.starts_with("msg"))
            {
                *id = None;
            }
        }
        match self {
            Self::Copilot { model, name } => {
                request.model = Some(name.clone());
                model.completion(request).await
            }
            Self::Chatgpt(model) => model.completion(request).await,
        }
    }
}

fn copilot_model(client: &copilot::Client, name: &str, api: Api) -> Model {
    // Rig 0.42 chooses Copilot's transport from the configured model before applying
    // the public per-request model override. Its Responses route only recognizes
    // Codex names. Select that transport, then send the actual model via request.model;
    // the routing selector is never sent to the service. This also supports GPT-6.
    let route = if api == Api::Responses {
        copilot::GPT_5_3_CODEX
    } else {
        name
    };
    Model::Copilot {
        model: client.completion_model(route),
        name: name.to_owned(),
    }
}

fn lease(dir: &Path) -> Result<fs::File> {
    let path = dir.join(".lock");
    if let Ok(metadata) = fs::symlink_metadata(&path)
        && (!metadata.is_file() || metadata.file_type().is_symlink())
    {
        bail!("OAuth lock must be a regular file: {}", path.display());
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.try_lock().context("OAuth credentials are already in use; stop the running Mokuzu process before logging in or starting another one")?;
    Ok(file)
}

fn client(provider: Provider, dir: &Path, interactive: bool) -> Result<Client> {
    client_with_prompt(provider, dir, interactive, |url, code| {
        eprintln!("Open {url} and enter {code}");
    })
}

fn client_with_prompt(
    provider: Provider,
    dir: &Path,
    interactive: bool,
    on_prompt: impl Fn(String, String) + Send + Sync + 'static,
) -> Result<Client> {
    Ok(match provider {
        Provider::GithubCopilot => Client::Copilot(
            copilot::Client::builder()
                .oauth()
                .base_url(provider.default_base_url())
                .token_dir(dir)
                .allow_device_flow(interactive)
                .on_device_code(move |prompt| on_prompt(prompt.verification_uri, prompt.user_code))
                .build()
                .context("failed to build GitHub Copilot OAuth client")?,
        ),
        Provider::Chatgpt => Client::Chatgpt(
            chatgpt::Client::builder()
                .oauth()
                .base_url(provider.default_base_url())
                .token_dir(dir)
                .originator("mokuzu")
                .default_instructions("")
                .user_agent(concat!("mokuzu/", env!("CARGO_PKG_VERSION")))
                .allow_device_flow(interactive)
                .on_device_code(move |prompt| on_prompt(prompt.verification_uri, prompt.user_code))
                .build()
                .context("failed to build ChatGPT OAuth client")?,
        ),
        _ => bail!("{} does not use OAuth", provider.section()),
    })
}

/// Keep credentials beside the chosen config, never in Rig's shared default cache.
/// Rig overwrites these files during renewal, so precreate them with mode 0600.
fn credentials_dir(config: &Path, provider: Provider) -> Result<PathBuf> {
    if !provider.uses_oauth() {
        bail!("{} does not use OAuth", provider.section());
    }
    let dir = private_dir(config, provider.section())?;
    let files: &[(&str, &[u8])] = match provider {
        Provider::GithubCopilot => &[("access-token", b""), ("api-key.json", b"{}")],
        Provider::Chatgpt => &[("auth.json", b"{}")],
        _ => unreachable!(),
    };
    for (name, initial) in files {
        let path = dir.join(name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(mut file) => file.write_all(initial)?,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(&path)?;
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    bail!(
                        "OAuth credential file must be a regular file: {}",
                        path.display()
                    );
                }
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(fs::canonicalize(dir)?)
}

/// `oauth/<section>` beside the config, created owner-only.
fn private_dir(config: &Path, section: &str) -> Result<PathBuf> {
    let parent = config
        .parent()
        .context("config path has no parent directory")?;
    let root = parent.join("oauth");
    let dir = root.join(section);
    for path in [&root, &dir] {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            bail!(
                "OAuth credential directory must be a real directory: {}",
                path.display()
            );
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

// xAI's shared CLI OAuth client, the one Grok's own tools sign in with; its consent screen may
// say Grok Build. The scopes let a SuperGrok or X Premium sign-in call api.x.ai.
const XAI_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const XAI_SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const XAI_DISCOVERY: &str = "https://auth.x.ai/.well-known/openid-configuration";
const XAI_DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// Renew this long before the access token runs out, so a call never starts on a dying one.
const XAI_RENEW_EARLY_SECS: i64 = 300;

/// What `auth.json` holds for xAI.
#[derive(Serialize, Deserialize)]
struct XaiTokens {
    access_token: String,
    /// xAI rotates it on every renewal, so the new one must be saved before the old is lost.
    refresh_token: String,
    /// Unix seconds.
    expires_at: i64,
    token_endpoint: String,
}

impl XaiTokens {
    /// From a token endpoint answer; a renewal may leave out the refresh token to keep `refresh`.
    fn from_answer(body: &Value, refresh: Option<&str>, token_endpoint: &str) -> Result<Self> {
        Ok(Self {
            access_token: body["access_token"]
                .as_str()
                .context("xAI sign-in returned no access token")?
                .to_owned(),
            refresh_token: body["refresh_token"]
                .as_str()
                .or(refresh)
                .context("xAI sign-in returned no refresh token")?
                .to_owned(),
            expires_at: chrono::Utc::now().timestamp()
                + body["expires_in"]
                    .as_i64()
                    .context("xAI sign-in returned no token lifetime")?,
            token_endpoint: token_endpoint.to_owned(),
        })
    }

    /// Replaces `path` whole, so a crash mid-write never loses the rotated refresh token.
    fn save(&self, path: &Path) -> Result<()> {
        let temp = path.with_extension("json.tmp");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(&serde_json::to_vec(self)?)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        Ok(())
    }
}

/// The xAI sign-in `mokuzu login xai` saved, renewed in place; one process holds it at a time.
pub struct Xai {
    path: PathBuf,
    web: reqwest::Client,
    tokens: tokio::sync::Mutex<XaiTokens>,
    _lease: fs::File,
}

impl std::fmt::Debug for Xai {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Xai(<redacted>)")
    }
}

impl Xai {
    /// Loads the sign-in saved beside `config`.
    pub fn open(config: &Path) -> Result<Self> {
        let dir = private_dir(config, "xai")?;
        let lease = lease(&dir)?;
        let path = dir.join("auth.json");
        let tokens = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .context("voice.provider = \"xai\" needs a sign-in: run `mokuzu login xai`")?;
        Ok(Self {
            path,
            web: xai_web()?,
            tokens: tokio::sync::Mutex::new(tokens),
            _lease: lease,
        })
    }

    /// A current access token, renewed first when it is about to run out.
    pub async fn token(&self) -> Result<String> {
        let mut tokens = self.tokens.lock().await;
        if tokens.expires_at - XAI_RENEW_EARLY_SECS > chrono::Utc::now().timestamp() {
            return Ok(tokens.access_token.clone());
        }
        let form = [
            ("grant_type", "refresh_token"),
            ("client_id", XAI_CLIENT_ID),
            ("refresh_token", tokens.refresh_token.as_str()),
        ];
        let (ok, body) = xai_post(&self.web, &tokens.token_endpoint, &form).await?;
        if !ok {
            bail!(
                "xAI sign-in could not be renewed ({}); run `mokuzu login xai`",
                xai_error(&body)
            );
        }
        let fresh =
            XaiTokens::from_answer(&body, Some(&tokens.refresh_token), &tokens.token_endpoint)?;
        fresh.save(&self.path)?;
        *tokens = fresh;
        Ok(tokens.access_token.clone())
    }
}

fn xai_web() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("mokuzu/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build the xAI sign-in client")
}

/// Posts `form` to an xAI OAuth endpoint; whether it succeeded, and the JSON answer.
async fn xai_post(
    web: &reqwest::Client,
    url: &str,
    form: &[(&str, &str)],
) -> Result<(bool, Value)> {
    let response = web
        .post(url)
        .header("Accept", "application/json")
        .form(form)
        .send()
        .await
        .context("xAI sign-in request failed")?;
    let ok = response.status().is_success();
    let body = response
        .json()
        .await
        .context("xAI sign-in answer unreadable")?;
    Ok((ok, body))
}

fn xai_error(body: &Value) -> &str {
    body["error_description"]
        .as_str()
        .or(body["error"].as_str())
        .unwrap_or("unknown error")
}

/// Endpoint `key` from xAI's discovery document, only if it is on x.ai over HTTPS.
fn xai_endpoint(discovery: &Value, key: &str) -> Result<String> {
    let url = discovery[key]
        .as_str()
        .with_context(|| format!("xAI sign-in discovery has no {key}"))?;
    let parsed = reqwest::Url::parse(url)?;
    let host = parsed.host_str().unwrap_or_default();
    if parsed.scheme() != "https" || !(host == "x.ai" || host.ends_with(".x.ai")) {
        bail!("xAI sign-in discovery points {key} outside x.ai: {url}");
    }
    Ok(url.to_owned())
}

/// Signs in to xAI with a device code, saving the tokens beside `config`. `on_prompt` gets the
/// URL to open and the code to enter there.
pub async fn login_xai(
    config: &Path,
    on_prompt: impl Fn(String, String) + Send + Sync + 'static,
) -> Result<()> {
    let dir = private_dir(config, "xai")?;
    let _lease = lease(&dir)?;
    let web = xai_web()?;
    let discovery: Value = web
        .get(XAI_DISCOVERY)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .context("xAI sign-in discovery failed")?
        .json()
        .await
        .context("xAI sign-in discovery unreadable")?;
    let device_endpoint = xai_endpoint(&discovery, "device_authorization_endpoint")?;
    let token_endpoint = xai_endpoint(&discovery, "token_endpoint")?;
    let form = [("client_id", XAI_CLIENT_ID), ("scope", XAI_SCOPE)];
    let (ok, device) = xai_post(&web, &device_endpoint, &form).await?;
    if !ok {
        bail!("xAI refused the sign-in: {}", xai_error(&device));
    }
    let field = |key: &str| {
        device[key]
            .as_str()
            .with_context(|| format!("xAI sign-in returned no {key}"))
    };
    let device_code = field("device_code")?;
    let url = field("verification_uri_complete").or_else(|_| field("verification_uri"))?;
    on_prompt(url.to_owned(), field("user_code")?.to_owned());
    let mut interval = Duration::from_secs(device["interval"].as_u64().unwrap_or(5));
    let deadline =
        Instant::now() + Duration::from_secs(device["expires_in"].as_u64().unwrap_or(1800));
    while Instant::now() < deadline {
        tokio::time::sleep(interval).await;
        let form = [
            ("grant_type", XAI_DEVICE_GRANT),
            ("client_id", XAI_CLIENT_ID),
            ("device_code", device_code),
        ];
        let (ok, body) = xai_post(&web, &token_endpoint, &form).await?;
        if ok {
            return XaiTokens::from_answer(&body, None, &token_endpoint)?
                .save(&dir.join("auth.json"));
        }
        match body["error"].as_str() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += Duration::from_secs(5),
            _ => bail!("xAI sign-in failed: {}", xai_error(&body)),
        }
    }
    bail!("the xAI sign-in code expired; try again")
}

pub async fn login(config: &Path, provider: Provider) -> Result<()> {
    login_with_prompt(config, provider, |url, code| {
        eprintln!("Open {url} and enter {code}");
    })
    .await?;
    eprintln!(
        "Signed in to {}. Select it with `mokuzu configure`.",
        provider.section()
    );
    Ok(())
}

pub async fn login_with_prompt(
    config: &Path,
    provider: Provider,
    on_prompt: impl Fn(String, String) + Send + Sync + 'static,
) -> Result<()> {
    let dir = credentials_dir(config, provider)?;
    let _lease = lease(&dir)?;
    match client_with_prompt(provider, &dir, true, on_prompt)? {
        Client::Copilot(client) => client
            .authorize()
            .await
            .context("GitHub Copilot login failed")?,
        Client::Chatgpt(client) => client.authorize().await.context(
            "ChatGPT login failed; enable device code authorization in ChatGPT settings if needed",
        )?,
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use rig_core::message::{AssistantContent, Message};
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn config_path() -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "mokuzu-oauth-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ))
            .join("config.toml")
    }

    #[test]
    fn credentials_are_private_and_isolated_and_existing_tokens_survive() {
        let config = config_path();
        let copilot = credentials_dir(&config, Provider::GithubCopilot).unwrap();
        let chatgpt = credentials_dir(&config, Provider::Chatgpt).unwrap();
        assert_ne!(copilot, chatgpt);
        assert_eq!(
            fs::metadata(&copilot).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let token = chatgpt.join("auth.json");
        fs::write(&token, "saved credentials").unwrap();
        fs::set_permissions(&token, fs::Permissions::from_mode(0o644)).unwrap();
        credentials_dir(&config, Provider::Chatgpt).unwrap();
        assert_eq!(fs::read_to_string(&token).unwrap(), "saved credentials");
        assert_eq!(
            fs::metadata(token).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(config.parent().unwrap()).unwrap();
    }

    #[test]
    fn credential_symlinks_are_rejected() {
        let config = config_path();
        let dir = credentials_dir(&config, Provider::Chatgpt).unwrap();
        let target = dir.join("outside");
        fs::write(&target, "private").unwrap();
        fs::remove_file(dir.join("auth.json")).unwrap();
        std::os::unix::fs::symlink(&target, dir.join("auth.json")).unwrap();
        assert!(credentials_dir(&config, Provider::Chatgpt).is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "private");
        fs::remove_dir_all(config.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_credential_directory_cannot_have_two_refresh_owners() {
        let config = config_path();
        let dir = credentials_dir(&config, Provider::Chatgpt).unwrap();
        let guard = lease(&dir).unwrap();
        assert!(lease(&dir).is_err());
        drop(guard);
        assert!(lease(&dir).is_ok());
        fs::remove_dir_all(config.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn xai_renews_a_dying_token_and_keeps_the_rotated_refresh_token() {
        let config = config_path();
        let missing = Xai::open(&config).unwrap_err();
        assert!(format!("{missing:#}").contains("mokuzu login xai"));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !String::from_utf8_lossy(&request).contains("refresh_token=old-refresh") {
                let mut chunk = [0; 4096];
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0, "renewal must send the saved refresh token");
                request.extend_from_slice(&chunk[..read]);
            }
            let body = json!({"access_token": "new-access", "refresh_token": "new-refresh",
                "expires_in": 21600})
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let path = private_dir(&config, "xai").unwrap().join("auth.json");
        XaiTokens {
            access_token: "old-access".to_owned(),
            refresh_token: "old-refresh".to_owned(),
            expires_at: chrono::Utc::now().timestamp() + 60,
            token_endpoint: endpoint,
        }
        .save(&path)
        .unwrap();
        let xai = Xai::open(&config).unwrap();
        assert!(Xai::open(&config).is_err(), "one process renews at a time");
        assert_eq!(xai.token().await.unwrap(), "new-access");
        server.await.unwrap();
        assert_eq!(
            xai.token().await.unwrap(),
            "new-access",
            "fresh tokens are reused"
        );
        let saved: XaiTokens = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.refresh_token, "new-refresh");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(config.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn background_clients_fail_without_starting_a_device_flow() {
        crate::install_tls_provider();
        for provider in [Provider::GithubCopilot, Provider::Chatgpt] {
            let config = config_path();
            let dir = credentials_dir(&config, provider).unwrap();
            let result = tokio::time::timeout(std::time::Duration::from_secs(1), async {
                match client(provider, &dir, false).unwrap() {
                    Client::Copilot(client) => client.authorize().await.is_err(),
                    Client::Chatgpt(client) => client.authorize().await.is_err(),
                }
            })
            .await
            .expect("a service must not wait for an interactive login");
            assert!(result);
            fs::remove_dir_all(config.parent().unwrap()).unwrap();
        }
    }

    /// A local HTTP peer exercises Rig's real request conversion and reply parsing.
    pub(crate) async fn reply_server(
        body: String,
        sse: bool,
    ) -> (String, tokio::task::JoinHandle<(String, Value)>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let (headers, body_start) = loop {
                let mut chunk = [0; 8192];
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0, "HTTP request was cut short");
                bytes.extend_from_slice(&chunk[..read]);
                assert!(bytes.len() < 1_000_000);
                if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                    let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().unwrap())
                        })
                        .unwrap();
                    if bytes.len() >= end + 4 + length {
                        break (headers, end + 4);
                    }
                }
            };
            let request = serde_json::from_slice(&bytes[body_start..]).unwrap();
            let content_type = if sse {
                "text/event-stream"
            } else {
                "application/json"
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            (headers, request)
        });
        (url, task)
    }

    #[tokio::test]
    async fn both_subscription_transports_return_tool_calls_for_mokuzu_to_approve() {
        crate::install_tls_provider();
        for (chatgpt, responses) in [(false, false), (false, true), (true, true)] {
            let name = if chatgpt {
                "gpt-6.1-sol"
            } else if responses {
                "gpt-6-luna"
            } else {
                "claude-sonnet-5.5"
            };
            let tool = json!({"name": "read_file", "arguments": "{\"path\":\"/tmp/note\"}"});
            let responses_reply = json!({
                "id": "resp_test", "object": "response", "created_at": 1,
                "status": "completed", "model": name,
                "output": [{"type": "function_call", "id": "fc_test", "call_id": "call_test",
                    "name": tool["name"], "arguments": tool["arguments"], "status": "completed"}],
                "tools": [], "usage": {"input_tokens": 1, "output_tokens": 2, "total_tokens": 3}
            });
            let response = if chatgpt {
                format!(
                    "data: {}\n\ndata: [DONE]\n\n",
                    json!({
                        "type": "response.completed",
                        "response": responses_reply
                    })
                )
            } else if responses {
                responses_reply.to_string()
            } else {
                json!({"id": "chatcmpl-test", "object": "chat.completion", "created": 1, "model": name,
                    "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                        "role": "assistant", "content": null,
                        "tool_calls": [{"id": "call_test", "type": "function", "function": tool}]
                    }}], "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}
                }).to_string()
            };
            let (url, server) = reply_server(response, chatgpt).await;
            let model = if chatgpt {
                Model::Chatgpt(
                    chatgpt::Client::builder()
                        .api_key(chatgpt::ChatGPTAuth::AccessToken {
                            access_token: "test-token".to_owned(),
                            account_id: None,
                        })
                        .base_url(&url)
                        .default_instructions("")
                        .build()
                        .unwrap()
                        .completion_model(name),
                )
            } else {
                copilot_model(
                    &copilot::Client::builder()
                        .api_key("test-token")
                        .base_url(&url)
                        .build()
                        .unwrap(),
                    name,
                    if responses {
                        Api::Responses
                    } else {
                        Api::Openai
                    },
                )
            };
            let mut config = Config::parse("[opencode-go]\napi_key = \"unused\"\n").unwrap();
            config.effort = crate::config::Effort::Max;
            config.api = if responses {
                Api::Responses
            } else {
                Api::Openai
            };
            let mut previous = Message::assistant("Earlier answer");
            if let Message::Assistant { id, .. } = &mut previous {
                *id = Some("foreign-message-id".to_owned());
            }
            let request = crate::agent::request(
                &config,
                "Mokuzu owns tools",
                &[previous, Message::user("Read the note")],
                &[],
            );
            let tool_count = request.tools.len();
            let response =
                tokio::time::timeout(std::time::Duration::from_secs(5), model.complete(request))
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(
                response.provider,
                if chatgpt { "chatgpt" } else { "copilot" }
            );
            assert!(response.choice.iter().any(|content| matches!(content,
                AssistantContent::ToolCall(call) if call.function.name == "read_file"
                    && call.function.arguments["path"] == "/tmp/note")));
            let (headers, body) = server.await.unwrap();
            assert_eq!(
                body["model"], name,
                "the actual selected model must reach the service"
            );
            assert!(!body.to_string().contains(copilot::GPT_5_3_CODEX));
            assert!(headers.starts_with(if responses {
                "POST /responses "
            } else {
                "POST /chat/completions "
            }));
            assert_eq!(body["tools"].as_array().unwrap().len(), tool_count);
            if responses {
                assert_eq!(body["reasoning"]["effort"], "max", "{body}");
            } else {
                assert_eq!(body["reasoning_effort"], "high", "capped: {body}");
            }
            if chatgpt {
                assert_eq!(body["stream"], true);
                assert_eq!(body["store"], false);
                assert_eq!(body["instructions"], "Mokuzu owns tools");
                assert!(body.get("max_output_tokens").is_none());
                assert!(!body.to_string().contains("foreign-message-id"));
            }
        }
    }
}
