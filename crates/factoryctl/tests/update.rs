//! `factoryctl update` / `update --install` end to end against a local
//! `file://` manifest and archive — the exact shapes
//! `scripts/package-release.sh` produces — with `HOME` pointed at an empty
//! directory so no launchd job is found (the daemon-restart step is the
//! one part that can't run in a test).

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

use factory_core::{
    PROTOCOL_VERSION,
    local::{LocalRequest, LocalResponse, RequestEnvelope, ServerFrame},
};
use serde_json::Value;

const BINARIES: [&str; 4] = ["factoryd", "factory-runner", "factoryctl", "factory-tui"];

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(root.path().join("home")).unwrap();
        fs::create_dir_all(root.path().join("user-home")).unwrap();
        Self { root }
    }

    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    fn write_binaries(&self, directory: &Path, version: &str) {
        fs::create_dir_all(directory).unwrap();
        for name in BINARIES {
            let path = directory.join(name);
            fs::write(&path, format!("#!/bin/sh\necho {name} {version}\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn activate(&self, version: &str) {
        let bin = self.home().join("bin");
        self.write_binaries(&bin.join(version), version);
        std::os::unix::fs::symlink(version, bin.join("current")).unwrap();
    }

    /// Writes an archive of four fake executables and a manifest naming
    /// `version`, returns the manifest's `file://` URL.
    fn publish(&self, version: &str, sha_override: Option<&str>) -> String {
        self.publish_at(version, version, sha_override)
    }

    fn publish_at(&self, version: &str, path_version: &str, sha_override: Option<&str>) -> String {
        let source = self.root.path().join(format!("src-{path_version}"));
        self.write_binaries(&source, version);
        let archive = self
            .root
            .path()
            .join(format!("dark-factory-v{path_version}.tar.gz"));
        assert!(
            Command::new("tar")
                .arg("-czf")
                .arg(&archive)
                .arg("-C")
                .arg(&source)
                .args(BINARIES)
                .status()
                .unwrap()
                .success()
        );
        let sha = match sha_override {
            Some(sha) => sha.to_owned(),
            None => {
                let output = Command::new("shasum")
                    .args(["-a", "256"])
                    .arg(&archive)
                    .output()
                    .unwrap();
                String::from_utf8(output.stdout).unwrap()[..64].to_owned()
            }
        };
        let manifest = self.root.path().join(format!("latest-{path_version}.json"));
        fs::write(
            &manifest,
            serde_json::json!({
                "version": version,
                "tag": format!("v{version}"),
                "assets": {
                    factoryctl::update::platform_key(): {
                        "url": format!("file://{}", archive.display()),
                        "sha256": sha,
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        format!("file://{}", manifest.display())
    }

    fn factoryctl_json(&self, url: &str, args: &[&str]) -> (i32, Value, String) {
        assert!(args.contains(&"--json"), "JSON helper requires --json");
        let output = self.command(url, args).output().expect("run factoryctl");
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        let json: Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|error| {
            panic!("stdout is not one JSON object ({error}): {stdout:?}\nstderr: {stderr}")
        });
        (output.status.code().unwrap_or(-1), json, stderr)
    }

    fn command(&self, url: &str, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_factoryctl"));
        command.args(args);
        command
            .env("DARK_FACTORY_HOME", self.home())
            .env("HOME", self.root.path().join("user-home"))
            .env("DARK_FACTORY_UPDATE_URL", url);
        command
    }

    fn human_command(&self, url: &str, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_factoryctl"));
        command
            .args(args)
            .env("DARK_FACTORY_HOME", self.home())
            .env("HOME", self.root.path().join("user-home"))
            .env("DARK_FACTORY_UPDATE_URL", url);
        command
    }

    #[cfg(target_os = "macos")]
    fn write_launchd_job(&self) {
        let user_home = self.root.path().join("user-home");
        let plist = user_home.join("Library/LaunchAgents/com.dark-factory.factoryd.plist");
        fs::create_dir_all(plist.parent().unwrap()).unwrap();
        fs::write(
            plist,
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                 <plist version=\"1.0\"><dict>\
                 <key>ProgramArguments</key><array><string>{}/bin/current/factoryd</string></array>\
                 <key>EnvironmentVariables</key><dict>\
                 <key>DARK_FACTORY_HOME</key><string>{}</string>\
                 </dict></dict></plist>\n",
                self.home().display(),
                self.home().display()
            ),
        )
        .unwrap();
    }

    #[cfg(target_os = "macos")]
    fn write_malformed_launchd_job(&self) {
        let user_home = self.root.path().join("user-home");
        let plist = user_home.join("Library/LaunchAgents/com.dark-factory.factoryd.plist");
        fs::create_dir_all(plist.parent().unwrap()).unwrap();
        fs::write(
            plist,
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>ProgramArguments</key><array><string>{}/bin/current/factoryd</string><string>--max-active-runs</string><string>malformed</string></array><key>EnvironmentVariables</key><dict><key>DARK_FACTORY_HOME</key><string>{}</string></dict></dict></plist>",
                self.home().display(),
                self.home().display()
            ),
        )
        .unwrap();
    }

    #[cfg(target_os = "macos")]
    fn fake_launchctl(&self, success: bool) -> (PathBuf, PathBuf) {
        let tools = self.root.path().join(if success {
            "tools-success"
        } else {
            "tools-failure"
        });
        fs::create_dir_all(&tools).unwrap();
        let log = tools.join("launchctl.log");
        let program = tools.join("launchctl");
        fs::write(
            &program,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$FAKE_LAUNCHCTL_LOG\"\nexit {}\n",
                i32::from(!success)
            ),
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        (tools, log)
    }

    #[cfg(target_os = "macos")]
    fn fake_launchctl_first_reload_fails(&self) -> (PathBuf, PathBuf) {
        let tools = self.root.path().join("tools-first-reload-fails");
        fs::create_dir_all(&tools).unwrap();
        let log = tools.join("launchctl.log");
        let count = tools.join("bootstrap-count");
        let program = tools.join("launchctl");
        fs::write(
            &program,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$FAKE_LAUNCHCTL_LOG\"\ncase \"$1\" in\n  print) echo 'pid = 4242'; exit 0 ;;\n  bootout) exit 0 ;;\n  bootstrap) n=0; test -f \"{}\" && n=$(cat \"{}\"); n=$((n + 1)); echo \"$n\" > \"{}\"; test \"$n\" -gt 6; exit $? ;;\nesac\nexit 1\n",
                count.display(),
                count.display(),
                count.display(),
            ),
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        (tools, log)
    }
}

fn read_link(path: &Path) -> String {
    fs::read_link(path).unwrap().to_string_lossy().into_owned()
}

fn serve_health_once(socket: &Path, version: &str) -> thread::JoinHandle<()> {
    let listener = UnixListener::bind(socket).unwrap();
    let version = version.to_owned();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(
            serde_json::from_str::<RequestEnvelope>(&line).unwrap(),
            RequestEnvelope::new(LocalRequest::Health)
        );
        let frame = ServerFrame::Response {
            protocol_version: PROTOCOL_VERSION,
            response: LocalResponse::Health {
                runner_path: "/tmp/factory-runner".to_owned(),
                factoryctl_path: "/tmp/factoryctl".to_owned(),
                version,
                process_id: 0,
            },
        };
        serde_json::to_writer(&mut stream, &frame).unwrap();
        stream.write_all(b"\n").unwrap();
    })
}

#[cfg(target_os = "macos")]
fn serve_managed_health_once(
    socket: &Path,
    version: &str,
    home: &Path,
    process_id: u32,
) -> thread::JoinHandle<()> {
    let listener = UnixListener::bind(socket).unwrap();
    let version = version.to_owned();
    let runner = home.join("bin/current/factory-runner");
    let factoryctl = home.join("bin/current/factoryctl");
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(
            serde_json::from_str::<RequestEnvelope>(&line).unwrap(),
            RequestEnvelope::new(LocalRequest::Health)
        );
        let frame = ServerFrame::Response {
            protocol_version: PROTOCOL_VERSION,
            response: LocalResponse::Health {
                runner_path: runner.to_string_lossy().into_owned(),
                factoryctl_path: factoryctl.to_string_lossy().into_owned(),
                version,
                process_id,
            },
        };
        serde_json::to_writer(&mut stream, &frame).unwrap();
        stream.write_all(b"\n").unwrap();
    })
}

