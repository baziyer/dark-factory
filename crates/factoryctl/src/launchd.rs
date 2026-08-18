//! The `launchd` job that keeps `factoryd` running, rendered from
//! `launchd/com.dark-factory.factoryd.plist.template` (embedded here so a
//! binary-only install never needs the repository).
//!
//! `factoryctl init` and `factoryctl update --install` own this file
//! through [`apply`]: read whatever job is there, keep its
//! extra daemon arguments and environment, make sure its `PATH` can find
//! the provider CLIs, point it at `bin/current/factoryd`, write it at
//! `0600`, and reload it. A reload restarts only the daemon — every
//! session's runner is a detached process tree the new daemon reconnects
//! to (`ARCHITECTURE.md`, invariant 4; the template's `AbandonProcessGroup`
//! and the runner's own process group are what make that true under
//! launchd).

use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

use crate::capacity::{DEFAULT_MAX_ACTIVE_RUNS, MAX_MAX_ACTIVE_RUNS};
use crate::install;

pub const LABEL: &str = "com.dark-factory.factoryd";
const TEMPLATE: &str = include_str!("../../../launchd/com.dark-factory.factoryd.plist.template");
/// What launchd gives a job that sets no `PATH` of its own.
pub const LAUNCHD_DEFAULT_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
/// Enough to find Homebrew/system tools; the provider CLIs' own directories
/// are prepended by [`merged_path`].
pub const BASE_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";
pub const MAX_ACTIVE_RUNS_ARGUMENT: &str = "--max-active-runs";
/// Daemon settings that live in the job's environment and that
/// `factoryctl init` takes from the environment it runs in when set there
/// (an existing job's value is kept otherwise; `update --install` never
/// changes them). `CODEX_HOME` is which Codex account agents seed their
/// credentials from.
pub const INIT_CARRIED_ENVIRONMENT: [&str; 1] = ["CODEX_HOME"];

/// The subset of this process's environment that [`INIT_CARRIED_ENVIRONMENT`]
/// names and that is set (non-empty).
#[must_use]
pub fn carried_environment() -> BTreeMap<String, String> {
    INIT_CARRIED_ENVIRONMENT
        .iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .filter(|value| !value.is_empty())
                .map(|value| ((*key).to_owned(), value))
        })
        .collect()
}
const BOOTSTRAP_ATTEMPTS: u32 = 6;
const BOOTSTRAP_RETRY: Duration = Duration::from_millis(500);

/// `~/Library/LaunchAgents/com.dark-factory.factoryd.plist`.
#[must_use]
pub fn plist_path(user_home: &Path) -> PathBuf {
    user_home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

/// What an existing job runs and with which environment, read back through
/// `plutil` so no plist parsing lives here.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExistingJob {
    pub program_arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
}

impl ExistingJob {
    /// The `$DARK_FACTORY_HOME` this job runs with: its own environment
    /// variable, else — for a job rendered by hand from the old template,
    /// which passed explicit paths — the directory of its `--database`,
    /// else Dark Factory's default under `user_home`.
    #[must_use]
    pub fn home(&self, user_home: &Path) -> PathBuf {
        if let Some(home) = self.environment.get("DARK_FACTORY_HOME") {
            return PathBuf::from(home);
        }
        let mut arguments = self.program_arguments.iter();
        while let Some(argument) = arguments.next() {
            if argument == "--database" {
                if let Some(parent) = arguments.next().and_then(|db| Path::new(db).parent()) {
                    return parent.to_path_buf();
                }
            }
        }
        user_home.join(".dark-factory")
    }
}

pub fn read_existing(plist: &Path) -> Result<Option<ExistingJob>, String> {
    if !plist.exists() {
        return Ok(None);
    }
    let output = Command::new("plutil")
        .args(["-convert", "json", "-o", "-"])
        .arg(plist)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("could not run plutil: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{} is not a readable plist: {}",
            plist.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("plutil output for {} is not JSON: {error}", plist.display()))?;
    let program_arguments = value["ProgramArguments"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let environment = value["EnvironmentVariables"]
        .as_object()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Some(ExistingJob {
        program_arguments,
        environment,
    }))
}

