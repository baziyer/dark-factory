//! Task-episode delivery into resident agent sessions.
//!
//! TRANSITION (5A -> 5C): the per-run ephemeral-runner supervision loop that
//! used to live here (spawn a fresh `factory-runner` per task, decode its
//! stream-JSON, recover it after a restart) is gone. Migration 0014 moved
//! everything it depended on (runner identity, provider session, observer
//! health) off `runs` and onto `sessions`, and the target model spawns one
//! resident PTY-backed session per agent instead of one process per task
//! (see TRACK5-DESIGN.md section 1 and TRACK5-WIRE.md). Real session
//! spawning, PTY-typed delivery, and restart recovery are 5C's track
//! (`Store::recoverable_sessions`, `Store::create_session`, and the seam at
//! `local_api::stop_hook_reply` are already in place for it).
//!
//! What remains meaningful without that: `StartTask` (the operator's
//! explicit "deliver now") opens a task-episode (`Store::open_run_episode`)
//! inside an agent's *already-live* session. Against an agent with no live
//! session yet, it fails with `Error::NoLiveSession` until 5C wires up
//! spawning.

use std::{
    fs, io,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::PathBuf,
};

use factory_core::{AgentId, ProjectId, RunId, TaskId};
use thiserror::Error;
use tokio::{sync::watch, task::JoinHandle};

use crate::daemon_state::{DaemonState, DaemonStateError};

const PRIVATE_DIRECTORY_MODE: u32 = 0o700;

/// Bounds still meaningful without per-task runner spawning.
pub struct Config {
    /// Root directory future sessions' runner runtime directories live
    /// under. Validated (existing, private, owner-only) at `spawn()` time
    /// so 5C's session spawn has a ready-made private root.
    pub runtime_root: PathBuf,
    /// `$DARK_FACTORY_HOME`: root of the project/agent guidance tree (see
    /// `factory_core::paths` and `guidance`).
    pub guidance_root: PathBuf,
    pub max_active_runs: usize,
}

/// One explicit queued task to deliver now into its agent's live session.
pub struct StartTask {
    pub project_id: ProjectId,
    pub task_id: TaskId,
    pub agent_id: AgentId,
    pub parent_run_id: Option<RunId>,
    pub worktree: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartedRun {
    pub run_id: RunId,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("execution concurrency must be greater than zero")]
    InvalidConcurrency,
    #[error("runner runtime root is not a private owner-only directory")]
    InvalidRuntimeRoot,
    #[error("daemon state failed: {0}")]
    State(#[from] DaemonStateError),
    #[error("execution manager has stopped")]
    ManagerStopped,
    #[error("agent has no live session yet; session spawning lands in a later track")]
    NoLiveSession,
    #[error("system clock is before the Unix epoch")]
    InvalidClock,
}

/// Bounded handle for task-episode delivery.
#[derive(Clone)]
pub struct Handle {
    state: DaemonState,
    shutdown: watch::Sender<bool>,
}

impl Handle {
    /// Opens a task-episode inside the agent's live session. Durable once
    /// this returns `Ok`.
    pub async fn start_task(&self, input: StartTask) -> Result<StartedRun, Error> {
        let StartTask {
            project_id,
            agent_id,
            task_id,
            ..
        } = input;
        let lookup_project_id = project_id.clone();
        let lookup_agent_id = agent_id.clone();
        let session = self
            .state
            .with_store(move |store| {
                store.live_session_for_agent(&lookup_project_id, &lookup_agent_id)
            })
            .await?
            .ok_or(Error::NoLiveSession)?;
        let opened_at_ms = now_ms()?;
        let run_id = self
            .state
            .commit_and_publish(move |store| {
                let opened = store.open_run_episode(&session.id, &task_id, opened_at_ms)?;
                let run_id = opened.run.id.clone();
                Ok((run_id, opened.events))
            })
            .await?;
        Ok(StartedRun { run_id })
    }

