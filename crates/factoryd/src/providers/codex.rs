//! Non-interactive Codex provider.
//!
//! Each admitted run gets one fresh `codex exec` process and one startup
//! input. Output remains opaque; authority comes from the daemon's run state,
//! never from a provider stream decoder or a resumable process.

use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
};

use crate::providers::{
    Capabilities, Provider, ProviderError, ProviderLaunch, SpawnContext, hooks,
};

pub const PERMISSION_MODES: [&str; 2] = ["on-request", "never"];

const HOOKS_BEGIN_MARKER: &str = "# --- dark-factory hooks BEGIN ---";
const HOOKS_END_MARKER: &str = "# --- dark-factory hooks END ---";
/// Trust and sandbox settings, in a marker block separate from
/// [`HOOKS_BEGIN_MARKER`]/[`HOOKS_END_MARKER`] (not because either could
/// not hold both, but because they are independent concerns changed by
/// independent code paths -- keeping them apart means a future change to
/// one never risks fumbling the other's exact regenerated shape).
const CONFIG_BEGIN_MARKER: &str = "# --- dark-factory config BEGIN ---";
const CONFIG_END_MARKER: &str = "# --- dark-factory config END ---";
/// `sandbox_mode` is a root-table key, not something that can live inside
/// either marker block above: both blocks are appended at the end of the
/// file, after any `[table]` headers the operator's own copied-forward
/// config.toml already had (Codex's own project trust entries, in
/// particular -- confirmed against this machine's real `~/.codex/
/// config.toml`, which ends with dozens of `[projects."..."]` tables). A
/// bare `key = value` line appended after those would silently become a
/// member of the *last* table in the file instead of the root table Codex
/// actually reads `sandbox_mode` from. [`insert_root_level_line`] instead
/// inserts this immediately before the first `[table]` header anywhere in
/// the document (or at the very end, if there is none), which is always a
/// valid root-table position regardless of what the source file looked
/// like.
const SANDBOX_MODE_COMMENT_AND_LINE: &str = "\
# --- dark-factory sandbox_mode override (kept as a plain root-table key,
# not inside the config block below -- see CodexProvider's own comment) ---
sandbox_mode = \"workspace-write\"";
const MINIMAL_CONFIG_TOML: &str =
    "# Dark Factory generated Codex home (no ~/.codex/config.toml was found to copy).\n";

/// The native posture used when factory auto mode is off and an agent has
/// no explicit override. Auto mode uses Codex's full bypass flag instead;
/// an explicit `on-request`/`never` profile always wins.
const DEFAULT_APPROVAL_POLICY: &str = "on-request";

/// Top-level tables never copied from the operator's real `~/.codex/
/// config.toml` into a fresh attempt-owned seed:
///
/// - `mcp_servers`: the concrete bug this exists to fix -- Codex stalls at
///   "Starting MCP servers" launching every one of the operator's own MCP
///   servers inside a headless factory attempt that never needed
///   them, several of which expect an interactive terminal/browser/local
///   dev server that is not there.
/// - `projects`: the operator's own per-repo trust decisions (which repos
///   *they* have approved running Codex against unprompted) have no
///   bearing on an attempt's own daemon-owned source, which
///   `rewrite_config_block` already grants trust to explicitly, every
///   spawn, on its own terms.
/// - `hooks`: covers both `[[hooks.<Event>]]`/`[[hooks.<Event>.hooks]]`
///   (the operator's own hook commands, which must never run inside a
///   daemon-owned attempt -- this seed's whole point is that
///   `rewrite_hooks_block` is the *only* source of hooks here) and the
///   plain `[hooks.state]` table Codex persists trust decisions into
///   (`docs/providers.md` documents the exact shape found on a real
///   machine) -- neither belongs in an isolated `CODEX_HOME` that never
///   asks for hook trust in the first place
///   (`--dangerously-bypass-hook-trust`).
///
/// Anything else -- `model`, `model_provider`/`model_providers.*`,
/// `approval_policy`, the operator's own `sandbox_mode` (immediately
/// overridden by `rewrite_config_block` anyway, but harmless to inherit as
/// a starting point), and any other root-level scalar -- is kept: those
/// are the provider settings an isolated attempt may inherit, and
/// none of them reference the operator's own environment.
const DROPPED_SEED_TABLES: [&str; 3] = ["mcp_servers", "projects", "hooks"];

/// Filters `document` (a copy of the operator's real `config.toml`) down
/// to the allow-list [`DROPPED_SEED_TABLES`] documents, for the *initial*
/// seed only (`seed_codex_home_once` never overwrites an existing seeded
/// `config.toml` -- this runs once for a new attempt home). Not a
/// general TOML parser, same tradeoff every other marker/table scanner in
/// this module already makes (`strip_marked_block`,
/// `first_table_header_offset`): a table is identified purely by its
/// `[table]`/`[[array-of-tables]]` header line, dropped (header line
/// through the line before the next header) if its top-level key --
/// everything before the first `.` inside the brackets -- is in
/// [`DROPPED_SEED_TABLES`]. Root-level scalars (before the first header)
/// are never dropped by this function.
fn filter_operator_config_for_seed(document: &str) -> String {
    let mut kept = String::with_capacity(document.len());
    let mut dropping = false;
    for line in document.split_inclusive('\n') {
        if line.trim_start().starts_with('[') {
            dropping = table_header_top_level_key(line)
                .is_some_and(|key| DROPPED_SEED_TABLES.contains(&key.as_str()));
        }
        if !dropping {
            kept.push_str(line);
        }
    }
    kept
}

/// The top-level key of a `[table]`/`[[array-of-tables]]` header line --
/// `"hooks"` for both `[hooks.state]` and `[[hooks.SessionStart.hooks]]`,
/// `"projects"` for `[projects."/abs/path"]` -- or `None` if `line` is not
/// a recognizable header line at all.
fn table_header_top_level_key(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let inner = trimmed
        .strip_prefix("[[")
        .and_then(|rest| rest.strip_suffix("]]"))
        .or_else(|| {
            trimmed
                .strip_prefix('[')
                .and_then(|rest| rest.strip_suffix(']'))
        })?;
    let key = inner.split('.').next().unwrap_or(inner).trim();
    Some(key.trim_matches('"').to_owned())
}

