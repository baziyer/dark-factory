//! Filesystem layout for operator- and agent-editable guidance files.
//!
//! Project and agent guidance, memory, and standing instructions live as
//! plain Markdown files in a sister folder under `$DARK_FACTORY_HOME`,
//! Munder-Difflin style, rather than as opaque database columns. SQLite
//! remains the durable ledger for projects, agents, tasks, runs, events, and
//! messages; this module only computes the paths for the guidance tree. It
//! performs no filesystem I/O itself so both the daemon and local clients can
//! agree on the layout without either one needing filesystem access.
//!
//! Layout:
//!
//! ```text
//! $DARK_FACTORY_HOME/projects/<project_id>/PROJECT.md
//! $DARK_FACTORY_HOME/projects/<project_id>/agents/<agent_id>/instructions.md
//! $DARK_FACTORY_HOME/projects/<project_id>/agents/<agent_id>/memory.md
//! $DARK_FACTORY_HOME/projects/<project_id>/agents/<agent_id>/codex-home/       (reserved, not yet created)
//! $DARK_FACTORY_HOME/projects/<project_id>/agents/<agent_id>/claude-settings.json  (reserved, not yet created)
//! $DARK_FACTORY_HOME/projects/<project_id>/worktrees/<agent_id>/               (reserved, not yet created)
//! ```
//!
//! The `codex-home`, `claude-settings.json`, and `worktrees/<agent_id>`
//! entries are reserved paths for later tracks (per-agent `CODEX_HOME`,
//! generated Claude Code hooks settings, and a per-agent git worktree); this
//! module only computes them today and creates none of them.
//!
//! `ProjectId` and `AgentId` are already constrained to ASCII letters,
//! digits, hyphens, and underscores, so they are always safe single path
//! components (never `.`, `..`, or containing a path separator).

use std::{
    env, error,
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
};

use crate::{AgentId, ProjectId};

/// `$DARK_FACTORY_HOME` (or `$HOME/.dark-factory`) could not be resolved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HomeError;

impl fmt::Display for HomeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HOME or DARK_FACTORY_HOME must be set")
    }
}

impl error::Error for HomeError {}

/// Resolves the daemon's private state root exactly the way `factoryd`
/// resolves it: `$DARK_FACTORY_HOME` if set, otherwise `$HOME/.dark-factory`.
///
/// This is the single source of truth for that resolution; the daemon and
/// any local client must call this instead of duplicating the lookup.
pub fn dark_factory_home() -> Result<PathBuf, HomeError> {
    resolve_home(env::var_os("DARK_FACTORY_HOME"), env::var_os("HOME"))
}

fn resolve_home(explicit: Option<OsString>, home: Option<OsString>) -> Result<PathBuf, HomeError> {
    if let Some(path) = explicit {
        return Ok(PathBuf::from(path));
    }
    let home = home.ok_or(HomeError)?;
    Ok(PathBuf::from(home).join(".dark-factory"))
}

/// Root of every project's guidance tree: `<home>/projects`.
#[must_use]
pub fn projects_root(home: &Path) -> PathBuf {
    home.join("projects")
}

/// One project's guidance directory: `<home>/projects/<project_id>`.
#[must_use]
pub fn project_dir(home: &Path, project_id: &ProjectId) -> PathBuf {
    projects_root(home).join(project_id.as_str())
}

/// Operator-editable project guidance: `<project_dir>/PROJECT.md`.
#[must_use]
pub fn project_guidance_path(home: &Path, project_id: &ProjectId) -> PathBuf {
    project_dir(home, project_id).join("PROJECT.md")
}

/// Root of one project's agent guidance trees: `<project_dir>/agents`.
#[must_use]
pub fn agents_root(home: &Path, project_id: &ProjectId) -> PathBuf {
    project_dir(home, project_id).join("agents")
}

/// One agent's guidance directory: `<agents_root>/<agent_id>`.
#[must_use]
pub fn agent_dir(home: &Path, project_id: &ProjectId, agent_id: &AgentId) -> PathBuf {
    agents_root(home, project_id).join(agent_id.as_str())
}

