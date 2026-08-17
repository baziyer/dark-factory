//! `factoryctl update` / `update --install` end to end against a local
//! `file://` manifest and archive — the exact shapes
//! `scripts/package-release.sh` produces — with `HOME` pointed at an empty
//! directory so no launchd job is found (the daemon-restart step is the
//! one part that can't run in a test).

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
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

    /// Writes an archive of four fake executables and a manifest naming
    /// `version`, returns the manifest's `file://` URL.
    fn publish(&self, version: &str, sha_override: Option<&str>) -> String {
        let source = self.root.path().join(format!("src-{version}"));
        fs::create_dir_all(&source).unwrap();
        for name in BINARIES {
            let path = source.join(name);
            fs::write(&path, format!("#!/bin/sh\necho {name} {version}\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let archive = self
            .root
            .path()
            .join(format!("dark-factory-v{version}.tar.gz"));
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
        let manifest = self.root.path().join(format!("latest-{version}.json"));
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

    fn factoryctl(&self, url: &str, args: &[&str]) -> (i32, Value, String) {
        let output = Command::new(env!("CARGO_BIN_EXE_factoryctl"))
            .args(args)
            .env("DARK_FACTORY_HOME", self.home())
            .env("HOME", self.root.path().join("user-home"))
            .env("DARK_FACTORY_UPDATE_URL", url)
            .output()
            .expect("run factoryctl");
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        let json: Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|error| {
            panic!("stdout is not one JSON object ({error}): {stdout:?}\nstderr: {stderr}")
        });
        (output.status.code().unwrap_or(-1), json, stderr)
    }
}

fn read_link(path: &Path) -> String {
    fs::read_link(path).unwrap().to_string_lossy().into_owned()
}

#[test]
fn update_reports_a_newer_release_and_caches_the_check() {
    let fixture = Fixture::new();
    let url = fixture.publish("999.0.0", None);
    let (code, report, _) = fixture.factoryctl(&url, &["update"]);
    assert_eq!(code, 0);
    assert_eq!(report["current"], factoryctl::update::CURRENT_VERSION);
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
fn update_with_nothing_newer_is_a_no_op_for_install_too() {
    let fixture = Fixture::new();
    let url = fixture.publish("0.0.1", None);
    let (code, report, _) = fixture.factoryctl(&url, &["update"]);
    assert_eq!(code, 0);
    assert_eq!(report["update_available"], false);
    let (code, report, _) = fixture.factoryctl(&url, &["update", "--install"]);
    assert_eq!(code, 0);
    assert_eq!(report["installed"], false);
    assert!(!fixture.home().join("bin").exists());
}

#[test]
fn update_reports_an_unreachable_manifest_as_an_error() {
    let fixture = Fixture::new();
    let (code, report, _) = fixture.factoryctl("http://127.0.0.1:9/never", &["update"]);
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
    let (code, report, stderr) = fixture.factoryctl(&url, &["update", "--install"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(report["installed"], "999.0.0");
    assert_eq!(report["launchd"], "not_installed");
    assert_eq!(report["health"]["ok"], false);
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
    let (code, report, _) = fixture.factoryctl(&url, &["update", "--install"]);
    assert_eq!(code, 0);
    assert_eq!(report["installed"], "999.0.0");
    assert_eq!(read_link(&bin.join("current")), "999.0.0");
}