#[cfg(target_os = "macos")]
fn prepend_path(command: &mut Command, directory: &Path) {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let paths = std::iter::once(directory.to_path_buf()).chain(std::env::split_paths(&inherited));
    command.env("PATH", std::env::join_paths(paths).unwrap());
}

#[test]
fn update_reports_a_newer_release_and_caches_the_check() {
    let fixture = Fixture::new();
    let url = fixture.publish("999.0.0", None);
    let (code, report, _) = fixture.factoryctl_json(&url, &["update", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(report["current"], factoryctl::update::CURRENT_VERSION);
    assert!(report["active"].is_null());
    assert_eq!(report["latest"], "999.0.0");
    assert_eq!(report["update_available"], true);
    assert!(
        report["asset"]["url"]
            .as_str()
            .unwrap()
            .starts_with("file://")
    );
    let cache = fixture.home().join("update-check.json");
    let cached: factoryctl::update::UpdateCheck =
        serde_json::from_slice(&fs::read(&cache).unwrap()).unwrap();
    assert_eq!(
        cached.latest.as_ref().map(|m| m.version.as_str()),
        Some("999.0.0")
    );
    assert!(cached.error.is_none());
}

#[test]
fn update_is_human_readable_by_default_and_names_both_versions() {
    let fixture = Fixture::new();
    fixture.activate("0.1.0");
    let url = fixture.publish("999.0.0", None);
    let output = fixture.human_command(&url, &["update"]).output().unwrap();
    assert!(
        output.status.success(),
        "stderr={} stdout={}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("invoking factoryctl: "));
    assert!(stdout.contains("active runtime (bin/current): 0.1.0\n"));
    assert!(stdout.contains("latest release: 999.0.0\n"));
    assert!(stdout.contains("update --install: available\n"));
    assert!(!stdout.trim_start().starts_with('{'));

    let json_output = fixture
        .human_command(&url, &["update", "--json"])
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&json_output.stdout).unwrap();
    assert_eq!(report["current"], factoryctl::update::CURRENT_VERSION);
    assert_eq!(report["active"], "0.1.0");
}

#[test]
fn human_update_output_sanitizes_manifest_text_but_json_keeps_it_exact() {
    let fixture = Fixture::new();
    let hostile = "999.0.0-rc\u{1b}[2J\nupdate --install: forged";
    let url = fixture.publish_at(hostile, "hostile", None);
    let output = fixture.human_command(&url, &["update"]).output().unwrap();
    assert!(
        output.status.success(),
        "stderr={} stdout={}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!output.stdout.contains(&0x1b_u8));
    assert_eq!(
        output.stdout.iter().filter(|&&byte| byte == b'\n').count(),
        4
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("latest release: 999.0.0-rc?[2J?update --install: forged\n"));

    let (_, report, _) = fixture.factoryctl_json(&url, &["update", "--json"]);
    assert_eq!(report["latest"], hostile);
}

#[test]
fn matching_active_release_reports_no_install_work_human_and_json() {
    let fixture = Fixture::new();
    let version = factoryctl::update::CURRENT_VERSION;
    fixture.activate(version);
    let url = fixture.publish(version, None);

    let output = fixture.human_command(&url, &["update"]).output().unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("update --install: not needed\n")
    );

    let (_, report, _) = fixture.factoryctl_json(&url, &["update", "--json"]);
    assert_eq!(report["update_available"], false);

    let socket = fixture.home().join("factory.sock");
    let server = serve_health_once(&socket, version);
    let output = fixture
        .human_command(
            &url,
            &["--socket", socket.to_str().unwrap(), "update", "--install"],
        )
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("update --install: already up to date (daemon healthy)\n"),
        "{stdout}"
    );
}

#[test]
fn human_install_branches_report_their_actual_work() {
    let fixture = Fixture::new();
    fixture.activate("0.1.0");
    let url = fixture.publish("999.0.0", None);
    let output = fixture
        .human_command(&url, &["update", "--install"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("update --install: installed 999.0.0 (restart the daemon yourself)\n")
    );

    let fixture = Fixture::new();
    fixture.activate("999.0.0");
    let url = fixture.publish(factoryctl::update::CURRENT_VERSION, None);
    let output = fixture
        .human_command(&url, &["update", "--install"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("update --install: not needed\n"),
        "{stdout}"
    );
}

#[test]
fn homebrew_bootstrap_updates_an_older_active_runtime() {
    let fixture = Fixture::new();
    fixture.activate("0.1.0");
    let latest = factoryctl::update::CURRENT_VERSION;
    let url = fixture.publish(latest, None);

    let (code, report, stderr) = fixture.factoryctl_json(&url, &["update", "--json"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(report["current"], latest);
    assert_eq!(report["active"], "0.1.0");
    assert_eq!(report["latest"], latest);
    assert_eq!(report["update_available"], true);

    let (code, report, stderr) = fixture.factoryctl_json(&url, &["update", "--install", "--json"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(report["installed"], latest);
    assert_eq!(report["launchd"], "not_installed");
    assert_eq!(read_link(&fixture.home().join("bin/current")), latest);
}

#[cfg(target_os = "macos")]
#[test]
fn update_malformed_existing_capacity_restores_runtime_and_reports_unchanged_job() {
    let fixture = Fixture::new();
    fixture.activate("0.1.0");
    fixture.write_malformed_launchd_job();
    let plist = fixture
        .root
        .path()
        .join("user-home/Library/LaunchAgents/com.dark-factory.factoryd.plist");
    let old_plist = fs::read_to_string(&plist).unwrap();
    let url = fixture.publish(factoryctl::update::CURRENT_VERSION, None);
    let (tools, log) = fixture.fake_launchctl_first_reload_fails();
    let socket = fixture.home().join("f.sock");
    let server = serve_managed_health_once(&socket, "0.1.0", &fixture.home(), 4242);

    let mut command = fixture.command(&url, &["update", "--install", "--json"]);
    prepend_path(&mut command, &tools);
    let output = command
        .env("FAKE_LAUNCHCTL_LOG", &log)
        .env("DARK_FACTORY_SOCKET", &socket)
        .output()
        .unwrap();
    server.join().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("bin/current rolled back to 0.1.0"),
        "{stderr}"
    );
    assert!(
        stderr.contains("launchd plist and job were unchanged"),
        "{stderr}"
    );
    assert!(stderr.contains("restored runtime is healthy"), "{stderr}");
    assert_eq!(read_link(&fixture.home().join("bin/current")), "0.1.0");
    assert_eq!(fs::read_to_string(plist).unwrap(), old_plist);
    assert!(!fs::read_to_string(log).unwrap().contains("bootstrap"));
}

#[test]
fn production_update_revalidates_a_candidate_after_a_competing_newer_activation() {
    let fixture = Fixture::new();
    fixture.activate("0.1.0");
    let file_manifest_url = fixture.publish("0.2.0", None);
    let manifest_path = PathBuf::from(file_manifest_url.strip_prefix("file://").unwrap());
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    let archive_url = manifest["assets"][factoryctl::update::platform_key()]["url"]
        .as_str()
        .unwrap()
        .to_owned();
    let archive_path = PathBuf::from(archive_url.strip_prefix("file://").unwrap());
    let archive = fs::read(archive_path).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/manifest", listener.local_addr().unwrap());
    let archive_url = format!("http://{}/archive", listener.local_addr().unwrap());
    manifest["assets"][factoryctl::update::platform_key()]["url"] = Value::String(archive_url);
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let (requested_tx, requested_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        for request_number in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            let body = if request_number == 0 {
                manifest_bytes.clone()
            } else {
                requested_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                archive.clone()
            };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
        }
    });

    let mut command = fixture.command(&url, &["update", "--install", "--json"]);
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    requested_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("production update reached its archive download");
    fixture.write_binaries(&fixture.home().join("bin/0.3.0"), "0.3.0");
    let (lock, snapshot) = factoryctl::runtime::MutationLock::begin(
        &fixture.home(),
        &fixture
            .root
            .path()
            .join("user-home/Library/LaunchAgents/com.dark-factory.factoryd.plist"),
    )
    .unwrap();
    assert_eq!(snapshot.active_version.as_deref(), Some("0.1.0"));
    factoryctl::install::activate(&fixture.home(), "0.3.0").unwrap();
    drop(lock);
    release_tx.send(()).unwrap();
    let output = child.wait_with_output().unwrap();
    server.join().unwrap();
    let stdout: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout["installed"], false);
    assert_eq!(stdout["active"], "0.3.0");
    assert_eq!(read_link(&fixture.home().join("bin/current")), "0.3.0");
}

#[test]
fn matching_active_runtime_and_daemon_are_a_no_op() {
    let fixture = Fixture::new();
    let latest = factoryctl::update::CURRENT_VERSION;
    fixture.activate(latest);
    let url = fixture.publish(latest, None);
    let socket = fixture.home().join("f.sock");
    let server = serve_health_once(&socket, latest);

    let mut command = fixture.command(&url, &["update", "--install", "--json"]);
    let output = command
        .env("DARK_FACTORY_SOCKET", &socket)
        .output()
        .unwrap();
    server.join().unwrap();
    let stdout: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout["installed"], latest);
    assert_eq!(stdout["launchd"], "unchanged");
    assert_eq!(stdout["health"]["version"], latest);
    assert!(String::from_utf8_lossy(&output.stderr).contains("already installed and running"));
    assert_eq!(read_link(&fixture.home().join("bin/current")), latest);
}

#[cfg(target_os = "macos")]
#[test]
fn launchd_reload_failure_rolls_back_the_active_runtime() {
    let fixture = Fixture::new();
    fixture.activate("0.1.0");
    fixture.write_launchd_job();
    let latest = factoryctl::update::CURRENT_VERSION;
    let url = fixture.publish(latest, None);
    let (tools, log) = fixture.fake_launchctl(false);

    let mut command = fixture.command(&url, &["update", "--install", "--json"]);
    prepend_path(&mut command, &tools);
    let output = command.env("FAKE_LAUNCHCTL_LOG", &log).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("bin/current rolled back to 0.1.0"),
        "{stderr}"
    );
    assert!(stderr.contains("launchd recovery failed"), "{stderr}");
    assert_eq!(read_link(&fixture.home().join("bin/current")), "0.1.0");
    assert!(fixture.home().join("bin").join(latest).is_dir());
    let calls = fs::read_to_string(log).unwrap();
    assert!(calls.contains("bootout"));
    assert!(calls.contains("bootstrap"));
}

#[cfg(target_os = "macos")]
#[test]
fn update_failed_activation_restores_runtime_before_old_job_and_checks_managed_health() {
    let fixture = Fixture::new();
    fixture.activate("0.1.0");
    fixture.write_launchd_job();
    let plist = fixture
        .root
        .path()
        .join("user-home/Library/LaunchAgents/com.dark-factory.factoryd.plist");
    let old_plist = fs::read_to_string(&plist).unwrap();
    let latest = factoryctl::update::CURRENT_VERSION;
    let url = fixture.publish(latest, None);
    let (tools, log) = fixture.fake_launchctl_first_reload_fails();
    let socket = fixture.home().join("f.sock");
    let server = serve_managed_health_once(&socket, "0.1.0", &fixture.home(), 4242);

    let mut command = fixture.command(&url, &["update", "--install", "--json"]);
    prepend_path(&mut command, &tools);
    let output = command
        .env("FAKE_LAUNCHCTL_LOG", &log)
        .env("DARK_FACTORY_SOCKET", &socket)
        .output()
        .unwrap();
    server.join().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("restored runtime is healthy"), "{stderr}");
    assert_eq!(read_link(&fixture.home().join("bin/current")), "0.1.0");
    assert_eq!(fs::read_to_string(plist).unwrap(), old_plist);
    let calls = fs::read_to_string(log).unwrap();
    assert_eq!(calls.matches("bootstrap").count(), 7);
    assert_eq!(
        fs::read_to_string(
            fixture
                .root
                .path()
                .join("tools-first-reload-fails/bootstrap-count")
        )
        .unwrap()
        .trim(),
        "7"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn launchd_reload_restarts_into_the_new_active_runtime() {
    let fixture = Fixture::new();
    fixture.activate("0.1.0");
    fixture.write_launchd_job();
    let latest = factoryctl::update::CURRENT_VERSION;
    let url = fixture.publish(latest, None);
    let (tools, log) = fixture.fake_launchctl(true);
    let socket = fixture.home().join("f.sock");
    let server = serve_health_once(&socket, latest);

    let mut command = fixture.command(&url, &["update", "--install", "--json"]);
    prepend_path(&mut command, &tools);
    let output = command
        .env("FAKE_LAUNCHCTL_LOG", &log)
        .env("DARK_FACTORY_SOCKET", &socket)
        .output()
        .unwrap();
    server.join().unwrap();
    let stdout: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout["installed"], latest);
    assert_eq!(stdout["launchd"], "reloaded");
    assert_eq!(stdout["health"]["version"], latest);
    assert_eq!(read_link(&fixture.home().join("bin/current")), latest);
    let calls = fs::read_to_string(log).unwrap();
    assert!(calls.contains("bootout"));
    assert!(calls.contains("bootstrap"));
}

#[test]
fn update_with_nothing_newer_is_a_no_op_for_install_too() {
    let fixture = Fixture::new();
    let url = fixture.publish("0.0.1", None);
    let (code, report, _) = fixture.factoryctl_json(&url, &["update", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(report["update_available"], false);
    let (code, report, _) = fixture.factoryctl_json(&url, &["update", "--install", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(report["installed"], false);
    assert!(!fixture.home().join("bin").exists());
}

#[test]
fn update_never_downgrades_a_newer_active_runtime() {
    let fixture = Fixture::new();
    fixture.activate("999.0.0");
    let url = fixture.publish(factoryctl::update::CURRENT_VERSION, None);
    let (code, report, stderr) = fixture.factoryctl_json(&url, &["update", "--install", "--json"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(report["active"], "999.0.0");
    assert_eq!(report["update_available"], false);
    assert_eq!(report["installed"], false);
    assert_eq!(read_link(&fixture.home().join("bin/current")), "999.0.0");
}

#[test]
fn update_reports_an_unreachable_manifest_as_an_error() {
    let fixture = Fixture::new();
    let (code, report, _) =
        fixture.factoryctl_json("http://127.0.0.1:9/never", &["update", "--json"]);
    assert_eq!(code, 1);
    assert!(report["error"].as_str().unwrap().contains("failed"));
    assert_eq!(report["update_available"], false);
}

#[test]
fn install_verifies_unpacks_and_activates_then_reports_no_launchd_job() {
    let fixture = Fixture::new();
    let bad = fixture.publish("999.0.0", Some("00"));
    let output = Command::new(env!("CARGO_BIN_EXE_factoryctl"))
        .args(["update", "--install"])
        .env("DARK_FACTORY_HOME", fixture.home())
        .env("HOME", fixture.root.path().join("user-home"))
        .env("DARK_FACTORY_UPDATE_URL", &bad)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("checksum mismatch"));
    assert!(!fixture.home().join("bin/999.0.0").exists());

    let url = fixture.publish("999.0.0", None);
    let (code, report, stderr) = fixture.factoryctl_json(&url, &["update", "--install", "--json"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(report["installed"], "999.0.0");
    assert_eq!(report["launchd"], "not_installed");
    assert!(
        report.get("health").is_none(),
        "no daemon was restarted, so no health claim: {report}"
    );
    let bin = fixture.home().join("bin");
    for name in BINARIES {
        let installed = bin.join("999.0.0").join(name);
        assert!(installed.is_file(), "{}", installed.display());
        assert_ne!(
            fs::metadata(&installed).unwrap().permissions().mode() & 0o111,
            0
        );
    }
    assert_eq!(read_link(&bin.join("current")), "999.0.0");
    assert!(!bin.join(".staging-999.0.0").exists());
    assert_eq!(
        fs::metadata(&bin).unwrap().permissions().mode() & 0o777,
        0o700
    );
    // No launchd job was ever written for this HOME.
    assert!(!fixture.root.path().join("user-home/Library").exists());

    // Running it again with the same release already on disk just re-activates.
    let (code, report, _) = fixture.factoryctl_json(&url, &["update", "--install", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(report["installed"], "999.0.0");
    assert_eq!(read_link(&bin.join("current")), "999.0.0");
}
