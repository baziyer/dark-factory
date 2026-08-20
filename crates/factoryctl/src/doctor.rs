//! `factoryctl doctor`: diagnostic checks of this machine's Dark Factory
//! install, one line per check, non-zero exit if anything fails. It never
//! repairs or reconfigures the install; the release check can refresh the
//! update cache. Every probe it uses (`crate::probes`) is the same one
//! `factoryctl init` and `update --install` use, so the three commands can't
//! disagree about what "healthy" means.

use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

use factory_core::local::{LocalRequest, LocalResponse, MAX_PROJECT_PAGE_ITEMS, ServerFrame};
use factoryctl::probes::PROBED_PROGRAMS;
use factoryctl::{Client, install, launchd, probes, update};
use serde::Serialize;

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
    let active_version = update::active_version(&home);
    checks.push(check_install(&home, &active_version));
    let active_version = active_version
        .as_ref()
        .ok()
        .and_then(|version| version.as_deref());
    let daemon = check_daemon(socket, active_version);
    let daemon_reachable = daemon.status != Status::Fail;
    checks.push(daemon);
    checks.push(check_launchd(user_home.as_deref()));
    for program in PROBED_PROGRAMS {
        checks.push(check_program(program));
    }
    checks.push(check_codex_seed(user_home.as_deref()));
    if daemon_reachable {
        match Client::authenticated_from_file(socket, home.join("operator.token")) {
            Ok(client) => checks.extend(check_projects(&client)),
            Err(error) => checks.push(Check::fail("operator credential", error.to_string())),
        }
    }
    checks.push(check_update(&home, active_version));

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

fn check_install(home: &Path, active_version: &Result<Option<String>, String>) -> Check {
    let link = install::current_link(home);
    match active_version {
        Ok(Some(version)) if version == update::CURRENT_VERSION => {
            Check::ok("install", format!("bin/current -> {version}"))
        }
        Ok(Some(version)) if update::is_newer(version, update::CURRENT_VERSION) => Check::ok(
            "install",
            format!(
                "bin/current -> {version} (newer than this factoryctl {})",
                update::CURRENT_VERSION
            ),
        ),
        Ok(Some(version)) => Check::warn(
            "install",
            format!(
                "bin/current -> {version}, this factoryctl is {} (`factoryctl update --install` activates the latest release)",
                update::CURRENT_VERSION
            ),
        ),
        Ok(None) => Check::warn(
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
        Err(error) => Check::fail("install", error.clone()),
    }
}

fn check_daemon(socket: &Path, active_version: Option<&str>) -> Check {
    let expected = active_version.unwrap_or(update::CURRENT_VERSION);
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
            } else if version != expected {
                Check::warn(
                    "daemon",
                    format!(
                        "running {version} at {}, active runtime is {expected} (restart the daemon to pick up the installed binaries)",
                        socket.display(),
                    ),
                )
            } else {
                Check::ok(
                    "daemon",
                    format!("{version} at {} (matches active runtime)", socket.display()),
                )
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
                "loaded, but its PATH lacks the directory of {} — attempts won't find it (`factoryctl init` re-renders the job)",
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
                    "not on PATH; daemon-owned Changes will remain unavailable",
                )
            } else {
                Check::warn(
                    program,
                    "not on PATH; attempts using this provider cannot start",
                )
            }
        }
    }
}

/// Which Codex home agents seed their credentials from, and whether it is
/// logged in.
fn check_codex_seed(user_home: Option<&Path>) -> Check {
    let Some(user_home) = user_home else {
        return Check::warn("codex-seed", "HOME is not set");
    };
    if probes::locate_on_path("codex").is_none() {
        return Check::ok("codex-seed", "codex is not installed; nothing to seed");
    }
    let job = launchd::read_existing(&launchd::plist_path(user_home))
        .ok()
        .flatten();
    let seed_home = probes::codex_seed_home(job.as_ref().map(|job| &job.environment), user_home);
    let overridden = seed_home != user_home.join(".codex");
    if seed_home.join("auth.json").is_file() {
        Check::ok(
            "codex-seed",
            format!(
                "{} (auth.json present{})",
                seed_home.display(),
                if overridden { "; via CODEX_HOME" } else { "" }
            ),
        )
    } else {
        Check::warn(
            "codex-seed",
            format!(
                "{} has no auth.json; Codex agents will have no credentials (log that home in with `CODEX_HOME={} codex login`)",
                seed_home.display(),
                seed_home.display()
            ),
        )
    }
}

/// Validate the operator-configured source roots. Stage 1 deliberately has no
/// source-worktree diagnostics because workers cannot own source paths.
fn check_projects(client: &Client) -> Vec<Check> {
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
        checks.push(Check::ok(
            format!("project:{}", project.id),
            format!(
                "root {} (worker source Changes are disabled until Stage 2)",
                root.display()
            ),
        ));
    }
    checks
}

fn check_update(home: &Path, active_version: Option<&str>) -> Check {
    let check = update::check(home, &update::manifest_url(), update::now_ms(), false);
    let installed = active_version.unwrap_or(&check.current);
    match (check.available_from(installed), &check.error) {
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
            format!("{installed} is the latest installed release"),
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