/// Non-interactive [`Provider`] for Codex. Launches `codex exec
/// --dangerously-bypass-hook-trust [--model M]
/// (--dangerously-bypass-approvals-and-sandbox | -c approval_policy=M)
/// -`, reading exactly one task from stdin, with `CODEX_HOME` pointed at
/// this attempt's generated home. `--dangerously-bypass-hook-trust` is
/// unconditional: the hooks
/// this provider writes are 100% daemon-authored into an isolated
/// `CODEX_HOME` the operator never hand-edits, which already is the
/// vetting Codex's normal hook-trust prompt would otherwise ask for. See
/// `docs/providers.md`.
pub struct CodexProvider {
    /// The source used to seed a fresh attempt-owned `CODEX_HOME`
    /// (`config.toml`, `auth.json`): the daemon's own `$CODEX_HOME` if set
    /// — Codex's own convention, and how a factory runs on a different
    /// account than the operator's shell (`CODEX_HOME=~/.codex-dogfood`
    /// in the launchd job) — else `$HOME/.codex`; overridable for tests via
    /// [`CodexProvider::with_source_home`].
    source_home: Option<PathBuf>,
}

impl CodexProvider {
    /// Resolves the seed source exactly as `codex` itself resolves its home:
    /// `$CODEX_HOME` if set, else `$HOME/.codex`. `None` (neither set)
    /// means a fresh attempt home always starts from
    /// [`MINIMAL_CONFIG_TOML`] with no `auth.json` link — Codex will then
    /// have no subscription credentials, same as running `codex` with no
    /// prior login.
    #[must_use]
    pub fn new() -> Self {
        Self::from_environment(std::env::var_os("CODEX_HOME"), std::env::var_os("HOME"))
    }

    fn from_environment(codex_home: Option<OsString>, home: Option<OsString>) -> Self {
        Self {
            source_home: codex_home
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .or_else(|| home.map(|home| PathBuf::from(home).join(".codex"))),
        }
    }

    #[cfg(test)]
    fn with_source_home(source_home: PathBuf) -> Self {
        Self {
            source_home: Some(source_home),
        }
    }
}

impl Default for CodexProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for CodexProvider {
    fn spawn_spec(&self, ctx: &SpawnContext) -> Result<ProviderLaunch, ProviderError> {
        let codex_home = ctx.agent_dir.join("codex-home");
        seed_codex_home_once(&codex_home, self.source_home.as_deref())?;
        rewrite_hooks_block(&codex_home, &ctx.factoryctl_path, &ctx.hook_token_path)?;
        rewrite_config_block(&codex_home, &ctx.source_root)?;

        let mut args = vec![
            "exec".to_owned(),
            "--dangerously-bypass-hook-trust".to_owned(),
        ];
        if let Some(model) = &ctx.model {
            args.push("--model".to_owned());
            args.push(model.clone());
        }
        if let Some(reasoning_effort) = &ctx.reasoning_effort {
            args.push("-c".to_owned());
            args.push(format!("model_reasoning_effort=\"{reasoning_effort}\""));
        }
        // Always explicit -- never Codex's own un-set `on-request` default
        // -- so an unattended agent never silently inherits a native
        // approval prompt nobody is there to answer. See
        // `DEFAULT_APPROVAL_POLICY`'s own doc comment.
        if ctx.permission_mode.is_none() && ctx.auto_mode {
            args.push("--dangerously-bypass-approvals-and-sandbox".to_owned());
        } else {
            let approval_policy = ctx
                .permission_mode
                .as_deref()
                .unwrap_or(DEFAULT_APPROVAL_POLICY);
            args.push("-c".to_owned());
            args.push(format!("approval_policy=\"{approval_policy}\""));
        }
        args.push("-".to_owned());

        Ok(ProviderLaunch {
            program: PathBuf::from("codex"),
            args,
            env: vec![(
                "CODEX_HOME".to_owned(),
                codex_home.to_string_lossy().into_owned(),
            )],
            startup_input: ctx.startup_input.clone(),
        })
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::for_provider(factory_core::Provider::Codex, &PERMISSION_MODES)
    }
}

/// Idempotently seeds `codex_home` (mode `0700`, created if missing) the
/// first time it is used: copies `source_home/config.toml` if present
/// (filtered down to what a factory worker needs by
/// [`filter_operator_config_for_seed`] -- see [`DROPPED_SEED_TABLES`]),
/// else writes [`MINIMAL_CONFIG_TOML`]; copies every `source_home/rules/
/// *.rules` file present (an operator's own approval *and* `forbid` rules
/// -- see [`seed_rules_directory`]'s own doc comment for why these are
/// copied, unlike `config.toml`'s `mcp_servers`/`projects`/`hooks`, which
/// are deliberately dropped); symlinks `source_home/auth.json` if present,
/// re-pointing a link the daemon made when the seed home changed. Existing
/// files are never overwritten by this function — `config.toml` and each
/// copied `rules/*.rules` file are one-time seeds, not a sync (an
/// operator's or Codex's own later additions to either are never clobbered
/// by a later spawn); only the `auth.json` link follows the seed home. The
/// hooks and sandbox/trust config blocks are refreshed separately on every
/// spawn.
fn seed_codex_home_once(
    codex_home: &Path,
    source_home: Option<&Path>,
) -> Result<(), ProviderError> {
    hooks::ensure_private_dir(codex_home).map_err(|source| ProviderError::Seed {
        path: codex_home.to_path_buf(),
        source,
    })?;

    let config_path = codex_home.join("config.toml");
    if !config_path.exists() {
        let contents = source_home
            .map(|home| home.join("config.toml"))
            .and_then(|path| fs::read_to_string(path).ok())
            .map(|raw| filter_operator_config_for_seed(&raw).into_bytes())
            .unwrap_or_else(|| MINIMAL_CONFIG_TOML.as_bytes().to_vec());
        hooks::write_private_file(&config_path, &contents).map_err(|source| {
            ProviderError::Seed {
                path: config_path.clone(),
                source,
            }
        })?;
    }

    seed_rules_directory(codex_home, source_home)?;

    if let Some(source_home) = source_home {
        let auth_path = codex_home.join("auth.json");
        let source_auth = source_home.join("auth.json");
        // The credentials link follows the daemon's seed home: create it if
        // missing, re-point it if it is a link to somewhere else (the seed
        // home changed -- a different Codex account). A regular file, which
        // only an operator could have put there, is never touched.
        let existing_link = fs::read_link(&auth_path).ok();
        let is_regular_file =
            fs::symlink_metadata(&auth_path).is_ok_and(|m| !m.file_type().is_symlink());
        if source_auth.exists()
            && !is_regular_file
            && existing_link.as_deref() != Some(source_auth.as_path())
        {
            if existing_link.is_some() {
                fs::remove_file(&auth_path).map_err(|source| ProviderError::Seed {
                    path: auth_path.clone(),
                    source,
                })?;
            }
            std::os::unix::fs::symlink(&source_auth, &auth_path).map_err(|source| {
                ProviderError::Seed {
                    path: auth_path.clone(),
                    source,
                }
            })?;
        }
    }
    Ok(())
}

