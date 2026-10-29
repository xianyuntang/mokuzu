---
name: config-coder
description: Rules for changing mokuzu's config file format. Use whenever you add, rename, remove, or change a key in config.toml (crates/mokuzu/src/config.rs), so the `mokuzu configure` TUI (crates/mokuzu/src/onboarding.rs) and the docs change in the same commit.
---

# Config Coder

Every config key a user may set must be settable from `mokuzu configure`.
A config change is not done until the TUI, docs, and tests match it.

## Checklist

Change all of these in the same commit:

1. **`crates/mokuzu/src/config.rs`** and **`config/migrate.rs`**
   - Raw `File*` struct field (serde, `deny_unknown_fields`).
   - Resolved `Config` field, with validation in `Config::parse`; no default (see below).
   - A test in `config.rs` tests for parsing and for rejecting bad values.
2. **`crates/mokuzu/src/onboarding.rs` (the TUI)**
   - Field index const. Keep `MCP_FIRST` last and bump it; MCP Keep/Remove rows start there.
   - `fields.push(field(step, label, kind, value))` in `Form::new`, prefilled from `current`
     (the loaded `Config`), else the same default `config.rs` uses.
     - Pick the `STEP_*` const that fits; general settings go in `STEP_ADVANCED`.
     - `Kind::Select` for a small fixed set (use `options_with` so hand-edited values survive),
       `Kind::Secret` for tokens and passwords, `Kind::Text` otherwise.
   - Conditional visibility in `Form::shown` if it depends on another field.
   - Validation in a `Form` method, called from `check_step` for its step and from `answers`.
     Return the same error rules as `config.rs`, so the user sees them before saving.
   - Field in `Answers`, written by `render_toml`. Top-level keys go before the first `[table]`.
   - Update `rendered_config_parses_back` and `steps_validate_and_hide_conditional_fields`.
3. **`config.example.toml`**: commented entry with what it does and its default.
4. **`README.md`**: update where the key's behavior is described, if anywhere.
5. **`crates/mokuzu/src/settings.rs`** (agent's `settings` tool): add the key to `KEYS`
   only if the model should be able to change it. Never add secrets.

## No hidden keys

Every key goes on the form, even rarely touched ones (they go in Advanced). A key missing from
the form is silently reset by `mokuzu configure`.

## No defaults; migrations instead

Every setting is spelled out in the file. `File` fields are required (no `#[serde(default)]`, no
`unwrap_or` in `Config::parse`); only whole optional features (a `[discord]` or
`[tools.searxng]` section, an MCP server) may be absent. `render_toml` writes every key, turning
an empty form field into its value (see `ModelOptions`).

A format change (new key, rename, removal, new meaning) bumps `VERSION` in
`crates/mokuzu/src/config/migrate.rs` and adds a step to `MIGRATIONS` that rewrites a file of the
previous version with `toml_edit`, keeping comments: fill a new key with the value mokuzu used
before, rename, or drop. `Config::load` runs pending steps on start and saves the result (the old
file stays as `config.toml.bak`); `Config::parse` runs them in memory. Never edit a released
step; add a new one. Test the step on an old file, and that `config.example.toml` and
`render_toml` output need no migration.

## Verify

```sh
cargo clippy -p mokuzu --all-targets
cargo test -p mokuzu
```

Then run `mokuzu configure` against a copy of a real config, save, and diff the file.
