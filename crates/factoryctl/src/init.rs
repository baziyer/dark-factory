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

use factoryctl::update;

use crate::{install, launchd, probes};

pub struct Options {
    /// Skip the consent prompt (`--yes`).
    pub yes: bool,
    /// Install binaries only; leave launchd alone (`--no-launchd`).
    pub no_launchd: bool,
}

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
                    " -- agents get no worktree of their own and run in the project root"
                } else {
                    " -- agents with this provider cannot start until it is"
                }
            ),
        }
    }

    // Which Codex account agents will use: the launchd job's setting if it
    // has one, else this shell's, else the operator's own ~/.codex.
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
    install::activate(&home, version)?;
    println!("install: bin/current -> {version}");

    // 4. What Dark Factory writes outside $DARK_FACTORY_HOME -- said every
    //    time, whether or not launchd is touched.
    let claude_json = user_home.join(".claude.json");
    println!(
        "\nDark Factory writes three things outside {}:\n  \
         - {} (the launchd job that keeps factoryd running; rewritten by `factoryctl update --install`)\n  \
         - {}: a `hasTrustDialogAccepted` entry per agent worktree it creates, so a new Claude session\n    \
           never blocks on the trust prompt (only if that file already exists and parses; nothing else in it changes)\n  \
         - each project's own git repository: `git worktree add -b agent/<id>` per agent (the worktree goes with the\n    \
           agent; the branch stays)\n  \
         Codex sessions get a per-agent CODEX_HOME seeded from {}'s config.toml with its auth.json symlinked, inside {}.",
        home.display(),
        plist.display(),
        claude_json.display(),
        seed_home.display(),
        home.display()
    );
    if !claude_json.is_file() {
        println!(
            "  ({} does not exist yet -- run `claude` once, or the pre-trust step is skipped)",
            claude_json.display()
        );
    }
    if options.no_launchd {
        print_next_steps(&home, false);
        return Ok(0);
    }
    if !options.yes
        && !confirm(
            "Install and load the launchd job? (N only skips launchd; a daemon you start yourself still does the above) [y/N] ",
        )?
    {
        println!("stopped before touching launchd; binaries are installed and activated");
        print_next_steps(&home, false);
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
    launchd::apply(
        &home,
        &plist,
        existing.as_ref(),
        &probes::provider_directories(),
        &carried,
    )?;
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
            println!(
                "daemon: not answering with {version} yet ({error}); see {}/logs/factoryd.stderr.log",
                home.display()
            );
            print_next_steps(&home, true);
            return Ok(1);
        }
    }
    print_next_steps(&home, true);
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

fn print_next_steps(home: &Path, daemon: bool) {
    println!(
        "\nnext:\n  export PATH=\"{}:$PATH\"   # factoryctl and factory-tui\n  factoryctl doctor{}",
        install::current_link(home).display(),
        if daemon {
            "\n  factoryctl project add --id demo --name Demo --root \"$PWD\"\n  factory-tui"
        } else {
            "\n  factoryd &   # or `factoryctl init` again to install the launchd job"
        }
    );
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
