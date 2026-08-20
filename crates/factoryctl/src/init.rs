//! `factoryctl init`: guided first install on this machine.
//!
//! Creates `$DARK_FACTORY_HOME`, installs the running build's sibling
//! binaries as `bin/<version>` + `bin/current`, checks that `claude`/
//! `codex`/`git` resolve, states what Dark Factory writes outside its own
//! home, asks before touching launchd, then renders and loads the launchd
//! job with a `PATH` that can find those CLIs and waits for the daemon to
//! answer with this version. Re-running is safe: an existing job keeps its
//! extra arguments and environment (its `PATH` is repaired if a provider
//! moved), an installed version is not overwritten, and a hand-started
//! daemon on the same socket is refused rather than raced.

use std::{
    env, fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use factory_core::local::{LocalRequest, LocalResponse, ServerFrame};
use factoryctl::{Client, install, launchd, probes, runtime, update};

pub struct Options {
    /// Skip the consent prompt (`--yes`).
    pub yes: bool,
    /// Install binaries only; leave launchd alone (`--no-launchd`).
    pub no_launchd: bool,
}

const HEALTH_WAIT: Duration = Duration::from_secs(30);

pub fn run(options: &Options, socket: &Path) -> Result<i32, String> {
    let home = factory_core::paths::dark_factory_home().map_err(|error| error.to_string())?;
    let user_home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME is not set")?;

    // 1. The private state directory (the daemon refuses a symlink, so we
    //    do too, in its words).
    let created = match fs::symlink_metadata(&home) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(format!(
                "{} must not be a symbolic link (the daemon refuses it too)",
                home.display()
            ));
        }
        Ok(_) => false,
        Err(_) => true,
    };
    install::create_private_dir(&home)?;
    install::create_private_dir(&home.join("logs"))?;
    println!(
        "home: {} ({})",
        home.display(),
        if created { "created" } else { "exists" }
    );

    // 2. Provider CLIs and git.
    for program in probes::PROBED_PROGRAMS {
        match probes::locate_on_path(program) {
            Some(path) => {
                let version =
                    probes::probe_version(&path).unwrap_or_else(|| "version unknown".to_owned());
                println!("{program}: {version} ({})", path.display());
            }
            None => println!(
                "{program}: not on PATH{}",
                if program == "git" {
                    " -- daemon-owned Changes will remain unavailable"
                } else {
                    " -- attempts using this provider cannot start until it is"
                }
            ),
        }
    }

    // Which Codex account agents will use.
    let plist = launchd::plist_path(&user_home);
    let existing = launchd::read_existing(&plist)?;
    // The same precedence launchd::apply uses: this shell's CODEX_HOME wins,
    // else the job's, else ~/.codex.
    let carried = launchd::carried_environment();
    let seed_home = carried
        .get("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            probes::codex_seed_home(existing.as_ref().map(|job| &job.environment), &user_home)
        });
    println!(
        "codex home for agents: {} ({}; run init with CODEX_HOME=<other home> to use another account)",
        seed_home.display(),
        if seed_home.join("auth.json").is_file() {
            "auth.json present"
        } else {
            "no auth.json -- Codex agents will have no credentials until that home is logged in"
        }
    );

    // 3. Install this build.
    let source = env::current_exe()
        .map_err(|error| error.to_string())?
        .parent()
        .ok_or("factoryctl has no parent directory")?
        .to_path_buf();
    let version = update::CURRENT_VERSION;
    let destination = install::version_dir(&home, version);
    let same_place = fs::canonicalize(&source)
        .ok()
        .is_some_and(|source| Some(source) == fs::canonicalize(&destination).ok());
    if same_place {
        println!("install: already running from {}", destination.display());
    } else if destination.exists() {
        if !same_binaries(&source, &destination) {
            return Err(format!(
                "{} exists but differs from this build at {}; remove that directory to install this build",
                destination.display(),
                source.display()
            ));
        }
        println!(
            "install: {} already holds this build",
            destination.display()
        );
    } else {
        install::install_from_dir(&home, &source, version)?;
        println!("install: {} <- {}", destination.display(), source.display());
    }
    let (_runtime_lock, snapshot) = runtime::MutationLock::begin(&home, &plist)?;
    let existing = launchd::read_existing(&plist)?;
    if let Some(existing) = &existing {
        launchd::check_home(existing, &home, &user_home)?;
    }
    let previous_version = snapshot.active_version.clone();
    install::activate(&home, version)?;
    println!("install: bin/current -> {version}");

    // 4. What Dark Factory writes outside $DARK_FACTORY_HOME -- said every
    //    time, whether or not launchd is touched.
    println!(
        "\nOutside {}, Dark Factory writes only {} (the launchd job that keeps factoryd running; rewritten by `factoryctl update --install`).\n  \
         Stage 1 does not create source worktrees, mutate project repositories, or edit ~/.claude.json.\n  \
         Codex attempts seed private per-run configuration from {} inside {}.",
        home.display(),
        plist.display(),
        seed_home.display(),
        home.display()
    );
    if options.no_launchd {
        print_next_steps(&home, NextSteps::StartDaemon);
        return Ok(0);
    }
    if !options.yes
        && !confirm(
            "Install and load the launchd job? (N only skips launchd; a daemon you start yourself still does the above) [y/N] ",
        )?
    {
        println!("stopped before touching launchd; binaries are installed and activated");
        print_next_steps(&home, NextSteps::StartDaemon);
        return Ok(0);
    }

    // 5. The launchd job. Refuse to race a daemon someone started by hand
    //    on the same socket: launchd's copy would crash-loop on AddrInUse
    //    while the old one kept answering health.
    if let Some(existing) = &existing {
        launchd::check_home(existing, &home, &user_home)?;
    }
    if !probes::launchd_loaded() && probes::daemon_answers(socket) {
        return Err(format!(
            "a factoryd already answers at {} but is not managed by launchd; stop it first",
            socket.display()
        ));
    }
    let apply = launchd::apply_with_rollback(
        &home,
        &plist,
        existing.as_ref(),
        &probes::provider_directories(),
        &carried,
        None,
        || snapshot.restore_runtime(&home),
    );
    if let Err(error) = apply {
        return Err(runtime::rollback_report(
            &error,
            previous_version.as_deref(),
            |previous| {
                probes::wait_for_managed_daemon(socket, HEALTH_WAIT, Some(previous), &home)
                    .map(|_| ())
            },
        ));
    }
    println!(
        "launchd: {} loaded{}",
        plist.display(),
        if existing.is_some() {
            " (rewritten; the daemon restarted)"
        } else {
            ""
        }
    );
    match probes::wait_for_daemon(socket, Duration::from_secs(20), Some(version)) {
        Ok(version) => println!("daemon: {version} answering at {}", socket.display()),
        Err(error) => {
            let rollback = runtime::rollback_after_health_failure(
                &home,
                &plist,
                &snapshot,
                version,
                |previous| {
                    probes::wait_for_managed_daemon(socket, HEALTH_WAIT, Some(previous), &home)
                        .map(|_| ())
                },
            );
            println!(
                "daemon: not answering with {version} yet ({error}); rollback {}; see {}/logs/factoryd.stderr.log",
                rollback.as_ref().map(|()| "restored").unwrap_or("failed"),
                home.display()
            );
            if let Err(rollback_error) = rollback {
                println!("rollback: {rollback_error}");
            }
            print_next_steps(&home, NextSteps::DoctorOnly);
            return Ok(1);
        }
    }
    let client = Client::authenticated_from_file(socket, home.join("operator.token"))
        .map_err(|error| error.to_string())?;
    let (next_steps, diagnostic) = project_next_steps(has_projects(&client));
    if let Some(diagnostic) = diagnostic {
        println!("{diagnostic}");
    }
    print_next_steps(&home, next_steps);
    Ok(0)
}