/// A `PATH` for the job: `existing` (or, when the job had none, launchd's
/// default plus [`BASE_PATH`]) with, for each `(program, directory)` in
/// `required` — the provider CLIs and where this shell resolves them, see
/// [`crate::probes::provider_directories`] — `directory` prepended only if
/// no entry already there resolves `program`. Re-rendering a job therefore
/// repairs a `PATH` that stopped covering `claude`/`codex` (a new `nvm`
/// version, say) without changing which binary a job that still finds
/// them runs.
#[must_use]
pub fn merged_path(existing: Option<&str>, required: &[(&str, PathBuf)]) -> String {
    let mut entries: Vec<PathBuf> = match existing {
        Some(existing) => std::env::split_paths(existing).collect(),
        None => std::env::split_paths(LAUNCHD_DEFAULT_PATH)
            .chain(std::env::split_paths(BASE_PATH))
            .collect(),
    };
    let mut inserted = 0;
    for (program, directory) in required {
        let resolvable = entries.iter().any(|entry| {
            let candidate = entry.join(program);
            fs::metadata(&candidate).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        });
        if !resolvable && !entries.contains(directory) {
            entries.insert(inserted.min(entries.len()), directory.clone());
            inserted += 1;
        }
    }
    let mut seen: Vec<PathBuf> = Vec::new();
    entries.retain(|entry| {
        if seen.contains(entry) {
            false
        } else {
            seen.push(entry.clone());
            true
        }
    });
    std::env::join_paths(entries)
        .map(|joined| joined.to_string_lossy().into_owned())
        .unwrap_or_else(|_| BASE_PATH.to_owned())
}

