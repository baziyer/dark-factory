//! Small, explicit `PreToolUse` deny policy. This is a tripwire, not an OS
//! sandbox: providers run as the operator and can encode commands in ways a
//! JSON hook cannot safely interpret. The exact boundary lives in SECURITY.md.
//!
//! GitHub publication is deliberately not one of this policy's decisions.
//! `scripts/github-comment.sh` is a safe transport convention, not a source
//! of authority; credentials and daemon capabilities belong to the shared
//! authenticated API boundary.

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
    let mut paths = ["file_path", "path", "notebook_path"]
        .into_iter()
        .filter_map(|key| input.get(key).and_then(Value::as_str));
    let commands = if matches!(tool_name, "Bash" | "Shell" | "shell") {
        shell_commands(command)
    } else {
        Ok(Vec::new())
    };

    let denied_by = if commands.is_err() {
        Some("unsupported_shell_syntax")
    } else if changes_capacity(commands.as_deref().unwrap_or_default()) {
        Some("capacity_operator_only")
    } else if changes_repository_authority(commands.as_deref().unwrap_or_default()) {
        Some("repository_authority_operator_only")
    } else if destructive_git(commands.as_deref().unwrap_or_default()) {
        Some("destructive_git")
    } else if recursive_force_delete_outside(commands.as_deref().unwrap_or_default(), worktree) {
        Some("recursive_delete_outside_worktree")
    } else if paths.any(secret_path)
        || shell_accesses_secret(commands.as_deref().unwrap_or_default())
    {
        Some("secret_path")
    } else {
        None
    };
    Decision {
        tool_name: tool_name.to_owned(),
        denied_by,
    }
}

fn changes_capacity(commands: &[Vec<ShellWord>]) -> bool {
    commands.iter().any(|words| {
        let Some((program, args)) = resolve_command(words) else {
            return false;
        };
        program == "factoryctl" && args.windows(2).any(|words| words == ["capacity", "set"])
    })
}

