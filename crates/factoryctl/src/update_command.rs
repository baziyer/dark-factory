//! `factoryctl update [--install]`.
//!
//! `update` fetches the release manifest (always fresh — an operator asking
//! wants the real answer; the result is still cached for `factory-tui`'s
//! hourly status-line check) and prints where this build stands. `--install`
//! then downloads and verifies the newer release into
//! `$DARK_FACTORY_HOME/bin/<version>/`, repoints `bin/current`, and — if
//! this machine's launchd job exists — rewrites it to run from `bin/current`
//! and reloads it, restarting only the daemon; running sessions are never
//! touched (`ARCHITECTURE.md`, invariant 4). Without a launchd job the
//! binaries are installed and activated and the operator restarts the daemon
//! however they run it.

use std::{
    env,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use factory_core::local::{LocalRequest, LocalResponse, ServerFrame};
use factoryctl::{
    Client,
    update::{self, UpdateCheck},
};
use serde_json::json;

use crate::{install, launchd};

const HEALTH_WAIT: Duration = Duration::from_secs(20);

pub struct Options {
    pub install: bool,
}

pub fn run(options: &Options, socket: &Path) -> Result<i32, String> {
    let home = factory_core::paths::dark_factory_home().map_err(|error| error.to_string())?;
    let check = update::check(&home, &update::manifest_url(), now_ms(), true);
    let available = check.available().cloned();

    if !options.install {
        println!("{}", check_json(&check));
        return Ok(if check.latest.is_none() && check.error.is_some() {
            1
        } else {
            0
        });
    }

    let Some(manifest) = available else {
        let mut report = check_json(&check);
        report["installed"] = json!(false);
        println!("{report}");
        return Ok(if check.latest.is_none() && check.error.is_some() {
            1
        } else {
            0
        });
    };

    let mut log = |line: &str| eprintln!("update: {line}");
    let installed = if install::version_dir(&home, &manifest.version).exists() {
        log(&format!(
            "bin/{} already present; activating it",
            manifest.version
        ));
        install::version_dir(&home, &manifest.version)
    } else {
        install::install_release(&home, &manifest, &mut log)?
    };
    install::activate(&home, &manifest.version)?;
    log(&format!("bin/current -> {}", manifest.version));

    let launchd_state = match reload_launchd(&home)? {
        Some(plist) => {
            log(&format!("rewrote and reloaded {}", plist.display()));
            "reloaded"
        }
        None => {
            log("no launchd job installed; restart the daemon yourself to run the new version");
            "not_installed"
        }
    };
    let health = if launchd_state == "reloaded" {
        wait_for_health(socket)
    } else {
        json!({ "ok": false, "error": "daemon not restarted" })
    };
    println!(
        "{}",
        json!({
            "installed": manifest.version,
            "bin": installed,
            "current": install::current_link(&home),
            "launchd": launchd_state,
            "health": health,
        })
    );
    Ok(0)
}

fn check_json(check: &UpdateCheck) -> serde_json::Value {
    let available = check.available();
    let mut report = json!({
        "current": check.current,
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

/// Rewrites the launchd job to run `<home>/bin/current/factoryd` (keeping
/// whatever other arguments and `PATH` it already had) and reloads it.
/// Returns the plist path, or `None` when no job is installed.
fn reload_launchd(home: &Path) -> Result<Option<PathBuf>, String> {
    let user_home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME is not set")?;
    let plist = launchd::plist_path(&user_home);
    let Some(existing) = launchd::read_existing(&plist)? else {
        return Ok(None);
    };
    let factoryd = install::current_link(home).join("factoryd");
    let rendered = launchd::render(
        home,
        &factoryd,
        &launchd::carried_arguments(&existing.program_arguments),
        existing.path_env.as_deref().unwrap_or(launchd::BASE_PATH),
    );
    launchd::install(&plist, &rendered, home)?;
    launchd::reload(rustix::process::getuid().as_raw(), &plist)?;
    Ok(Some(plist))
}

fn wait_for_health(socket: &Path) -> serde_json::Value {
    let client = Client::new(socket);
    let deadline = Instant::now() + HEALTH_WAIT;
    loop {
        let last_error =
            match client.request_with_timeout(LocalRequest::Health, Duration::from_secs(2)) {
                Ok(ServerFrame::Response {
                    response: LocalResponse::Health { version, .. },
                    ..
                }) => return json!({ "ok": true, "version": version }),
                Ok(_) => "unexpected reply to health".to_owned(),
                Err(error) => error.to_string(),
            };
        if Instant::now() >= deadline {
            return json!({ "ok": false, "error": last_error });
        }
        thread::sleep(Duration::from_millis(500));
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}