/// Whether `a` and `b` hold byte-identical copies of the four binaries.
fn same_binaries(a: &Path, b: &Path) -> bool {
    install::BINARIES.iter().all(|name| {
        fs::read(a.join(name))
            .ok()
            .is_some_and(|bytes| Some(bytes) == fs::read(b.join(name)).ok())
    })
}

#[derive(Clone, Copy)]
enum NextSteps {
    StartDaemon,
    DoctorOnly,
    FirstProject,
    Fleet,
}

fn print_next_steps(home: &Path, next_steps: NextSteps) {
    println!("{}", next_steps_text(home, next_steps));
}

fn next_steps_text(home: &Path, next_steps: NextSteps) -> String {
    format!(
        "\nnext:\n  export PATH=\"{}:$PATH\"   # factoryctl and factory-tui\n  factoryctl doctor{}",
        install::current_link(home).display(),
        match next_steps {
            NextSteps::StartDaemon => {
                "\n  factoryd &   # or `factoryctl init` again to install the launchd job"
            }
            NextSteps::DoctorOnly => "",
            NextSteps::FirstProject => {
                "\n  factoryctl project add --id demo --name Demo --root \"$PWD\"\n  factory-tui"
            }
            NextSteps::Fleet => "\n  factoryctl status\n  factory-tui",
        }
    )
}

