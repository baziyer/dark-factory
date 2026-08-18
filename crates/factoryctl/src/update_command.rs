//! `factoryctl update [--install]`.
//!
//! `update` fetches the release manifest (always fresh — an operator asking
//! wants the real answer; the result is still cached for `factory-tui`'s
//! hourly status-line check) and reports both the invoking binary and active
//! installed runtime. `--install` then downloads and verifies the release into
//! `$DARK_FACTORY_HOME/bin/<version>/`, repoints `bin/current`, and — if
//! this machine's launchd job exists — rewrites it to run from `bin/current`
//! and reloads it, restarting only the daemon; running sessions are never
//! touched (`ARCHITECTURE.md`, invariant 4). Without a launchd job the
//! binaries are installed and activated and the operator restarts the daemon
//! however they run it.
//!
//! Order matters: every read-only check (manifest, existing job, its home)
//! runs before anything on disk changes; `bin/current` is only repointed
//! once the new version is complete; and if the job cannot be reloaded,
//! `bin/current` goes back to what it was. Exit 0 only when the daemon that
//! answers afterwards is the new version.

use std::{
    env,
    path::{Path, PathBuf},
    time::Duration,
};

use factoryctl::update::UpdateCheck;
use factoryctl::{install, launchd, probes, runtime, update};
use serde_json::json;

const HEALTH_WAIT: Duration = Duration::from_secs(30);

pub struct Options {
    pub install: bool,
}

pub fn run(options: &Options, socket: &Path) -> Result<i32, String> {
    let home =
        absolute(factory_core::paths::dark_factory_home().map_err(|error| error.to_string())?);
    let check = update::check(&home, &update::manifest_url(), update::now_ms(), true);
    let active_version = install::active_version(&home)?;

    if !options.install {
        println!("{}", check_json(&check, active_version.as_deref()));
        return Ok(check_exit_code(&check));
    }
    let Some(manifest) = install_candidate(&check, active_version.as_deref()).cloned() else {
        let mut report = check_json(&check, active_version.as_deref());
        report["installed"] = json!(false);
        println!("{report}");
        return Ok(check_exit_code(&check));
    };

    // Read-only preflight, before anything changes on disk.
    let user_home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME is not set")?;
    let plist = launchd::plist_path(&user_home);
    let existing = launchd::read_existing(&plist)?;
    if let Some(existing) = &existing {
        launchd::check_home(existing, &home, &user_home)?;
    }
    let mut log = |line: &str| eprintln!("update: {line}");
    if active_version.as_deref() == Some(manifest.version.as_str())
        && probes::wait_for_daemon(socket, Duration::from_secs(2), Some(&manifest.version)).is_ok()
    {
        log(&format!(
            "{} is already installed and running",
            manifest.version
        ));
        println!(
            "{}",
            json!({
                "installed": manifest.version,
                "current": install::current_link(&home),
                "launchd": "unchanged",
                "health": { "ok": true, "version": manifest.version },
            })
        );
        return Ok(0);
    }

    let installed = install::install_release(&home, &manifest, &mut log)?;
    let _runtime_lock = runtime::MutationLock::acquire(&home)?;
    let snapshot = _runtime_lock.snapshot(&home, &plist)?;
    let previous_version = snapshot.active_version;
    let previous_plist = snapshot.plist;
    let existing = launchd::read_existing(&plist)?;
    if let Some(existing) = &existing {
        launchd::check_home(existing, &home, &user_home)?;
    }
    install::activate(&home, &manifest.version)?;
    log(&format!("bin/current -> {}", manifest.version));

    let Some(existing) = existing else {
        log("no launchd job installed; restart the daemon yourself to run the new version");
        println!(
            "{}",
            json!({
                "installed": manifest.version,
                "bin": installed,
                "current": install::current_link(&home),
                "launchd": "not_installed",
            })
        );
        return Ok(0);
    };

    if let Err(error) = launchd::apply_with_rollback(
        &home,
        &plist,
        Some(&existing),
        &probes::provider_directories(),
        &std::collections::BTreeMap::new(),
        None,
        {
            let rollback_home = home.clone();
            let previous_version = previous_version.clone();
            move || match previous_version.as_deref() {
                Some(previous) => install::activate(&rollback_home, previous),
                None => Ok(()),
            }
        },
    ) {
        let recovery = error
            .contains("launchd plist and job rolled back")
            .then_some(previous_version.as_deref())
            .flatten()
            .map(|previous| {
                probes::wait_for_managed_daemon(socket, HEALTH_WAIT, Some(previous), &home)
                    .map(|_| ())
            });
        let runtime_outcome = match previous_version.as_deref() {
            Some(previous) if error.contains("runtime rollback failed") => {
                format!("bin/current could NOT be rolled back to {previous}")
            }
            Some(previous) => format!("bin/current rolled back to {previous}"),
            None => format!(
                "bin/current stays at {} (there was no previous version)",
                manifest.version
            ),
        };
        return Err(match recovery {
            Some(Ok(())) => format!("{error}; {runtime_outcome}; restored runtime is healthy"),
            Some(Err(recovery)) => {
                format!("{error}; {runtime_outcome}; restored runtime health failed: {recovery}")
            }
            None if error.contains("launchd plist and job rolled back") => {
                format!(
                    "{error}; {runtime_outcome}; no previous managed runtime was available for health checking"
                )
            }
            None => format!("{error}; {runtime_outcome}"),
        });
    }
    log(&format!("rewrote and reloaded {}", plist.display()));
    match probes::wait_for_daemon(socket, HEALTH_WAIT, Some(&manifest.version)) {
        Ok(version) => {
            println!(
                "{}",
                json!({
                    "installed": manifest.version,
                    "bin": installed,
                    "current": install::current_link(&home),
                    "launchd": "reloaded",
                    "health": { "ok": true, "version": version },
                })
            );
            Ok(0)
        }
        Err(error) => {
            let rollback = match (previous_version.as_deref(), previous_plist.as_deref()) {
                (Some(previous), Some(previous_plist)) => install::activate(&home, previous)
                    .and_then(|()| {
                        let rollback_home = home.clone();
                        let manifest_version = manifest.version.clone();
                        launchd::restore_with_rollback(&plist, &home, previous_plist, move || {
                            install::activate(&rollback_home, &manifest_version)
                        })
                    })
                    .and_then(|()| {
                        probes::wait_for_managed_daemon(socket, HEALTH_WAIT, Some(previous), &home)
                            .map(|_| ())
                    }),
                _ => Err("no previous managed runtime is available".to_owned()),
            };
            println!(
                "{}",
                json!({
                    "installed": manifest.version,
                    "bin": installed,
                    "current": install::current_link(&home),
                    "launchd": "reloaded",
                    "health": { "ok": false, "error": error },
                    "rollback": rollback.as_ref().map(|()| "restored").unwrap_or("failed"),
                })
            );
            if let Err(rollback_error) = rollback {
                eprintln!("update: rollback failed: {rollback_error}");
            }
            eprintln!(
                "update: the new daemon did not answer within {}s ({error}); see {}/logs/factoryd.stderr.log, \
                 or roll back with `ln -sfn {} {}` and `launchctl kickstart -k gui/$(id -u)/{}`",
                HEALTH_WAIT.as_secs(),
                home.display(),
                previous_version.as_deref().unwrap_or("<previous-version>"),
                install::current_link(&home).display(),
                launchd::LABEL
            );
            Ok(1)
        }
    }
}

