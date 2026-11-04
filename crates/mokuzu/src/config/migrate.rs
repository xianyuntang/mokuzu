//! Config migrations: each one rewrites a config file from one format version to the next, so a
//! format change upgrades existing files instead of keeping every old shape loadable. A file's
//! version is its top-level `version`; files from before versioning are version 0.

use std::path::Path;

use anyhow::{Context, Result, bail};
use toml_edit::{DocumentMut, InlineTable, Item, Table, TableLike, value};

use super::{
    DEFAULT_COMPACT_AT_TOKENS, DEFAULT_MAX_TOKENS, DEFAULT_SCREEN, DEFAULT_VIEWER_PORT, Provider,
};

/// The format this build reads and writes.
pub const VERSION: i64 = 3;

/// `MIGRATIONS[n]` turns a version `n` file into a version `n + 1` one. To change the format, add
/// a step here and bump `VERSION`.
const MIGRATIONS: [fn(&mut DocumentMut) -> Result<()>; VERSION as usize] =
    [spell_out_defaults, add_browser, built_in_browser];

/// `text` upgraded to `VERSION`, comments and order kept; `None` when it is already there.
pub fn migrate(text: &str) -> Result<Option<String>> {
    let mut doc: DocumentMut = text.parse().context("config is not valid TOML")?;
    let version = match doc.get("version") {
        None => 0,
        Some(item) => item.as_integer().context("version must be an integer")?,
    };
    if version == VERSION {
        return Ok(None);
    }
    let Some(steps) = usize::try_from(version)
        .ok()
        .and_then(|from| MIGRATIONS.get(from..))
    else {
        bail!(
            "config version {version} is not one this mokuzu reads (up to {VERSION}); \
             run `mokuzu update`"
        );
    };
    for step in steps {
        step(&mut doc)?;
    }
    doc["version"] = value(VERSION);
    Ok(Some(doc.to_string()))
}

/// 0 → 1: every setting is written out, so the file says what mokuzu runs with. Version 0 filled
/// in defaults for missing keys; this writes those defaults in, names the provider section in use
/// as `model.provider` (so a file can keep both providers' keys), and drops the leftover
/// `[tools.bash]` and `[tools.claude_code]`. Values it cannot place (no provider section) are
/// left for `parse` to report.
fn spell_out_defaults(doc: &mut DocumentMut) -> Result<()> {
    let present: Vec<Provider> = Provider::ALL
        .into_iter()
        .filter(|provider| doc.contains_key(provider.section()))
        .collect();
    for provider in &present {
        fill(
            table(doc, &[provider.section()])?,
            "base_url",
            provider.default_base_url(),
        );
    }
    let model = table(doc, &["model"])?;
    if let [provider] = present[..] {
        fill(model, "provider", provider.section());
    }
    let provider = match model.get("provider").and_then(Item::as_str) {
        Some("anthropic") => Some(Provider::Anthropic),
        Some("opencode-go") => Some(Provider::OpencodeGo),
        Some("github-copilot") => Some(Provider::GithubCopilot),
        Some("chatgpt") => Some(Provider::Chatgpt),
        _ => None,
    };
    if let Some(provider) = provider {
        fill(model, "name", provider.default_model());
        if let Some(name) = model.get("name").and_then(Item::as_str) {
            let api = provider.api_for(name).name();
            fill(model, "api", api);
        }
    }
    fill(model, "max_tokens", i64::from(DEFAULT_MAX_TOKENS));
    fill(
        model,
        "compact_at_tokens",
        i64::try_from(DEFAULT_COMPACT_AT_TOKENS)?,
    );
    let name = model.get("name").and_then(Item::as_str).map(str::to_owned);

    if !doc.contains_key("timezone") {
        doc["timezone"] = value(super::system_timezone().name());
    }
    let approval = table(doc, &["approval"])?;
    fill(approval, "mode", "ask");
    match approval.get("mode").and_then(Item::as_str) {
        Some("auto") => {
            if let Some(name) = name {
                fill(approval, "model", name);
            }
        }
        // Version 0 ignored a reviewer under `ask`; version 1 refuses one.
        _ => {
            approval.remove("model");
        }
    }
    fill(table(doc, &["log"])?, "level", "warn");
    let database = std::env::home_dir()
        .context("cannot find home directory for the default database path")?
        .join(".local/share/mokuzu/mokuzu.db");
    fill(
        table(doc, &["database"])?,
        "path",
        database.to_string_lossy().as_ref(),
    );
    let browser = table(doc, &["browser"])?;
    fill(browser, "width", i64::from(DEFAULT_SCREEN.0));
    fill(browser, "height", i64::from(DEFAULT_SCREEN.1));
    fill(browser, "viewer_port", i64::from(DEFAULT_VIEWER_PORT));

    if let Some(tools) = doc.get_mut("tools").and_then(Item::as_table_like_mut) {
        tools.remove("bash");
        tools.remove("claude_code");
        for search in ["searxng", "brave"] {
            if let Some(search) = tools.get_mut(search).and_then(Item::as_table_like_mut) {
                fill(search, "results", 5);
            }
        }
        if tools.is_empty() {
            doc.remove("tools");
        }
    }
    let servers = doc
        .get_mut("mcp")
        .and_then(|mcp| mcp.get_mut("servers"))
        .and_then(Item::as_table_like_mut);
    for (_, server) in servers.into_iter().flat_map(|servers| servers.iter_mut()) {
        if let Some(server) = server.as_table_like_mut() {
            fill(server, "approve", true);
        }
    }
    let guilds = doc
        .get_mut("discord")
        .and_then(|discord| discord.get_mut("guilds"))
        .and_then(Item::as_table_like_mut);
    for (_, guild) in guilds.into_iter().flat_map(|guilds| guilds.iter_mut()) {
        if let Some(guild) = guild.as_table_like_mut() {
            fill(guild, "mention_only", false);
        }
    }
    Ok(())
}

