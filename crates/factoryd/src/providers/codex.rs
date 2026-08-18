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
    ffi::OsString,
    fs, io,
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

/// The native posture used when factory auto mode is off and an agent has
/// no explicit override. Auto mode uses Codex's full bypass flag instead;
/// an explicit `on-request`/`never` profile always wins.
const DEFAULT_APPROVAL_POLICY: &str = "on-request";

/// Pre-approves this agent's own `factoryctl` calls the same way Codex's
/// own interactive "don't ask again for commands that start with
/// `factoryctl`" choice does -- confirmed against a real dogfood session's
/// own operator-approved `CODEX_HOME/rules/default.rules`, which Codex
/// wrote in exactly this shape once the operator chose it by hand (see
/// `docs/providers.md`).
///
/// Confirmed from the installed `codex-cli 0.147.0` binary's own strings:
/// `prefix_rule`'s
/// `allow` decision is only ever consulted under `approval_policy =
/// "on-request"` ("you cannot request additional permissions unless the
/// approval policy is OnRequest"). It is seeded for auto-off or explicitly
/// `on-request` sessions. When it is consulted, be precise about what it
/// grants: the same binary's strings describe `prefix_rule` as valid "only
/// with `sandbox_permissions: \"require_escalated\"`", i.e. this rule
/// pre-approves **unsandboxed** execution of any command whose parsed
/// prefix is `factoryctl` -- every subcommand, not just the task/agent
/// verbs a session's own delivery composes, including `factoryctl update`
/// (replaces the installed binaries) and `factoryctl init` (rewrites the
/// launchd job). Codex's own exec-policy checks (`forbid` rules, and its
/// built-in "blocked by policy" denials) are unaffected either way --
/// `approval_policy` and rules only ever gate the `allow` side.
const FACTORYCTL_PREFIX_RULE: &str = "prefix_rule(pattern=[\"factoryctl\"], decision=\"allow\")\n";

/// Top-level tables never copied from the operator's real `~/.codex/
/// config.toml` into a fresh per-agent seed (this track's item 7):
///
/// - `mcp_servers`: the concrete bug this exists to fix -- Codex stalls at
///   "Starting MCP servers" launching every one of the operator's own MCP
///   servers inside a headless factory worker session that never needed
///   them, several of which expect an interactive terminal/browser/local
///   dev server that is not there.
/// - `projects`: the operator's own per-repo trust decisions (which repos
///   *they* have approved running Codex against unprompted) have no
///   bearing on a factory worker's own worktree, which
///   `rewrite_config_block` already grants trust to explicitly, every
///   spawn, on its own terms.
/// - `hooks`: covers both `[[hooks.<Event>]]`/`[[hooks.<Event>.hooks]]`
///   (the operator's own hook commands, which must never run inside a
///   daemon-owned session -- this seed's whole point is that
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
/// are exactly "what a factory worker needs" per this track's brief, and
/// none of them reference the operator's own environment.
const DROPPED_SEED_TABLES: [&str; 3] = ["mcp_servers", "projects", "hooks"];

/// Filters `document` (a copy of the operator's real `config.toml`) down
/// to the allow-list [`DROPPED_SEED_TABLES`] documents, for the *initial*
/// seed only (`seed_codex_home_once` never overwrites an existing seeded
/// `config.toml` -- this runs once per agent, not once per spawn). Not a
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

