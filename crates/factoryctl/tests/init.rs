//! `factoryctl init` and `factoryctl doctor` end to end on a throwaway
//! `$DARK_FACTORY_HOME` and `HOME`. The launchd step is the one thing a
//! test can't exercise (it would load a real job into this user's launchd
//! domain), so `init` runs with `--no-launchd`, and the consent prompt is
//! exercised by *not* passing `--yes` on a non-terminal stdin.

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
};

use factory_core::{
    PROTOCOL_VERSION,
    local::{LocalRequest, LocalResponse, RequestEnvelope, ServerFrame},
};

const SIBLINGS: [&str; 3] = ["factoryd", "factory-runner", "factory-tui"];

/// A copy of the real `factoryctl` next to three fake siblings, so
/// `current_exe().parent()` — what `init` installs from — is a directory
/// this test controls.
fn staged_factoryctl(root: &Path) -> PathBuf {
    let source = root.join("build");
    fs::create_dir_all(&source).unwrap();
    for name in SIBLINGS {
        let path = source.join(name);
        fs::write(&path, format!("#!/bin/sh\necho {name}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let factoryctl = source.join("factoryctl");
    fs::copy(env!("CARGO_BIN_EXE_factoryctl"), &factoryctl).unwrap();
    factoryctl
}

fn run(factoryctl: &Path, root: &Path, args: &[&str]) -> (i32, String, String) {
    run_with_env(factoryctl, root, args, &[])
}

fn run_with_env(
    factoryctl: &Path,
    root: &Path,
    args: &[&str],
    extra: &[(&str, &Path)],
) -> (i32, String, String) {
    let mut command = Command::new(factoryctl);
    command
        .args(args)
        .env("DARK_FACTORY_HOME", root.join("home"))
        .env("HOME", root.join("user-home"))
        .env("DARK_FACTORY_UPDATE_URL", "http://127.0.0.1:9/never")
        // The developer's shell may point CODEX_HOME at another account.
        .env_remove("CODEX_HOME")
        .stdin(Stdio::null());
    for (key, value) in extra {
        command.env(key, value);
    }
    let output = command.output().expect("run factoryctl");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

#[test]
fn init_creates_the_home_installs_this_build_and_activates_it() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("user-home")).unwrap();
    let factoryctl = staged_factoryctl(root.path());
    let home = root.path().join("home");

    let (code, stdout, stderr) = run(&factoryctl, root.path(), &["init", "--yes", "--no-launchd"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.contains("home: ") && stdout.contains("(created)"),
        "{stdout}"
    );
    assert!(stdout.contains("git: "), "{stdout}");
    assert!(
        stdout.contains(&format!(
            "codex home for agents: {}",
            root.path().join("user-home/.codex").display()
        )),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "install: bin/current -> {}",
            factoryctl::update::CURRENT_VERSION
        )),
        "{stdout}"
    );
    assert_eq!(
        fs::metadata(&home).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(home.join("logs"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let version_dir = home.join("bin").join(factoryctl::update::CURRENT_VERSION);
    for name in SIBLINGS.iter().chain(["factoryctl"].iter()) {
        let installed = version_dir.join(name);
        assert!(installed.is_file(), "{}", installed.display());
        assert_ne!(
            fs::metadata(&installed).unwrap().permissions().mode() & 0o111,
            0
        );
    }
    assert_eq!(
        fs::read_link(home.join("bin/current")).unwrap(),
        Path::new(factoryctl::update::CURRENT_VERSION)
    );
    // --no-launchd never touches ~/Library.
    assert!(!root.path().join("user-home/Library").exists());

    // A second run with the same build is a no-op for the binaries and still succeeds...
    let (code, stdout, _) = run(&factoryctl, root.path(), &["init", "--yes", "--no-launchd"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("(exists)") && stdout.contains("already holds this build"),
        "{stdout}"
    );
    // ...but a rebuilt binary under the same version is refused rather than silently kept.
    fs::write(
        factoryctl.parent().unwrap().join("factory-tui"),
        "#!/bin/sh\necho rebuilt\n",
    )
    .unwrap();
    let (code, _, stderr) = run(&factoryctl, root.path(), &["init", "--yes", "--no-launchd"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("differs from this build"), "{stderr}");
    // The disclosure is printed even when launchd is skipped.
    assert!(stdout.contains("outside"), "{stdout}");
}

#[test]
fn init_refuses_to_touch_launchd_without_consent_on_a_non_terminal() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("user-home")).unwrap();
    let factoryctl = staged_factoryctl(root.path());
    let (code, stdout, stderr) = run(&factoryctl, root.path(), &["init"]);
    assert_eq!(code, 1, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.contains("--yes"), "{stderr}");
    // Binaries were still installed and activated before the prompt...
    assert!(root.path().join("home/bin/current/factoryctl").exists());
    // ...but nothing reached launchd.
    assert!(!root.path().join("user-home/Library").exists());
}

#[test]
fn doctor_reports_each_check_and_fails_without_a_daemon() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("user-home")).unwrap();
    let factoryctl = staged_factoryctl(root.path());
    // Before init: home missing is a failure, and so is the daemon.
    let (code, stdout, _) = run(&factoryctl, root.path(), &["doctor"]);
    assert_eq!(code, 1);
    assert!(stdout.contains("[FAIL] home:"), "{stdout}");
    assert!(stdout.contains("[FAIL] daemon:"), "{stdout}");

    run(&factoryctl, root.path(), &["init", "--yes", "--no-launchd"]);
    let (code, stdout, _) = run(&factoryctl, root.path(), &["doctor", "--json"]);
    assert_eq!(code, 1, "{stdout}");
    let report: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(report["ok"], false);
    let checks = report["checks"].as_array().unwrap();
    let status = |name: &str| {
        checks
            .iter()
            .find(|check| check["name"] == name)
            .unwrap_or_else(|| panic!("no check named {name}: {stdout}"))["status"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(status("home"), "ok");
    assert_eq!(status("install"), "ok");
    assert_eq!(status("daemon"), "fail");
    assert_eq!(status("launchd"), "warn");
    assert_eq!(status("git"), "ok");
    assert_eq!(status("claude.json"), "warn");
    // Only meaningful where codex is installed (the check is `ok`, n/a, otherwise).
    let codex_installed = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("codex").is_file()));
    if codex_installed {
        assert_eq!(
            status("codex-seed"),
            "warn",
            "the throwaway HOME has no ~/.codex/auth.json"
        );
    }
    // A CODEX_HOME with credentials is reported as the home in effect.
    let dogfood = root.path().join("codex-dogfood");
    fs::create_dir_all(&dogfood).unwrap();
    fs::write(dogfood.join("auth.json"), "{}").unwrap();
    let (_, stdout, _) = run_with_env(
        &factoryctl,
        root.path(),
        &["init", "--yes", "--no-launchd"],
        &[("CODEX_HOME", &dogfood)],
    );
    assert!(
        stdout.contains(&format!(
            "codex home for agents: {} (auth.json present",
            dogfood.display()
        )),
        "{stdout}"
    );
    assert_eq!(status("update"), "warn");
}

fn doctor_report(active_version: &str, daemon_version: &str) -> serde_json::Value {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("user-home")).unwrap();
    let factoryctl = staged_factoryctl(root.path());
    let home = root.path().join("home");
    let (code, _, stderr) = run(&factoryctl, root.path(), &["init", "--yes", "--no-launchd"]);
    assert_eq!(code, 0, "{stderr}");

    let active = home.join("bin").join(active_version);
    fs::create_dir_all(&active).unwrap();
    for name in SIBLINGS.iter().chain(["factoryctl"].iter()) {
        let binary = active.join(name);
        fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::remove_file(home.join("bin/current")).unwrap();
    std::os::unix::fs::symlink(active_version, home.join("bin/current")).unwrap();

    let now_ms = factoryctl::update::now_ms();
    let cache = serde_json::json!({
        "checked_at_ms": now_ms,
        "current": factoryctl::update::CURRENT_VERSION,
        "latest": {
            "version": factoryctl::update::CURRENT_VERSION,
            "assets": {
                factoryctl::update::platform_key(): {
                    "url": "https://example.invalid/release.tar.gz",
                    "sha256": "00"
                }
            }
        }
    });
    fs::write(
        home.join("update-check.json"),
        serde_json::to_vec(&cache).unwrap(),
    )
    .unwrap();

    let socket = home.join("f.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let daemon_version = daemon_version.to_owned();
    let server = thread::spawn(move || {
        let replies = [
            LocalResponse::Health {
                runner_path: "/tmp/factory-runner".to_owned(),
                factoryctl_path: "/tmp/factoryctl".to_owned(),
                version: daemon_version,
            },
            LocalResponse::Projects {
                projects: Vec::new(),
                next_after_id: None,
            },
        ];
        for (index, response) in replies.into_iter().enumerate() {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request = serde_json::from_str::<RequestEnvelope>(&line).unwrap();
            if index == 0 {
                assert_eq!(request, RequestEnvelope::new(LocalRequest::Health));
            } else {
                assert!(matches!(request.request, LocalRequest::ListProjects { .. }));
            }
            serde_json::to_writer(
                &mut stream,
                &ServerFrame::Response {
                    protocol_version: PROTOCOL_VERSION,
                    response,
                },
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
        }
    });

    let (code, stdout, stderr) = run(&factoryctl, root.path(), &["doctor", "--json"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    server.join().unwrap();
    serde_json::from_str(stdout.trim()).unwrap()
}

fn named_check<'a>(report: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == name)
        .unwrap_or_else(|| panic!("missing {name}: {report}"))
}

#[test]
fn doctor_compares_daemon_and_release_with_the_stale_active_runtime() {
    let report = doctor_report("0.1.0", "0.1.0");
    assert_eq!(named_check(&report, "install")["status"], "warn");
    assert!(
        named_check(&report, "install")["detail"]
            .as_str()
            .unwrap()
            .contains("bin/current -> 0.1.0")
    );
    assert_eq!(named_check(&report, "daemon")["status"], "ok");
    assert!(
        named_check(&report, "daemon")["detail"]
            .as_str()
            .unwrap()
            .contains("matches active runtime")
    );
    assert_eq!(named_check(&report, "update")["status"], "warn");
    assert!(
        named_check(&report, "update")["detail"]
            .as_str()
            .unwrap()
            .contains(&format!(
                "v{} available",
                factoryctl::update::CURRENT_VERSION
            ))
    );
}

#[test]
fn doctor_accepts_a_newer_active_runtime_without_recommending_a_downgrade() {
    let report = doctor_report("999.0.0", "999.0.0");
    let install = named_check(&report, "install");
    assert_eq!(install["status"], "ok");
    assert!(
        install["detail"]
            .as_str()
            .unwrap()
            .contains("newer than this factoryctl")
    );
    assert!(
        !install["detail"]
            .as_str()
            .unwrap()
            .contains("update --install")
    );
    assert_eq!(named_check(&report, "daemon")["status"], "ok");
    let update = named_check(&report, "update");
    assert_eq!(update["status"], "ok");
    assert_eq!(update["detail"], "999.0.0 is the latest installed release");
}
