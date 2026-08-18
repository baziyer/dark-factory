//! One git worktree per agent (D3, `TRACK5-WIRE.md`): `CreateAgent`
//! provisions `$DARK_FACTORY_HOME/projects/<project>/worktrees/<agent>` on
//! branch `agent/<agent_id>` from the project's default branch, and a
//! resident session's cwd is that worktree; `DeleteAgent` removes it again
//! unless it is dirty.
//!
//! Every operation shells out to the real `git` binary (`tokio::process`,
//! non-blocking) rather than a Git library: worktrees are an operator-
//! visible, inspectable directory the daemon does not otherwise touch, and
//! `git`'s own CLI is the one thing guaranteed to agree with whatever else
//! (the agent's own git commands, the operator's) touches the same
//! checkout concurrently.

use std::{path::Path, time::Duration};

use factory_core::status::WorktreeStatus;
use tokio::process::Command;

const STATUS_DEADLINE: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("git worktree operation failed: {0}")]
    Git(String),
    #[error(
        "worktree has modified or untracked files; commit, discard, or remove it manually \
             with `git worktree remove --force`"
    )]
    Dirty,
    #[error("could not run git: {0}")]
    Io(#[from] std::io::Error),
}

/// Whether `project_root` is inside a git working tree at all -- the
/// fallback case (`D3`: "use the project root") uses this to decide
/// whether to attempt a worktree in the first place.
pub async fn is_git_repo(project_root: &Path) -> bool {
    run_git(project_root, &["rev-parse", "--is-inside-work-tree"])
        .await
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true"
        })
}

/// Creates `worktree_dir` as a new worktree of the repository at
/// `project_root`, on `branch`, from the remote's default branch
/// (`origin/HEAD`), falling back to local `main` for repositories without
/// a remote. If `branch` already exists
/// (an agent re-created after a prior `DeleteAgent`, or a hand-created
/// branch of the same name), reuses it instead of failing -- matching
/// `TRACK5-WIRE.md`'s "branch may already exist -> reuse".
pub async fn add(
    project_root: &Path,
    worktree_dir: &Path,
    branch: &str,
) -> Result<(), WorktreeError> {
    if let Some(parent) = worktree_dir.parent() {
        let parent = parent.to_path_buf();
        tokio::task::spawn_blocking(move || std::fs::create_dir_all(parent))
            .await
            .map_err(|error| WorktreeError::Git(format!("directory worker panicked: {error}")))??;
    }
    let target = worktree_dir.to_string_lossy().into_owned();
    let branch_ref = format!("refs/heads/{branch}");
    if git_ref_exists(project_root, &branch_ref).await? {
        return add_existing_branch(project_root, &target, branch).await;
    }
    let base = if git_ref_exists(project_root, "refs/remotes/origin/HEAD").await? {
        "refs/remotes/origin/HEAD"
    } else if git_ref_exists(project_root, "refs/heads/main").await? {
        "refs/heads/main"
    } else {
        return Err(WorktreeError::Git(
            "project has neither origin/HEAD nor a local main branch".into(),
        ));
    };
    let fresh = run_git(
        project_root,
        &["worktree", "add", "-b", branch, &target, base],
    )
    .await?;
    if fresh.status.success() {
        return Ok(());
    }
    let fresh_stderr = String::from_utf8_lossy(&fresh.stderr).into_owned();
    if !fresh_stderr.contains("already exists") {
        return Err(WorktreeError::Git(fresh_stderr));
    }
    add_existing_branch(project_root, &target, branch).await
}

async fn add_existing_branch(
    project_root: &Path,
    target: &str,
    branch: &str,
) -> Result<(), WorktreeError> {
    let reused = run_git(project_root, &["worktree", "add", target, branch]).await?;
    if reused.status.success() {
        return Ok(());
    }
    Err(WorktreeError::Git(
        String::from_utf8_lossy(&reused.stderr).into_owned(),
    ))
}

async fn git_ref_exists(project_root: &Path, reference: &str) -> Result<bool, WorktreeError> {
    let output = run_git(
        project_root,
        &["show-ref", "--verify", "--quiet", reference],
    )
    .await?;
    Ok(output.status.success())
}

/// Removes `worktree_dir` from the repository at `project_root`. Refuses
/// (`WorktreeError::Dirty`) on modified or untracked files rather than
/// discarding them; the caller decides what that means for the request
/// that triggered it (`DeleteAgent` -> `Conflict`, per `TRACK5-WIRE.md`).
pub async fn remove(project_root: &Path, worktree_dir: &Path) -> Result<(), WorktreeError> {
    let target = worktree_dir.to_string_lossy().into_owned();
    let output = run_git(project_root, &["worktree", "remove", &target]).await?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("contains modified or untracked files")
        || stderr.contains("is dirty")
        || stderr.contains("locked working tree")
    {
        return Err(WorktreeError::Dirty);
    }
    Err(WorktreeError::Git(stderr.into_owned()))
}