/// Name of the built-in browser's MCP server, and so the prefix of its tools.
pub const BROWSER_SERVER: &str = "browser";
/// The package the built-in browser runs; a server whose arguments name it is that browser.
pub const BROWSER_PACKAGE: &str = "invisible-playwright-mcp";

/// `[mcp.servers.browser]` as version 2 wrote it: a Firefox patched so sites see an ordinary
/// browser, drawing on the virtual desktop when there is no screen, keeping sign-ins in a
/// profile next to `database`.
fn browser_server(database: &Path) -> Table {
    let profile = database
        .parent()
        .unwrap_or(Path::new("."))
        .join("stealthfox");
    let mut env = InlineTable::new();
    env.insert("STEALTHFOX_HEADLESS", "0".into());
    env.insert(
        "STEALTHFOX_PROFILE_DIR",
        profile.to_string_lossy().as_ref().into(),
    );
    let mut server = Table::new();
    server["command"] = value("uvx");
    server["args"] = value(toml_edit::Array::from_iter([BROWSER_PACKAGE]));
    server["env"] = value(env);
    // It only browses, so it runs without asking, like `fetch_url`.
    server["approve"] = value(false);
    server
}

/// 1 → 2: mokuzu comes with a browser, as `[mcp.servers.browser]`, unless a server already runs
/// it or the name is taken.
fn add_browser(doc: &mut DocumentMut) -> Result<()> {
    let database = doc
        .get("database")
        .and_then(|database| database.get("path"))
        .and_then(Item::as_str)
        .context("database.path is missing")?
        .to_owned();
    let mcp = implicit_table(doc.as_table_mut(), "mcp")?;
    let servers = implicit_table(mcp, "servers")?;
    let runs_it = servers.iter().any(|(_, server)| runs_browser(server));
    if !runs_it && !servers.contains_key(BROWSER_SERVER) {
        servers.insert(
            BROWSER_SERVER,
            Item::Table(browser_server(Path::new(&database))),
        );
    }
    Ok(())
}

/// Whether an MCP server table runs the browser package.
fn runs_browser(server: &Item) -> bool {
    server
        .get("args")
        .and_then(Item::as_array)
        .is_some_and(|args| {
            args.iter().any(|arg| {
                arg.as_str()
                    .is_some_and(|arg| arg.contains(BROWSER_PACKAGE))
            })
        })
}

/// 2 → 3: the browser is built in, turned on by `browser.enabled`, instead of an MCP server.
/// `[mcp.servers.browser]`, when it runs the browser, is dropped for `enabled = true` with its
/// `approve`; without it (removed, or the user's own server under another name) the built-in
/// one stays off.
fn built_in_browser(doc: &mut DocumentMut) -> Result<()> {
    let servers = doc
        .get_mut("mcp")
        .and_then(|mcp| mcp.get_mut("servers"))
        .and_then(Item::as_table_like_mut);
    let ours = servers.and_then(|servers| {
        servers
            .get(BROWSER_SERVER)
            .is_some_and(runs_browser)
            .then(|| servers.remove(BROWSER_SERVER))
            .flatten()
    });
    let approve = ours
        .as_ref()
        .and_then(|server| server.get("approve"))
        .and_then(Item::as_bool)
        .unwrap_or(false);
    let browser = table(doc, &["browser"])?;
    fill(browser, "enabled", ours.is_some());
    fill(browser, "approve", approve);
    Ok(())
}

/// The table `name` in `parent`, created without a header of its own when missing.
fn implicit_table<'a>(parent: &'a mut dyn TableLike, name: &str) -> Result<&'a mut dyn TableLike> {
    parent
        .entry(name)
        .or_insert_with(|| {
            let mut table = Table::new();
            table.set_implicit(true);
            Item::Table(table)
        })
        .as_table_like_mut()
        .with_context(|| format!("`{name}` in the config is not a table"))
}

