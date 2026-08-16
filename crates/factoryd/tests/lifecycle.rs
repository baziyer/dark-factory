use std::{
    fs,
    io::Read,
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink},
        net::{UnixListener as StdUnixListener, UnixStream},
    },
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use factoryd::lifecycle::DaemonInstance;

fn private_directory(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o777
}

#[test]
fn one_database_has_one_lifetime_owner_even_with_different_sockets() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("state/factory.db");
    let first_socket = directory.path().join("control/first.sock");
    let second_socket = directory.path().join("control/second.sock");

    let first = DaemonInstance::claim(&database, &first_socket).unwrap();
    let error = DaemonInstance::claim(&database, &second_socket).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);

    drop(first);
    DaemonInstance::claim(&database, &second_socket).unwrap();
}

#[test]
fn claim_creates_private_non_symlink_state_and_lock_files() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("state/factory.db");
    let socket = directory.path().join("control/f.sock");

    let instance = DaemonInstance::claim(&database, &socket).unwrap();

    assert_eq!(mode(database.parent().unwrap()), 0o700);
    assert_eq!(mode(socket.parent().unwrap()), 0o700);
    assert_eq!(mode(instance.database_path()), 0o600);
    assert_eq!(mode(instance.lock_path()), 0o600);
    assert!(
        fs::symlink_metadata(instance.database_path())
            .unwrap()
            .file_type()
            .is_file()
    );
    assert!(
        fs::symlink_metadata(instance.lock_path())
            .unwrap()
            .file_type()
            .is_file()
    );
}

#[test]
fn claim_rejects_insecure_or_symlinked_parents_without_changing_them() {
    let directory = tempfile::tempdir().unwrap();
    let insecure = directory.path().join("insecure");
    private_directory(&insecure);
    fs::set_permissions(&insecure, fs::Permissions::from_mode(0o755)).unwrap();
    let safe = directory.path().join("safe");
    private_directory(&safe);

    let error = DaemonInstance::claim(&insecure.join("factory.db"), &safe.join("insecure.sock"))
        .unwrap_err();
    assert!(error.to_string().contains("owner-only"));
    assert_eq!(mode(&insecure), 0o755);

    let link = directory.path().join("linked-state");
    symlink(&safe, &link).unwrap();
    let error =
        DaemonInstance::claim(&link.join("factory.db"), &safe.join("linked.sock")).unwrap_err();
    assert!(error.to_string().contains("symbolic link"));
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn claim_rejects_a_database_or_lock_file_that_is_not_private_and_regular() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("state");
    private_directory(&state);
    let control = directory.path().join("control");
    private_directory(&control);
    let database = state.join("factory.db");
    fs::write(&database, []).unwrap();
    fs::set_permissions(&database, fs::Permissions::from_mode(0o644)).unwrap();

    let error = DaemonInstance::claim(&database, &control.join("first.sock")).unwrap_err();
    assert!(error.to_string().contains("0600"));
    assert_eq!(mode(&database), 0o644);

    fs::set_permissions(&database, fs::Permissions::from_mode(0o600)).unwrap();
    let lock = state.join("factory.db.lock");
    symlink(&database, &lock).unwrap();
    let error = DaemonInstance::claim(&database, &control.join("second.sock")).unwrap_err();
    assert!(error.to_string().contains("regular file"));
    assert!(fs::symlink_metadata(lock).unwrap().file_type().is_symlink());
}

#[test]
fn bind_recovers_a_stale_socket_but_preserves_live_and_regular_endpoints() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("state");
    let control = directory.path().join("control");
    private_directory(&state);
    private_directory(&control);

    let stale_path = control.join("stale.sock");
    let stale = StdUnixListener::bind(&stale_path).unwrap();
    drop(stale);
    let stale_instance = DaemonInstance::claim(&state.join("stale.db"), &stale_path).unwrap();
    let (listener, cleanup) = stale_instance.bind_socket().unwrap();
    assert!(
        fs::symlink_metadata(&stale_path)
            .unwrap()
            .file_type()
            .is_socket()
    );
    assert_eq!(mode(&stale_path), 0o600);
    drop(listener);
    drop(cleanup);
    assert!(!stale_path.exists());

    let live_path = control.join("live.sock");
    let live = StdUnixListener::bind(&live_path).unwrap();
    let live_instance = DaemonInstance::claim(&state.join("live.db"), &live_path).unwrap();
    assert!(live_instance.bind_socket().is_err());
    assert!(
        fs::symlink_metadata(&live_path)
            .unwrap()
            .file_type()
            .is_socket()
    );
    drop(live);

    let regular_path = control.join("keep.txt");
    fs::write(&regular_path, "keep me").unwrap();
    let regular_instance = DaemonInstance::claim(&state.join("regular.db"), &regular_path).unwrap();
    assert!(regular_instance.bind_socket().is_err());
    assert_eq!(fs::read_to_string(&regular_path).unwrap(), "keep me");
}

#[test]
fn socket_cleanup_never_unlinks_a_replacement_path() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("state/factory.db");
    let socket = directory.path().join("control/f.sock");
    let instance = DaemonInstance::claim(&database, &socket).unwrap();
    let (listener, cleanup) = instance.bind_socket().unwrap();
    drop(listener);

    fs::remove_file(&socket).unwrap();
    fs::write(&socket, "replacement").unwrap();
    drop(cleanup);

    assert_eq!(fs::read_to_string(socket).unwrap(), "replacement");
}

#[test]
fn sigterm_is_a_clean_shutdown_and_the_socket_can_be_rebound() {
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let database = directory.path().join("factory.db");
    let socket = directory.path().join("f.sock");
    let factory_home = directory.path().join("home");
    let mut child = Command::new(env!("CARGO_BIN_EXE_factoryd"))
        .args([
            "--database",
            database.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
        ])
        .env("DARK_FACTORY_HOME", factory_home)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    wait_until_ready(&mut child, &socket);
    let signal = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(signal.success());
    let status = wait_for_exit(&mut child);

    assert!(status.success(), "factoryd did not handle SIGTERM cleanly");
    assert!(!socket.exists(), "clean shutdown left a stale socket");
    let rebound = StdUnixListener::bind(&socket).unwrap();
    drop(rebound);
}

fn wait_until_ready(child: &mut Child, socket: &Path) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if UnixStream::connect(socket).is_ok() {
            return;
        }
        if let Some(status) = child.try_wait().unwrap() {
            let mut stderr = String::new();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();
            panic!("factoryd exited before becoming ready: {status}: {stderr}");
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    panic!("factoryd did not become ready");
}

fn wait_for_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    panic!("factoryd did not stop after SIGTERM");
}