/// Standing guidance an operator or the agent itself can edit:
/// `<agent_dir>/instructions.md`.
#[must_use]
pub fn agent_instructions_path(home: &Path, project_id: &ProjectId, agent_id: &AgentId) -> PathBuf {
    agent_dir(home, project_id, agent_id).join("instructions.md")
}

/// Durable memory the agent itself is expected to append to:
/// `<agent_dir>/memory.md`.
#[must_use]
pub fn agent_memory_path(home: &Path, project_id: &ProjectId, agent_id: &AgentId) -> PathBuf {
    agent_dir(home, project_id, agent_id).join("memory.md")
}

/// Reserved path for a future per-agent git worktree, sibling to `agents/`:
/// `<project_dir>/worktrees/<agent_id>`. Not created by this track; a later
/// track owns provisioning it (path computation only).
#[must_use]
pub fn agent_worktree_dir(home: &Path, project_id: &ProjectId, agent_id: &AgentId) -> PathBuf {
    project_dir(home, project_id)
        .join("worktrees")
        .join(agent_id.as_str())
}

/// Reserved path for a future per-agent `CODEX_HOME`:
/// `<agent_dir>/codex-home`. Not created by this track (path computation
/// only).
#[must_use]
pub fn agent_codex_home_dir(home: &Path, project_id: &ProjectId, agent_id: &AgentId) -> PathBuf {
    agent_dir(home, project_id, agent_id).join("codex-home")
}

/// Reserved path for a future generated Claude Code hooks settings file:
/// `<agent_dir>/claude-settings.json`. Not created by this track (path
/// computation only).
#[must_use]
pub fn agent_claude_settings_path(
    home: &Path,
    project_id: &ProjectId,
    agent_id: &AgentId,
) -> PathBuf {
    agent_dir(home, project_id, agent_id).join("claude-settings.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> ProjectId {
        ProjectId::try_from("factory").unwrap()
    }

    fn agent() -> AgentId {
        AgentId::try_from("god").unwrap()
    }

    #[test]
    fn layout_matches_the_documented_sister_folder() {
        let home = Path::new("/home/user/.dark-factory");
        assert_eq!(
            project_guidance_path(home, &project()),
            Path::new("/home/user/.dark-factory/projects/factory/PROJECT.md")
        );
        assert_eq!(
            agent_instructions_path(home, &project(), &agent()),
            Path::new("/home/user/.dark-factory/projects/factory/agents/god/instructions.md")
        );
        assert_eq!(
            agent_memory_path(home, &project(), &agent()),
            Path::new("/home/user/.dark-factory/projects/factory/agents/god/memory.md")
        );
    }

    #[test]
    fn reserved_future_paths_match_the_documented_sister_folder() {
        let home = Path::new("/home/user/.dark-factory");
        assert_eq!(
            agent_worktree_dir(home, &project(), &agent()),
            Path::new("/home/user/.dark-factory/projects/factory/worktrees/god")
        );
        assert_eq!(
            agent_codex_home_dir(home, &project(), &agent()),
            Path::new("/home/user/.dark-factory/projects/factory/agents/god/codex-home")
        );
        assert_eq!(
            agent_claude_settings_path(home, &project(), &agent()),
            Path::new("/home/user/.dark-factory/projects/factory/agents/god/claude-settings.json")
        );
    }

    #[test]
    fn resolve_home_prefers_the_explicit_override() {
        assert_eq!(
            resolve_home(
                Some(OsString::from("/explicit/state")),
                Some(OsString::from("/home/user"))
            )
            .unwrap(),
            Path::new("/explicit/state")
        );
    }

    #[test]
    fn resolve_home_falls_back_to_home_dot_dark_factory() {
        assert_eq!(
            resolve_home(None, Some(OsString::from("/home/user"))).unwrap(),
            Path::new("/home/user/.dark-factory")
        );
    }

    #[test]
    fn resolve_home_requires_one_of_the_two_variables() {
        assert_eq!(resolve_home(None, None), Err(HomeError));
    }
}
