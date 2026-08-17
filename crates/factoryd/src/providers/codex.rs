//! [`CodexProvider`]: the interactive-session [`Provider`] impl for Codex,
//! plus the per-agent `CODEX_HOME` it seeds and the hooks block it rewrites
//! into `config.toml` per session. See `docs/providers.md`.
//!
//! Track 5's pivot from a non-interactive `codex exec --json` pipe-mode
//! adapter (session identity confirmed by decoding Codex's own JSONL event
//! stream) to a resident interactive `codex` process under a PTY (session
//! identity learned from the `SessionStart` hook payload, state driven by
//! hooks thereafter) deleted that whole decoder here: `Decoder`,
//! `Observation`, item-tracking state, `Outcome`, `FailureReason`, the
//! non-interactive `prepare()`, and their fixtures/tests
//! (`crates/factoryd/tests/codex.rs`) are gone (~540 LOC, see
//! `TRACK5-DESIGN.md` §7). `validate_thread_id` is the one piece that
//! survived unchanged: both the old decoder and the new [`CodexProvider`]
//! need to confirm a Codex thread identity is a canonical UUID.

use std::{
    fs,
    path::{Path, PathBuf},
};

use uuid::Uuid;

use crate::providers::{
    Capabilities, InteractiveLaunch, Provider, ProviderError, SpawnContext, hooks,
};

fn validate_thread_id(value: &str) -> Result<(), ()> {
    let parsed = Uuid::parse_str(value).map_err(|_| ())?;
    if parsed.hyphenated().to_string() == value {
        Ok(())
    } else {
        Err(())
    }
}

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

/// Interactive-session [`Provider`] for Codex. Launches `codex
/// --dangerously-bypass-hook-trust [--model M] [-c
/// approval_policy="<permission_mode>"] [resume <thread-id>]` with
/// `CODEX_HOME` pointed at this agent's own seeded home (per orchestrator
/// amendment A2, `TRACK5-DESIGN.md`: per *agent*, not per session, so
/// `codex resume` can find its own prior rollout file across a stop and
/// restart). `--dangerously-bypass-hook-trust` is unconditional: the hooks
/// this provider writes are 100% daemon-authored into an isolated
/// `CODEX_HOME` the operator never hand-edits, which already is the
/// vetting Codex's normal hook-trust prompt would otherwise ask for. See
/// `docs/providers.md`.
pub struct CodexProvider {
    /// The operator's real Codex home to seed a fresh per-agent
    /// `CODEX_HOME` from (`config.toml`, `auth.json`). Defaults to
    /// `$HOME/.codex`; overridable for tests via
    /// [`CodexProvider::with_source_home`].
    source_home: Option<PathBuf>,
}

