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
    let words = command.split_whitespace().collect::<Vec<_>>();
    let simple_git = words.first().is_some_and(|word| {
        Path::new(word.trim_matches(|c| matches!(c, '\'' | '"')))
            .file_name()
            .is_some_and(|name| name == "git")
    });
    let chained_git = contains_shell_control(command) && words.iter().any(|word| *word == "git");
    if !simple_git && !chained_git {
        return false;
    }
    words.windows(2).any(|w| w == ["reset", "--hard"])
        || (words.contains(&"push") && words.iter().any(|arg| force_push_option(arg)))
        || words
            .windows(2)
            .any(|w| w[0] == "branch" && matches!(w[1], "-D" | "-d" | "--delete"))
}

fn force_push_option(argument: &str) -> bool {
    argument.starts_with("--force") || (argument.starts_with("-f") && !argument.starts_with("--"))
}

fn contains_shell_control(command: &str) -> bool {
    command
        .chars()
        .any(|character| matches!(character, ';' | '&' | '|' | '\n' | '\r'))
}

fn recursive_force_delete_outside(command: &str, worktree: &Path) -> bool {
    let words = command.split_whitespace().collect::<Vec<_>>();
    let Some(rm) = words.iter().position(|word| *word == "rm") else {
        return false;
    };
    let args = &words[rm + 1..];
    let recursive = args
        .iter()
        .any(|arg| arg.starts_with('-') && (arg.contains('r') || arg.contains('R')));
    let force = args
        .iter()
        .any(|arg| arg.starts_with('-') && arg.contains('f'));
    if !recursive || !force {
        return false;
    }
    // We intentionally recognize only one simple `rm` invocation. Shell
    // control flow, a changed cwd, quoting, expansion, and indirection are
    // ambiguous at this boundary and therefore denied, not interpreted.
    if rm != 0
        || contains_shell_control(command)
        || command.chars().any(|character| {
            matches!(
                character,
                '\'' | '"' | '\\' | '$' | '`' | '*' | '?' | '[' | ']' | '{' | '}' | '<' | '>'
            )
        })
    {
        return true;
    }
    let targets = args
        .iter()
        .filter(|arg| !arg.starts_with('-'))
        .collect::<Vec<_>>();
    targets.is_empty()
        || !targets.iter().all(|target| {
            let path = Path::new(target);
            if path.is_absolute() {
                path.starts_with(worktree)
                    && path.components().all(|component| {
                        matches!(
                            component,
                            std::path::Component::RootDir | std::path::Component::Normal(_)
                        )
                    })
            } else {
                !target.starts_with('~')
                    && !path.components().any(|part| {
                        matches!(
                            part,
                            std::path::Component::ParentDir | std::path::Component::Prefix(_)
                        )
                    })
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
        for command in [
            "git push --force-with-lease origin main",
            "git push --force-with-lease=main origin main",
            "git push --force-if-includes origin main",
            "git push -fHEAD origin main",
            "/usr/bin/git push --force origin main",
            "cd repo && git push --force origin main",
        ] {
            assert_eq!(
                decide(&bash(command), root).denied_by,
                Some("destructive_git"),
                "{command}"
            );
        }
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
            decide(&bash("git push origin feature"), root).denied_by,
            None
        );
        assert_eq!(decide(&bash("echo git push --force"), root).denied_by, None);
        assert_eq!(
            decide(&bash("rm -rf /tmp/worktree/target"), root).denied_by,
            None
        );
        assert_eq!(decide(&bash("rm -rf target"), root).denied_by, None);
        assert_eq!(
            decide(&bash("rm -rf ../other"), root).denied_by,
            Some("recursive_delete_outside_worktree")
        );
        for command in [
            "rm -rf /tmp/worktree/../other",
            "cd /tmp && rm -rf other",
            "rm -rf target && echo done",
            "rm -rf $TARGET",
        ] {
            assert_eq!(
                decide(&bash(command), root).denied_by,
                Some("recursive_delete_outside_worktree"),
                "{command}"
            );
        }
    }
}
