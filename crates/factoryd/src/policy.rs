//! Small, explicit `PreToolUse` deny policy. This is a tripwire, not an OS
//! sandbox: providers run as the operator and can encode commands in ways a
//! JSON hook cannot safely interpret. The exact boundary lives in SECURITY.md.

use std::path::Path;

use serde_json::Value;

#[derive(Debug, Eq, PartialEq)]
pub struct Decision {
    pub tool_name: String,
    pub denied_by: Option<&'static str>,
}

pub fn decide(payload: &Value, worktree: &Path) -> Decision {
    let tool_name = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let input = payload.get("tool_input").unwrap_or(&Value::Null);
    let command = input.get("command").and_then(Value::as_str).unwrap_or("");
    let paths = ["file_path", "path", "notebook_path"]
        .into_iter()
        .filter_map(|key| input.get(key).and_then(Value::as_str));

    let denied_by = if destructive_git(command) {
        Some("destructive_git")
    } else if recursive_force_delete_outside(command, worktree) {
        Some("recursive_delete_outside_worktree")
    } else if paths.chain(command_paths(command)).any(secret_path) {
        Some("secret_path")
    } else {
        None
    };
    Decision {
        tool_name: tool_name.to_owned(),
        denied_by,
    }
}

fn destructive_git(command: &str) -> bool {
    let normalized = command.split_whitespace().collect::<Vec<_>>();
    normalized.contains(&"git")
        && (normalized.windows(2).any(|w| w == ["reset", "--hard"])
            || (normalized.contains(&"push")
                && normalized
                    .iter()
                    .any(|arg| matches!(*arg, "--force" | "-f") || arg.starts_with("--force=")))
            || normalized
                .windows(2)
                .any(|w| w[0] == "branch" && matches!(w[1], "-D" | "-d" | "--delete")))
}

fn recursive_force_delete_outside(command: &str, worktree: &Path) -> bool {
    let words = command.split_whitespace().collect::<Vec<_>>();
    let Some(rm) = words.iter().position(|word| *word == "rm") else {
        return false;
    };
    let args = &words[rm + 1..];
    let recursive = args
        .iter()
        .any(|arg| arg.starts_with('-') && arg.contains('r'));
    let force = args
        .iter()
        .any(|arg| arg.starts_with('-') && arg.contains('f'));
    if !recursive || !force {
        return false;
    }
    let targets = args
        .iter()
        .filter(|arg| !arg.starts_with('-'))
        .collect::<Vec<_>>();
    targets.is_empty()
        || !targets.iter().all(|target| {
            let target = target.trim_matches(|c| matches!(c, '\'' | '"'));
            let path = Path::new(target);
            if path.is_absolute() {
                path.starts_with(worktree)
            } else {
                !target.starts_with('~')
                    && !target.contains('$')
                    && !path
                        .components()
                        .any(|part| part == std::path::Component::ParentDir)
            }
        })
}

fn command_paths(command: &str) -> impl Iterator<Item = &str> {
    command
        .split_whitespace()
        .filter(|word| word.contains('/') || word.starts_with('.'))
}

fn secret_path(path: &str) -> bool {
    let path = path.trim_matches(|c| matches!(c, '\'' | '"' | ',' | ';'));
    path.split('/')
        .any(|part| matches!(part, ".ssh" | ".aws" | ".gnupg" | ".env"))
        || path.ends_with("/auth.json")
        || path.ends_with("/credentials")
        || path.contains("/.config/gcloud/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bash(command: &str) -> Value {
        json!({"tool_name":"Bash","tool_input":{"command":command}})
    }

    #[test]
    fn denies_explicit_dangerous_commands_and_secret_paths() {
        let root = Path::new("/tmp/worktree");
        assert_eq!(
            decide(&bash("git push --force origin main"), root).denied_by,
            Some("destructive_git")
        );
        assert_eq!(
            decide(&bash("rm -rf /tmp/other"), root).denied_by,
            Some("recursive_delete_outside_worktree")
        );
        assert_eq!(
            decide(
                &json!({"tool_name":"Read","tool_input":{"file_path":"/Users/me/.ssh/id_ed25519"}}),
                root
            )
            .denied_by,
            Some("secret_path")
        );
    }

    #[test]
    fn permits_reversible_worktree_operations() {
        let root = Path::new("/tmp/worktree");
        assert_eq!(decide(&bash("git status --short"), root).denied_by, None);
        assert_eq!(
            decide(&bash("rm -rf /tmp/worktree/target"), root).denied_by,
            None
        );
        assert_eq!(decide(&bash("rm -rf target"), root).denied_by, None);
        assert_eq!(
            decide(&bash("rm -rf ../other"), root).denied_by,
            Some("recursive_delete_outside_worktree")
        );
    }
}