impl CodexProvider {
    /// Resolves the seed source from `$HOME/.codex`, matching Codex's own
    /// default `CODEX_HOME`. `None` (no `$HOME`) means a fresh per-agent
    /// home always starts from [`MINIMAL_CONFIG_TOML`] with no `auth.json`
    /// link — Codex will then have no subscription credentials, same as
    /// running `codex` with no prior login.
    #[must_use]
    pub fn new() -> Self {
        Self {
            source_home: std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")),
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
    fn spawn_spec(&self, ctx: &SpawnContext) -> Result<InteractiveLaunch, ProviderError> {
        let codex_home = ctx.agent_dir.join("codex-home");
        seed_codex_home_once(&codex_home, self.source_home.as_deref())?;
        rewrite_hooks_block(&codex_home, &ctx.factoryctl_path, &ctx.hook_token_path)?;
        rewrite_config_block(&codex_home, &ctx.agent_dir, &ctx.socket_path, &ctx.worktree)?;

        let mut args = vec!["--dangerously-bypass-hook-trust".to_owned()];
        if let Some(model) = &ctx.model {
            args.push("--model".to_owned());
            args.push(model.clone());
        }
        if let Some(permission_mode) = &ctx.permission_mode {
            args.push("-c".to_owned());
            args.push(format!("approval_policy=\"{permission_mode}\""));
        }
        if let Some(thread_id) = &ctx.resume {
            validate_thread_id(thread_id).map_err(|_| ProviderError::ResumeIdNotUuid {
                value: thread_id.clone(),
            })?;
            args.push("resume".to_owned());
            args.push(thread_id.clone());
        }

        Ok(InteractiveLaunch {
            program: PathBuf::from("codex"),
            args,
            env: vec![(
                "CODEX_HOME".to_owned(),
                codex_home.to_string_lossy().into_owned(),
            )],
            generated_files: vec![codex_home.join("config.toml")],
        })
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            hooks: true,
            resume: true,
            permission_modes: &PERMISSION_MODES,
        }
    }
}

/// Idempotently seeds `codex_home` (mode `0700`, created if missing) the
/// first time it is used: copies `source_home/config.toml` if present, else
/// writes [`MINIMAL_CONFIG_TOML`]; symlinks `source_home/auth.json` if
/// present and not already linked. Existing files are never overwritten —
/// this is a one-time seed, not a sync. The hooks block is refreshed
/// separately, every spawn, by [`rewrite_hooks_block`].
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
            .and_then(|path| fs::read(path).ok())
            .unwrap_or_else(|| MINIMAL_CONFIG_TOML.as_bytes().to_vec());
        hooks::write_private_file(&config_path, &contents).map_err(|source| {
            ProviderError::Seed {
                path: config_path.clone(),
                source,
            }
        })?;
    }

    if let Some(source_home) = source_home {
        let auth_path = codex_home.join("auth.json");
        let source_auth = source_home.join("auth.json");
        if fs::symlink_metadata(&auth_path).is_err() && source_auth.exists() {
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

/// Idempotently rewrites the daemon-owned hooks block in `codex_home`'s
/// `config.toml`, replacing everything between the `BEGIN`/`END` markers if
/// present, else appending a fresh block. Called on every spawn: the hook
/// token path changes per session, so this keeps the seeded config current
/// without disturbing whatever the operator's real `config.toml` carried
/// forward from the one-time seed (model, provider, trust settings, ...).
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

/// Idempotently rewrites the daemon-owned sandbox/trust configuration in
/// `codex_home`'s `config.toml`: `sandbox_mode = "workspace-write"` (a
/// root-table key -- see [`SANDBOX_MODE_COMMENT_AND_LINE`]'s own doc
/// comment for why it cannot simply live inside the marker block below)
/// plus a `CONFIG_BEGIN_MARKER`/`CONFIG_END_MARKER` block holding
/// `[sandbox_workspace_write]` (`writable_roots`/`network_access`, so the
/// sandbox that gates a session's own spawned tool calls can still reach
/// this agent's guidance directory and the daemon's control socket -- a
/// Unix socket connect needs write access to the socket path under the
/// seatbelt sandbox) and `[projects."<worktree>"]` (`trust_level =
/// "trusted"`, so the very first Codex session in a fresh Dark Factory
/// worktree never blocks on Codex's own trust prompt, matching
/// `ClaudeProvider::pretrust_worktree`). Called on every spawn, after
/// [`rewrite_hooks_block`]: the worktree/agent-dir/socket path can change
/// per session, so this keeps them current without disturbing whatever
/// else the operator's real `config.toml` carried forward from the
/// one-time seed.
///
/// Every path is canonicalized (symlinks resolved) before use, falling
/// back to the given path unchanged if canonicalization fails -- found
/// manually against a real session (this track's item 6 check): both
/// Codex's own trust check and its seatbelt sandbox's path matching
/// operate on resolved paths, so a `$DARK_FACTORY_HOME` under a symlink
/// (`/tmp` -> `/private/tmp` on macOS, the concrete case this was found
/// against) would otherwise silently write entries that never match.
fn rewrite_config_block(
    codex_home: &Path,
    agent_dir: &Path,
    socket_path: &Path,
    worktree: &Path,
) -> Result<(), ProviderError> {
    let agent_dir = canonicalize_or_given(agent_dir);
    let worktree = canonicalize_or_given(worktree);
    let socket_directory = socket_path
        .parent()
        .map_or_else(|| canonicalize_or_given(socket_path), canonicalize_or_given);

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
    rewritten.push_str(&config_block_toml(&agent_dir, &socket_directory, &worktree));
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

fn config_block_toml(agent_dir: &Path, socket_directory: &Path, worktree: &Path) -> String {
    let mut block = String::new();
    block.push_str(CONFIG_BEGIN_MARKER);
    block.push('\n');
    block.push_str("[sandbox_workspace_write]\n");
    block.push_str(&format!(
        "writable_roots = [{}, {}]\n",
        toml_string(&agent_dir.to_string_lossy()),
        toml_string(&socket_directory.to_string_lossy()),
    ));
    block.push_str("network_access = false\n");
    block.push('\n');
    block.push_str(&format!(
        "[projects.{}]\n",
        toml_string(&worktree.to_string_lossy())
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
/// already set it to -- this machine's real `~/.codex/config.toml` already
/// has one), and, critically, never leaves its *own* two comment lines
/// behind to accumulate one more copy on every single spawn. Found the hard
/// way running this manually (this track's item 6 check): the first version
/// of this function only matched the `key = value` line itself, not the
/// comment above it, so `rewrite_config_block` silently grew its own
/// `config.toml` by two duplicate comment lines on every spawn -- 73 copies
/// after a handful of manual session restarts, before this was caught. A
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
    for event in hooks::HOOK_EVENTS {
        let name = event.provider_event_name();
        let command = hooks::hook_command(factoryctl_path, hook_token_path, event);
        block.push_str(&format!("[[hooks.{name}]]\n"));
        block.push_str(&format!("[[hooks.{name}.hooks]]\n"));
        block.push_str("type = \"command\"\n");
        block.push_str(&format!("command = \"{}\"\n", toml_escape(&command)));
        block.push_str("timeout = 30\n\n");
    }
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

    use factory_core::{AgentId, ProjectId, SessionId};
    use serde_json::Value;

    use super::*;

    fn context(directory: &Path) -> SpawnContext {
        SpawnContext {
            agent_id: AgentId::try_from("worker-1").unwrap(),
            project_id: ProjectId::try_from("factory").unwrap(),
            session_id: SessionId::try_from("2f5a1e2e-2222-4444-8888-0123456789ab").unwrap(),
            worktree: directory.join("worktree"),
            model: None,
            permission_mode: None,
            resume: None,
            hook_token_path: directory.join("runtime").join("hook.token"),
            factoryctl_path: PathBuf::from("/abs/factoryctl"),
            agent_dir: directory.join("agent-dir"),
            socket_path: directory.join("f.sock"),
        }
    }

    #[test]
    fn fresh_launch_has_no_resume_argument_and_sets_codex_home() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        let launch = CodexProvider::with_source_home(directory.path().join("no-real-home"))
            .spawn_spec(&ctx)
            .unwrap();

        assert_eq!(launch.program, PathBuf::from("codex"));
        assert_eq!(
            launch.args,
            vec!["--dangerously-bypass-hook-trust".to_owned()]
        );
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
    fn resume_launch_passes_model_approval_policy_and_thread_id_in_order() {
        let directory = tempfile::tempdir().unwrap();
        let mut ctx = context(directory.path());
        ctx.model = Some("gpt-5-codex".to_owned());
        ctx.permission_mode = Some("never".to_owned());
        ctx.resume = Some("9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d".to_owned());
        let launch = CodexProvider::with_source_home(directory.path().join("no-real-home"))
            .spawn_spec(&ctx)
            .unwrap();

        assert_eq!(
            launch.args,
            vec![
                "--dangerously-bypass-hook-trust".to_owned(),
                "--model".to_owned(),
                "gpt-5-codex".to_owned(),
                "-c".to_owned(),
                "approval_policy=\"never\"".to_owned(),
                "resume".to_owned(),
                "9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d".to_owned(),
            ]
        );
    }

    #[test]
    fn resume_rejects_a_non_uuid_thread_id() {
        let directory = tempfile::tempdir().unwrap();
        let mut ctx = context(directory.path());
        ctx.resume = Some("not-a-uuid".to_owned());
        let result =
            CodexProvider::with_source_home(directory.path().join("no-real-home")).spawn_spec(&ctx);
        assert!(matches!(result, Err(ProviderError::ResumeIdNotUuid { .. })));
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

    #[test]
    fn copies_the_real_config_and_symlinks_auth_json_on_first_seed_only() {
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
        assert_eq!(contents.matches("[[hooks.Stop]]").count(), 1);
    }

    #[test]
    fn hooks_block_toml_has_the_exact_designed_shape_for_one_event() {
        let block = hooks_block_toml(
            Path::new("/abs/factoryctl"),
            Path::new("/abs/runs/session-1/hook.token"),
        );
        assert!(block.starts_with(HOOKS_BEGIN_MARKER));
        assert!(block.trim_end().ends_with(HOOKS_END_MARKER));
        assert!(block.contains(
            "[[hooks.Stop]]\n[[hooks.Stop.hooks]]\ntype = \"command\"\ncommand = \"'/abs/factoryctl' hook --token-file '/abs/runs/session-1/hook.token' Stop\"\ntimeout = 30\n"
        ));
    }

    #[test]
    fn capabilities_declare_hooks_resume_and_the_supported_permission_modes() {
        let capabilities = CodexProvider::new().capabilities();
        assert!(capabilities.hooks);
        assert!(capabilities.resume);
        assert_eq!(capabilities.permission_modes, PERMISSION_MODES);
    }

    #[test]
    fn config_toml_generated_by_a_fresh_seed_parses_under_codex_doctor() {
        // Guards against a schema regression without spawning a real
        // interactive session: `codex doctor` is a read-only diagnostic
        // that parses `CODEX_HOME/config.toml` and reports whether it
        // loaded, matching the manual verification recorded in this
        // track's report (`config.toml parse: ok` under `--strict-config`
        // for exactly this generated shape).
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
        let block = config_block_toml(
            Path::new("/abs/agent-dir"),
            Path::new("/abs"),
            Path::new("/abs/worktrees/worker-1"),
        );
        assert_eq!(
            block,
            "# --- dark-factory config BEGIN ---\n\
             [sandbox_workspace_write]\n\
             writable_roots = [\"/abs/agent-dir\", \"/abs\"]\n\
             network_access = false\n\
             \n\
             [projects.\"/abs/worktrees/worker-1\"]\n\
             trust_level = \"trusted\"\n\
             # --- dark-factory config END ---\n"
        );
    }

    #[test]
    fn spawn_spec_sets_sandbox_mode_writable_roots_and_project_trust() {
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
        assert!(contents.contains("[sandbox_workspace_write]"));
        // Every path is canonicalized before being written (symlinks
        // resolved -- see `rewrite_config_block`'s own doc comment for
        // why, found manually against a real session where
        // `$DARK_FACTORY_HOME` was under `/tmp`, itself a symlink to
        // `/private/tmp` on macOS): compare against the same
        // canonicalize-or-fall-back-to-given resolution the production
        // code applies, not the raw `SpawnContext` paths, which a tempdir
        // root under `/var/folders/...` (itself commonly a symlink to
        // `/private/var/folders/...` on macOS) would otherwise mismatch.
        assert!(contents.contains(&format!(
            "writable_roots = [{}, {}]",
            toml_string(&canonicalize_or_given(&ctx.agent_dir).to_string_lossy()),
            toml_string(
                &canonicalize_or_given(ctx.socket_path.parent().unwrap()).to_string_lossy()
            ),
        )));
        assert!(contents.contains("network_access = false"));
        assert!(contents.contains(&format!(
            "[projects.{}]",
            toml_string(&canonicalize_or_given(&ctx.worktree).to_string_lossy())
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
        assert_eq!(contents.matches("[sandbox_workspace_write]").count(), 1);
        assert_eq!(contents.matches("trust_level = \"trusted\"").count(), 1);
        // Regression: the first version of this rewrite only deduplicated
        // the `sandbox_mode = ...` line itself, not the two explanatory
        // comment lines above it -- those accumulated one more copy on
        // every spawn (73 copies after a handful of manual session
        // restarts, this track's item 6 check). Assert on the comment
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
    fn a_real_configs_own_sandbox_mode_is_replaced_not_duplicated_and_its_trailing_project_tables_survive()
     {
        // The exact shape this track's manual check found on a real
        // machine's `~/.codex/config.toml`: root-level scalars (including
        // the operator's own `sandbox_mode`), then dozens of trailing
        // `[projects."..."]` tables. Appending Dark Factory's own
        // `sandbox_mode` line naively after those tables would silently
        // make it a member of the *last* `[projects...]` table instead of
        // the root table -- this proves it does not.
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
             trust_level = \"trusted\"\n",
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
        // The operator's own settings and both pre-existing project trust
        // entries round-trip untouched.
        assert!(contents.contains("model = \"gpt-5.6\""));
        assert!(contents.contains("approval_policy = \"on-request\""));
        assert!(contents.contains("[projects.\"/Users/op/other-repo\"]"));
        assert!(contents.contains("[projects.\"/Users/op/another-repo\"]"));
        // This agent's own worktree gains a trust entry too.
        assert!(contents.contains(&format!(
            "[projects.{}]",
            toml_string(&ctx.worktree.to_string_lossy())
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
}
