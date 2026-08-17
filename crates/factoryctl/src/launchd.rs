//! The `launchd` job that keeps `factoryd` running, rendered from
//! `launchd/com.dark-factory.factoryd.plist.template` (embedded here so a
//! binary-only install never needs the repository).
//!
//! `factoryctl update --install` (and `factoryctl init`) own this file: they
//! render it, write it at `0600` under `~/Library/LaunchAgents`, and reload
//! it. A reload restarts only the daemon — every session's runner is a
//! detached process tree the new daemon reconnects to (`ARCHITECTURE.md`,
//! invariant 4).

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub const LABEL: &str = "com.dark-factory.factoryd";
const TEMPLATE: &str = include_str!("../../../launchd/com.dark-factory.factoryd.plist.template");
/// What a rendered job's `PATH` gets when nothing better is known: enough
/// to find Homebrew/system tools; `claude`/`codex` directories are added by
/// the caller from wherever they resolve right now.
pub const BASE_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// `~/Library/LaunchAgents/com.dark-factory.factoryd.plist`.
#[must_use]
pub fn plist_path(user_home: &Path) -> PathBuf {
    user_home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

/// What an existing job runs and with which `PATH`, read back through
/// `plutil` so no plist parsing lives here.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExistingJob {
    pub program_arguments: Vec<String>,
    pub path_env: Option<String>,
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
    let path_env = value["EnvironmentVariables"]["PATH"]
        .as_str()
        .map(str::to_owned);
    Ok(Some(ExistingJob {
        program_arguments,
        path_env,
    }))
}

/// Renders the job: `factoryd` (an absolute path, normally
/// `<home>/bin/current/factoryd`) plus `extra_arguments`, with `PATH` and
/// `DARK_FACTORY_HOME` in its environment and logs under `<home>/logs`.
#[must_use]
pub fn render(home: &Path, factoryd: &Path, extra_arguments: &[String], path_env: &str) -> String {
    let arguments = std::iter::once(factoryd.to_string_lossy().into_owned())
        .chain(extra_arguments.iter().cloned())
        .map(|argument| format!("        <string>{}</string>", escape(&argument)))
        .collect::<Vec<_>>()
        .join("\n");
    TEMPLATE
        .replace("__PROGRAM_ARGUMENTS__", &arguments)
        .replace("__PATH__", &escape(path_env))
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

/// Writes `content` at `plist` with mode `0600` (atomically: temp file,
/// then rename), creating `~/Library/LaunchAgents` if needed, and creates
/// `<home>/logs` (`0700`) so launchd can open the log files.
pub fn install(plist: &Path, content: &str, home: &Path) -> Result<(), String> {
    let logs = home.join("logs");
    fs::create_dir_all(&logs)
        .map_err(|error| format!("could not create {}: {error}", logs.display()))?;
    fs::set_permissions(&logs, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("could not set permissions on {}: {error}", logs.display()))?;
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
/// then `bootstrap`. launchd caches a job's `ProgramArguments`, so a plain
/// `kickstart -k` would keep running the *old* binary after a rewrite —
/// this is the only sequence that picks up a changed plist.
pub fn reload(uid: u32, plist: &Path) -> Result<(), String> {
    let domain = format!("gui/{uid}");
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{LABEL}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let output = Command::new("launchctl")
        .arg("bootstrap")
        .arg(&domain)
        .arg(plist)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("could not run launchctl: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "launchctl bootstrap {domain} {} failed: {}",
            plist.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_fills_every_placeholder_and_escapes() {
        let rendered = render(
            Path::new("/Users/me/.dark-factory"),
            Path::new("/Users/me/.dark-factory/bin/current/factoryd"),
            &["--max-active-runs".to_owned(), "6".to_owned()],
            "/Users/me/.local/bin:/opt/homebrew/bin:/usr/bin:/bin&more",
        );
        assert!(!rendered.contains("__"), "{rendered}");
        assert!(rendered.contains("<string>/Users/me/.dark-factory/bin/current/factoryd</string>"));
        assert!(
            rendered.contains("<string>--max-active-runs</string>\n        <string>6</string>")
        );
        assert!(rendered.contains(
            "<string>/Users/me/.local/bin:/opt/homebrew/bin:/usr/bin:/bin&amp;more</string>"
        ));
        assert!(rendered.contains(
            "<key>DARK_FACTORY_HOME</key>\n        <string>/Users/me/.dark-factory</string>"
        ));
        assert!(
            rendered.contains("<string>/Users/me/.dark-factory/logs/factoryd.stderr.log</string>")
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
            "/usr/bin:/bin",
        );
        fs::write(&plist, rendered).unwrap();
        let existing = read_existing(&plist).unwrap().unwrap();
        assert_eq!(
            existing.program_arguments,
            ["/h/bin/current/factoryd", "--max-active-runs", "2"].map(str::to_owned)
        );
        assert_eq!(existing.path_env.as_deref(), Some("/usr/bin:/bin"));
        assert_eq!(
            read_existing(&root.path().join("missing.plist")).unwrap(),
            None
        );
    }
}
