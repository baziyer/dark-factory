//! `factoryctl doctor`: read-only checks of this machine's Dark Factory
//! install, one line per check, non-zero exit if anything fails. Every
//! probe it uses (`crate::probes`) is the same one `factoryctl init` and
//! `update --install` use, so the three commands can't disagree about what
//! "healthy" means.

use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

use factory_core::{
    ProjectId,
    local::{
        LocalRequest, LocalResponse, MAX_AGENT_PAGE_ITEMS, MAX_PROJECT_PAGE_ITEMS, ServerFrame,
    },
};
use factoryctl::{Client, update};
use serde::Serialize;

use crate::{
    install, launchd,
    probes::{self, PROBED_PROGRAMS},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

#[derive(Clone, Debug, Serialize)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
}

impl Check {
    fn ok(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: Status::Ok,
            detail: detail.into(),
        }
    }
    fn warn(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: Status::Warn,
            detail: detail.into(),
        }
    }
    fn fail(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: Status::Fail,
            detail: detail.into(),
        }
    }
}

pub struct Options {
    pub json: bool,
}

pub fn run(options: &Options, socket: &Path) -> Result<i32, String> {
    let home = factory_core::paths::dark_factory_home().map_err(|error| error.to_string())?;
    let user_home = std::env::var_os("HOME").map(PathBuf::from);
    let mut checks = Vec::new();

    checks.push(check_home(&home));
    checks.push(check_install(&home));
    let daemon = check_daemon(socket);
    let daemon_reachable = daemon.status != Status::Fail;
    checks.push(daemon);
    checks.push(check_launchd(user_home.as_deref()));
    for program in PROBED_PROGRAMS {
        checks.push(check_program(program));
    }
    checks.push(check_claude_json(user_home.as_deref()));
    if daemon_reachable {
        checks.extend(check_projects(&Client::new(socket), &home));
    }
    checks.push(check_update(&home));

    let failed = checks.iter().any(|check| check.status == Status::Fail);
    if options.json {
        println!("{}", serde_json::json!({ "ok": !failed, "checks": checks }));
    } else {
        for check in &checks {
            let tag = match check.status {
                Status::Ok => "ok  ",
                Status::Warn => "warn",
                Status::Fail => "FAIL",
            };
            println!("[{tag}] {}: {}", check.name, check.detail);
        }
    }
    Ok(if failed { 1 } else { 0 })
}