fn project_next_steps(result: Result<bool, String>) -> (NextSteps, Option<String>) {
    match result {
        Ok(false) => (NextSteps::FirstProject, None),
        Ok(true) => (NextSteps::Fleet, None),
        Err(error) => (
            NextSteps::Fleet,
            Some(format!("projects: could not list projects: {error}")),
        ),
    }
}

fn has_projects(client: &Client) -> Result<bool, String> {
    let frame = client
        .request(LocalRequest::ListProjects {
            after_id: None,
            limit: 1,
        })
        .map_err(|error| error.to_string())?;
    let ServerFrame::Response {
        response: LocalResponse::Projects { projects, .. },
        ..
    } = frame
    else {
        return Err("unexpected reply to list projects".into());
    };
    Ok(!projects.is_empty())
}

fn confirm(prompt: &str) -> Result<bool, String> {
    let stdin = io::stdin();
    if !stdin.is_terminal() {
        return Err("stdin is not a terminal; pass --yes to consent non-interactively".into());
    }
    print!("{prompt}");
    io::stdout().flush().map_err(|error| error.to_string())?;
    let mut answer = String::new();
    stdin
        .read_line(&mut answer)
        .map_err(|error| error.to_string())?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader, Write},
        os::unix::net::UnixListener,
        path::Path,
        thread,
    };

    use factory_core::{
        PROTOCOL_VERSION, ProjectId, ProjectSnapshot,
        local::{LocalRequest, LocalResponse, RequestEnvelope, ServerFrame},
    };

    use super::{NextSteps, has_projects, next_steps_text, project_next_steps};

    fn project() -> ProjectSnapshot {
        ProjectSnapshot {
            id: ProjectId::try_from("factory").unwrap(),
            name: "Factory".into(),
            root: "/tmp/factory".into(),
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    fn next_steps_for(projects: Vec<ProjectSnapshot>) -> String {
        next_steps_for_response(LocalResponse::Projects {
            projects,
            next_after_id: None,
        })
        .0
    }

    fn next_steps_for_response(response: LocalResponse) -> (String, Option<String>) {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("factory.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            assert_eq!(
                serde_json::from_str::<RequestEnvelope>(&request).unwrap(),
                RequestEnvelope::new(LocalRequest::ListProjects {
                    after_id: None,
                    limit: 1,
                })
            );
            serde_json::to_writer(
                &mut stream,
                &ServerFrame::Response {
                    protocol_version: PROTOCOL_VERSION,
                    response,
                },
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
        });

        let result = has_projects(&factoryctl::Client::new(socket));
        server.join().unwrap();
        let (next_steps, diagnostic) = project_next_steps(result);
        (next_steps_text(directory.path(), next_steps), diagnostic)
    }

    #[test]
    fn fresh_home_gets_a_first_project_path() {
        let next_steps = next_steps_for(Vec::new());
        assert!(next_steps.contains("factoryctl project add --id demo"));
        assert!(next_steps.contains("factory-tui"));
        assert!(!next_steps.contains("factoryctl status"));
    }

    #[test]
    fn existing_project_home_skips_demo_creation() {
        let next_steps = next_steps_for(vec![project()]);
        assert!(next_steps.contains("factoryctl status"));
        assert!(next_steps.contains("factory-tui"));
        assert!(!next_steps.contains("factoryctl project add"));
    }

    #[test]
    fn project_list_failure_is_reported_without_suggesting_demo_creation() {
        let (next_steps, diagnostic) = next_steps_for_response(LocalResponse::Health {
            runner_path: "/tmp/factory-runner".into(),
            factoryctl_path: "/tmp/factoryctl".into(),
            version: "0.2.0".into(),
            process_id: 0,
        });
        assert_eq!(
            diagnostic.as_deref(),
            Some("projects: could not list projects: unexpected reply to list projects")
        );
        assert!(next_steps.contains("factoryctl status"));
        assert!(next_steps.contains("factory-tui"));
        assert!(!next_steps.contains("factoryctl project add"));
    }

    #[test]
    fn daemon_startup_failure_only_suggests_diagnosis() {
        let next_steps = next_steps_text(Path::new("/tmp/home"), NextSteps::DoctorOnly);
        assert!(next_steps.contains("factoryctl doctor"));
        assert!(!next_steps.contains("factoryctl project add"));
        assert!(!next_steps.contains("factoryctl status"));
        assert!(!next_steps.contains("factoryd &"));
    }
}
