use std::{env, io::Write, path::PathBuf, process};

use factory_core::local::{
    LocalRequest, LocalResponse, MAX_EVENT_PAGE_ITEMS, MAX_PROJECT_PAGE_ITEMS, MAX_TASK_PAGE_ITEMS,
    ServerFrame,
};
use factoryctl::Client;
use uuid::Uuid;

const USAGE: &str = "usage: factoryctl [--socket PATH] <health|project|task|events> ...";
const PROJECT_LIST_LIMIT: u32 = MAX_PROJECT_PAGE_ITEMS;
const TASK_LIST_LIMIT: u32 = MAX_TASK_PAGE_ITEMS;
const EVENT_LIST_LIMIT: u32 = MAX_EVENT_PAGE_ITEMS;

#[derive(Debug, Eq, PartialEq)]
enum CliCommand {
    Health,
    ProjectAdd {
        name: String,
        root: String,
    },
    ProjectList {
        after_id: Option<String>,
        limit: u32,
    },
    TaskAdd {
        project_id: String,
        parent_task_id: Option<String>,
        title: String,
        body: String,
        priority: i32,
    },
    TaskList {
        project_id: String,
        after_id: Option<String>,
        limit: u32,
    },
    Events {
        after_sequence: i64,
        limit: u32,
        follow: bool,
    },
}

fn main() {
    let exit_code = match run() {
        Ok(code) => code,
        Err(message) => {
            let error = serde_json::json!({ "error": message });
            eprintln!("{error}");
            1
        }
    };
    process::exit(exit_code);
}

fn run() -> Result<i32, String> {
    let (explicit_socket, command) = parse_args(env::args().skip(1).collect())?;
    let environment_socket = env::var("DARK_FACTORY_SOCKET").ok();
    let factory_home = env::var("DARK_FACTORY_HOME").ok();
    let home = env::var("HOME").ok();
    let socket = resolve_socket_path(
        explicit_socket.as_deref(),
        environment_socket.as_deref(),
        factory_home.as_deref(),
        home.as_deref(),
    )?;
    let client = Client::new(socket);
    let stdout = std::io::stdout();
    let mut output = stdout.lock();

    if let CliCommand::Events {
        after_sequence,
        follow: true,
        ..
    } = command
    {
        for frame in client
            .subscribe(after_sequence)
            .map_err(|error| error.to_string())?
        {
            let frame = frame.map_err(|error| error.to_string())?;
            write_frame(&mut output, &frame)?;
            if is_error(&frame) {
                return Ok(2);
            }
        }
        return Ok(0);
    }

    let frame = client
        .request(request_for(command)?)
        .map_err(|error| error.to_string())?;
    write_frame(&mut output, &frame)?;
    Ok(if is_error(&frame) { 2 } else { 0 })
}

fn write_frame(output: &mut impl Write, frame: &ServerFrame) -> Result<(), String> {
    serde_json::to_writer(&mut *output, frame).map_err(|error| error.to_string())?;
    output.write_all(b"\n").map_err(|error| error.to_string())?;
    output.flush().map_err(|error| error.to_string())
}

fn is_error(frame: &ServerFrame) -> bool {
    matches!(
        frame,
        ServerFrame::Response {
            response: LocalResponse::Error { .. },
            ..
        }
    )
}

fn parse_args(mut args: Vec<String>) -> Result<(Option<String>, CliCommand), String> {
    let socket = take_option(&mut args, "--socket")?;
    if args.is_empty() {
        return Err(USAGE.into());
    }

    let command = args.remove(0);
    match command.as_str() {
        "health" => {
            require_empty(&args)?;
            Ok((socket, CliCommand::Health))
        }
        "project" => parse_project(args).map(|command| (socket, command)),
        "task" => parse_task(args).map(|command| (socket, command)),
        "events" => parse_events(args).map(|command| (socket, command)),
        _ => Err(format!("unknown command {command:?}; {USAGE}")),
    }
}

fn parse_project(mut args: Vec<String>) -> Result<CliCommand, String> {
    let action = take_action(&mut args, "project")?;
    match action.as_str() {
        "add" => {
            let name = required_option(&mut args, "--name")?;
            let root = required_option(&mut args, "--root")?;
            require_empty(&args)?;
            Ok(CliCommand::ProjectAdd { name, root })
        }
        "list" => {
            let after_id = take_option(&mut args, "--after")?;
            let (limit, _) = take_limit(&mut args, PROJECT_LIST_LIMIT, PROJECT_LIST_LIMIT)?;
            require_empty(&args)?;
            Ok(CliCommand::ProjectList { after_id, limit })
        }
        _ => Err(format!("unknown project action {action:?}")),
    }
}