    /// Stops accepting new work. There is no background supervision loop to
    /// tear down in this reduced module; kept so `main.rs`'s shutdown
    /// sequencing does not need to know that.
    pub async fn shutdown(&self) -> Result<(), Error> {
        let _ = self.shutdown.send(true);
        Ok(())
    }
}

/// Starts the (currently trivial) execution manager.
pub fn spawn(
    config: Config,
    state: DaemonState,
) -> Result<(Handle, JoinHandle<Result<(), Error>>), Error> {
    if config.max_active_runs == 0 {
        return Err(Error::InvalidConcurrency);
    }
    prepare_runtime_root(&config.runtime_root)?;
    let runtime = tokio::runtime::Handle::try_current().map_err(|_| Error::ManagerStopped)?;
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let join = runtime.spawn(async move {
        let _ = shutdown_rx.changed().await;
        Ok(())
    });
    Ok((
        Handle {
            state,
            shutdown: shutdown_tx,
        },
        join,
    ))
}

fn now_ms() -> Result<i64, Error> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| Error::InvalidClock)?
        .as_millis();
    i64::try_from(millis).map_err(|_| Error::InvalidClock)
}

fn prepare_runtime_root(path: &std::path::Path) -> Result<(), Error> {
    if !path.is_absolute() {
        return Err(Error::InvalidRuntimeRoot);
    }
    let parent = path.parent().ok_or(Error::InvalidRuntimeRoot)?;
    match fs::symlink_metadata(parent) {
        Ok(metadata) => verify_private_directory_metadata(&metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(PRIVATE_DIRECTORY_MODE)
                .create(parent)
                .map_err(|_| Error::InvalidRuntimeRoot)?;
            let metadata = fs::symlink_metadata(parent).map_err(|_| Error::InvalidRuntimeRoot)?;
            verify_private_directory_metadata(&metadata)?;
        }
        Err(_) => return Err(Error::InvalidRuntimeRoot),
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) => verify_private_directory_metadata(&metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .mode(PRIVATE_DIRECTORY_MODE)
                .create(path)
                .map_err(|_| Error::InvalidRuntimeRoot)?;
            let metadata = fs::symlink_metadata(path).map_err(|_| Error::InvalidRuntimeRoot)?;
            verify_private_directory_metadata(&metadata)?;
        }
        Err(_) => return Err(Error::InvalidRuntimeRoot),
    }
    Ok(())
}

fn verify_private_directory_metadata(metadata: &fs::Metadata) -> Result<(), Error> {
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o777 != PRIVATE_DIRECTORY_MODE
    {
        return Err(Error::InvalidRuntimeRoot);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::store::Store;

    fn private_tempdir() -> tempfile::TempDir {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    fn config(directory: &std::path::Path) -> Config {
        Config {
            runtime_root: directory.join("runs"),
            guidance_root: directory.to_path_buf(),
            max_active_runs: 1,
        }
    }

    #[tokio::test]
    async fn spawn_rejects_zero_concurrency() {
        let directory = private_tempdir();
        let state = DaemonState::new(Store::open_in_memory().unwrap());
        let mut cfg = config(directory.path());
        cfg.max_active_runs = 0;
        assert!(matches!(spawn(cfg, state), Err(Error::InvalidConcurrency)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_task_without_a_live_session_fails_clearly() {
        let directory = private_tempdir();
        let state = DaemonState::new(Store::open_in_memory().unwrap());
        let (handle, join) = spawn(config(directory.path()), state).unwrap();
        let result = handle
            .start_task(StartTask {
                project_id: ProjectId::try_from("factory").unwrap(),
                task_id: TaskId::try_from("task-1").unwrap(),
                agent_id: AgentId::try_from("agent-1").unwrap(),
                parent_run_id: None,
                worktree: directory.path().to_path_buf(),
            })
            .await;
        assert!(matches!(result, Err(Error::NoLiveSession)));
        handle.shutdown().await.unwrap();
        join.await.unwrap().unwrap();
    }
}