fn changes_repository_authority(commands: &[Vec<ShellWord>]) -> bool {
    commands.iter().any(|words| {
        let Some((program, args)) = resolve_command(words) else {
            return false;
        };
        program == "factoryctl"
            && args
                .windows(3)
                .any(|words| words == ["project", "repository", "set"])
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ShellWord {
    Arg(String),
    Redirect(String),
}

fn destructive_git(commands: &[Vec<ShellWord>]) -> bool {
    commands.iter().any(|words| {
        let Some((program, args)) = resolve_command(words) else {
            return false;
        };
        program == "git"
            && (args.windows(2).any(|w| w == ["reset", "--hard"])
                || destructive_push(&args)
                || args
                    .windows(2)
                    .any(|w| w[0] == "branch" && matches!(w[1].as_str(), "-D" | "-d" | "--delete")))
    })
}

fn destructive_push(args: &[String]) -> bool {
    let Some(push_index) = args.iter().position(|word| word == "push") else {
        return false;
    };
    args[push_index + 1..].iter().any(|arg| {
        force_push_option(arg)
            || delete_push_option(arg)
            || (!arg.starts_with('-') && (arg.starts_with('+') || arg.starts_with(':')))
    })
}

fn force_push_option(argument: &str) -> bool {
    argument.starts_with("--force") || (argument.starts_with("-f") && !argument.starts_with("--"))
}

fn delete_push_option(argument: &str) -> bool {
    argument == "--delete"
        || argument.starts_with("--delete=")
        || (argument.starts_with("-d") && !argument.starts_with("--"))
}

fn recursive_force_delete_outside(commands: &[Vec<ShellWord>], worktree: &Path) -> bool {
    commands.iter().enumerate().any(|(index, words)| {
        resolve_command(words).is_some_and(|(program, args)| {
            program == "rm" && unsafe_recursive_delete(index, &args, commands.len(), worktree)
        })
    })
}

fn unsafe_recursive_delete(
    index: usize,
    args: &[String],
    command_count: usize,
    worktree: &Path,
) -> bool {
    let recursive = args
        .iter()
        .any(|arg| arg.starts_with('-') && (arg.contains('r') || arg.contains('R')));
    let force = args
        .iter()
        .any(|arg| arg.starts_with('-') && arg.contains('f'));
    if !recursive || !force {
        return false;
    }
    // A preceding command may have changed cwd or other shell state. Do not
    // guess where a relative destructive target resolves.
    if index > 0 || command_count > 1 {
        return true;
    }
    let targets = args
        .iter()
        .filter(|arg| !arg.starts_with('-'))
        .collect::<Vec<_>>();
    targets.is_empty()
        || !targets.iter().all(|target| {
            if target.chars().any(|character| {
                matches!(
                    character,
                    '$' | '`' | '\\' | '*' | '?' | '[' | ']' | '{' | '}'
                )
            }) {
                return false;
            }
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

fn shell_accesses_secret(commands: &[Vec<ShellWord>]) -> bool {
    const FILE_COMMANDS: [&str; 9] = [
        "cat", "cp", "head", "less", "mv", "rm", "tail", "tee", "touch",
    ];
    commands.iter().any(|words| {
        let Some((program, args)) = resolve_command(words) else {
            return false;
        };
        (FILE_COMMANDS.contains(&program.as_str())
            && args
                .iter()
                .filter(|word| !word.starts_with('-'))
                .any(|word| secret_path(word)))
            || words.iter().enumerate().any(|(index, word)| {
                matches!(word, ShellWord::Redirect(operator) if matches!(operator.as_str(), "<" | ">" | ">>" | "<>")
                    && words.get(index + 1).and_then(shell_arg).is_some_and(secret_path))
            })
    })
}

fn shell_arg(word: &ShellWord) -> Option<&str> {
    match word {
        ShellWord::Arg(word) => Some(word),
        ShellWord::Redirect(_) => None,
    }
}

fn resolve_command(words: &[ShellWord]) -> Option<(String, Vec<String>)> {
    let mut clean = Vec::new();
    let mut cursor = 0;
    while cursor < words.len() {
        if shell_arg(&words[cursor])
            .is_some_and(|word| word.chars().all(|character| character.is_ascii_digit()))
            && words
                .get(cursor + 1)
                .is_some_and(|word| matches!(word, ShellWord::Redirect(_)))
        {
            cursor += 3;
        } else if matches!(words[cursor], ShellWord::Redirect(_)) {
            cursor += 2;
        } else {
            clean.push(shell_arg(&words[cursor])?.to_owned());
            cursor += 1;
        }
    }
    let mut cursor = 0;
    loop {
        while clean.get(cursor).is_some_and(|word| is_assignment(word)) {
            cursor += 1;
        }
        let basename = Path::new(clean.get(cursor)?).file_name()?.to_str()?;
        if matches!(basename, "command" | "builtin" | "exec") {
            cursor += 1;
            while clean.get(cursor).is_some_and(|word| word.starts_with('-')) {
                cursor += 1;
            }
        } else if basename == "env" {
            cursor += 1;
            while clean
                .get(cursor)
                .is_some_and(|word| word.starts_with('-') || is_assignment(word))
            {
                cursor += 1;
            }
        } else if basename == "sudo" {
            cursor += 1;
            // The accepted grammar supports plain `sudo COMMAND`. Options
            // can consume following values and are deliberately ambiguous.
            if clean.get(cursor).is_some_and(|word| word.starts_with('-')) {
                return None;
            }
        } else {
            return Some((basename.to_owned(), clean[cursor + 1..].to_vec()));
        }
    }
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name.chars().enumerate().all(|(index, character)| {
            character == '_'
                || character.is_ascii_alphabetic()
                || index > 0 && character.is_ascii_digit()
        })
}

/// Tokenizes only command positions and arguments needed by the small
/// policy above. Arguments and syntactic redirections stay distinct so a
/// quoted `>` remains ordinary data. Heredoc bodies are removed before
/// tokenizing, so review prose is never promoted to code.
fn shell_commands(command: &str) -> Result<Vec<Vec<ShellWord>>, ()> {
    let command = without_heredoc_bodies(command)?;
    let mut commands = Vec::new();
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut characters = command.chars().peekable();
    while let Some(character) = characters.next() {
        if let Some(delimiter) = quote {
            if character == delimiter {
                quote = None;
            } else if delimiter == '"' && matches!(character, '$' | '`') {
                return Err(());
            } else if delimiter == '"' && character == '\\' {
                word.push(characters.next().ok_or(())?);
            } else {
                word.push(character);
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            '\\' => word.push(characters.next().ok_or(())?),
            '$' | '`' | '(' | ')' | '*' | '?' | '[' | ']' | '{' | '}' => return Err(()),
            '#' if word.is_empty() => {
                for rest in characters.by_ref() {
                    if rest == '\n' {
                        break;
                    }
                }
                if !words.is_empty() {
                    commands.push(std::mem::take(&mut words));
                }
            }
            ' ' | '\t' => {
                if !word.is_empty() {
                    words.push(ShellWord::Arg(std::mem::take(&mut word)));
                }
            }
            ';' | '&' | '|' | '\n' | '\r' => {
                if !word.is_empty() {
                    words.push(ShellWord::Arg(std::mem::take(&mut word)));
                }
                if !words.is_empty() {
                    commands.push(std::mem::take(&mut words));
                }
            }
            '<' | '>' => {
                if !word.is_empty() {
                    words.push(ShellWord::Arg(std::mem::take(&mut word)));
                }
                let mut operator = character.to_string();
                if characters.peek().is_some_and(|next| *next == character) {
                    operator.push(characters.next().expect("peeked character exists"));
                    if operator == "<<" && characters.peek() == Some(&'-') {
                        operator.push(characters.next().expect("peeked character exists"));
                    }
                }
                words.push(ShellWord::Redirect(operator));
            }
            _ => word.push(character),
        }
    }
    if quote.is_some() {
        return Err(());
    }
    if !word.is_empty() {
        words.push(ShellWord::Arg(word));
    }
    if !words.is_empty() {
        commands.push(words);
    }
    if commands
        .iter()
        .any(|words| unsupported_wrapper(words) || malformed_redirection(words))
    {
        return Err(());
    }
    Ok(commands)
}

fn malformed_redirection(words: &[ShellWord]) -> bool {
    words.iter().enumerate().any(|(index, word)| {
        matches!(word, ShellWord::Redirect(_))
            && words
                .get(index + 1)
                .is_none_or(|target| matches!(target, ShellWord::Redirect(_)))
    })
}

fn unsupported_wrapper(words: &[ShellWord]) -> bool {
    // Redirections are stripped by `resolve_command`; wrapper validation
    // only needs the argument stream.
    let words = words_without_redirections(words);
    unsupported_wrapper_args(&words)
}

fn words_without_redirections(words: &[ShellWord]) -> Vec<&str> {
    let mut clean = Vec::new();
    let mut cursor = 0;
    while cursor < words.len() {
        if matches!(words[cursor], ShellWord::Redirect(_)) {
            cursor += 2;
        } else {
            if let Some(word) = shell_arg(&words[cursor]) {
                clean.push(word);
            }
            cursor += 1;
        }
    }
    clean
}

fn unsupported_wrapper_args(words: &[&str]) -> bool {
    let mut cursor = 0;
    while words.get(cursor).is_some_and(|word| is_assignment(word)) {
        cursor += 1;
    }
    loop {
        let wrapper = words
            .get(cursor)
            .and_then(|word| Path::new(word).file_name())
            .and_then(|name| name.to_str());
        match wrapper {
            Some("sudo") => {
                cursor += 1;
                if words.get(cursor).is_some_and(|word| word.starts_with('-')) {
                    return true;
                }
            }
            Some("env") => {
                cursor += 1;
                while let Some(word) = words.get(cursor) {
                    if is_assignment(word) || matches!(*word, "-i" | "--ignore-environment" | "--")
                    {
                        cursor += 1;
                    } else {
                        return word.starts_with('-');
                    }
                }
                return false;
            }
            Some("exec") => {
                return words
                    .get(cursor + 1)
                    .is_some_and(|word| word.starts_with('-') && *word != "--");
            }
            Some("command" | "builtin") => {
                cursor += 1;
                while words.get(cursor).is_some_and(|word| word.starts_with('-')) {
                    cursor += 1;
                }
            }
            _ => return false,
        }
    }
}

#[derive(Clone)]
struct Heredoc {
    delimiter: String,
    quoted: bool,
}

fn without_heredoc_bodies(command: &str) -> Result<String, ()> {
    let mut kept = Vec::new();
    let mut heredoc: Option<Heredoc> = None;
    for line in command.lines() {
        if let Some(expected) = &heredoc {
            if line.trim() == expected.delimiter {
                heredoc = None;
            } else if !expected.quoted
                && line
                    .chars()
                    .any(|character| matches!(character, '$' | '`' | '\\'))
            {
                return Err(());
            }
            continue;
        }
        kept.push(line);
        if let Some(after) = heredoc_delimiter_source(line) {
            let raw = after.split_whitespace().next().ok_or(())?;
            let raw = raw.strip_prefix('-').unwrap_or(raw);
            let quoted = (raw.starts_with('\'') && raw.ends_with('\''))
                || (raw.starts_with('"') && raw.ends_with('"'));
            let delimiter = raw.trim_matches(|c| matches!(c, '\'' | '"'));
            if delimiter.is_empty() {
                return Err(());
            }
            heredoc = Some(Heredoc {
                delimiter: delimiter.to_owned(),
                quoted,
            });
        }
    }
    Ok(kept.join("\n"))
}

fn heredoc_delimiter_source(line: &str) -> Option<&str> {
    let mut quote = None;
    let mut characters = line.char_indices().peekable();
    while let Some((index, character)) = characters.next() {
        if let Some(delimiter) = quote {
            if character == delimiter {
                quote = None;
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            '<' if characters.peek().is_some_and(|(_, next)| *next == '<') => {
                return line.get(index + 2..).map(str::trim);
            }
            _ => {}
        }
    }
    None
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
            "git push origin +main:main",
            "git push --delete origin obsolete",
            "git push -d origin obsolete",
            "git push --delete=obsolete origin",
            "git push -dobsolete origin",
            "git push origin :obsolete",
            "/usr/bin/git push --force origin main",
            "cd repo && git push --force origin main",
            "scripts/github-comment.sh issue 80\ngit push --force origin main",
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
        assert_eq!(
            decide(&bash("cat ~/.ssh/id_ed25519"), root).denied_by,
            Some("secret_path")
        );
        assert_eq!(
            decide(&bash("echo replacement > ~/.aws/credentials"), root).denied_by,
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
        assert_eq!(
            decide(&bash("git push origin HEAD:feature"), root).denied_by,
            None
        );
        assert_eq!(decide(&bash("echo git push --force"), root).denied_by, None);
        assert_eq!(
            decide(&bash("scripts/github-comment.sh issue 80"), root).denied_by,
            None
        );
        assert_eq!(
            decide(&bash("scripts/github-comment.sh pr 95"), root).denied_by,
            None
        );
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
            "echo preparing; rm -rf /tmp/other",
        ] {
            assert_eq!(
                decide(&bash(command), root).denied_by,
                Some("recursive_delete_outside_worktree"),
                "{command}"
            );
        }
    }

    #[test]
    fn accepted_shell_grammar_is_table_driven() {
        let root = Path::new("/tmp/worktree");
        let cases = [
            ("env git push --force origin main", Some("destructive_git")),
            (
                "FOO=1 git push --force origin main",
                Some("destructive_git"),
            ),
            ("command git reset --hard HEAD", Some("destructive_git")),
            ("sudo git branch -D main", Some("destructive_git")),
            (
                "sudo FOO=1 git push --force origin main",
                Some("destructive_git"),
            ),
            ("g\\it push --force origin main", Some("destructive_git")),
            (
                "echo ok | git push --force origin main",
                Some("destructive_git"),
            ),
            (
                "env rm -rf /tmp/other",
                Some("recursive_delete_outside_worktree"),
            ),
            (
                "r\\m -rf /tmp/other",
                Some("recursive_delete_outside_worktree"),
            ),
            ("command cat ~/.ssh/id_ed25519", Some("secret_path")),
            ("echo replacement>~/.aws/credentials", Some("secret_path")),
            ("cat<~/.ssh/id_ed25519", Some("secret_path")),
            ("rm -rf $TARGET", Some("unsupported_shell_syntax")),
            ("echo $(date)", Some("unsupported_shell_syntax")),
            ("echo `date`", Some("unsupported_shell_syntax")),
            ("cat <(printf safe)", Some("unsupported_shell_syntax")),
            (
                "cat <<EOF\n$(rm -rf /tmp/other)\nEOF",
                Some("unsupported_shell_syntax"),
            ),
            (
                "cat <<EOF\n`cat ~/.ssh/id_ed25519`\nEOF",
                Some("unsupported_shell_syntax"),
            ),
            (
                "sudo -u root git push --force",
                Some("unsupported_shell_syntax"),
            ),
            (
                "env -u HOME git push --force",
                Some("unsupported_shell_syntax"),
            ),
            ("echo >", Some("unsupported_shell_syntax")),
            ("env git status --short", None),
            ("FOO=1 scripts/github-comment.sh issue 80", None),
            ("command git status --short", None),
            ("sudo echo reviewed", None),
            ("printf safe | cat", None),
            ("echo reviewed>report.txt", None),
            ("printf '%s' '>'", None),
            ("printf '%s' '<'", None),
            ("scripts/github-comment.sh issue 80", None),
            ("printf '%s' 'foo>bar'", None),
            ("scripts/github-comment.sh pr 95", None),
        ];
        for (command, expected) in cases {
            assert_eq!(
                decide(&bash(command), root).denied_by,
                expected,
                "{command}"
            );
        }
    }

    #[test]
    fn github_publication_is_not_a_hook_policy_boundary() {
        let root = Path::new("/tmp/worktree");
        for command in [
            // The helper is a transport convention; the hook cannot safely
            // distinguish this form from aliases, interpreters, or raw APIs.
            "scripts/github-comment.sh issue 80",
            "gh issue comment 80 --body literal",
            "sh -c 'gh issue comment 80 --body literal'",
            "xargs gh issue comment 80",
            "gh alias set publish 'issue comment 80'",
            "gh api repos/example/project/issues/80/comments -f body=literal",
            // Read-only GitHub commands must remain ordinary allowed work.
            "gh issue view 80",
            "gh pr view 95",
            "gh api repos/example/project/pulls/95",
        ] {
            assert_eq!(decide(&bash(command), root).denied_by, None, "{command}");
        }
    }

    #[test]
    fn capacity_mutation_is_denied_for_every_supported_command_wrapper() {
        let root = Path::new("/tmp/worktree");
        for command in [
            "factoryctl capacity set 8",
            "FOO=1 factoryctl capacity set 8",
            "env FOO=1 factoryctl capacity set 8",
            "command factoryctl capacity set 8",
            "exec factoryctl capacity set 8",
            "printf ready; factoryctl capacity set 8",
            "printf ready | factoryctl capacity set 8",
        ] {
            assert_eq!(
                decide(&bash(command), root).denied_by,
                Some("capacity_operator_only"),
                "{command}"
            );
        }
        assert_eq!(
            decide(&bash("factoryctl capacity status"), root).denied_by,
            None
        );
    }
}