fn parse_task(mut args: Vec<String>) -> Result<CliCommand, String> {
    let action = take_action(&mut args, "task")?;
    match action.as_str() {
        "add" => {
            let project_id = required_option(&mut args, "--project")?;
            let parent_task_id = take_option(&mut args, "--parent")?;
            let title = required_option(&mut args, "--title")?;
            let body = required_option(&mut args, "--body")?;
            let priority = take_option(&mut args, "--priority")?
                .map(|value| parse_number(&value, "--priority"))
                .transpose()?
                .unwrap_or(0);
            require_empty(&args)?;
            Ok(CliCommand::TaskAdd {
                project_id,
                parent_task_id,
                title,
                body,
                priority,
            })
        }
        "list" => {
            let project_id = required_option(&mut args, "--project")?;
            let after_id = take_option(&mut args, "--after")?;
            let (limit, _) = take_limit(&mut args, TASK_LIST_LIMIT, TASK_LIST_LIMIT)?;
            require_empty(&args)?;
            Ok(CliCommand::TaskList {
                project_id,
                after_id,
                limit,
            })
        }
        _ => Err(format!("unknown task action {action:?}")),
    }
}

fn parse_events(mut args: Vec<String>) -> Result<CliCommand, String> {
    let after_sequence = take_option(&mut args, "--after")?
        .map(|value| parse_number(&value, "--after"))
        .transpose()?
        .unwrap_or(0);
    if after_sequence < 0 {
        return Err("--after must be zero or greater".into());
    }
    let (limit, explicit_limit) = take_limit(&mut args, EVENT_LIST_LIMIT, MAX_EVENT_PAGE_ITEMS)?;
    let follow = take_flag(&mut args, "--follow")?;
    if follow && explicit_limit {
        return Err("--limit cannot be used with --follow".into());
    }
    require_empty(&args)?;
    Ok(CliCommand::Events {
        after_sequence,
        limit,
        follow,
    })
}

fn request_for(command: CliCommand) -> Result<LocalRequest, String> {
    match command {
        CliCommand::Health => Ok(LocalRequest::Health),
        CliCommand::ProjectAdd { name, root } => Ok(LocalRequest::CreateProject {
            id: generated_id()?,
            name,
            root,
        }),
        CliCommand::ProjectList { after_id, limit } => Ok(LocalRequest::ListProjects {
            after_id: after_id
                .map(|id| parse_id(id, "project cursor"))
                .transpose()?,
            limit,
        }),
        CliCommand::TaskAdd {
            project_id,
            parent_task_id,
            title,
            body,
            priority,
        } => Ok(LocalRequest::CreateTask {
            id: generated_id()?,
            project_id: parse_id(project_id, "project")?,
            parent_task_id: parent_task_id
                .map(|id| parse_id(id, "parent task"))
                .transpose()?,
            title,
            body,
            priority,
        }),
        CliCommand::TaskList {
            project_id,
            after_id,
            limit,
        } => Ok(LocalRequest::ListTasks {
            project_id: parse_id(project_id, "project")?,
            after_id: after_id.map(|id| parse_id(id, "task cursor")).transpose()?,
            limit,
        }),
        CliCommand::Events {
            after_sequence,
            limit,
            follow,
        } => Ok(if follow {
            LocalRequest::Subscribe { after_sequence }
        } else {
            LocalRequest::EventsAfter {
                sequence: after_sequence,
                limit,
            }
        }),
    }
}

fn generated_id<T>() -> Result<T, String>
where
    T: TryFrom<String>,
    T::Error: std::fmt::Display,
{
    T::try_from(Uuid::new_v4().hyphenated().to_string()).map_err(|error| error.to_string())
}

fn parse_id<T>(value: String, label: &str) -> Result<T, String>
where
    T: TryFrom<String>,
    T::Error: std::fmt::Display,
{
    T::try_from(value).map_err(|error| format!("invalid {label} ID: {error}"))
}

fn resolve_socket_path(
    explicit: Option<&str>,
    environment: Option<&str>,
    factory_home: Option<&str>,
    home: Option<&str>,
) -> Result<PathBuf, String> {
    if let Some(path) = explicit.filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = environment.filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = factory_home.filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path).join("f.sock"));
    }
    home.filter(|path| !path.is_empty())
        .map(|path| PathBuf::from(path).join(".dark-factory/f.sock"))
        .ok_or_else(|| "no socket configured and HOME is unavailable".into())
}

fn take_action(args: &mut Vec<String>, command: &str) -> Result<String, String> {
    if args.is_empty() {
        Err(format!("{command} requires an action"))
    } else {
        Ok(args.remove(0))
    }
}

fn required_option(args: &mut Vec<String>, name: &str) -> Result<String, String> {
    take_option(args, name)?.ok_or_else(|| format!("{name} is required"))
}

fn take_option(args: &mut Vec<String>, name: &str) -> Result<Option<String>, String> {
    let Some(index) = args.iter().position(|argument| argument == name) else {
        return Ok(None);
    };
    if args.iter().skip(index + 1).any(|argument| argument == name) {
        return Err(format!("{name} may only be provided once"));
    }
    if index + 1 >= args.len() || args[index + 1].starts_with("--") {
        return Err(format!("{name} requires a value"));
    }
    let value = args.remove(index + 1);
    args.remove(index);
    if value.is_empty() {
        return Err(format!("{name} must not be empty"));
    }
    Ok(Some(value))
}

