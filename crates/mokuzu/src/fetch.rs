//! `fetch_url` tool: reads a public web page as plain text. It runs without approval, so it
//! refuses private, loopback, and link-local addresses, including via DNS and redirects.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::CONTENT_TYPE;
use reqwest::redirect::Policy;
use rig_core::completion::ToolDefinition;
use rig_core::message::Image;
use serde_json::{Value, json};

use crate::document;

const TIMEOUT: Duration = Duration::from_secs(20);
/// Bytes of a web page read before the rest is ignored; documents may be up to
/// `document::MAX_BYTES`, read whole.
pub const MAX_BYTES: usize = 3_000_000;
/// Characters returned per call; `offset` pages through the rest.
const MAX_CHARS: usize = 15_000;
const MAX_REDIRECTS: usize = 5;
/// Wrap width for rendered HTML.
const WIDTH: usize = 100;
/// A fetched page: final URL, text, and a PDF's preview note and page images.
type Fetched = (String, String, Option<(String, Vec<Image>)>);

pub fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "fetch_url".to_owned(),
        description: format!(
            "Read a public web page (http or https) as plain text, e.g. a search result, docs page, or \
             article. Returns up to {MAX_CHARS} characters; for longer pages, call again with the \
             `offset` it gives you. When it fails (e.g. HTTP 403), comes back empty or nearly so, asks \
             for JavaScript, or needs a sign-in, open the page with a browser tool if you have one, \
             instead of giving up. Private and local network addresses are refused."
        ),
        parameters: json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "The page to read."},
                "offset": {
                    "type": "integer",
                    "description": "Character offset to continue from, for long pages. Default 0.",
                },
            },
            "required": ["url"],
        }),
    }
}

#[derive(Debug)]
pub struct Fetcher {
    client: reqwest::Client,
}

impl Fetcher {
    pub fn new() -> Result<Self> {
        let redirects = Policy::custom(|attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else if let Err(err) = check_url(attempt.url()) {
                attempt.error(err.to_string())
            } else {
                attempt.follow()
            }
        });
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(concat!("mokuzu/", env!("CARGO_PKG_VERSION")))
            .dns_resolver(Arc::new(PublicOnly))
            .redirect(redirects)
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self { client })
    }

    /// Runs one tool call; failures come back as text for the model. A PDF's first pages come
    /// back as images too, on the first call for it (`offset` 0).
    pub async fn run(&self, args: &Value) -> (String, Vec<Image>) {
        let Some(url) = args["url"].as_str().map(str::trim) else {
            return ("error: `url` is required".to_owned(), Vec::new());
        };
        let offset = args["offset"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(0);
        match self.fetch(url, offset == 0).await {
            Ok((final_url, text, preview)) => {
                let mut out = page(&final_url, &text, offset);
                let Some((note, images)) = preview else {
                    return (out, Vec::new());
                };
                out.push_str(&format!("\n\n{note}"));
                (out, images)
            }
            Err(err) => (format!("error: {err:#}"), Vec::new()),
        }
    }

    /// Returns the final URL after redirects, the page as text, and with `preview`, a PDF's
    /// first pages as images and a note about them.
    async fn fetch(&self, url: &str, preview: bool) -> Result<Fetched> {
        let (final_url, content_type, body) = self.download(url).await?;
        if document::is_document(&body) {
            let preview = if preview && body.starts_with(b"%PDF-") {
                document::preview(body.clone(), &final_url).await
            } else {
                None
            };
            return Ok((final_url, document::extract(body).await?, preview));
        }
        Ok((final_url, to_text(&content_type, &body)?, None))
    }

    /// Returns the final URL after redirects, the lowercased content type, and the body: whole for
    /// a document up to `document::MAX_BYTES`, cut to `MAX_BYTES` for anything else.
    pub async fn download(&self, url: &str) -> Result<(String, String, Vec<u8>)> {
        let url = Url::parse(url).context("not a valid URL")?;
        check_url(&url)?;
        let mut response = self
            .client
            .get(url)
            .send()
            .await
            .context("request failed")?;
        let status = response.status();
        if !status.is_success() {
            bail!("the server returned HTTP {status}");
        }
        let final_url = response.url().to_string();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.context("failed to read the page")? {
            body.extend_from_slice(&chunk);
            if document::is_document(&body) {
                if body.len() > document::MAX_BYTES {
                    bail!("the document is over {} bytes", document::MAX_BYTES);
                }
            } else if body.len() >= MAX_BYTES {
                body.truncate(MAX_BYTES);
                break;
            }
        }
        Ok((final_url, content_type, body))
    }
}

/// Only http(s), and never a literal private or local IP (hostnames are checked at DNS time).
fn check_url(url: &Url) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https") {
        bail!("only http and https URLs can be read");
    }
    let host = url.host_str().context("the URL has no host")?;
    if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>()
        && !is_public(ip)
    {
        bail!("{host} is a private or local address");
    }
    Ok(())
}

/// Resolves hostnames but drops private and local addresses, so neither DNS nor redirects reach them.
struct PublicOnly;