fn check_exit_code(check: &UpdateCheck) -> i32 {
    if check.latest.is_none() && check.error.is_some() {
        1
    } else {
        0
    }
}

/// The release to install or reconcile. An active runtime equal to the
/// manifest still enters the existing health check, which is the no-op when
/// the daemon already matches and the restart path when it does not.
fn install_candidate<'a>(
    check: &'a UpdateCheck,
    active_version: Option<&str>,
) -> Option<&'a update::Manifest> {
    let manifest = check.latest.as_ref()?;
    if !manifest.assets.contains_key(update::platform_key()) {
        return None;
    }
    match active_version {
        Some(active) => (active == manifest.version || update::is_newer(&manifest.version, active))
            .then_some(manifest),
        None => update::is_newer(&manifest.version, &check.current).then_some(manifest),
    }
}

fn check_json(check: &UpdateCheck, active_version: Option<&str>) -> serde_json::Value {
    let available = check.available_from(active_version.unwrap_or(&check.current));
    let mut report = json!({
        "current": check.current,
        "active": active_version,
        "checked_at_ms": check.checked_at_ms,
        "update_available": available.is_some(),
    });
    if let Some(latest) = &check.latest {
        report["latest"] = json!(latest.version);
        if let Some(asset) = latest.assets.get(update::platform_key()) {
            report["asset"] = json!(asset);
        }
    }
    if let Some(error) = &check.error {
        report["error"] = json!(error);
    }
    report
}

/// A relative `$DARK_FACTORY_HOME` would otherwise render relative paths
/// into the launchd job.
fn absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        env::current_dir().map_or(path.clone(), |cwd| cwd.join(path))
    }
}