/// Interactive-session [`Provider`] for Codex. Launches `codex
/// --dangerously-bypass-hook-trust [--model M]
/// (--dangerously-bypass-approvals-and-sandbox | -c approval_policy=M)
/// [resume <thread-id>]` with
/// `CODEX_HOME` pointed at this agent's own seeded home (per orchestrator
/// amendment A2, `TRACK5-DESIGN.md`: per *agent*, not per session, so
/// `codex resume` can find its own prior rollout file across a stop and
/// restart). `--dangerously-bypass-hook-trust` is unconditional: the hooks
/// this provider writes are 100% daemon-authored into an isolated
/// `CODEX_HOME` the operator never hand-edits, which already is the
/// vetting Codex's normal hook-trust prompt would otherwise ask for. See
/// `docs/providers.md`.
pub struct CodexProvider {
    /// The Codex home to seed a fresh per-agent `CODEX_HOME` from
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
    /// means a fresh per-agent home always starts from
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
    fn spawn_spec(&self, ctx: &SpawnContext) -> Result<InteractiveLaunch, ProviderError> {
        let codex_home = ctx.agent_dir.join("codex-home");
        seed_codex_home_once(&codex_home, self.source_home.as_deref())?;
        rewrite_hooks_block(&codex_home, &ctx.factoryctl_path, &ctx.hook_token_path)?;
        rewrite_config_block(&codex_home, &ctx.agent_dir, &ctx.socket_path, &ctx.worktree)?;
        ensure_factoryctl_rule_present(&codex_home)?;

        let mut args = vec!["--dangerously-bypass-hook-trust".to_owned()];
        if let Some(model) = &ctx.model {
            args.push("--model".to_owned());
            args.push(model.clone());
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
/// hooks block, the `factoryctl` rule ([`ensure_factoryctl_rule_present`]),
/// and the sandbox/trust config block are all refreshed separately, every
/// spawn, by their own dedicated `rewrite_*`/`ensure_*` functions.
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
    // identically to "nothing to copy" (review round 2 finding D, same
    // reasoning as `ensure_factoryctl_rule_present`'s own fix below).
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

/// Idempotently ensures `codex_home/rules/default.rules` contains
/// [`FACTORYCTL_PREFIX_RULE`], appending it if an existing file -- copied
/// from the operator by [`seed_rules_directory`], or one Codex itself
/// wrote after an earlier manual approval, or simply absent -- does not
/// already have it. Unlike the seed-once copy above, this runs on *every*
/// spawn: an agent whose `rules/default.rules` already existed before this
/// line shipped (this track's own dogfood fleet had exactly one such
/// agent) would otherwise never receive it, since seeding only ever
/// touches a file that does not yet exist. Never removes anything already
/// in the file -- an operator's own `forbid` rules, or another `prefix_rule`
/// Codex wrote, are left exactly as they were.
///
/// Skips the append if the file already mentions `factoryctl` *at all*,
/// not just this exact `allow` line (review round 2 finding E): an
/// operator whose copied-forward rules already carry, say,
/// `prefix_rule(pattern=["factoryctl"], decision="forbid")` must not get
/// an `allow` appended right next to it -- which decision Codex's own
/// conflict resolution would then apply between the two is not established
/// here, so the safer default is to leave an operator's own existing
/// decision about `factoryctl` alone entirely rather than add a second,
/// possibly contradictory one.
///
/// A missing file is the legitimate empty case (`ErrorKind::NotFound`,
/// e.g. no operator source home at all) and starts from an empty string;
/// any *other* read failure (permission denied, non-UTF-8 content, a
/// transient I/O error) must not be treated the same way -- review round 2
/// finding D: this runs on a file `seed_rules_directory` may have just
/// copied an operator's own `forbid` rules into, and folding a real read
/// failure into "empty file" would overwrite them with a file containing
/// only the `factoryctl` rule, silently destroying them. Fails the spawn
/// instead, like every other write in this module.
fn ensure_factoryctl_rule_present(codex_home: &Path) -> Result<(), ProviderError> {
    let rules_path = codex_home.join("rules").join("default.rules");
    let existing = match fs::read_to_string(&rules_path) {
        Ok(contents) => contents,
        Err(source) if source.kind() == io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(ProviderError::Seed {
                path: rules_path,
                source,
            });
        }
    };
    if existing.contains("factoryctl") {
        return Ok(());
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(FACTORYCTL_PREFIX_RULE);
    hooks::write_private_file(&rules_path, updated.as_bytes()).map_err(|source| {
        ProviderError::Seed {
            path: rules_path,
            source,
        }
    })
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
/// `[sandbox_workspace_write]` (`writable_roots` so the sandbox that gates
/// a session's own spawned tool calls can still reach this agent's
/// guidance directory and the daemon's control socket's directory;
/// `network_access = true`, this track's change from the previous `false`
/// -- confirmed live that `false` denies even the *local* Unix-socket
/// connect the daemon's own control socket needs (seatbelt has no notion
/// of "just localhost"), which blocked not only a worker's `git push`/`gh
/// pr create` but the orchestrator's own non-outbox-covered `factoryctl`
/// calls (`agent add`/`task add`/`task assign`/`session list`; only `task
/// done`/`task blocked`/`agent message` have the outbox fallback,
/// `docs/providers.md`). This is a real widening -- general outbound
/// network access, not just the daemon's socket -- accepted per
/// `SECURITY.md`'s own boundary ("the operating-system user it runs as";
/// "an agent's own `permission_mode` widens or narrows that"), not a gap:
/// the alternative tried and rejected was a provider-wide
/// `danger-full-access` bypass, which traded this hang for a worse one
/// (`codex_apps`'s own MCP server hangs indefinitely at startup under it,
/// `docs/providers.md`)) and `[projects."<worktree>"]` (`trust_level =
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
    block.push_str("network_access = true\n");
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

/// Codex-only hook events, wired in addition to [`hooks::HOOK_EVENTS`]
/// (which both providers share): `PermissionRequest` is a Codex 0.147.0
/// addition with no Claude Code equivalent name (Claude's permission
/// prompts already surface through the shared `Notification` event) -- see
/// `ProviderHookEvent`'s own doc comment for the observe-only contract
/// this relies on.
const CODEX_ONLY_HOOK_EVENTS: [factory_core::ProviderHookEvent; 1] =
    [factory_core::ProviderHookEvent::PermissionRequest];

fn hooks_block_toml(factoryctl_path: &Path, hook_token_path: &Path) -> String {
    let mut block = String::new();
    block.push_str(HOOKS_BEGIN_MARKER);
    block.push('\n');
    for event in hooks::HOOK_EVENTS.into_iter().chain(CODEX_ONLY_HOOK_EVENTS) {
        push_hook_entry(&mut block, factoryctl_path, hook_token_path, event);
    }
    block.push_str(HOOKS_END_MARKER);
    block.push('\n');
    block
}

/// Appends one `[[hooks.<Event>]]` entry, same shape for every event
/// (Codex-only or shared): a single `type = "command"` handler invoking
/// `factoryctl hook`, 30 second timeout. Codex clamps only `SessionEnd`'s
/// timeout (to 3s, confirmed by its own `clamping SessionEnd hook timeout
/// to <n>s` log line); every other event, `PermissionRequest` included,
/// keeps the configured value.
fn push_hook_entry(
    block: &mut String,
    factoryctl_path: &Path,
    hook_token_path: &Path,
    event: factory_core::ProviderHookEvent,
) {
    let name = event.provider_event_name();
    let command = hooks::hook_command(factoryctl_path, hook_token_path, event);
    block.push_str(&format!("[[hooks.{name}]]\n"));
    block.push_str(&format!("[[hooks.{name}.hooks]]\n"));
    block.push_str("type = \"command\"\n");
    block.push_str(&format!("command = \"{}\"\n", toml_escape(&command)));
    block.push_str("timeout = 30\n\n");
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
            auto_mode: true,
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
            vec![
                "--dangerously-bypass-hook-trust".to_owned(),
                "--dangerously-bypass-approvals-and-sandbox".to_owned(),
            ],
            "auto mode bypasses both approvals and the sandbox"
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
                "--dangerously-bypass-hook-trust".to_owned(),
                "-c".to_owned(),
                "approval_policy=\"on-request\"".to_owned(),
            ],
            "an operator's own agent profile permission_mode always wins over \
             DEFAULT_APPROVAL_POLICY"
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

    /// Q2's pre-seeded approval: a fresh agent's very first `factoryctl`
    /// call must never block on the same prompt tonight's dogfood run hit
    /// (`god`'s first `agent add`) -- confirmed against a real dogfood
    /// session's own operator-approved `CODEX_HOME/rules/default.rules`,
    /// which is exactly this shape once Codex itself writes it.
    #[test]
    fn seeds_a_default_rules_file_pre_approving_factoryctl() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        CodexProvider::with_source_home(directory.path().join("missing"))
            .spawn_spec(&ctx)
            .unwrap();

        let rules_path = directory
            .path()
            .join("agent-dir")
            .join("codex-home")
            .join("rules")
            .join("default.rules");
        let contents = fs::read_to_string(&rules_path).unwrap();
        assert_eq!(contents, FACTORYCTL_PREFIX_RULE);

        let metadata = fs::metadata(&rules_path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        let dir_metadata = fs::metadata(rules_path.parent().unwrap()).unwrap();
        assert_eq!(dir_metadata.permissions().mode() & 0o777, 0o700);
    }

    /// Mirrors `copies_the_real_config_once_and_keeps_the_auth_link_on_the_seed_home`'s
    /// own "a real user edit is preserved" proof, for the rules file: once
    /// seeded, a later addition -- an operator's own interactive approval
    /// of some other command, or Codex's own rewrite of the file -- is
    /// never clobbered by a later spawn re-seeding.
    #[test]
    fn a_later_addition_to_the_seeded_rules_file_is_never_clobbered() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        let provider = CodexProvider::with_source_home(directory.path().join("missing"));
        provider.spawn_spec(&ctx).unwrap();

        let rules_path = directory
            .path()
            .join("agent-dir")
            .join("codex-home")
            .join("rules")
            .join("default.rules");
        fs::write(
            &rules_path,
            format!(
                "{FACTORYCTL_PREFIX_RULE}prefix_rule(pattern=[\"sleep\", \"30\"], decision=\"allow\")\n"
            ),
        )
        .unwrap();

        provider.spawn_spec(&ctx).unwrap();
        let contents = fs::read_to_string(&rules_path).unwrap();
        assert!(contents.contains("sleep"));
        assert!(contents.contains(FACTORYCTL_PREFIX_RULE.trim_end()));
    }

    /// Adversarial review finding 4: a real dogfood agent
    /// (`first-floor-worker-2`) already had `rules/default.rules` -- from
    /// Codex itself approving some other command -- without the
    /// `factoryctl` rule, so the old seed-once-if-missing logic would never
    /// have given it one. `ensure_factoryctl_rule_present` runs every
    /// spawn, not just at seed time, so the very next spawn appends it
    /// without disturbing what was already there.
    #[test]
    fn an_existing_rules_file_missing_the_factoryctl_rule_gets_it_appended_on_the_next_spawn() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        let rules_path = directory
            .path()
            .join("agent-dir")
            .join("codex-home")
            .join("rules")
            .join("default.rules");
        fs::create_dir_all(rules_path.parent().unwrap()).unwrap();
        fs::write(
            &rules_path,
            "prefix_rule(pattern=[\"./scripts/local-ci.sh\"], decision=\"allow\")\n",
        )
        .unwrap();

        CodexProvider::with_source_home(directory.path().join("missing"))
            .spawn_spec(&ctx)
            .unwrap();

        let contents = fs::read_to_string(&rules_path).unwrap();
        assert!(
            contents.contains("./scripts/local-ci.sh"),
            "the agent's own already-approved rule must survive"
        );
        assert!(
            contents.contains(FACTORYCTL_PREFIX_RULE.trim_end()),
            "an existing agent must retroactively get the factoryctl rule too"
        );
    }

    /// Adversarial review round 2, finding E: an operator's own explicit
    /// `factoryctl` decision -- even a `forbid` -- must never get a second,
    /// possibly contradictory `allow` appended next to it.
    #[test]
    fn an_existing_factoryctl_forbid_rule_never_gets_an_allow_appended_next_to_it() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        let rules_path = directory
            .path()
            .join("agent-dir")
            .join("codex-home")
            .join("rules")
            .join("default.rules");
        fs::create_dir_all(rules_path.parent().unwrap()).unwrap();
        let forbid_rule = "prefix_rule(pattern=[\"factoryctl\"], decision=\"forbid\")\n";
        fs::write(&rules_path, forbid_rule).unwrap();

        CodexProvider::with_source_home(directory.path().join("missing"))
            .spawn_spec(&ctx)
            .unwrap();

        let contents = fs::read_to_string(&rules_path).unwrap();
        assert_eq!(
            contents, forbid_rule,
            "an operator's own factoryctl decision must be left completely alone, \
             not have an allow appended next to it"
        );
    }

