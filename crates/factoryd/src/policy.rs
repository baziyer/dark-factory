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

pub fn decide(payload: &Value, source_root: &Path) -> Decision {
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
    } else if direct_rust_toolchain(commands.as_deref().unwrap_or_default()) {
        Some("daemon_owned_rust_verification")
    } else if direct_mutable_cargo_output(commands.as_deref().unwrap_or_default()) {
        Some("mutable_cargo_output")
    } else if disallowed_git(commands.as_deref().unwrap_or_default()) {
        Some("destructive_or_publication_git")
    } else if recursive_force_delete_outside(commands.as_deref().unwrap_or_default(), source_root) {
        Some("recursive_delete_outside_source")
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

fn direct_rust_toolchain(commands: &[Vec<ShellWord>]) -> bool {
    commands.iter().any(|words| {
        resolve_command(words).is_some_and(|(program, _)| {
            matches!(program_name(&program), Some("cargo" | "rustc" | "rustup"))
        })
    })
}

fn direct_mutable_cargo_output(commands: &[Vec<ShellWord>]) -> bool {
    commands.iter().any(|words| {
        resolve_command(words).is_some_and(|(program, _)| {
            let components = Path::new(&program)
                .components()
                .filter_map(|component| match component {
                    std::path::Component::Normal(component) => component.to_str(),
                    _ => None,
                })
                .collect::<Vec<_>>();
            components.iter().enumerate().any(|(index, component)| {
                *component == "target"
                    && components[index + 1..]
                        .iter()
                        .position(|part| matches!(*part, "debug" | "release"))
                        .is_some_and(|profile| index + profile + 2 < components.len())
            })
        })
    })
}

fn program_name(program: &str) -> Option<&str> {
    Path::new(program).file_name()?.to_str()
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ShellWord {
    Arg(String),
    Redirect(String),
}

fn disallowed_git(commands: &[Vec<ShellWord>]) -> bool {
    commands.iter().any(|words| {
        let Some((program, args)) = resolve_command(words) else {
            return false;
        };
        if program_name(&program) != Some("git") {
            return false;
        }
        let Some((subcommand, args)) = git_subcommand(&args) else {
            return false;
        };
        match subcommand {
            "push" => true,
            "reset" => args.iter().any(|arg| arg == "--hard"),
            "branch" => args
                .iter()
                .any(|arg| matches!(arg.as_str(), "-d" | "-D" | "--delete")),
            _ => false,
        }
    })
}

fn git_subcommand(args: &[String]) -> Option<(&str, &[String])> {
    let mut cursor = 0;
    while let Some(argument) = args.get(cursor) {
        match argument.as_str() {
            "--" => {
                cursor += 1;
                break;
            }
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--super-prefix"
            | "--config-env" => cursor += 2,
            _ if argument.starts_with("-C") || argument.starts_with("-c") => cursor += 1,
            _ if argument.starts_with("--git-dir=")
                || argument.starts_with("--work-tree=")
                || argument.starts_with("--namespace=")
                || argument.starts_with("--super-prefix=")
                || argument.starts_with("--config-env=") =>
            {
                cursor += 1;
            }
            _ if argument.starts_with('-') => cursor += 1,
            _ => break,
        }
    }
    let subcommand = args.get(cursor)?;
    Some((subcommand, &args[cursor + 1..]))
}

fn recursive_force_delete_outside(commands: &[Vec<ShellWord>], source_root: &Path) -> bool {
    commands.iter().enumerate().any(|(index, words)| {
        resolve_command(words).is_some_and(|(program, args)| {
            program_name(&program) == Some("rm")
                && unsafe_recursive_delete(index, &args, commands.len(), source_root)
        })
    })
}

fn unsafe_recursive_delete(
    index: usize,
    args: &[String],
    command_count: usize,
    source_root: &Path,
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
                path.starts_with(source_root)
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
        (program_name(&program).is_some_and(|program| FILE_COMMANDS.contains(&program))
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
        let program = clean.get(cursor)?;
        let basename = program_name(program)?;
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
            return Some((program.to_owned(), clean[cursor + 1..].to_vec()));
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
        let root = Path::new("/tmp/change");
        for command in [
            "git push origin feature",
            "git push --force origin main",
            "git push --force-with-lease origin main",
            "git push origin +main:main",
            "git push --delete origin obsolete",
            "/usr/bin/git push origin main",
            "git -C source push origin main",
            "git -Csource push origin main",
            "git --no-pager push origin main",
            "env FOO=1 git -c push.default=current push origin main",
            "FOO=1 git push origin main",
            "command git reset --hard HEAD",
            "sudo git branch -D obsolete",
            "g\\it push origin main",
            "echo ready | git push origin main",
            "git reset --hard HEAD",
            "git branch -D obsolete",
        ] {
            assert_eq!(
                decide(&bash(command), root).denied_by,
                Some("destructive_or_publication_git"),
                "{command}"
            );
        }
        assert_eq!(
            decide(&bash("rm -rf /tmp/other"), root).denied_by,
            Some("recursive_delete_outside_source")
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
    fn denies_direct_rust_toolchains_for_daemon_owned_verification() {
        let root = Path::new("/tmp/change");
        for command in [
            "cargo test --workspace",
            "cargo metadata --format-version 1",
            "/usr/bin/cargo +stable check",
            "env CARGO_TERM_COLOR=never cargo test",
            "command rustc --version",
            "exec rustup run stable cargo test",
            "echo ready | cargo test",
        ] {
            assert_eq!(
                decide(&bash(command), root).denied_by,
                Some("daemon_owned_rust_verification"),
                "{command}"
            );
        }
    }

    #[test]
    fn denies_direct_mutable_cargo_output_launches() {
        let root = Path::new("/tmp/change");
        for command in [
            "./target/debug/app",
            "/tmp/change/target/release/app --check",
            "env target/aarch64-unknown-linux-gnu/debug/deps/workspace_test --exact case",
            "command ./target/release/deps/workspace_test",
            "echo ready | ./target/debug/app",
        ] {
            assert_eq!(
                decide(&bash(command), root).denied_by,
                Some("mutable_cargo_output"),
                "{command}"
            );
        }

        for command in [
            "ls target/debug",
            "rm target/debug/obsolete",
            "printf '%s' target/debug/app",
        ] {
            assert_eq!(decide(&bash(command), root).denied_by, None, "{command}");
        }
    }

    #[test]
    fn permits_operations_inside_the_change_source() {
        let root = Path::new("/tmp/change");
        for command in [
            "git status --short",
            "git diff -- src/lib.rs",
            "git apply update.patch",
            "git add src/lib.rs",
            "git mv old.rs new.rs",
            "git rm obsolete.rs",
            "git commit -m push",
        ] {
            assert_eq!(decide(&bash(command), root).denied_by, None, "{command}");
        }
        assert_eq!(decide(&bash("echo git push --force"), root).denied_by, None);
        assert_eq!(
            decide(
                &bash(
                    "gh issue comment 80 --body 'reviewed git push --force and ~/.ssh/id_ed25519'"
                ),
                root
            )
            .denied_by,
            None
        );
        assert_eq!(
            decide(
                &bash("gh pr comment 95 --body-file - <<'EOF'\nI tried git push --force and it was denied.\nI also checked ~/.ssh/id_ed25519 and rm -rf /tmp/other prose.\nEOF"),
                root
            )
            .denied_by,
            None
        );
        assert_eq!(
            decide(&bash("rm -rf /tmp/change/target"), root).denied_by,
            None
        );
        assert_eq!(decide(&bash("rm -rf target"), root).denied_by, None);
        assert_eq!(
            decide(&bash("rm -rf ../other"), root).denied_by,
            Some("recursive_delete_outside_source")
        );
        for command in [
            "rm -rf /tmp/change/../other",
            "cd /tmp && rm -rf other",
            "rm -rf target && echo done",
            "echo preparing; rm -rf /tmp/other",
        ] {
            assert_eq!(
                decide(&bash(command), root).denied_by,
                Some("recursive_delete_outside_source"),
                "{command}"
            );
        }
    }

    #[test]
    fn accepted_shell_grammar_is_table_driven() {
        let root = Path::new("/tmp/change");
        let cases = [
            (
                "env rm -rf /tmp/other",
                Some("recursive_delete_outside_source"),
            ),
            (
                "r\\m -rf /tmp/other",
                Some("recursive_delete_outside_source"),
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
            (
                "FOO=1 gh issue comment 80 --body 'git push --force ~/.ssh/id'",
                None,
            ),
            ("command git status --short", None),
            ("sudo echo reviewed", None),
            ("printf safe | cat", None),
            ("echo reviewed>report.txt", None),
            ("printf '%s' '>'", None),
            ("printf '%s' '<'", None),
            ("gh issue comment 80 --body '>'", None),
            (
                "gh issue comment 80 --body 'literal > ~/.aws/credentials'",
                None,
            ),
            ("printf '%s' 'foo>bar'", None),
            (
                "gh pr comment 95 --body-file - <<EOF\nplain git push --force prose\nEOF",
                None,
            ),
            (
                "gh pr comment 95 --body-file - <<'EOF'\n$(rm -rf /tmp/other) is prose\nEOF",
                None,
            ),
        ];
        for (command, expected) in cases {
            assert_eq!(
                decide(&bash(command), root).denied_by,
                expected,
                "{command}"
            );
        }
    }
}
