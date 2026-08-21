#![cfg(target_os = "macos")]

use std::{
    collections::BTreeMap,
    env,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use factoryctl::{
    install,
    launchd::{self, ApplyRequest, LaunchdTarget},
    probes, runtime,
};

const FIRST: &str = "0.0.1";
const SECOND: &str = "0.0.2";
const CRASHING: &str = "0.0.3";

fn required_path(name: &str) -> PathBuf {
    PathBuf::from(env::var_os(name).unwrap_or_else(|| panic!("{name} is required")))
}

fn mark(root: &Path, name: &str) {
    fs::write(root.join(name), b"\n").unwrap();
}

fn record_pid(root: &Path, pid: u32) {
    writeln!(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("observed-pids"))
            .unwrap(),
        "{pid}"
    )
    .unwrap();
}

fn managed_pid(target: &LaunchdTarget, socket: &Path, home: &Path) -> u32 {
    probes::wait_for_managed_daemon_for(
        target,
        socket,
        Duration::from_secs(15),
        Some(env!("CARGO_PKG_VERSION")),
        home,
    )
    .unwrap();
    launchd::job_pid_for(target).unwrap().unwrap()
}

fn apply(
    target: &LaunchdTarget,
    home: &Path,
    plist: &Path,
    existing: Option<&launchd::ExistingJob>,
    environment: &BTreeMap<String, String>,
    rollback: impl FnMut() -> Result<(), String>,
) {
    launchd::apply_with_rollback_for(
        ApplyRequest {
            target,
            home,
            plist,
            existing,
            provider_directories: &[],
            extra_environment: environment,
            capacity: Some(1),
        },
        rollback,
    )
    .unwrap();
}

#[test]
#[ignore = "opt-in: loads a randomized disposable launchd job"]
fn disposable_launchd_release_replacement() {
    assert_eq!(
        env::var("DARK_FACTORY_LAUNCHD_RELEASE_PROOF").as_deref(),
        Ok("1")
    );
    let root = required_path("DARK_FACTORY_LAUNCHD_FIXTURE_ROOT");
    let source = required_path("DARK_FACTORY_LAUNCHD_SOURCE_DIR");
    let label = env::var("DARK_FACTORY_LAUNCHD_LABEL").unwrap();
    assert!(label.starts_with("com.dark-factory.fixture."));
    assert!(
        label
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || ".-".contains(character))
    );

    let user_home = root.join("user-home");
    let home = root.join("factory-home");
    let socket = home.join("f.sock");
    let target = LaunchdTarget::new(format!("gui/{}", rustix::process::getuid().as_raw()), label);
    let plist = launchd::plist_path_for(&user_home, &target);
    assert!(plist.starts_with(&root));
    install::create_private_dir(&user_home).unwrap();
    install::create_private_dir(&home).unwrap();
    install::create_private_dir(&root.join("tmp")).unwrap();

    install::install_from_dir(&home, &source, FIRST).unwrap();
    install::install_from_dir(&home, &source, SECOND).unwrap();
    install::activate(&home, FIRST).unwrap();

    let mut environment = BTreeMap::from([
        ("HOME".to_owned(), user_home.to_string_lossy().into_owned()),
        (
            "PATH".to_owned(),
            env::var("DARK_FACTORY_LAUNCHD_SAFE_PATH").unwrap(),
        ),
        (
            "DARK_FACTORY_SOCKET".to_owned(),
            socket.to_string_lossy().into_owned(),
        ),
        (
            "TMPDIR".to_owned(),
            root.join("tmp").to_string_lossy().into_owned(),
        ),
    ]);
    if let Ok(rustup_home) = env::var("RUSTUP_HOME") {
        environment.insert("RUSTUP_HOME".to_owned(), rustup_home);
    }
    if let Ok(toolchain) = env::var("RUSTUP_TOOLCHAIN") {
        environment.insert("RUSTUP_TOOLCHAIN".to_owned(), toolchain);
    }

    apply(&target, &home, &plist, None, &environment, || Ok(()));
    let first_pid = managed_pid(&target, &socket, &home);
    record_pid(&root, first_pid);
    mark(&root, "first-live");

    let (replacement_lock, replacement_snapshot) =
        runtime::MutationLock::begin(&home, &plist).unwrap();
    let existing = launchd::read_existing(&plist).unwrap().unwrap();
    install::activate(&home, SECOND).unwrap();
    apply(
        &target,
        &home,
        &plist,
        Some(&existing),
        &environment,
        || replacement_snapshot.restore_runtime(&home),
    );
    drop(replacement_lock);
    assert_eq!(
        install::active_version(&home).unwrap().as_deref(),
        Some(SECOND)
    );
    let second_pid = managed_pid(&target, &socket, &home);
    assert_ne!(second_pid, first_pid);
    record_pid(&root, second_pid);
    mark(&root, "second-live");
    if env::var("DARK_FACTORY_LAUNCHD_FAIL_AFTER_SECOND").as_deref() == Ok("1") {
        panic!("intentional failure after the replacement job became live");
    }

    let crashing_source = root.join("crashing-source");
    fs::create_dir(&crashing_source).unwrap();
    for binary in install::BINARIES {
        fs::copy(source.join(binary), crashing_source.join(binary)).unwrap();
    }
    fs::write(crashing_source.join("factoryd"), "#!/bin/sh\nexit 42\n").unwrap();
    fs::set_permissions(
        crashing_source.join("factoryd"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    install::install_from_dir(&home, &crashing_source, CRASHING).unwrap();

    let (_failure_lock, failure_snapshot) = runtime::MutationLock::begin(&home, &plist).unwrap();
    let existing = launchd::read_existing(&plist).unwrap().unwrap();
    install::activate(&home, CRASHING).unwrap();
    apply(
        &target,
        &home,
        &plist,
        Some(&existing),
        &environment,
        || failure_snapshot.restore_runtime(&home),
    );
    probes::wait_for_managed_daemon_for(
        &target,
        &socket,
        Duration::from_secs(3),
        Some(env!("CARGO_PKG_VERSION")),
        &home,
    )
    .expect_err("the deliberately crashing runtime must not become healthy");
    mark(&root, "crash-observed");

    runtime::rollback_after_health_failure_for(
        &target,
        &home,
        &plist,
        &failure_snapshot,
        CRASHING,
        |previous| {
            if previous != SECOND {
                return Err(format!("rollback selected {previous}, expected {SECOND}"));
            }
            probes::wait_for_managed_daemon_for(
                &target,
                &socket,
                Duration::from_secs(15),
                Some(env!("CARGO_PKG_VERSION")),
                &home,
            )
            .map(|_| ())
        },
    )
    .unwrap();
    assert_eq!(
        install::active_version(&home).unwrap().as_deref(),
        Some(SECOND)
    );
    let restored_pid = managed_pid(&target, &socket, &home);
    assert_ne!(restored_pid, second_pid);
    record_pid(&root, restored_pid);
    mark(&root, "rollback-live");
}