    /// Adversarial review finding 6: an operator's own `rules/*.rules`
    /// files -- including `forbid` rules they wrote to harden their own
    /// `~/.codex` -- must be carried into a fresh agent's seeded home, not
    /// silently dropped the way `config.toml`'s `mcp_servers`/`projects`/
    /// `hooks` tables deliberately are.
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
        assert!(default_contents.contains("git"));
        assert!(
            default_contents.contains(FACTORYCTL_PREFIX_RULE.trim_end()),
            "the operator's own default.rules must still gain the factoryctl rule"
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
        assert_eq!(contents.matches("[[hooks.Stop]]").count(), 1);
        assert_eq!(contents.matches("[[hooks.PermissionRequest]]").count(), 1);
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

    /// `PermissionRequest` (this track's fix for
    /// docs/dogfood/2026-08-17.md's "a session blocked on a provider
    /// approval prompt still shows `working`") is Codex-only -- wired in
    /// addition to `hooks::HOOK_EVENTS`, not a member of it (Claude Code
    /// has no equivalent event name; see `ProviderHookEvent`'s doc
    /// comment) -- so this asserts it separately from the shared-shape
    /// test above, same command/timeout shape, un-clamped 30s timeout
    /// (only `SessionEnd` is clamped, to 3s, confirmed against the real
    /// Codex 0.147.0 binary's own log line).
    #[test]
    fn hooks_block_toml_includes_the_codex_only_permission_request_event() {
        let block = hooks_block_toml(
            Path::new("/abs/factoryctl"),
            Path::new("/abs/runs/session-1/hook.token"),
        );
        assert!(block.contains(
            "[[hooks.PermissionRequest]]\n[[hooks.PermissionRequest.hooks]]\ntype = \"command\"\ncommand = \"'/abs/factoryctl' hook --token-file '/abs/runs/session-1/hook.token' PermissionRequest\"\ntimeout = 30\n"
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
             network_access = true\n\
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
        assert!(
            contents.contains("network_access = true"),
            "workers must reach the network for git push/gh pr create and \
             the orchestrator's own non-outbox-covered factoryctl calls \
             need the daemon's control socket -- see config_block_toml's \
             own doc comment"
        );
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
    fn a_real_configs_own_sandbox_mode_is_replaced_not_duplicated_and_a_surviving_trailing_table_is_undisturbed()
     {
        // The exact shape this track's manual check found on a real
        // machine's `~/.codex/config.toml`: root-level scalars (including
        // the operator's own `sandbox_mode`), then dozens of trailing
        // `[projects."..."]` tables (dropped entirely at seed time by
        // this track's `filter_operator_config_for_seed` -- see the
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
        // factory worker's session (this track's item 7).
        assert!(!contents.contains("/Users/op/other-repo"));
        assert!(!contents.contains("/Users/op/another-repo"));
        // This agent's own worktree still gains a trust entry, from
        // `rewrite_config_block` -- unrelated to what was (or wasn't)
        // seeded.
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

    /// This track's item 7, dedicated fixture: an operator `config.toml`
    /// carrying exactly the three shapes that motivated the fix --
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
        // `[[hooks.SessionStart]]` is dropped from the *seed*; the daemon's
        // own identical-looking header still appears, written fresh by
        // `rewrite_hooks_block` -- this is what proves the seed's own copy
        // was actually filtered, not that the file has zero
        // `hooks.SessionStart` headers at all.
        assert_eq!(contents.matches("[[hooks.SessionStart]]").count(), 1);
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