impl Resolve for PublicOnly {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|addr| is_public(addr.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(format!("{host} resolves only to private or local addresses").into());
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || a == 0
                // 100.64.0.0/10: carrier-grade NAT, also Tailscale.
                || (a == 100 && b & 0xc0 == 64))
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_public(IpAddr::V4(v4)),
            None => {
                let first = v6.segments()[0];
                !(v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_multicast()
                    // fc00::/7 unique local, fe80::/10 link-local.
                    || first & 0xfe00 == 0xfc00
                    || first & 0xffc0 == 0xfe80)
            }
        },
    }
}

/// HTML becomes wrapped text; other text types pass through. Assumes UTF-8.
// ponytail: no charset detection; add encoding_rs if Big5 or GBK pages show up garbled.
fn to_text(content_type: &str, body: &[u8]) -> Result<String> {
    let html = content_type.contains("html")
        || (content_type.is_empty() && body.trim_ascii_start().starts_with(b"<"));
    let text = if html {
        html2text::from_read(body, WIDTH).context("failed to render the page")?
    } else if content_type.starts_with("text/")
        || content_type.contains("json")
        || content_type.contains("xml")
        || (content_type.is_empty() && std::str::from_utf8(body).is_ok())
    {
        String::from_utf8_lossy(body).into_owned()
    } else {
        bail!("can't read {content_type} content as text");
    };
    // Collapse runs of blank lines left by layout markup.
    let mut out = String::with_capacity(text.len());
    let mut blank = 0;
    for line in text.lines() {
        let line = line.trim_end();
        blank = if line.is_empty() { blank + 1 } else { 0 };
        if blank <= 1 {
            out.push_str(line);
            out.push('\n');
        }
    }
    Ok(out.trim().to_owned())
}

/// One page of `text` starting at character `offset`, with a pointer to the next page.
fn page(url: &str, text: &str, offset: usize) -> String {
    let total = text.chars().count();
    if total == 0 {
        return format!(
            "URL: {url}\nThe page has no readable text; it may need JavaScript \
             or a login; try a browser tool."
        );
    }
    if offset >= total && total > 0 {
        return format!("error: offset {offset} is past the end ({total} characters)");
    }
    let chunk: String = text.chars().skip(offset).take(MAX_CHARS).collect();
    let end = offset + chunk.chars().count();
    let mut out = format!("URL: {url}\nCharacters {offset}-{end} of {total}\n\n{chunk}");
    if end < total {
        out.push_str(&format!(
            "\n\n[{} more characters; call fetch_url again with offset={end}]",
            total - end
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_and_local_addresses_are_refused() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254",
            "100.100.1.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:192.168.1.1",
        ] {
            assert!(!is_public(ip.parse().expect("ip")), "{ip}");
        }
        for ip in ["1.1.1.1", "140.112.8.116", "2606:4700::1111"] {
            assert!(is_public(ip.parse().expect("ip")), "{ip}");
        }
        let url = |s: &str| Url::parse(s).expect("url");
        assert!(check_url(&url("http://127.0.0.1:8080/")).is_err());
        assert!(check_url(&url("http://[::1]/")).is_err());
        assert!(check_url(&url("file:///etc/passwd")).is_err());
        assert!(check_url(&url("https://example.com/")).is_ok());
    }

    #[tokio::test]
    async fn hostnames_resolving_to_loopback_are_refused() {
        let fetcher = Fetcher::new().expect("client");
        let out = fetcher.run(&json!({"url": "http://localhost:9/"})).await.0;
        assert!(out.starts_with("error:"), "{out}");
        let out = fetcher.run(&json!({"url": "http://127.0.0.1:9/"})).await.0;
        assert!(out.contains("private or local"), "{out}");
    }

    #[test]
    fn html_renders_to_text_and_binary_is_refused() {
        let html = b"<html><head><style>p{}</style><script>x()</script></head>\
                     <body><h1>Title</h1><p>Hello <b>world</b></p>\n\n\n<p>Bye</p></body></html>";
        let text = to_text("text/html; charset=utf-8", html).expect("html");
        assert!(
            text.contains("Title") && text.contains("Hello **world**") && text.contains("Bye"),
            "{text:?}"
        );
        assert!(!text.contains("x()") && !text.contains("p{}"), "{text}");
        assert!(!text.contains("\n\n\n"));
        assert_eq!(
            to_text("application/json", b"{\"a\":1}").expect("json"),
            "{\"a\":1}"
        );
        assert!(to_text("image/png", b"\x89PNG").is_err());
    }

    #[test]
    fn page_splits_long_text() {
        let text = "字".repeat(MAX_CHARS + 10);
        let first = page("https://x", &text, 0);
        assert!(first.contains(&format!("Characters 0-{MAX_CHARS} of {}", MAX_CHARS + 10)));
        assert!(first.ends_with(&format!("offset={MAX_CHARS}]")));
        let rest = page("https://x", &text, MAX_CHARS);
        assert!(rest.ends_with(&"字".repeat(10)));
        assert!(page("https://x", "short", 99).starts_with("error:"));
        assert!(page("https://x", "", 0).contains("browser tool"));
    }
}