fn check_home(home: &Path) -> Check {
    let display = home.display();
    let metadata = match fs::symlink_metadata(home) {
        Ok(metadata) => metadata,
        Err(_) => {
            return Check::fail(
                "home",
                format!("{display} does not exist (run `factoryctl init`)"),
            );
        }
    };
    if metadata.file_type().is_symlink() {
        return Check::fail(
            "home",
            format!("{display} must not be a symbolic link (the daemon refuses it too)"),
        );
    }
    if !metadata.is_dir() {
        return Check::fail("home", format!("{display} is not a directory"));
    }
    let uid = rustix::process::getuid().as_raw();
    if metadata.uid() != uid {
        return Check::fail(
            "home",
            format!("{display} is owned by uid {}, not {uid}", metadata.uid()),
        );
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode != 0o700 {
        return Check::fail(
            "home",
            format!("{display} has mode {mode:04o}; the daemon requires 0700"),
        );
    }
    Check::ok("home", format!("{display} (0700)"))
}

fn check_install(home: &Path) -> Check {
    let link = install::current_link(home);
    match fs::read_link(&link) {
        Ok(target) => {
            let version = target.to_string_lossy().into_owned();
            match install::verify_binaries(&install::version_dir(home, &version)) {
                Ok(()) => Check::ok(
                    "install",
                    format!(
                        "bin/current -> {version} (this factoryctl is {})",
                        update::CURRENT_VERSION
                    ),
                ),
                Err(error) => {
                    Check::fail("install", format!("bin/current -> {version}, but {error}"))
                }
            }
        }
        Err(_) => Check::warn(
            "install",
            format!(
                "no installed release at {} (running {} from {}; `factoryctl init` installs it)",
                link.display(),
                update::CURRENT_VERSION,
                std::env::current_exe()
                    .ok()
                    .and_then(|exe| exe.parent().map(Path::to_path_buf))
                    .map_or_else(|| "?".to_owned(), |dir| dir.display().to_string())
            ),
        ),
    }
}

fn check_daemon(socket: &Path) -> Check {
    match Client::new(socket).request_with_timeout(LocalRequest::Health, Duration::from_secs(5)) {
        Ok(ServerFrame::Response {
            response: LocalResponse::Health { version, .. },
            ..
        }) => {
            if version.is_empty() {
                Check::warn(
                    "daemon",
                    format!(
                        "reachable at {} but predates version reporting",
                        socket.display()
                    ),
                )
            } else if version != update::CURRENT_VERSION {
                Check::warn(
                    "daemon",
                    format!(
                        "running {version} at {}, this factoryctl is {} (restart the daemon to pick up the installed binaries)",
                        socket.display(),
                        update::CURRENT_VERSION
                    ),
                )
            } else {
                Check::ok("daemon", format!("{version} at {}", socket.display()))
            }
        }
        Ok(_) => Check::fail("daemon", "unexpected reply to health"),
        Err(error) => Check::fail(
            "daemon",
            format!("{} unreachable: {error}", socket.display()),
        ),
    }
}

fn check_launchd(user_home: Option<&Path>) -> Check {
    let Some(user_home) = user_home else {
        return Check::warn("launchd", "HOME is not set; cannot locate the job");
    };
    let plist = launchd::plist_path(user_home);
    let existing = match launchd::read_existing(&plist) {
        Ok(Some(existing)) => existing,
        Ok(None) => {
            return Check::warn(
                "launchd",
                format!(
                    "{} not installed (the daemon is not managed by launchd)",
                    plist.display()
                ),
            );
        }
        Err(error) => return Check::fail("launchd", error),
    };
    let program = existing.program_arguments.first().map(PathBuf::from);
    let program_ok = program
        .as_deref()
        .and_then(|program| fs::metadata(program).ok())
        .is_some_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0);
    if !program_ok {
        return Check::fail(
            "launchd",
            format!(
                "{} runs {} which is not an executable file",
                plist.display(),
                program.map_or_else(|| "(nothing)".to_owned(), |p| p.display().to_string())
            ),
        );
    }
    // A job with no PATH of its own runs with launchd's default -- the exact
    // footgun launchd/README.md warns about -- so that is what gets checked.
    let path_env = existing
        .environment
        .get("PATH")
        .cloned()
        .unwrap_or_else(|| launchd::LAUNCHD_DEFAULT_PATH.to_owned());
    let mut missing = Vec::new();
    for program in ["claude", "codex"] {
        if let Some(found) = probes::locate_on_path(program) {
            let directory = found.parent().map(Path::to_path_buf).unwrap_or_default();
            if !std::env::split_paths(&path_env).any(|entry| entry == directory) {
                missing.push(format!("{program} ({})", directory.display()));
            }
        }
    }
    let loaded = probes::launchd_loaded();
    match (loaded, missing.is_empty()) {
        (true, true) => Check::ok("launchd", format!("{} loaded", launchd::LABEL)),
        (true, false) => Check::warn(
            "launchd",
            format!(
                "loaded, but its PATH lacks the directory of {} — sessions won't find it (`factoryctl init` re-renders the job)",
                missing.join(", ")
            ),
        ),
        (false, _) => Check::warn(
            "launchd",
            format!(
                "{} exists but is not loaded (launchctl bootstrap gui/$(id -u) {})",
                plist.display(),
                plist.display()
            ),
        ),
    }
}

fn check_program(program: &'static str) -> Check {
    match probes::locate_on_path(program) {
        Some(path) => match probes::probe_version(&path) {
            Some(version) => Check::ok(program, format!("{version} ({})", path.display())),
            None => Check::warn(
                program,
                format!("{} found but --version failed", path.display()),
            ),
        },
        None => {
            if program == "git" {
                Check::warn(
                    "git",
                    "not on PATH; agents get no worktree of their own and run in the project root",
                )
            } else {
                Check::warn(
                    program,
                    "not on PATH; agents with this provider cannot start",
                )
            }
        }
    }
}

fn check_claude_json(user_home: Option<&Path>) -> Check {
    let Some(user_home) = user_home else {
        return Check::warn("claude.json", "HOME is not set");
    };
    let path = user_home.join(".claude.json");
    match fs::read(&path) {
        Ok(bytes) if serde_json::from_slice::<serde_json::Value>(&bytes).is_ok() => Check::ok(
            "claude.json",
            format!("{} present; worktree pre-trust applies", path.display()),
        ),
        Ok(_) => Check::warn(
            "claude.json",
            format!(
                "{} is not valid JSON; worktree pre-trust will be skipped",
                path.display()
            ),
        ),
        Err(_) => Check::warn(
            "claude.json",
            format!(
                "{} missing (run `claude` once); until then every new Claude session asks to trust its worktree",
                path.display()
            ),
        ),
    }
}

