use std::{
    fs,
    os::unix::fs::OpenOptionsExt as _,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn runner() -> &'static str {
    env!("CARGO_BIN_EXE_factory-runner")
}

#[test]
fn expected_parent_mismatch_exits_without_activation_or_exec() {
    let directory = tempfile::tempdir().unwrap();
    let activation = directory.path().join("never.activate");
    let marker = directory.path().join("executed");
    let actual_parent = rustix::process::getpid().as_raw_nonzero().get();
    let wrong_parent = actual_parent.checked_add(1).unwrap();

    let mut child = Command::new(runner())
        .arg("--exec-gate")
        .arg(&activation)
        .arg("--expected-parent-pid")
        .arg(wrong_parent.to_string())
        .arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg("printf executed > \"$1\"")
        .arg("gate-test")
        .arg(&marker)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "mismatched gate did not exit");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(status.success());
    assert!(!marker.exists());
}

#[test]
fn parent_exit_before_activation_cannot_leave_an_executable_orphan() {
    let directory = tempfile::tempdir().unwrap();
    let activation = directory.path().join("late.activate");
    let marker = directory.path().join("must-not-execute");
    let child_pid_file = directory.path().join("gate-pid");
    let parent = Command::new("/bin/sh")
        .arg("-c")
        .arg(
            r#""$RUNNER" --exec-gate "$ACTIVATION" --expected-parent-pid "$$" -- /usr/bin/touch "$MARKER" &
printf '%s' "$!" > "$CHILD_PID_FILE""#,
        )
        .env("RUNNER", runner())
        .env("ACTIVATION", &activation)
        .env("MARKER", &marker)
        .env("CHILD_PID_FILE", &child_pid_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert!(parent.wait_with_output().unwrap().status.success());

    let child_pid = fs::read_to_string(child_pid_file)
        .unwrap()
        .parse::<i32>()
        .unwrap();
    let child_pid = rustix::process::Pid::from_raw(child_pid).unwrap();
    let activation_file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&activation)
        .unwrap();
    activation_file.sync_all().unwrap();

    let deadline = Instant::now() + Duration::from_secs(2);
    while rustix::process::test_kill_process(child_pid).is_ok() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(rustix::process::test_kill_process(child_pid).is_err());
    assert!(!marker.exists());
}

#[test]
fn activation_execs_target_with_the_gate_pid() {
    let directory = tempfile::tempdir().unwrap();
    let activation = directory.path().join("runner.activate");
    let marker = directory.path().join("executed-pid");
    let expected_parent = rustix::process::getpid().as_raw_nonzero().get();
    let mut child = Command::new(runner())
        .arg("--exec-gate")
        .arg(&activation)
        .arg("--expected-parent-pid")
        .arg(expected_parent.to_string())
        .arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg("printf '%s' \"$$\" > \"$1\"")
        .arg("gate-test")
        .arg(&marker)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let prepared_pid = child.id();

    let activation_file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&activation)
        .unwrap();
    activation_file.sync_all().unwrap();
    assert!(child.wait().unwrap().success());

    assert_eq!(
        fs::read_to_string(marker).unwrap().parse::<u32>().unwrap(),
        prepared_pid
    );
}