/// Renders the job: `factoryd` (an absolute path, normally
/// `<home>/bin/current/factoryd`) plus `extra_arguments`, with
/// `environment` — every entry, plus `DARK_FACTORY_HOME` set to `home` —
/// and logs under `<home>/logs`.
#[must_use]
pub fn render(
    home: &Path,
    factoryd: &Path,
    extra_arguments: &[String],
    environment: &BTreeMap<String, String>,
) -> String {
    let arguments = std::iter::once(factoryd.to_string_lossy().into_owned())
        .chain(extra_arguments.iter().cloned())
        .map(|argument| format!("        <string>{}</string>", escape(&argument)))
        .collect::<Vec<_>>()
        .join("\n");
    let mut environment = environment.clone();
    environment.insert(
        "DARK_FACTORY_HOME".to_owned(),
        home.to_string_lossy().into_owned(),
    );
    let environment = environment
        .iter()
        .map(|(key, value)| {
            format!(
                "        <key>{}</key>\n        <string>{}</string>",
                escape(key),
                escape(value)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    TEMPLATE
        .replace("__PROGRAM_ARGUMENTS__", &arguments)
        .replace("__ENVIRONMENT__", &environment)
        .replace("__DARK_FACTORY_HOME__", &escape(&home.to_string_lossy()))
}

/// The arguments worth carrying from an existing job into a re-rendered
/// one: everything except the program itself and the `--runner`/
/// `--factoryctl` pair, which must point at the newly activated binaries
/// (the daemon finds both as siblings of its own executable anyway).
#[must_use]
pub fn carried_arguments(program_arguments: &[String]) -> Vec<String> {
    let mut carried = Vec::new();
    let mut arguments = program_arguments.iter().skip(1);
    while let Some(argument) = arguments.next() {
        if argument == "--runner" || argument == "--factoryctl" {
            arguments.next();
            continue;
        }
        carried.push(argument.clone());
    }
    carried
}

/// Reads the capacity flag from an existing job. A missing flag is a legacy
/// job and uses the conservative default; a malformed flag is surfaced rather
/// than silently replaced by a plausible value.
pub fn max_active_runs(program_arguments: &[String]) -> Result<Option<usize>, String> {
    let mut arguments = program_arguments.iter().skip(1);
    while let Some(argument) = arguments.next() {
        if argument != MAX_ACTIVE_RUNS_ARGUMENT {
            continue;
        }
        let value = arguments
            .next()
            .ok_or("--max-active-runs in the launchd job has no value")?;
        let value = value
            .parse::<usize>()
            .map_err(|_| "--max-active-runs in the launchd job is not an integer")?;
        if !(1..=MAX_MAX_ACTIVE_RUNS).contains(&value) {
            return Err(format!(
                "--max-active-runs in the launchd job must be between 1 and {MAX_MAX_ACTIVE_RUNS}"
            ));
        }
        return Ok(Some(value));
    }
    Ok(None)
}

/// Carries an existing job's arguments while replacing its one capacity flag.
#[must_use]
pub fn carried_arguments_with_capacity(
    program_arguments: &[String],
    capacity: usize,
) -> Vec<String> {
    let mut carried = carried_arguments(program_arguments);
    while let Some(index) = carried
        .iter()
        .position(|arg| arg == MAX_ACTIVE_RUNS_ARGUMENT)
    {
        carried.remove(index);
        if index < carried.len() {
            carried.remove(index);
        }
    }
    carried.push(MAX_ACTIVE_RUNS_ARGUMENT.to_owned());
    carried.push(capacity.to_string());
    carried
}

/// Refuses to re-render a job onto a different `$DARK_FACTORY_HOME` than
/// it runs with today: `factoryctl` may be running with a scratch home
/// (`docs/development/WORKFLOW.md`, "Developing the daemon without
/// disrupting a running factory"), and rewriting the operator's real job
/// to it would move their live factory.
pub fn check_home(existing: &ExistingJob, home: &Path, user_home: &Path) -> Result<(), String> {
    let job_home = existing.home(user_home);
    if same_path(&job_home, home) {
        return Ok(());
    }
    Err(format!(
        "the launchd job runs with DARK_FACTORY_HOME={} but this factoryctl is using {}; \
         run it with the job's home (or remove the job) rather than moving the job",
        job_home.display(),
        home.display()
    ))
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Renders the job for `home` (keeping an existing job's extra arguments
/// and environment; `PATH` merged with `provider_directories`), writes it
/// at `plist`, and reloads it. The caller has already read and checked
/// the existing job with [`read_existing`]/[`check_home`] — this is the
/// mutating half. A failed reload restores the prior plist before returning.
pub fn apply(
    home: &Path,
    plist: &Path,
    existing: Option<&ExistingJob>,
    provider_directories: &[(&str, PathBuf)],
    extra_environment: &BTreeMap<String, String>,
    capacity: Option<usize>,
) -> Result<(), String> {
    let mut environment = existing
        .map(|job| job.environment.clone())
        .unwrap_or_default();
    environment.extend(
        extra_environment
            .iter()
            .map(|(k, v)| (k.clone(), v.clone())),
    );
    let path = merged_path(
        environment.get("PATH").map(String::as_str),
        provider_directories,
    );
    environment.insert("PATH".to_owned(), path);
    let existing_arguments = existing.map_or(&[][..], |job| job.program_arguments.as_slice());
    let capacity = capacity
        .or(max_active_runs(existing_arguments)?.or(Some(DEFAULT_MAX_ACTIVE_RUNS)))
        .ok_or("capacity is not configured")?;
    let carried = carried_arguments_with_capacity(existing_arguments, capacity);
    let factoryd = install::current_link(home).join("factoryd");
    let content = render(home, &factoryd, &carried, &environment);
    let old_content = if plist.exists() {
        Some(fs::read_to_string(plist).map_err(|error| {
            format!(
                "could not read the existing launchd plist {}: {error}",
                plist.display()
            )
        })?)
    } else {
        None
    };
    install_and_reload(plist, home, &content, old_content, || {
        reload(rustix::process::getuid().as_raw(), plist)
    })
}

/// Restores a previously saved plist and reloads that exact managed job. This
/// is used after a post-reload health failure, where merely repointing
/// `bin/current` would leave launchd running the changed job state.
pub fn restore(plist: &Path, home: &Path, content: &str) -> Result<(), String> {
    let current = if plist.exists() {
        Some(fs::read_to_string(plist).map_err(|error| {
            format!(
                "could not read the current launchd plist {}: {error}",
                plist.display()
            )
        })?)
    } else {
        None
    };
    install_and_reload(plist, home, content, current, || {
        reload(rustix::process::getuid().as_raw(), plist)
    })
}

fn install_and_reload(
    plist: &Path,
    home: &Path,
    content: &str,
    old_content: Option<String>,
    mut reload: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    install(plist, content, home)?;
    match reload() {
        Ok(()) => Ok(()),
        Err(error) => {
            let rollback = match old_content {
                Some(content) => {
                    install(plist, &content, home)?;
                    reload().map_err(|recovery| {
                        format!(
                            "plist restored but managed launchd job recovery failed: {recovery}"
                        )
                    })
                }
                None => fs::remove_file(plist).map_err(|restore| restore.to_string()),
            };
            match rollback {
                Ok(()) => Err(format!("{error}; launchd plist and job rolled back")),
                Err(rollback) => Err(format!("{error}; launchd rollback failed: {rollback}")),
            }
        }
    }
}

/// Writes `content` at `plist` with mode `0600` (atomically: temp file,
/// then rename), creating `~/Library/LaunchAgents` if needed, and creates
/// `<home>/logs` (`0700`) so launchd can open the log files.
pub fn install(plist: &Path, content: &str, home: &Path) -> Result<(), String> {
    install::create_private_dir(&home.join("logs"))?;
    if let Some(parent) = plist.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    let temp = plist.with_extension("plist.tmp");
    fs::write(&temp, content)
        .map_err(|error| format!("could not write {}: {error}", temp.display()))?;
    fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("could not set permissions on {}: {error}", temp.display()))?;
    fs::rename(&temp, plist)
        .map_err(|error| format!("could not install {}: {error}", plist.display()))
}

/// (Re)loads the job from `plist`: `bootout` (ignored if it wasn't loaded)
/// then `bootstrap`, retried briefly because launchd can still be tearing
/// the old instance down when the first `bootstrap` arrives. launchd
/// caches a job's `ProgramArguments`, so a plain `kickstart -k` would keep
/// running the *old* binary after a rewrite — this is the only sequence
/// that picks up a changed plist.
pub fn reload(uid: u32, plist: &Path) -> Result<(), String> {
    let domain = format!("gui/{uid}");
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{LABEL}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let mut last_error = String::new();
    for attempt in 1..=BOOTSTRAP_ATTEMPTS {
        let output = Command::new("launchctl")
            .arg("bootstrap")
            .arg(&domain)
            .arg(plist)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| format!("could not run launchctl: {error}"))?;
        if output.status.success() {
            return Ok(());
        }
        last_error = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if attempt < BOOTSTRAP_ATTEMPTS {
            thread::sleep(BOOTSTRAP_RETRY);
        }
    }
    Err(format!(
        "launchctl bootstrap {domain} {} failed after {BOOTSTRAP_ATTEMPTS} attempts: {last_error}; \
         the job is unloaded -- run `launchctl bootstrap {domain} {}` to load it",
        plist.display(),
        plist.display()
    ))
}

/// Returns the PID launchd currently associates with its managed job.
pub fn job_pid(uid: u32) -> Result<Option<u32>, String> {
    let domain = format!("gui/{uid}/{LABEL}");
    let output = Command::new("launchctl")
        .args(["print", &domain])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("could not run launchctl print: {error}"))?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(parse_job_pid(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_job_pid(output: &str) -> Option<u32> {
    output.lines().find_map(|line| {
        let value = line.trim().strip_prefix("pid = ")?;
        value.parse().ok()
    })
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn render_fills_every_placeholder_carries_the_environment_and_escapes() {
        let rendered = render(
            Path::new("/Users/me/.dark-factory"),
            Path::new("/Users/me/.dark-factory/bin/current/factoryd"),
            &["--max-active-runs".to_owned(), "6".to_owned()],
            &environment(&[
                (
                    "PATH",
                    "/Users/me/.local/bin:/opt/homebrew/bin:/usr/bin:/bin&more",
                ),
                ("RUST_LOG", "info"),
                ("DARK_FACTORY_HOME", "/somewhere/else"),
            ]),
        );
        assert!(!rendered.contains("__"), "{rendered}");
        assert!(rendered.contains("<string>/Users/me/.dark-factory/bin/current/factoryd</string>"));
        assert!(
            rendered.contains("<string>--max-active-runs</string>\n        <string>6</string>")
        );
        assert!(rendered.contains(
            "<string>/Users/me/.local/bin:/opt/homebrew/bin:/usr/bin:/bin&amp;more</string>"
        ));
        assert!(rendered.contains("<key>RUST_LOG</key>\n        <string>info</string>"));
        assert!(
            rendered.contains(
                "<key>DARK_FACTORY_HOME</key>\n        <string>/Users/me/.dark-factory</string>"
            ),
            "the home always wins over whatever the environment said"
        );
        assert!(!rendered.contains("/somewhere/else"));
        assert!(
            rendered.contains("<string>/Users/me/.dark-factory/logs/factoryd.stderr.log</string>")
        );
        assert!(rendered.contains("<key>AbandonProcessGroup</key>\n    <true/>"));
    }

    #[test]
    fn merged_path_prepends_a_directory_only_when_no_entry_resolves_the_program() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("old-bin");
        let new = root.path().join("new-bin");
        for dir in [&old, &new] {
            fs::create_dir_all(dir).unwrap();
        }
        let executable = |dir: &Path, name: &str| {
            let path = dir.join(name);
            fs::write(&path, "#!/bin/sh\n").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        };
        executable(&old, "claude");
        executable(&new, "claude");
        executable(&new, "codex");
        let required = [("claude", new.clone()), ("codex", new.clone())];
        let old_s = old.to_string_lossy().into_owned();
        let new_s = new.to_string_lossy().into_owned();
        // The job already resolves `claude` from old-bin: that stays first; only
        // `codex` (unresolvable) pulls new-bin in, and once, after it.
        assert_eq!(
            merged_path(Some(&format!("{old_s}:/usr/bin")), &required),
            format!("{new_s}:{old_s}:/usr/bin")
        );
        // Both already resolvable: untouched.
        assert_eq!(
            merged_path(Some(&format!("{new_s}:/usr/bin")), &required),
            format!("{new_s}:/usr/bin")
        );
        // No PATH at all: launchd's default plus the base, with the provider dir first.
        assert_eq!(
            merged_path(None, &[("codex", new.clone())]),
            format!("{new_s}:{LAUNCHD_DEFAULT_PATH}:/opt/homebrew/bin:/usr/local/bin")
        );
    }

    #[test]
    fn carried_arguments_drop_program_runner_and_factoryctl_only() {
        let existing = [
            "/old/factoryd",
            "--database",
            "/x/factory.db",
            "--runner",
            "/old/factory-runner",
            "--factoryctl",
            "/old/factoryctl",
            "--max-active-runs",
            "3",
        ]
        .map(str::to_owned);
        assert_eq!(
            carried_arguments(&existing),
            ["--database", "/x/factory.db", "--max-active-runs", "3"].map(str::to_owned)
        );
        assert!(carried_arguments(&[]).is_empty());
        assert!(carried_arguments(&["/f".to_owned(), "--runner".to_owned()]).is_empty());
    }

    #[test]
    fn capacity_argument_is_bounded_and_replaced_once() {
        let existing = [
            "/old/factoryd",
            "--max-active-runs",
            "8",
            "--database",
            "/x/factory.db",
        ]
        .map(str::to_owned);
        assert_eq!(max_active_runs(&existing).unwrap(), Some(8));
        assert_eq!(
            carried_arguments_with_capacity(&existing, 12),
            ["--database", "/x/factory.db", "--max-active-runs", "12"].map(str::to_owned)
        );
        assert!(max_active_runs(&["/f", "--max-active-runs", "65"].map(str::to_owned)).is_err());
        assert_eq!(
            max_active_runs(&["/f", "--database", "/x"].map(str::to_owned)).unwrap(),
            None
        );
    }

    #[test]
    fn capacity_setting_persists_across_init_update_and_reinit_argument_carry() {
        let initial =
            ["/factoryd", "--max-active-runs", "8", "--database", "/db"].map(str::to_owned);
        let init =
            carried_arguments_with_capacity(&initial, max_active_runs(&initial).unwrap().unwrap());
        let update_input = std::iter::once("/factoryd".to_owned())
            .chain(init)
            .collect::<Vec<_>>();
        let update = carried_arguments_with_capacity(
            &update_input,
            max_active_runs(&update_input).unwrap().unwrap(),
        );
        let reinit_input = std::iter::once("/factoryd".to_owned())
            .chain(update)
            .collect::<Vec<_>>();
        let reinit = carried_arguments_with_capacity(
            &reinit_input,
            max_active_runs(&reinit_input).unwrap().unwrap(),
        );
        assert_eq!(max_active_runs(&reinit).unwrap(), Some(8));
        assert_eq!(
            reinit
                .iter()
                .filter(|argument| *argument == "--max-active-runs")
                .count(),
            1
        );
    }

    #[test]
    fn failed_reload_restores_the_previous_plist() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let plist = root.path().join("Library/LaunchAgents/job.plist");
        install(&plist, "old", &home).unwrap();
        let mut reloads = 0;
        let error = install_and_reload(&plist, &home, "new", Some("old".to_owned()), || {
            reloads += 1;
            if reloads == 1 {
                Err("reload failed".to_owned())
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert!(error.contains("plist and job rolled back"));
        assert_eq!(reloads, 2);
        assert_eq!(fs::read_to_string(plist).unwrap(), "old");
    }

    #[test]
    fn launchd_pid_parser_requires_a_real_job_pid() {
        assert_eq!(parse_job_pid("state = running\npid = 1234\n"), Some(1234));
        assert_eq!(parse_job_pid("state = running\n"), None);
        assert_eq!(parse_job_pid("pid = nope\n"), None);
    }

    #[test]
    fn existing_job_home_comes_from_env_then_database_then_default() {
        let user_home = Path::new("/Users/me");
        let with_env = ExistingJob {
            program_arguments: vec!["/f".to_owned()],
            environment: environment(&[("DARK_FACTORY_HOME", "/tmp/scratch")]),
        };
        assert_eq!(with_env.home(user_home), Path::new("/tmp/scratch"));
        let legacy = ExistingJob {
            program_arguments: ["/f", "--database", "/Users/me/df/factory.db"]
                .map(str::to_owned)
                .to_vec(),
            environment: environment(&[("PATH", "/usr/bin")]),
        };
        assert_eq!(legacy.home(user_home), Path::new("/Users/me/df"));
        let bare = ExistingJob::default();
        assert_eq!(bare.home(user_home), Path::new("/Users/me/.dark-factory"));
        assert!(check_home(&legacy, Path::new("/Users/me/df"), user_home).is_ok());
        assert!(check_home(&legacy, Path::new("/tmp/other"), user_home).is_err());
    }

    #[test]
    fn install_writes_a_private_file_and_creates_logs() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let plist = root.path().join("Library/LaunchAgents/x.plist");
        install(&plist, "<plist/>", &home).unwrap();
        assert_eq!(fs::read_to_string(&plist).unwrap(), "<plist/>");
        assert_eq!(
            fs::metadata(&plist).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(home.join("logs"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn read_existing_round_trips_through_plutil() {
        let root = tempfile::tempdir().unwrap();
        let plist = root.path().join("job.plist");
        let rendered = render(
            Path::new("/h"),
            Path::new("/h/bin/current/factoryd"),
            &["--max-active-runs".to_owned(), "2".to_owned()],
            &environment(&[("PATH", "/usr/bin:/bin"), ("LANG", "en_GB.UTF-8")]),
        );
        fs::write(&plist, rendered).unwrap();
        let existing = read_existing(&plist).unwrap().unwrap();
        assert_eq!(
            existing.program_arguments,
            ["/h/bin/current/factoryd", "--max-active-runs", "2"].map(str::to_owned)
        );
        assert_eq!(
            existing.environment,
            environment(&[
                ("DARK_FACTORY_HOME", "/h"),
                ("LANG", "en_GB.UTF-8"),
                ("PATH", "/usr/bin:/bin"),
            ])
        );
        assert_eq!(
            read_existing(&root.path().join("missing.plist")).unwrap(),
            None
        );
    }
}