fn take_flag(args: &mut Vec<String>, name: &str) -> Result<bool, String> {
    let Some(index) = args.iter().position(|argument| argument == name) else {
        return Ok(false);
    };
    args.remove(index);
    if args.iter().any(|argument| argument == name) {
        return Err(format!("{name} may only be provided once"));
    }
    Ok(true)
}

fn require_empty(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(format!("unexpected argument {:?}", args[0]))
    }
}

fn parse_number<T>(value: &str, name: &str) -> Result<T, String>
where
    T: std::str::FromStr,
{
    value
        .parse()
        .map_err(|_| format!("{name} requires a valid number"))
}

fn take_limit(args: &mut Vec<String>, default: u32, maximum: u32) -> Result<(u32, bool), String> {
    let explicit = take_option(args, "--limit")?;
    let was_explicit = explicit.is_some();
    let limit = explicit
        .map(|value| parse_number(&value, "--limit"))
        .transpose()?
        .unwrap_or(default);
    if !(1..=maximum).contains(&limit) {
        return Err(format!("--limit must be between 1 and {maximum}"));
    }
    Ok((limit, was_explicit))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use factory_core::local::LocalRequest;
    use uuid::Uuid;

    use super::{CliCommand, parse_args, request_for, resolve_socket_path};

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn explicit_socket_wins_then_environment_then_home_default() {
        assert_eq!(
            resolve_socket_path(
                Some("/explicit.sock"),
                Some("/env.sock"),
                Some("/factory-home"),
                Some("/home"),
            )
            .unwrap(),
            PathBuf::from("/explicit.sock")
        );
        assert_eq!(
            resolve_socket_path(
                None,
                Some("/env.sock"),
                Some("/factory-home"),
                Some("/home")
            )
            .unwrap(),
            PathBuf::from("/env.sock")
        );
        assert_eq!(
            resolve_socket_path(None, None, Some("/factory-home"), Some("/home")).unwrap(),
            PathBuf::from("/factory-home/f.sock")
        );
        assert_eq!(
            resolve_socket_path(None, None, None, Some("/home")).unwrap(),
            PathBuf::from("/home/.dark-factory/f.sock")
        );
        assert!(resolve_socket_path(None, None, None, None).is_err());
    }

    #[test]
    fn parses_the_minimal_project_and_task_commands() {
        assert_eq!(
            parse_args(args(&[
                "project",
                "add",
                "--name",
                "Dark Factory",
                "--root",
                "/work/dark-factory",
            ]))
            .unwrap(),
            (
                None,
                CliCommand::ProjectAdd {
                    name: "Dark Factory".into(),
                    root: "/work/dark-factory".into(),
                }
            )
        );
        assert_eq!(
            parse_args(args(&[
                "task",
                "add",
                "--project",
                "project-1",
                "--title",
                "Build client",
                "--body",
                "Use the socket",
                "--priority",
                "7",
                "--parent",
                "task-0",
            ]))
            .unwrap(),
            (
                None,
                CliCommand::TaskAdd {
                    project_id: "project-1".into(),
                    parent_task_id: Some("task-0".into()),
                    title: "Build client".into(),
                    body: "Use the socket".into(),
                    priority: 7,
                }
            )
        );
    }

    #[test]
    fn events_follow_is_an_explicit_subscription() {
        let (_, command) = parse_args(args(&["events", "--after", "12", "--follow"])).unwrap();
        assert_eq!(
            command,
            CliCommand::Events {
                after_sequence: 12,
                limit: 100,
                follow: true,
            }
        );
    }

    #[test]
    fn list_commands_parse_bounded_pagination() {
        assert_eq!(
            parse_args(args(&[
                "project",
                "list",
                "--after",
                "project-1",
                "--limit",
                "25",
            ]))
            .unwrap(),
            (
                None,
                CliCommand::ProjectList {
                    after_id: Some("project-1".into()),
                    limit: 25,
                }
            )
        );
        assert_eq!(
            parse_args(args(&[
                "task",
                "list",
                "--project",
                "project-1",
                "--after",
                "task-9",
            ]))
            .unwrap(),
            (
                None,
                CliCommand::TaskList {
                    project_id: "project-1".into(),
                    after_id: Some("task-9".into()),
                    limit: 10,
                }
            )
        );

        assert!(parse_args(args(&["project", "list", "--limit", "101"])).is_err());
        assert!(
            parse_args(args(&[
                "task",
                "list",
                "--project",
                "project-1",
                "--limit",
                "11",
            ]))
            .is_err()
        );
    }

    #[test]
    fn events_follow_rejects_an_explicit_limit() {
        let error = parse_args(args(&["events", "--follow", "--limit", "1"])).unwrap_err();
        assert_eq!(error, "--limit cannot be used with --follow");
    }

    #[test]
    fn create_commands_generate_valid_uuid_ids() {
        let request = request_for(CliCommand::ProjectAdd {
            name: "Dark Factory".into(),
            root: "/work/dark-factory".into(),
        })
        .unwrap();

        let LocalRequest::CreateProject { id, .. } = request else {
            panic!("expected create project request");
        };
        assert!(Uuid::parse_str(id.as_str()).is_ok());
    }
}