/// The table at `path`, created empty when missing.
fn table<'a>(doc: &'a mut DocumentMut, path: &[&str]) -> Result<&'a mut dyn TableLike> {
    let mut table: &mut dyn TableLike = doc.as_table_mut();
    for name in path {
        table = table
            .entry(name)
            .or_insert_with(toml_edit::table)
            .as_table_like_mut()
            .with_context(|| format!("`{name}` in the config is not a table"))?;
    }
    Ok(table)
}

/// Sets `key` in `table` unless the file already has it.
fn fill(table: &mut dyn TableLike, key: &str, default: impl Into<toml_edit::Value>) {
    if !table.contains_key(key) {
        table.insert(key, value(default));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_0_spells_out_every_default_and_keeps_comments() {
        let old = "# mine\n[model]\nname = \"glm-5.3\"\n\n[opencode-go]\napi_key = \"k\"\n\n\
                   [tools.bash]\ntimeout_secs = 5\n\n[mcp.servers.fs]\ncommand = \"fs\"\n";
        let new = migrate(old).expect("valid").expect("migrated");
        assert!(new.contains("# mine\n[model]"), "comment kept: {new}");
        for line in [
            "version = 3",
            "timezone = ",
            "provider = \"opencode-go\"",
            "api = \"openai\"",
            "max_tokens = 16000",
            "compact_at_tokens = 200000",
            "base_url = \"https://opencode.ai/zen/go/v1\"",
            "mode = \"ask\"",
            "level = \"warn\"",
            "path = ",
            "width = 1280",
            "viewer_port = 22050",
            "approve = true",
        ] {
            assert!(new.contains(line), "no {line:?} in {new}");
        }
        assert!(!new.contains("bash"), "leftover kept: {new}");
        assert!(
            new.find("version").expect("version") < new.find("[model]").expect("model"),
            "top-level keys come before the tables: {new}"
        );
        assert!(migrate(&new).expect("valid").is_none(), "already current");
        super::super::Config::parse(&new).expect("parses");
    }

    #[test]
    fn version_0_keeps_what_is_set() {
        let old = "timezone = \"Asia/Taipei\"\n[model]\nname = \"m\"\napi = \"gemini\"\n\
                   [anthropic]\napi_key = \"k\"\n[approval]\nmode = \"auto\"\n";
        let new = migrate(old).expect("valid").expect("migrated");
        assert!(new.contains("timezone = \"Asia/Taipei\""), "{new}");
        assert!(new.contains("api = \"gemini\""), "{new}");
        assert!(new.contains("provider = \"anthropic\""), "{new}");
        assert!(
            new.contains("model = \"m\""),
            "reviewer is the main model: {new}"
        );
    }

    #[test]
    fn version_1_gets_the_browser_unless_one_is_there() {
        let old = "version = 1\n[database]\npath = \"/home/me/.local/share/mokuzu/mokuzu.db\"\n\
                   [browser]\nwidth = 1280\n";
        let new = migrate(old).expect("valid").expect("migrated");
        assert!(new.contains("enabled = true\napprove = false"), "{new}");
        assert!(!new.contains("[mcp"), "no leftover MCP tables: {new}");
        assert!(migrate(&new).expect("valid").is_none(), "already current");

        let mine = format!(
            "{old}[mcp.servers.stealth]\ncommand = \"uvx\"\nargs = [\"invisible-playwright-mcp\"]\napprove = true\n"
        );
        let new = migrate(&mine).expect("valid").expect("migrated");
        assert!(
            new.contains("[mcp.servers.stealth]"),
            "kept the user's own: {new}"
        );
        assert!(new.contains("enabled = false"), "no second browser: {new}");
    }

    #[test]
    fn version_2_turns_the_browser_server_into_the_built_in_one() {
        let base = "version = 2\n[browser]\nwidth = 1280\n";
        let ours = format!(
            "{base}[mcp.servers.browser]\ncommand = \"uvx\"\nargs = [\"invisible-playwright-mcp\"]\n\
             approve = true\n[mcp.servers.fs]\ncommand = \"fs\"\napprove = true\n"
        );
        let new = migrate(&ours).expect("valid").expect("migrated");
        assert!(!new.contains("servers.browser"), "moved: {new}");
        assert!(new.contains("[mcp.servers.fs]"), "others kept: {new}");
        assert!(new.contains("enabled = true\napprove = true"), "{new}");

        let removed = migrate(base).expect("valid").expect("migrated");
        assert!(
            removed.contains("enabled = false\napprove = false"),
            "{removed}"
        );

        let theirs = format!("{base}[mcp.servers.browser]\nurl = \"https://x\"\napprove = true\n");
        let new = migrate(&theirs).expect("valid").expect("migrated");
        assert!(
            new.contains("[mcp.servers.browser]"),
            "not ours, kept: {new}"
        );
        assert!(new.contains("enabled = false"), "{new}");
    }

    #[test]
    fn unknown_versions_are_refused() {
        assert!(migrate(&format!("version = {}\n", VERSION + 1)).is_err());
        assert!(migrate("version = -1\n").is_err());
        assert!(migrate("version = \"1\"\n").is_err());
    }
}