/// `git status --porcelain=v1 --branch --no-optional-locks` of
/// `worktree_dir`, summarized for `factoryctl agent status`: the branch
/// (`None` on a detached `HEAD`) and how many entries are modified, staged,
/// or untracked. `--no-optional-locks` keeps this read from taking
/// `index.lock` under a live agent's own `git`. A failure (the directory is
/// gone, not a repository, ...) is reported in the summary's `error`, never
/// as a clean tree.
pub async fn status(worktree_dir: &Path) -> WorktreeStatus {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(worktree_dir)
        .args([
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "--branch",
        ])
        .env("GIT_TERMINAL_PROMPT", "0");
    status_command(worktree_dir, command, STATUS_DEADLINE).await
}

async fn status_command(
    worktree_dir: &Path,
    mut command: Command,
    deadline: Duration,
) -> WorktreeStatus {
    let path = worktree_dir.to_string_lossy().into_owned();
    let failed = |error: String| WorktreeStatus {
        path: path.clone(),
        branch: None,
        changed_files: 0,
        dirty: false,
        error: Some(error),
    };
    command.kill_on_drop(true);
    let output = match tokio::time::timeout(deadline, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => return failed(format!("could not run git: {error}")),
        Err(_) => return failed("git status timed out".to_owned()),
    };
    if !output.status.success() {
        return failed(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut branch = None;
    let mut changed_files = 0u32;
    for line in text.lines() {
        if let Some(header) = line.strip_prefix("## ") {
            // `main...origin/main [ahead 1]`, `HEAD (no branch)`, or
            // `No commits yet on main`.
            let name = header.split("...").next().unwrap_or(header).trim();
            branch = if name.starts_with("HEAD") {
                None
            } else {
                Some(
                    name.strip_prefix("No commits yet on ")
                        .unwrap_or(name)
                        .to_owned(),
                )
            };
        } else if !line.is_empty() {
            changed_files += 1;
        }
    }
    WorktreeStatus {
        path,
        branch,
        changed_files,
        dirty: changed_files > 0,
        error: None,
    }
}

async fn run_git(project_root: &Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn git(project_root: &Path, args: &[&str]) {
        let output = run_git(project_root, args).await.unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    async fn init_repo(directory: &Path) {
        git(directory, &["init", "-q", "-b", "main"]).await;
        git(directory, &["config", "user.email", "test@example.com"]).await;
        git(directory, &["config", "user.name", "Test"]).await;
        std::fs::write(directory.join("README.md"), b"hello\n").unwrap();
        git(directory, &["add", "README.md"]).await;
        git(directory, &["commit", "-q", "-m", "initial"]).await;
    }

    #[tokio::test]
    async fn is_git_repo_distinguishes_a_real_repo_from_a_plain_directory() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;
        assert!(is_git_repo(repo.path()).await);

        let plain = tempfile::tempdir().unwrap();
        assert!(!is_git_repo(plain.path()).await);
    }

    #[tokio::test]
    async fn add_creates_a_worktree_on_a_new_branch_from_main() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;
        let worktree_dir = repo.path().join("worktrees").join("curie");

        add(repo.path(), &worktree_dir, "agent/curie")
            .await
            .unwrap();

        assert!(worktree_dir.join("README.md").is_file());
        let output = run_git(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "agent/curie"
        );
    }

    #[tokio::test]
    async fn add_ignores_the_project_roots_current_checkout() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;
        git(repo.path(), &["checkout", "-q", "-b", "integration"]).await;
        std::fs::write(repo.path().join("integration-only.txt"), b"wrong base\n").unwrap();
        git(repo.path(), &["add", "integration-only.txt"]).await;
        git(repo.path(), &["commit", "-q", "-m", "integration work"]).await;
        let worktree_dir = repo.path().join("worktrees").join("curie");

        add(repo.path(), &worktree_dir, "agent/curie")
            .await
            .unwrap();

        assert!(!worktree_dir.join("integration-only.txt").exists());
        let base = run_git(
            &worktree_dir,
            &["merge-base", "--is-ancestor", "main", "HEAD"],
        )
        .await
        .unwrap();
        assert!(base.status.success());
    }

    #[tokio::test]
    async fn add_prefers_the_remote_default_over_local_main() {
        let source = tempfile::tempdir().unwrap();
        init_repo(source.path()).await;
        git(source.path(), &["checkout", "-q", "-b", "trunk"]).await;
        std::fs::write(source.path().join("TRUNK.md"), b"default\n").unwrap();
        git(source.path(), &["add", "TRUNK.md"]).await;
        git(source.path(), &["commit", "-q", "-m", "trunk work"]).await;
        let bare = tempfile::tempdir().unwrap();
        git(bare.path(), &["init", "-q", "--bare"]).await;
        let bare_path = bare.path().to_string_lossy();
        git(source.path(), &["remote", "add", "origin", &bare_path]).await;
        git(source.path(), &["push", "-q", "origin", "main", "trunk"]).await;
        git(bare.path(), &["symbolic-ref", "HEAD", "refs/heads/trunk"]).await;
        git(source.path(), &["fetch", "-q", "origin"]).await;
        git(source.path(), &["remote", "set-head", "origin", "-a"]).await;
        git(source.path(), &["checkout", "-q", "main"]).await;
        let worktree_dir = source.path().join("worktrees").join("curie");

        add(source.path(), &worktree_dir, "agent/curie")
            .await
            .unwrap();

        assert!(worktree_dir.join("TRUNK.md").is_file());
    }

    #[tokio::test]
    async fn status_reports_the_branch_and_counts_changed_files() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;
        let worktree_dir = repo.path().join("worktrees").join("curie");
        add(repo.path(), &worktree_dir, "agent/curie")
            .await
            .unwrap();

        let clean = status(&worktree_dir).await;
        assert_eq!(clean.branch.as_deref(), Some("agent/curie"));
        assert_eq!(clean.changed_files, 0);
        assert!(!clean.dirty);
        assert_eq!(clean.error, None);

        std::fs::write(worktree_dir.join("README.md"), "changed\n").unwrap();
        std::fs::write(worktree_dir.join("new.txt"), "untracked\n").unwrap();
        let dirty = status(&worktree_dir).await;
        assert_eq!(dirty.changed_files, 2);
        assert!(dirty.dirty);
        assert_eq!(dirty.path, worktree_dir.to_string_lossy());

        // Detached HEAD: no branch, still a status.
        git(&worktree_dir, &["checkout", "--detach"]).await;
        assert_eq!(status(&worktree_dir).await.branch, None);

        // A missing directory or a plain directory reports the failure, not a clean tree.
        let missing = status(&repo.path().join("missing")).await;
        assert!(missing.error.is_some());
        assert!(!missing.dirty);
        let plain = tempfile::tempdir().unwrap();
        assert!(status(plain.path()).await.error.is_some());
    }

    #[tokio::test]
    async fn status_reports_a_stalled_git_probe_as_unavailable() {
        let directory = tempfile::tempdir().unwrap();
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 60"]);

        let status = status_command(directory.path(), command, Duration::from_millis(10)).await;

        assert_eq!(status.error.as_deref(), Some("git status timed out"));
        assert!(!status.dirty);
        assert_eq!(status.changed_files, 0);
    }

    #[tokio::test]
    async fn add_reuses_an_existing_branch_of_the_same_name() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;
        git(repo.path(), &["branch", "agent/curie"]).await;
        let worktree_dir = repo.path().join("worktrees").join("curie");

        add(repo.path(), &worktree_dir, "agent/curie")
            .await
            .unwrap();

        assert!(worktree_dir.join("README.md").is_file());
    }

    #[tokio::test]
    async fn add_reuses_an_existing_branch_without_default_branch_metadata() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;
        git(repo.path(), &["branch", "-m", "trunk"]).await;
        git(repo.path(), &["branch", "agent/curie"]).await;
        let worktree_dir = repo.path().join("worktrees").join("curie");

        add(repo.path(), &worktree_dir, "agent/curie")
            .await
            .unwrap();

        let branch = run_git(&worktree_dir, &["branch", "--show-current"])
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&branch.stdout).trim(),
            "agent/curie"
        );
    }

    #[tokio::test]
    async fn remove_deletes_a_clean_worktree() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;
        let worktree_dir = repo.path().join("worktrees").join("curie");
        add(repo.path(), &worktree_dir, "agent/curie")
            .await
            .unwrap();

        remove(repo.path(), &worktree_dir).await.unwrap();

        assert!(!worktree_dir.exists());
    }

    #[tokio::test]
    async fn remove_refuses_a_dirty_worktree() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;
        let worktree_dir = repo.path().join("worktrees").join("curie");
        add(repo.path(), &worktree_dir, "agent/curie")
            .await
            .unwrap();
        std::fs::write(worktree_dir.join("scratch.txt"), b"uncommitted").unwrap();

        let error = remove(repo.path(), &worktree_dir).await.unwrap_err();

        assert!(matches!(error, WorktreeError::Dirty));
        assert!(worktree_dir.exists());
    }
}