/// One-time seed (per file, like `config.toml`) of `codex_home/rules/`
/// from `source_home/rules/*.rules`, if any exist: an operator who has
/// hardened their own `~/.codex` with approval and `forbid` rules
/// otherwise loses every one of them in every factory agent, silently,
/// since `rules/` -- unlike `config.toml` -- was never read from the
/// source home at all before this. Unlike `config.toml`'s allow-listed
/// tables, nothing here is filtered: a `forbid` rule an operator wrote for
/// their own machine has no `mcp_servers`/`projects`/`hooks`-shaped reason
/// to be dropped for a factory worker. Each destination file is written
/// only if it does not already exist, so a later spawn never clobbers an
/// operator's or Codex's own edit to an already-seeded file.
fn seed_rules_directory(
    codex_home: &Path,
    source_home: Option<&Path>,
) -> Result<(), ProviderError> {
    let Some(source_home) = source_home else {
        return Ok(());
    };
    let source_rules_dir = source_home.join("rules");
    // `NotFound` is the expected common case -- most operators have no
    // `~/.codex/rules/` at all (same fallback shape as `config.toml`'s own
    // "no ~/.codex/config.toml was found"). Any other error (e.g. the
    // directory exists but is unreadable) is surfaced rather than treated
    // identically to "nothing to copy".
    let entries = match fs::read_dir(&source_rules_dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(ProviderError::Seed {
                path: source_rules_dir,
                source,
            });
        }
    };
    seed_rule_paths(
        codex_home,
        &source_rules_dir,
        entries.map(|entry| entry.map(|entry| entry.path())),
    )
}

/// Copies the fallible path stream returned by `read_dir`. Kept separate
/// only so the iterator's per-entry error case can be tested without
/// relying on a filesystem race.
fn seed_rule_paths(
    codex_home: &Path,
    source_rules_dir: &Path,
    paths: impl IntoIterator<Item = io::Result<PathBuf>>,
) -> Result<(), ProviderError> {
    let rules_dir = codex_home.join("rules");
    for path in paths {
        let path = path.map_err(|source| ProviderError::Seed {
            path: source_rules_dir.to_path_buf(),
            source,
        })?;
        if path.extension().and_then(|extension| extension.to_str()) != Some("rules") {
            continue;
        }
        let Some(name) = path.file_name() else {
            continue;
        };
        let destination = rules_dir.join(name);
        if destination.exists() {
            continue;
        }
        // A real read failure here (permission denied, a race where the
        // file disappeared after `read_dir` listed it) must not be
        // treated as "nothing to copy" and silently skipped: an
        // operator's own forbid rule failing to reach a fresh agent, with
        // no error anywhere, is exactly the silent-fallback AGENTS.md
        // rule 3 forbids -- fail the spawn instead, like every other
        // write in this file.
        let contents = fs::read(&path).map_err(|source| ProviderError::Seed {
            path: path.clone(),
            source,
        })?;
        hooks::write_private_file(&destination, &contents).map_err(|source| {
            ProviderError::Seed {
                path: destination.clone(),
                source,
            }
        })?;
    }
    Ok(())
}

/// Replaces the generated hook block with this attempt's authenticated
/// `PreToolUse` policy hook. Operator hooks were removed while seeding.
fn rewrite_hooks_block(
    codex_home: &Path,
    factoryctl_path: &Path,
    hook_token_path: &Path,
) -> Result<(), ProviderError> {
    let config_path = codex_home.join("config.toml");
    let existing = fs::read_to_string(&config_path).map_err(|source| ProviderError::Seed {
        path: config_path.clone(),
        source,
    })?;
    let mut rewritten = strip_hooks_block(&existing);
    if !rewritten.is_empty() {
        rewritten.push_str("\n\n");
    }
    rewritten.push_str(&hooks_block_toml(factoryctl_path, hook_token_path));
    hooks::write_private_file(&config_path, rewritten.as_bytes()).map_err(|source| {
        ProviderError::Seed {
            path: config_path.clone(),
            source,
        }
    })
}

/// Rewrites the generated trust block for the exact attempt source root.
fn rewrite_config_block(codex_home: &Path, source_root: &Path) -> Result<(), ProviderError> {
    let source_root = canonicalize_or_given(source_root);

    let config_path = codex_home.join("config.toml");
    let existing = fs::read_to_string(&config_path).map_err(|source| ProviderError::Seed {
        path: config_path.clone(),
        source,
    })?;
    let without_config_block =
        strip_marked_block(&existing, CONFIG_BEGIN_MARKER, CONFIG_END_MARKER);
    let without_sandbox_mode = strip_root_level_sandbox_mode(&without_config_block);
    let mut rewritten =
        insert_root_level_line(&without_sandbox_mode, SANDBOX_MODE_COMMENT_AND_LINE)
            .trim_end()
            .to_owned();
    rewritten.push_str("\n\n");
    rewritten.push_str(&config_block_toml(&source_root));
    hooks::write_private_file(&config_path, rewritten.as_bytes()).map_err(|source| {
        ProviderError::Seed {
            path: config_path.clone(),
            source,
        }
    })
}

