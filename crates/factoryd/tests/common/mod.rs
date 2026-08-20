#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

fn workspace_target_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_factoryd"))
        .parent()
        .expect("factoryd binary has a parent directory")
        .to_path_buf()
}

fn prepared_binary(name: &str) -> PathBuf {
    let target_dir = workspace_target_dir();
    let path = target_dir.join(name);
    assert!(
        path.is_file(),
        "prepared {name} binary is missing at {}; run scripts/prepare-test-binaries.sh under the local-ci lease",
        path.display()
    );
    #[cfg(unix)]
    assert_ne!(
        std::fs::metadata(&path)
            .expect("prepared binary metadata")
            .permissions()
            .mode()
            & 0o111,
        0,
        "prepared {name} binary is not executable"
    );
    assert_eq!(
        path.parent(),
        Some(target_dir.as_path()),
        "{name} must come from the exact target directory containing factoryd"
    );
    path
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
    fn sibling_binary_provenance_is_the_factoryd_target_directory() {
        let target_dir = workspace_target_dir();
        for name in ["factory-runner", "factoryctl"] {
            let path = target_dir.join(name);
            assert_eq!(
                path.parent(),
                Some(target_dir.as_path()),
                "{name} must be a sibling of factoryd, not a PATH lookup"
            );
        }
    }

    #[test]
    fn prepared_siblings_are_executable() {
        let _ = factory_runner_path();
        let _ = factoryctl_path();
    }

    #[test]
    fn target_directory_is_an_absolute_directory() {
        let target_dir = workspace_target_dir();
        assert!(target_dir.is_absolute());
        assert!(target_dir.is_dir());
    }
}
