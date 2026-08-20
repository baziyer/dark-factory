use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

fn workspace_target_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_factoryd"))
        .parent()
        .expect("factoryd binary has a parent directory")
        .to_path_buf()
}

fn required_environment(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("focused test environment is missing {name}"))
}

fn file_identity(metadata: &std::fs::Metadata) -> String {
    format!(
        "{}:{}:{}:{}",
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime()
    )
}

fn digest(path: &Path) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(std::fs::read(path).expect("focused binary contents"));
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn validate_binary(path_variable: &str, identity_variable: &str, digest_variable: &str) -> PathBuf {
    let path = PathBuf::from(required_environment(path_variable));
    let capture_dir = PathBuf::from(required_environment("DARK_FACTORY_TEST_CAPTURE_DIR"));
    assert!(path.is_absolute(), "{path_variable} must be absolute");
    assert_eq!(
        path.parent(),
        Some(capture_dir.as_path()),
        "{path_variable} must be an exact private capture sibling"
    );

    let metadata = std::fs::symlink_metadata(&path)
        .unwrap_or_else(|error| panic!("{path_variable} is unavailable: {error}"));
    assert!(
        metadata.file_type().is_file(),
        "{path_variable} must be a regular non-symlink file"
    );
    assert_ne!(
        metadata.permissions().mode() & 0o111,
        0,
        "{path_variable} must be executable"
    );
    assert_eq!(
        file_identity(&metadata),
        required_environment(identity_variable),
        "{path_variable} identity changed before the consumer boundary"
    );
    assert_eq!(
        digest(&path),
        required_environment(digest_variable),
        "{path_variable} digest changed before the consumer boundary"
    );
    path
}

fn prepared_binary(name: &str) -> PathBuf {
    let (path_variable, identity_variable, digest_variable) = match name {
        "factory-runner" => (
            "DARK_FACTORY_TEST_FACTORY_RUNNER",
            "DARK_FACTORY_TEST_FACTORY_RUNNER_IDENTITY",
            "DARK_FACTORY_TEST_FACTORY_RUNNER_DIGEST",
        ),
        "factoryctl" => (
            "DARK_FACTORY_TEST_FACTORYCTL",
            "DARK_FACTORY_TEST_FACTORYCTL_IDENTITY",
            "DARK_FACTORY_TEST_FACTORYCTL_DIGEST",
        ),
        _ => panic!("unknown focused binary: {name}"),
    };
    let target_dir = workspace_target_dir();
    assert_eq!(
        PathBuf::from(required_environment("DARK_FACTORY_TEST_TARGET_DIR")),
        target_dir,
        "focused test target differs from the Cargo test target"
    );
    let worktree = PathBuf::from(required_environment("DARK_FACTORY_TEST_WORKTREE"));
    let expected_worktree = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("focused test worktree");
    assert_eq!(worktree, expected_worktree, "focused test worktree changed");
    validate_binary(path_variable, identity_variable, digest_variable)
}

pub fn factory_runner_path() -> PathBuf {
    prepared_binary("factory-runner")
}

pub fn factoryctl_path() -> PathBuf {
    prepared_binary("factoryctl")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focused_binary_paths_are_private_copies_of_the_real_target_build() {
        let target_dir = workspace_target_dir();
        let capture_dir = PathBuf::from(required_environment("DARK_FACTORY_TEST_CAPTURE_DIR"));
        assert!(target_dir.is_absolute());
        assert!(target_dir.is_dir());
        assert!(capture_dir.is_absolute());
        assert_ne!(target_dir, capture_dir);
        for name in ["factory-runner", "factoryctl"] {
            let path = match name {
                "factory-runner" => factory_runner_path(),
                "factoryctl" => factoryctl_path(),
                _ => unreachable!(),
            };
            assert_eq!(path.parent(), Some(capture_dir.as_path()));
        }
    }

    #[test]
    fn symlink_replacement_is_rejected_at_the_consumer_boundary() {
        let path = PathBuf::from(required_environment("DARK_FACTORY_TEST_FACTORY_RUNNER"));
        let backup = path.with_extension("original");
        std::fs::rename(&path, &backup).unwrap();
        std::os::unix::fs::symlink("factoryctl", &path).unwrap();
        let result = std::panic::catch_unwind(|| {
            validate_binary(
                "DARK_FACTORY_TEST_FACTORY_RUNNER",
                "DARK_FACTORY_TEST_FACTORY_RUNNER_IDENTITY",
                "DARK_FACTORY_TEST_FACTORY_RUNNER_DIGEST",
            )
        });
        std::fs::remove_file(&path).unwrap();
        std::fs::rename(backup, &path).unwrap();
        assert!(
            result.is_err(),
            "a symlink replacement reached the consumer"
        );
    }

    #[test]
    fn same_path_replacement_is_rejected_at_the_consumer_boundary() {
        let path = PathBuf::from(required_environment("DARK_FACTORY_TEST_FACTORYCTL"));
        let backup = path.with_extension("original");
        std::fs::rename(&path, &backup).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        let result = std::panic::catch_unwind(|| {
            validate_binary(
                "DARK_FACTORY_TEST_FACTORYCTL",
                "DARK_FACTORY_TEST_FACTORYCTL_IDENTITY",
                "DARK_FACTORY_TEST_FACTORYCTL_DIGEST",
            )
        });
        std::fs::remove_file(&path).unwrap();
        std::fs::rename(backup, &path).unwrap();
        assert!(
            result.is_err(),
            "a same-path replacement reached the consumer"
        );
    }
}