fn canonicalize_or_given(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

fn config_block_toml(source_root: &Path) -> String {
    let mut block = String::new();
    block.push_str(CONFIG_BEGIN_MARKER);
    block.push('\n');
    block.push_str(&format!(
        "[projects.{}]\n",
        toml_string(&source_root.to_string_lossy())
    ));
    block.push_str("trust_level = \"trusted\"\n");
    block.push_str(CONFIG_END_MARKER);
    block.push('\n');
    block
}

fn toml_string(value: &str) -> String {
    format!("\"{}\"", toml_escape(value))
}

/// Removes a previously written daemon hooks block (markers inclusive), if
/// present, leaving the rest of the file untouched (trailing whitespace
/// trimmed). Not a general TOML parser: it operates on the exact marker
/// lines [`hooks_block_toml`] writes.
fn strip_hooks_block(config: &str) -> String {
    strip_marked_block(config, HOOKS_BEGIN_MARKER, HOOKS_END_MARKER)
}

/// Removes a previously written `begin_marker`/`end_marker`-delimited block
/// (markers inclusive), if present, leaving the rest of the document
/// untouched (trailing whitespace trimmed). Shared by [`strip_hooks_block`]
/// and [`rewrite_config_block`]'s own `CONFIG_BEGIN_MARKER`/
/// `CONFIG_END_MARKER` block. Not a general TOML parser: it operates purely
/// on the exact marker lines this module writes.
fn strip_marked_block(document: &str, begin_marker: &str, end_marker: &str) -> String {
    let Some(begin) = document.find(begin_marker) else {
        return document.trim_end().to_owned();
    };
    let before = &document[..begin];
    let after_marker = &document[begin..];
    let after = after_marker.find(end_marker).map_or("", |end_offset| {
        &after_marker[end_offset + end_marker.len()..]
    });
    format!("{}{}", before.trim_end(), after)
        .trim_end()
        .to_owned()
}

/// The byte offset of the first `[table]`/`[[array-of-tables]]` header
/// line in `document` -- the boundary between TOML's implicit root table
/// and its first explicit section -- or `None` if the document has no
/// table header at all (every key is still a root-table key in that case).
/// A heuristic line scan, not a general TOML parser (same tradeoff
/// [`strip_marked_block`] already makes): a table-header-shaped line
/// inside a multi-line string value would be misread as a real boundary,
/// which real `config.toml` files in practice do not contain.
fn first_table_header_offset(document: &str) -> Option<usize> {
    let mut offset = 0;
    for line in document.split_inclusive('\n') {
        if line.trim_start().starts_with('[') {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

/// Removes every root-table `sandbox_mode = ...` assignment from
/// `document`'s prefix before its first table header, if any, *and* every
/// line that is exactly one of [`SANDBOX_MODE_COMMENT_AND_LINE`]'s own two
/// explanatory comment lines -- so [`insert_root_level_line`] re-inserting
/// Dark Factory's own value can never produce a duplicate-key TOML document
/// (regardless of what the operator's own copied-forward `config.toml`
/// already set it to), and never leaves its own comment lines behind to
/// accumulate on repeated rewrites. A
/// `sandbox_mode` key nested inside some other table (not a real Codex
/// config shape today) is out of scope and left untouched.
fn strip_root_level_sandbox_mode(document: &str) -> String {
    let boundary = first_table_header_offset(document).unwrap_or(document.len());
    let (root, rest) = document.split_at(boundary);
    let mut filtered_root = String::with_capacity(root.len());
    for line in root.lines() {
        if !is_sandbox_mode_key_or_its_own_comment(line) {
            filtered_root.push_str(line);
            filtered_root.push('\n');
        }
    }
    filtered_root.push_str(rest);
    filtered_root
}

fn is_sandbox_mode_key_or_its_own_comment(line: &str) -> bool {
    let trimmed = line.trim();
    if SANDBOX_MODE_COMMENT_AND_LINE
        .lines()
        .any(|constant_line| constant_line.trim() == trimmed)
    {
        return true;
    }
    line.trim_start()
        .split_once('=')
        .is_some_and(|(key, _)| key.trim() == "sandbox_mode")
}

/// Inserts `line` as a root-table entry: immediately before `document`'s
/// first `[table]` header, or at the end if it has none. Appending after
/// an existing table header would silently make `line` a member of that
/// table instead of the root table -- see [`SANDBOX_MODE_COMMENT_AND_LINE`]'s
/// own doc comment for why that matters here.
fn insert_root_level_line(document: &str, line: &str) -> String {
    let boundary = first_table_header_offset(document).unwrap_or(document.len());
    let (root, rest) = document.split_at(boundary);
    let root = root.trim_end();
    let mut result = String::new();
    if !root.is_empty() {
        result.push_str(root);
        result.push('\n');
    }
    result.push_str(line);
    result.push('\n');
    let rest = rest.trim_start_matches('\n');
    if !rest.is_empty() {
        result.push('\n');
        result.push_str(rest);
    }
    result
}

fn hooks_block_toml(factoryctl_path: &Path, hook_token_path: &Path) -> String {
    let mut block = String::new();
    block.push_str(HOOKS_BEGIN_MARKER);
    block.push('\n');
    let event = factory_core::ProviderHookEvent::PreToolUse;
    let name = event.provider_event_name();
    let command = hooks::hook_command(factoryctl_path, hook_token_path, event);
    block.push_str(&format!("[[hooks.{name}]]\n"));
    block.push_str(&format!("[[hooks.{name}.hooks]]\n"));
    block.push_str("type = \"command\"\n");
    block.push_str(&format!("command = \"{}\"\n", toml_escape(&command)));
    block.push_str("timeout = 30\n\n");
    block.push_str(HOOKS_END_MARKER);
    block.push('\n');
    block
}

fn toml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod provider_tests {
    use std::os::unix::fs::PermissionsExt;

    use factory_core::RunId;
    use serde_json::Value;

    use super::*;

    fn context(directory: &Path) -> SpawnContext {
        SpawnContext {
            run_id: RunId::try_from("2f5a1e2e-2222-4444-8888-0123456789ab").unwrap(),
            source_root: directory.join("source"),
            startup_input: b"fix the admitted task".to_vec(),
            model: None,
            reasoning_effort: None,
            permission_mode: None,
            auto_mode: true,
            hook_token_path: directory.join("runtime").join("hook.token"),
            factoryctl_path: PathBuf::from("/abs/factoryctl"),
            agent_dir: directory.join("agent-dir"),
        }
    }

    #[test]
    fn fresh_launch_is_noninteractive_and_sets_codex_home() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        let launch = CodexProvider::with_source_home(directory.path().join("no-real-home"))
            .spawn_spec(&ctx)
            .unwrap();

        assert_eq!(launch.program, PathBuf::from("codex"));
        assert_eq!(
            launch.args,
            vec![
                "exec".to_owned(),
                "--dangerously-bypass-hook-trust".to_owned(),
                "--dangerously-bypass-approvals-and-sandbox".to_owned(),
                "-".to_owned(),
            ],
            "auto mode bypasses both approvals and the sandbox"
        );
        assert_eq!(launch.startup_input, b"fix the admitted task");
        let codex_home = directory.path().join("agent-dir").join("codex-home");
        assert_eq!(
            launch.env,
            vec![(
                "CODEX_HOME".to_owned(),
                codex_home.to_string_lossy().into_owned()
            )]
        );
    }

    #[test]
    fn no_explicit_permission_uses_the_supported_default_when_auto_mode_is_off() {
        let directory = tempfile::tempdir().unwrap();
        let mut ctx = context(directory.path());
        ctx.auto_mode = false;
        let launch = CodexProvider::with_source_home(directory.path().join("no-real-home"))
            .spawn_spec(&ctx)
            .unwrap();

        assert_eq!(
            launch.args,
            vec![
                "exec".to_owned(),
                "--dangerously-bypass-hook-trust".to_owned(),
                "-c".to_owned(),
                "approval_policy=\"on-request\"".to_owned(),
                "-".to_owned(),
            ]
        );
    }

    #[test]
    fn launch_passes_model_and_approval_policy() {
        let directory = tempfile::tempdir().unwrap();
        let mut ctx = context(directory.path());
        ctx.model = Some("gpt-5-codex".to_owned());
        ctx.permission_mode = Some("never".to_owned());
        let launch = CodexProvider::with_source_home(directory.path().join("no-real-home"))
            .spawn_spec(&ctx)
            .unwrap();

        assert_eq!(
            launch.args,
            vec![
                "exec".to_owned(),
                "--dangerously-bypass-hook-trust".to_owned(),
                "--model".to_owned(),
                "gpt-5-codex".to_owned(),
                "-c".to_owned(),
                "approval_policy=\"never\"".to_owned(),
                "-".to_owned(),
            ]
        );
    }

    #[test]
    fn profile_reasoning_effort_is_passed_explicitly() {
        let directory = tempfile::tempdir().unwrap();
        let mut ctx = context(directory.path());
        ctx.model = Some("gpt-5.6-luna".to_owned());
        ctx.reasoning_effort = Some("medium".to_owned());
        let launch = CodexProvider::with_source_home(directory.path().join("no-real-home"))
            .spawn_spec(&ctx)
            .unwrap();

        assert!(launch.args.windows(2).any(|args| {
            args == [
                "-c".to_owned(),
                "model_reasoning_effort=\"medium\"".to_owned(),
            ]
        }));
    }

    #[test]
    fn an_explicit_permission_mode_overrides_the_never_default() {
        let directory = tempfile::tempdir().unwrap();
        let mut ctx = context(directory.path());
        ctx.permission_mode = Some("on-request".to_owned());
        let launch = CodexProvider::with_source_home(directory.path().join("no-real-home"))
            .spawn_spec(&ctx)
            .unwrap();

        assert_eq!(
            launch.args,
            vec![
                "exec".to_owned(),
                "--dangerously-bypass-hook-trust".to_owned(),
                "-c".to_owned(),
                "approval_policy=\"on-request\"".to_owned(),
                "-".to_owned(),
            ],
            "an operator's own agent profile permission_mode always wins over \
             DEFAULT_APPROVAL_POLICY"
        );
    }

    #[test]
    fn seeds_a_minimal_config_when_no_real_codex_home_exists() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        CodexProvider::with_source_home(directory.path().join("missing"))
            .spawn_spec(&ctx)
            .unwrap();

        let config_path = directory
            .path()
            .join("agent-dir")
            .join("codex-home")
            .join("config.toml");
        let contents = fs::read_to_string(&config_path).unwrap();
        assert!(contents.starts_with(MINIMAL_CONFIG_TOML.trim_end()));
        assert!(contents.contains(HOOKS_BEGIN_MARKER));
        assert!(contents.contains(HOOKS_END_MARKER));
        assert!(
            !directory
                .path()
                .join("agent-dir")
                .join("codex-home")
                .join("auth.json")
                .exists()
        );

        let metadata = fs::metadata(config_path.parent().unwrap()).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
    }

    /// Operator rules are copied unchanged; Dark Factory does not inject a
    /// generic rule that pre-approves its entire control-plane CLI.
    #[test]
    fn operator_rules_files_including_forbid_rules_are_copied_into_the_seeded_home() {
        let directory = tempfile::tempdir().unwrap();
        let real_home = directory.path().join("real-codex-home");
        let real_rules_dir = real_home.join("rules");
        fs::create_dir_all(&real_rules_dir).unwrap();
        fs::write(
            real_rules_dir.join("default.rules"),
            "prefix_rule(pattern=[\"git\", \"push\"], decision=\"allow\")\n",
        )
        .unwrap();
        fs::write(
            real_rules_dir.join("hardening.rules"),
            "forbid_rule(pattern=[\"rm\", \"-rf\", \"/\"])\n",
        )
        .unwrap();
        // Not a `.rules` file -- must be ignored, matching `config.toml`'s
        // own allow-list precision rather than a raw directory copy.
        fs::write(real_rules_dir.join("notes.txt"), "not a rules file\n").unwrap();

        let ctx = context(directory.path());
        CodexProvider::with_source_home(real_home)
            .spawn_spec(&ctx)
            .unwrap();

        let seeded_rules_dir = directory
            .path()
            .join("agent-dir")
            .join("codex-home")
            .join("rules");
        let default_contents = fs::read_to_string(seeded_rules_dir.join("default.rules")).unwrap();
        assert_eq!(
            default_contents,
            "prefix_rule(pattern=[\"git\", \"push\"], decision=\"allow\")\n"
        );
        let hardening_contents =
            fs::read_to_string(seeded_rules_dir.join("hardening.rules")).unwrap();
        assert!(
            hardening_contents.contains("forbid_rule"),
            "an operator's own forbid rules must be preserved, not dropped"
        );
        assert!(!seeded_rules_dir.join("notes.txt").exists());
    }

    #[test]
    fn a_rules_directory_entry_error_fails_instead_of_omitting_a_forbid_rule() {
        let directory = tempfile::tempdir().unwrap();
        let source_rules_dir = directory.path().join("source-rules");
        let forbid_path = source_rules_dir.join("hardening.rules");
        fs::create_dir_all(&source_rules_dir).unwrap();
        fs::write(
            &forbid_path,
            "forbid_rule(pattern=[\"rm\", \"-rf\", \"/\"])\n",
        )
        .unwrap();
        let codex_home = directory.path().join("codex-home");
        let entries = [
            Err(io::Error::other("directory entry disappeared")),
            Ok(forbid_path),
        ];

        let error = seed_rule_paths(&codex_home, &source_rules_dir, entries).unwrap_err();

        match error {
            ProviderError::Seed { path, source } => {
                assert_eq!(path, source_rules_dir);
                assert_eq!(source.kind(), io::ErrorKind::Other);
            }
            other => panic!("expected a seed error, got {other:?}"),
        }
        assert!(
            !codex_home.join("rules").join("hardening.rules").exists(),
            "an entry iteration error must abort seeding, never return success with a forbid rule omitted"
        );
    }

    #[test]
    fn copies_the_real_config_once_and_keeps_the_auth_link_on_the_seed_home() {
        let directory = tempfile::tempdir().unwrap();
        let real_home = directory.path().join("real-codex-home");
        fs::create_dir_all(&real_home).unwrap();
        fs::write(real_home.join("config.toml"), "model = \"gpt-5.6\"\n").unwrap();
        fs::write(real_home.join("auth.json"), "{\"token\":\"secret\"}").unwrap();

        let ctx = context(directory.path());
        let provider = CodexProvider::with_source_home(real_home.clone());
        provider.spawn_spec(&ctx).unwrap();

        let codex_home = directory.path().join("agent-dir").join("codex-home");
        let config_contents = fs::read_to_string(codex_home.join("config.toml")).unwrap();
        assert!(config_contents.starts_with("model = \"gpt-5.6\""));
        assert!(config_contents.contains(HOOKS_BEGIN_MARKER));
        let auth_link = fs::read_link(codex_home.join("auth.json")).unwrap();
        assert_eq!(auth_link, real_home.join("auth.json"));

        // A real user edit to the seeded config.toml after the first spawn
        // is preserved by later spawns: only the hooks block is refreshed.
        let seeded = fs::read_to_string(codex_home.join("config.toml")).unwrap();
        let base = strip_hooks_block(&seeded);
        fs::write(
            codex_home.join("config.toml"),
            format!("{base}\nmodel_reasoning_effort = \"xhigh\"\n"),
        )
        .unwrap();
        provider.spawn_spec(&ctx).unwrap();
        let after_second_spawn = fs::read_to_string(codex_home.join("config.toml")).unwrap();
        assert!(after_second_spawn.contains("model_reasoning_effort = \"xhigh\""));
        assert_eq!(after_second_spawn.matches(HOOKS_BEGIN_MARKER).count(), 1);

        // A different seed home (another Codex account) re-points the auth
        // link on the next spawn; the seeded config.toml is left alone.
        let other_home = directory.path().join("other-codex-home");
        fs::create_dir_all(&other_home).unwrap();
        fs::write(other_home.join("auth.json"), "{\"token\":\"other\"}").unwrap();
        CodexProvider::with_source_home(other_home.clone())
            .spawn_spec(&ctx)
            .unwrap();
        assert_eq!(
            fs::read_link(codex_home.join("auth.json")).unwrap(),
            other_home.join("auth.json")
        );
        assert!(
            fs::read_to_string(codex_home.join("config.toml"))
                .unwrap()
                .contains("model_reasoning_effort = \"xhigh\"")
        );
        // A regular auth.json an operator placed is never touched.
        fs::remove_file(codex_home.join("auth.json")).unwrap();
        fs::write(codex_home.join("auth.json"), "{\"token\":\"mine\"}").unwrap();
        provider.spawn_spec(&ctx).unwrap();
        assert!(
            fs::symlink_metadata(codex_home.join("auth.json"))
                .unwrap()
                .file_type()
                .is_file()
        );
    }

    #[test]
    fn hooks_block_is_rewritten_idempotently_across_spawns() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        let provider = CodexProvider::with_source_home(directory.path().join("missing"));
        provider.spawn_spec(&ctx).unwrap();
        provider.spawn_spec(&ctx).unwrap();
        provider.spawn_spec(&ctx).unwrap();

        let config_path = directory
            .path()
            .join("agent-dir")
            .join("codex-home")
            .join("config.toml");
        let contents = fs::read_to_string(&config_path).unwrap();
        assert_eq!(contents.matches(HOOKS_BEGIN_MARKER).count(), 1);
        assert_eq!(contents.matches(HOOKS_END_MARKER).count(), 1);
        assert_eq!(contents.matches("[[hooks.PreToolUse]]").count(), 1);
        assert!(!contents.contains("[[hooks.Stop]]"));
        assert!(!contents.contains("[[hooks.PermissionRequest]]"));
    }

    #[test]
    fn hooks_block_toml_has_the_exact_designed_shape_for_one_event() {
        let block = hooks_block_toml(
            Path::new("/abs/factoryctl"),
            Path::new("/abs/runs/attempt-1/hook.token"),
        );
        assert!(block.starts_with(HOOKS_BEGIN_MARKER));
        assert!(block.trim_end().ends_with(HOOKS_END_MARKER));
        assert!(block.contains(
            "[[hooks.PreToolUse]]\n[[hooks.PreToolUse.hooks]]\ntype = \"command\"\ncommand = \"'/abs/factoryctl' hook --token-file '/abs/runs/attempt-1/hook.token' PreToolUse\"\ntimeout = 30\n"
        ));
    }

    #[test]
    fn capabilities_declare_the_supported_permission_modes() {
        let capabilities = CodexProvider::new().capabilities();
        assert_eq!(capabilities.permission_modes, PERMISSION_MODES);
    }

    #[test]
    fn config_toml_generated_by_a_fresh_seed_parses_under_codex_doctor() {
        // Guards against a schema regression without spawning a real
        // provider process: `codex doctor` is a read-only diagnostic
        // that parses `CODEX_HOME/config.toml` and reports whether it
        // loaded under `--strict-config` for this generated shape.
        if std::process::Command::new("codex")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: codex is not installed in this environment");
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        CodexProvider::with_source_home(directory.path().join("missing"))
            .spawn_spec(&ctx)
            .unwrap();
        let codex_home = directory.path().join("agent-dir").join("codex-home");

        let output = std::process::Command::new("codex")
            .env("CODEX_HOME", &codex_home)
            .args(["--strict-config", "doctor", "--json"])
            .output()
            .unwrap();
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            report["checks"]["config.load"]["details"]["config.toml parse"],
            "ok"
        );
    }

    fn codex_is_installed() -> bool {
        std::process::Command::new("codex")
            .arg("--version")
            .output()
            .is_ok()
    }

    #[test]
    fn config_block_toml_has_the_exact_designed_shape() {
        let block = config_block_toml(Path::new("/abs/changes/change-1"));
        assert_eq!(
            block,
            "# --- dark-factory config BEGIN ---\n\
             [projects.\"/abs/changes/change-1\"]\n\
             trust_level = \"trusted\"\n\
             # --- dark-factory config END ---\n"
        );
    }

    #[test]
    fn spawn_spec_sets_sandbox_mode_and_only_project_trust() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        CodexProvider::with_source_home(directory.path().join("missing"))
            .spawn_spec(&ctx)
            .unwrap();

        let config_path = directory
            .path()
            .join("agent-dir")
            .join("codex-home")
            .join("config.toml");
        let contents = fs::read_to_string(&config_path).unwrap();
        assert_eq!(
            contents
                .matches("sandbox_mode = \"workspace-write\"")
                .count(),
            1
        );
        assert!(!contents.contains("[sandbox_workspace_write]"));
        assert!(!contents.contains("writable_roots"));
        assert!(!contents.contains("network_access"));
        assert!(contents.contains(&format!(
            "[projects.{}]",
            toml_string(&canonicalize_or_given(&ctx.source_root).to_string_lossy())
        )));
        assert!(contents.contains("trust_level = \"trusted\""));
        // sandbox_mode is a root-table key, positioned before every
        // `[table]` header this file has (both ours and, in this fresh
        // minimal-seed case, there are no others).
        let sandbox_mode_offset = contents.find("sandbox_mode = ").unwrap();
        let first_table_offset = contents.find('[').unwrap();
        assert!(sandbox_mode_offset < first_table_offset);
    }

    #[test]
    fn config_block_is_rewritten_idempotently_across_spawns() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        let provider = CodexProvider::with_source_home(directory.path().join("missing"));
        provider.spawn_spec(&ctx).unwrap();
        provider.spawn_spec(&ctx).unwrap();
        provider.spawn_spec(&ctx).unwrap();

        let config_path = directory
            .path()
            .join("agent-dir")
            .join("codex-home")
            .join("config.toml");
        let contents = fs::read_to_string(&config_path).unwrap();
        assert_eq!(contents.matches(CONFIG_BEGIN_MARKER).count(), 1);
        assert_eq!(contents.matches(CONFIG_END_MARKER).count(), 1);
        assert_eq!(contents.matches("sandbox_mode = ").count(), 1);
        assert!(!contents.contains("[sandbox_workspace_write]"));
        assert_eq!(contents.matches("trust_level = \"trusted\"").count(), 1);
        // Regression: the first version of this rewrite only deduplicated
        // the `sandbox_mode = ...` line itself, not the two explanatory
        // comment lines above it -- those accumulated one more copy on
        // every spawn (73 copies after repeated launches). Assert on the comment
        // text directly, not just the structural TOML content, since that
        // is exactly what the original bug's blind spot was.
        assert_eq!(
            contents
                .matches("dark-factory sandbox_mode override")
                .count(),
            1
        );
    }

    #[test]
    fn a_real_configs_own_sandbox_mode_is_replaced_not_duplicated_and_a_surviving_trailing_table_is_undisturbed()
     {
        // A representative operator config: root-level scalars (including
        // the operator's own `sandbox_mode`), then dozens of trailing
        // `[projects."..."]` tables (dropped entirely at seed time by
        // `filter_operator_config_for_seed` -- see the
        // dedicated `operator_config_is_filtered_...` test below -- so
        // this fixture also keeps one *surviving* trailing table,
        // `[model_providers.*]`, to prove `insert_root_level_line`
        // still finds the correct root-table boundary when a real
        // dropped-then-kept mix of trailing tables is present, not just
        // when every trailing table happens to be dropped). Appending
        // Dark Factory's own `sandbox_mode` line naively after those
        // tables would silently make it a member of the *last* survivor
        // instead of the root table -- this proves it does not.
        let directory = tempfile::tempdir().unwrap();
        let real_home = directory.path().join("real-codex-home");
        fs::create_dir_all(&real_home).unwrap();
        fs::write(
            real_home.join("config.toml"),
            "model = \"gpt-5.6\"\n\
             sandbox_mode = \"read-only\"\n\
             approval_policy = \"on-request\"\n\
             \n\
             [projects.\"/Users/op/other-repo\"]\n\
             trust_level = \"trusted\"\n\
             \n\
             [projects.\"/Users/op/another-repo\"]\n\
             trust_level = \"trusted\"\n\
             \n\
             [model_providers.custom]\n\
             name = \"Custom\"\n",
        )
        .unwrap();

        let ctx = context(directory.path());
        let provider = CodexProvider::with_source_home(real_home);
        provider.spawn_spec(&ctx).unwrap();

        let codex_home = directory.path().join("agent-dir").join("codex-home");
        let contents = fs::read_to_string(codex_home.join("config.toml")).unwrap();

        // Exactly one `sandbox_mode` assignment -- ours, not the
        // operator's (a comment in our own generated line also mentions
        // "sandbox_mode" by name, so this counts real assignments, not
        // every substring occurrence).
        assert_eq!(contents.matches("sandbox_mode = \"").count(), 1);
        assert!(contents.contains("sandbox_mode = \"workspace-write\""));
        assert!(!contents.contains("\"read-only\""));
        // The operator's own non-project settings round-trip untouched,
        // including a trailing table not on the drop list.
        assert!(contents.contains("model = \"gpt-5.6\""));
        assert!(contents.contains("approval_policy = \"on-request\""));
        assert!(contents.contains("[model_providers.custom]"));
        assert!(contents.contains("name = \"Custom\""));
        // The operator's own project trust entries do not: an operator's
        // decision to trust *their own* repos has no bearing on this
        // factory attempt.
        assert!(!contents.contains("/Users/op/other-repo"));
        assert!(!contents.contains("/Users/op/another-repo"));
        // This attempt's own daemon-owned source still gains a trust entry, from
        // `rewrite_config_block` -- unrelated to what was (or wasn't)
        // seeded.
        assert!(contents.contains(&format!(
            "[projects.{}]",
            toml_string(&ctx.source_root.to_string_lossy())
        )));

        if !codex_is_installed() {
            eprintln!(
                "skipping real codex doctor check: codex is not installed in this environment"
            );
            return;
        }
        let output = std::process::Command::new("codex")
            .env("CODEX_HOME", &codex_home)
            .args(["--strict-config", "doctor", "--json"])
            .output()
            .unwrap();
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            report["checks"]["config.load"]["details"]["config.toml parse"],
            "ok"
        );
    }

    /// An operator config carrying the three shapes that must not be
    /// inherited:
    /// `[mcp_servers.*]` (the "Starting MCP servers" stall), `[projects.*]`
    /// (an operator's own repo trust, irrelevant to a factory worker), and
    /// `[hooks.state]` (Codex's own persisted hook-trust bookkeeping) plus
    /// the `[[hooks.<Event>]]`/`[[hooks.<Event>.hooks]]` shape a real
    /// `~/.codex/config.toml` could also carry if the operator has their
    /// own hooks configured -- both variants have top-level key `hooks`.
    /// None of it should survive the seed; ordinary root-level settings
    /// and an unrelated table should.
    #[test]
    fn operator_config_is_filtered_to_the_documented_allow_list_at_seed() {
        let directory = tempfile::tempdir().unwrap();
        let real_home = directory.path().join("real-codex-home");
        fs::create_dir_all(&real_home).unwrap();
        fs::write(
            real_home.join("config.toml"),
            "model = \"gpt-5.6\"\n\
             model_provider = \"openai\"\n\
             approval_policy = \"on-request\"\n\
             \n\
             [mcp_servers.filesystem]\n\
             command = \"npx\"\n\
             args = [\"-y\", \"@modelcontextprotocol/server-filesystem\"]\n\
             \n\
             [mcp_servers.browser]\n\
             command = \"mcp-browser\"\n\
             \n\
             [projects.\"/Users/op/some-repo\"]\n\
             trust_level = \"trusted\"\n\
             \n\
             [hooks.state]\n\
             \"/Users/op/.codex/config.toml:SessionStart:0:0\" = true\n\
             \n\
             [[hooks.SessionStart]]\n\
             [[hooks.SessionStart.hooks]]\n\
             type = \"command\"\n\
             command = \"/Users/op/bin/operators-own-hook.sh\"\n\
             \n\
             [model_providers.custom]\n\
             name = \"Custom\"\n",
        )
        .unwrap();

        let ctx = context(directory.path());
        CodexProvider::with_source_home(real_home)
            .spawn_spec(&ctx)
            .unwrap();

        let codex_home = directory.path().join("agent-dir").join("codex-home");
        let contents = fs::read_to_string(codex_home.join("config.toml")).unwrap();

        // Dropped: every shape in `DROPPED_SEED_TABLES`.
        assert!(!contents.contains("mcp_servers"));
        assert!(!contents.contains("server-filesystem"));
        assert!(!contents.contains("mcp-browser"));
        assert!(!contents.contains("/Users/op/some-repo"));
        assert!(!contents.contains("hooks.state"));
        assert!(!contents.contains("operators-own-hook.sh"));
        // The operator's lifecycle hook is dropped; only the daemon's
        // authenticated authority hook is generated.
        assert!(!contents.contains("[[hooks.SessionStart]]"));
        assert_eq!(contents.matches("[[hooks.PreToolUse]]").count(), 1);
        assert!(contents.contains("factoryctl' hook --token-file"));

        // Kept: ordinary settings and an unrelated table.
        assert!(contents.contains("model = \"gpt-5.6\""));
        assert!(contents.contains("model_provider = \"openai\""));
        assert!(contents.contains("approval_policy = \"on-request\""));
        assert!(contents.contains("[model_providers.custom]"));
        assert!(contents.contains("name = \"Custom\""));

        if !codex_is_installed() {
            eprintln!(
                "skipping real codex doctor check: codex is not installed in this environment"
            );
            return;
        }
        let output = std::process::Command::new("codex")
            .env("CODEX_HOME", &codex_home)
            .args(["--strict-config", "doctor", "--json"])
            .output()
            .unwrap();
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            report["checks"]["config.load"]["details"]["config.toml parse"],
            "ok"
        );
    }

    #[test]
    fn filter_operator_config_for_seed_keeps_only_the_allow_list() {
        let filtered = filter_operator_config_for_seed(
            "model = \"gpt-5.6\"\n\
             \n\
             [mcp_servers.filesystem]\n\
             command = \"npx\"\n\
             \n\
             [projects.\"/abs/repo\"]\n\
             trust_level = \"trusted\"\n\
             \n\
             [hooks.state]\n\
             key = true\n\
             \n\
             [model_providers.custom]\n\
             name = \"Custom\"\n",
        );
        assert!(filtered.contains("model = \"gpt-5.6\""));
        assert!(filtered.contains("[model_providers.custom]"));
        assert!(filtered.contains("name = \"Custom\""));
        assert!(!filtered.contains("mcp_servers"));
        assert!(!filtered.contains("projects"));
        assert!(!filtered.contains("hooks"));
    }

    #[test]
    fn table_header_top_level_key_reads_the_key_before_the_first_dot() {
        assert_eq!(
            table_header_top_level_key("[hooks.state]"),
            Some("hooks".to_owned())
        );
        assert_eq!(
            table_header_top_level_key("[[hooks.SessionStart.hooks]]"),
            Some("hooks".to_owned())
        );
        assert_eq!(
            table_header_top_level_key("[projects.\"/abs/repo\"]"),
            Some("projects".to_owned())
        );
        assert_eq!(
            table_header_top_level_key("[model_providers.custom]"),
            Some("model_providers".to_owned())
        );
        assert_eq!(table_header_top_level_key("not a header"), None);
    }

    #[test]
    fn the_daemons_codex_home_overrides_the_operators_own() {
        let dogfood = CodexProvider::from_environment(
            Some("/Users/me/.codex-dogfood".into()),
            Some("/Users/me".into()),
        );
        assert_eq!(
            dogfood.source_home.as_deref(),
            Some(Path::new("/Users/me/.codex-dogfood"))
        );
        let personal = CodexProvider::from_environment(Some("".into()), Some("/Users/me".into()));
        assert_eq!(
            personal.source_home.as_deref(),
            Some(Path::new("/Users/me/.codex")),
            "an empty override means unset"
        );
        assert_eq!(
            CodexProvider::from_environment(None, None).source_home,
            None
        );
    }
}