/// Every project's root, and any worktree directory under
/// `projects/<id>/worktrees/` that no current agent owns.
fn check_projects(client: &Client, home: &Path) -> Vec<Check> {
    let projects = match list_projects(client) {
        Ok(projects) => projects,
        Err(error) => {
            return vec![Check::warn(
                "projects",
                format!("could not list projects: {error}"),
            )];
        }
    };
    if projects.is_empty() {
        return vec![Check::ok("projects", "none yet (`factoryctl project add`)")];
    }
    let mut checks = Vec::new();
    for project in projects {
        let root = Path::new(&project.root);
        if !root.is_dir() {
            checks.push(Check::fail(
                format!("project:{}", project.id),
                format!("root {} is missing", root.display()),
            ));
            continue;
        }
        let agent_ids = match list_agent_ids(client, &project.id) {
            Ok(ids) => ids,
            Err(error) => {
                checks.push(Check::warn(
                    format!("project:{}", project.id),
                    format!("could not list agents: {error}"),
                ));
                continue;
            }
        };
        let worktrees_dir = factory_core::paths::project_dir(home, &project.id).join("worktrees");
        let stale: Vec<String> = fs::read_dir(&worktrees_dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                    .filter_map(|entry| entry.file_name().into_string().ok())
                    .filter(|name| !agent_ids.iter().any(|id| id == name))
                    .collect()
            })
            .unwrap_or_default();
        if stale.is_empty() {
            checks.push(Check::ok(
                format!("project:{}", project.id),
                format!("{} agent(s), root {}", agent_ids.len(), root.display()),
            ));
        } else {
            checks.push(Check::warn(
                format!("project:{}", project.id),
                format!(
                    "stale worktree dir(s) with no agent: {} (git -C {} worktree remove <path>, then delete the directory)",
                    stale.join(", "),
                    root.display()
                ),
            ));
        }
    }
    checks
}

fn check_update(home: &Path) -> Check {
    let check = update::check(home, &update::manifest_url(), update::now_ms(), false);
    match (check.available(), &check.error) {
        (Some(manifest), _) => Check::warn(
            "update",
            format!(
                "v{} available (`factoryctl update --install`)",
                manifest.version
            ),
        ),
        (None, Some(error)) => Check::warn("update", format!("could not check: {error}")),
        (None, None) => Check::ok(
            "update",
            format!("{} is the latest release", update::CURRENT_VERSION),
        ),
    }
}

fn list_projects(client: &Client) -> Result<Vec<factory_core::ProjectSnapshot>, String> {
    let mut all = Vec::new();
    let mut after_id = None;
    loop {
        let frame = client
            .request(LocalRequest::ListProjects {
                after_id: after_id.clone(),
                limit: MAX_PROJECT_PAGE_ITEMS,
            })
            .map_err(|error| error.to_string())?;
        let ServerFrame::Response {
            response:
                LocalResponse::Projects {
                    projects,
                    next_after_id,
                },
            ..
        } = frame
        else {
            return Err("unexpected reply to list projects".into());
        };
        all.extend(projects);
        match next_after_id {
            Some(next) => after_id = Some(next),
            None => return Ok(all),
        }
    }
}

fn list_agent_ids(client: &Client, project_id: &ProjectId) -> Result<Vec<String>, String> {
    let mut all = Vec::new();
    let mut after_id = None;
    loop {
        let frame = client
            .request(LocalRequest::ListAgents {
                project_id: project_id.clone(),
                after_id: after_id.clone(),
                limit: MAX_AGENT_PAGE_ITEMS,
            })
            .map_err(|error| error.to_string())?;
        let ServerFrame::Response {
            response:
                LocalResponse::Agents {
                    agents,
                    next_after_id,
                },
            ..
        } = frame
        else {
            return Err("unexpected reply to list agents".into());
        };
        all.extend(agents.into_iter().map(|agent| agent.id.to_string()));
        match next_after_id {
            Some(next) => after_id = Some(next),
            None => return Ok(all),
        }
    }
}
