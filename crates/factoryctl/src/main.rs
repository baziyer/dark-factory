use std::{env, io::Write, path::PathBuf, process};

use factory_core::local::{
    LocalRequest, LocalResponse, MAX_AGENT_PAGE_ITEMS, MAX_EVENT_PAGE_ITEMS,
    MAX_PROJECT_PAGE_ITEMS, MAX_RUN_PAGE_ITEMS, MAX_TASK_PAGE_ITEMS, ServerFrame,
};
use factory_core::{AgentRole, Provider};
use factoryctl::Client;
use uuid::Uuid;

mod attach;
mod usage;

const USAGE: &str =
    "usage: factoryctl [--socket PATH] <health|usage|project|task|agent|run|attach|events> ...";
const HELP: &str = "Dark Factory local control plane

Run the daemon separately (launchd keeps it alive), then run `factory-tui` in a persistent terminal.

Commands:
  health                                      Check the daemon
  usage                                       Probe Codex subscription usage on demand
  project add|list|delete|get|guidance        Manage projects and their guidance file
  task add|list|get|start|retry|assign|cancel|update|delete
                                               Manage and run tasks
  agent add|list|delete|get|profile|message|inbox
                                               Manage agents, their guidance files, and their durable messages
  run list|stop                               List and stop process attempts
  attach --project P --session S              Attach to a terminal-mode session's PTY
  events [--follow]                           Read durable events

Run `factoryctl <command> --help` or `factoryctl <command> <action> --help`
for action-specific options.

Options:
  --socket PATH                      Use an explicit local socket
  -h, --help                         Show this help";
const HEALTH_HELP: &str = "usage: factoryctl health

Check that the daemon is reachable and responding.";
const USAGE_HELP: &str = "usage: factoryctl usage

Run a local Codex JSON-RPC probe against `codex` on PATH and print the
result. No daemon or socket is involved and nothing is persisted; Claude's
usage is read by running `/usage` inside Claude's own interactive terminal.";
const EVENTS_HELP: &str = "usage: factoryctl events [--after N] [--limit N] [--follow]

Read durable events from the daemon.

Options:
  --after N                Read events after this sequence (default 0)
  --limit N                 Page size (default and max: 100; not with --follow)
  --follow                   Stream events as they occur
  -h, --help                  Show this help";

const PROJECT_HELP: &str = "usage: factoryctl project <add|list|delete|get|guidance> [options]

Manage projects.

Actions:
  add       Create a new project
  list      List projects
  delete    Delete a project that has no non-terminal run
  get       Fetch one project, including its guidance file path
  guidance  Manage a project's standing guidance file

Run `factoryctl project <action> --help` for action-specific options.";
const PROJECT_ADD_HELP: &str = "usage: factoryctl project add --name TEXT --root PATH [options]

Create a new project.

Required:
  --name TEXT             Project name (1-160 bytes)
  --root PATH             Existing readable directory

Options:
  --id ID                  Explicit project ID (default: generated UUID)
  -h, --help                Show this help";
const PROJECT_LIST_HELP: &str = "usage: factoryctl project list [options]

List projects, ordered by ID.

Options:
  --after ID               Resume after this project ID
  --limit N                  Page size (default and max: 100)
  -h, --help                  Show this help";
const PROJECT_DELETE_HELP: &str = "usage: factoryctl project delete --project ID

Delete a project that has no non-terminal run. Cascades to delete every
task, agent, and run in the project.

Required:
  --project ID           Project to delete

Options:
  -h, --help              Show this help";
const PROJECT_GET_HELP: &str = "usage: factoryctl project get --project ID

Fetch one project, including the absolute path of its `PROJECT.md`
guidance file and the file's current contents.

Required:
  --project ID           Project to fetch

Options:
  -h, --help              Show this help";
const PROJECT_GUIDANCE_HELP: &str =
    "usage: factoryctl project guidance set --project ID --file PATH

Replace a project's `PROJECT.md` guidance file with the contents of a local
file. Written atomically (bounded, temp file plus rename).

Required:
  --project ID           Project to update
  --file PATH             Local file to read the new guidance text from

Options:
  -h, --help                Show this help";

const TASK_HELP: &str =
    "usage: factoryctl task <add|list|get|start|retry|assign|cancel|update|delete> [options]

Manage tasks within a project.

Actions:
  add       Create a new task
  list      List tasks in a project
  get       Fetch one task
  start     Start a queued task on an agent
  retry     Requeue a failed or cancelled task
  assign    Set or clear a task's queue owner without starting it
  cancel    Cancel a queued or blocked task
  update    Edit a queued task's title or body
  delete    Delete a task that has no active run

Run `factoryctl task <action> --help` for action-specific options.";
const TASK_ADD_HELP: &str =
    "usage: factoryctl task add --project ID --title TEXT --body TEXT [options]

Create a new task.

Required:
  --project ID          Project the task belongs to
  --title TEXT          Task title (1-240 bytes)
  --body TEXT           Task body

Options:
  --id ID                 Explicit task ID (default: generated UUID)
  --parent PARENT_ID      Parent task ID
  --priority N              Priority (default: 0)
  -h, --help                 Show this help";
const TASK_LIST_HELP: &str = "usage: factoryctl task list --project ID [options]

List tasks in a project, ordered by ID.

Required:
  --project ID           Project to list tasks from

Options:
  --after ID               Resume after this task ID
  --limit N                  Page size (default and max: 10)
  -h, --help                   Show this help";
const TASK_GET_HELP: &str = "usage: factoryctl task get --project ID --task ID

Fetch one task by ID.

Required:
  --project ID           Project the task belongs to
  --task ID              Task to fetch

Options:
  -h, --help              Show this help";
const TASK_START_HELP: &str =
    "usage: factoryctl task start --project ID --task ID --agent ID --worktree PATH [options]

Start a queued task on an idle agent.

Required:
  --project ID           Project the task belongs to
  --task ID              Task to start
  --agent ID             Agent to run it
  --worktree PATH        Working directory for the run

Options:
  --parent-run ID          Parent run ID (for orchestrator-spawned runs)
  -h, --help                 Show this help";
const TASK_RETRY_HELP: &str = "usage: factoryctl task retry --project ID --task ID

Requeue a failed or cancelled task.

Required:
  --project ID           Project the task belongs to
  --task ID              Task to retry

Options:
  -h, --help              Show this help";
const TASK_ASSIGN_HELP: &str = "usage: factoryctl task assign --project ID --task ID [--agent ID]

Set or clear a queued task's queue owner without starting it. Omit --agent
to clear the assignment back to the operator.

Required:
  --project ID           Project the task belongs to
  --task ID              Task to assign

Options:
  --agent ID               Agent to assign as queue owner
  -h, --help                 Show this help";
const TASK_CANCEL_HELP: &str = "usage: factoryctl task cancel --project ID --task ID

Cancel a queued or blocked task. The task keeps its current assignment and
can be retried later.

Required:
  --project ID           Project the task belongs to
  --task ID              Task to cancel

Options:
  -h, --help              Show this help";
const TASK_UPDATE_HELP: &str =
    "usage: factoryctl task update --project ID --task ID [--title TEXT] [--body TEXT]

Edit a queued task's title and/or body. At least one of --title or --body
is required.

Required:
  --project ID           Project the task belongs to
  --task ID              Task to update

Options:
  --title TEXT              New title (1-240 bytes)
  --body TEXT                 New body
  -h, --help                    Show this help";
const TASK_DELETE_HELP: &str = "usage: factoryctl task delete --project ID --task ID

Delete a task that has no non-terminal run, no subtasks, and no run that is
a parent of another run. Also deletes its terminal runs and any rows that
reference it (questions, dependencies, webhook capabilities).

Required:
  --project ID           Project the task belongs to
  --task ID              Task to delete

Options:
  -h, --help              Show this help";

const AGENT_HELP: &str =
    "usage: factoryctl agent <add|list|delete|get|profile|message|inbox> [options]

Manage agents within a project and their durable messages.

Actions:
  add       Create a new agent
  list      List agents in a project
  delete    Delete an agent that has no open run
  get       Fetch one agent, including its guidance file paths
  profile   Manage an agent's model, permission mode, and guidance files
  message   Send a durable message from one agent to another
  inbox     List an agent's durable messages

Run `factoryctl agent <action> --help` for action-specific options.";
const AGENT_ADD_HELP: &str =
    "usage: factoryctl agent add --project ID --role <orchestrator|worker> --provider <claude|codex> [options]

Create a new agent.

Required:
  --project ID           Project the agent belongs to
  --role ROLE              orchestrator or worker
  --provider PROVIDER      claude (or claude-code) or codex

Options:
  --id ID                    Explicit agent ID (default: generated UUID)
  --parent PARENT_ID         Parent agent ID
  --model MODEL               Provider model identifier for this agent
  -h, --help                   Show this help";
const AGENT_LIST_HELP: &str = "usage: factoryctl agent list --project ID [options]

List agents in a project, ordered by ID.

Required:
  --project ID           Project to list agents from

Options:
  --after ID               Resume after this agent ID
  --limit N                  Page size (default and max: 100)
  -h, --help                   Show this help";
const AGENT_DELETE_HELP: &str = "usage: factoryctl agent delete --project ID --agent ID

Delete an agent that has no open run and no child agents, and whose runs are
not the parent of another run. Its terminal runs are deleted too, and any
tasks still assigned to it become unassigned. Its agent profile row is
deleted, messages addressed to it are deleted, and messages it sent survive
with the sender cleared.

Required:
  --project ID           Project the agent belongs to
  --agent ID             Agent to delete

Options:
  -h, --help              Show this help";
const AGENT_GET_HELP: &str = "usage: factoryctl agent get --project ID --agent ID

Fetch one agent, including the absolute paths of its `instructions.md` and
`memory.md` guidance files and their current contents.

Required:
  --project ID           Project the agent belongs to
  --agent ID             Agent to fetch

Options:
  -h, --help              Show this help";
const AGENT_PROFILE_HELP: &str =
    "usage: factoryctl agent profile set --project ID --agent ID [options]

Update an agent's model, permission mode, and/or guidance files. Any flag
left unset carries the currently stored value forward unchanged, so this
cannot silently clear standing instructions or memory it was not asked to
change.

Required:
  --project ID                    Project the agent belongs to
  --agent ID                      Agent to update

Options:
  --model MODEL                     Provider model identifier
  --permission-mode MODE            Provider permission mode
  --instructions-file PATH          Local file to read new instructions.md contents from
  --memory-file PATH                Local file to read new memory.md contents from
  -h, --help                          Show this help";
const AGENT_MESSAGE_HELP: &str =
    "usage: factoryctl agent message --project ID --to AGENT_ID --body TEXT [options]

Send a durable message from one agent to another. Messages are delivered
into the recipient's inbox on their next run launch.

Required:
  --project ID           Project the agents belong to
  --to AGENT_ID          Recipient agent ID
  --body TEXT            Message body

Options:
  --id ID                    Explicit message ID (default: generated UUID)
  --from AGENT_ID             Sender agent ID (default: none/system)
  -h, --help                   Show this help";
const AGENT_INBOX_HELP: &str = "usage: factoryctl agent inbox --project ID --agent ID [options]

List an agent's durable messages, ordered by ID.

Required:
  --project ID           Project the agent belongs to
  --agent ID             Agent whose inbox to list

Options:
  --after ID               Resume after this message ID
  --limit N                  Page size (default and max: 100)
  -h, --help                   Show this help";

const RUN_HELP: &str = "usage: factoryctl run <list|stop> [options]

Inspect and control process attempts (runs).

Actions:
  list      List runs in a project
  stop      Request a graceful stop of a run

Run `factoryctl run <action> --help` for action-specific options.";
const RUN_LIST_HELP: &str = "usage: factoryctl run list --project ID [options]

List runs in a project, ordered by ID.

Required:
  --project ID           Project to list runs from

Options:
  --after ID               Resume after this run ID
  --limit N                  Page size (default and max: 100)
  -h, --help                   Show this help";
const RUN_STOP_HELP: &str = "usage: factoryctl run stop --project ID --run ID [--grace-ms N]

Request a graceful stop of a run. The daemon signals the runner and marks
stop intent on the run, so its next terminal event is recorded as stopped
rather than failed, and its task becomes cancelled instead of failed.

Required:
  --project ID           Project the run belongs to
  --run ID                Run to stop

Options:
  --grace-ms N              Grace period before a harder stop (default 0, max 60000)
  -h, --help                  Show this help";

const ATTACH_HELP: &str = "usage: factoryctl attach --project ID --session ID [--since-offset N]

Attach to a terminal-mode session's PTY: puts the local terminal in raw
mode, replays retained output from --since-offset (default 0, i.e. from
the start), then streams live output and forwards stdin as operator input.
Resizes the remote PTY to match the local terminal on attach and on every
SIGWINCH. Detach with Ctrl-] without affecting the session.

Required:
  --project ID               Project the session belongs to
  --session ID                Session to attach to (--run is accepted as an alias)

Options:
  --since-offset N            Replay retained output from this byte offset (default 0)
  -h, --help                    Show this help";

const PROJECT_LIST_LIMIT: u32 = MAX_PROJECT_PAGE_ITEMS;
const TASK_LIST_LIMIT: u32 = MAX_TASK_PAGE_ITEMS;
const AGENT_LIST_LIMIT: u32 = MAX_AGENT_PAGE_ITEMS;
const RUN_LIST_LIMIT: u32 = MAX_RUN_PAGE_ITEMS;
const EVENT_LIST_LIMIT: u32 = MAX_EVENT_PAGE_ITEMS;

#[derive(Debug, Eq, PartialEq)]
enum CliCommand {
    Help(&'static str),
    Health,
    Usage,
    ProjectAdd {
        id: Option<String>,
        name: String,
        root: String,
    },
    ProjectList {
        after_id: Option<String>,
        limit: u32,
    },
    ProjectDelete {
        project_id: String,
    },
    ProjectGet {
        project_id: String,
    },
    ProjectGuidanceSet {
        project_id: String,
        file: String,
    },
    TaskAdd {
        id: Option<String>,
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
    TaskStart {
        project_id: String,
        task_id: String,
        agent_id: String,
        parent_run_id: Option<String>,
        worktree: String,
    },
    Attach {
        project_id: String,
        session_id: String,
        since_offset: u64,
    },
    TaskRetry {
        project_id: String,
        task_id: String,
    },
    TaskAssign {
        project_id: String,
        task_id: String,
        agent_id: Option<String>,
    },
    TaskGet {
        project_id: String,
        task_id: String,
    },
    TaskCancel {
        project_id: String,
        task_id: String,
    },
    TaskUpdate {
        project_id: String,
        task_id: String,
        title: Option<String>,
        body: Option<String>,
    },
    TaskDelete {
        project_id: String,
        task_id: String,
    },
    AgentAdd {
        id: Option<String>,
        project_id: String,
        parent_agent_id: Option<String>,
        role: AgentRole,
        provider: Provider,
        model: Option<String>,
    },
    AgentList {
        project_id: String,
        after_id: Option<String>,
        limit: u32,
    },
    AgentGet {
        project_id: String,
        agent_id: String,
    },
    AgentProfileSet {
        project_id: String,
        agent_id: String,
        model: Option<String>,
        permission_mode: Option<String>,
        instructions_file: Option<String>,
        memory_file: Option<String>,
    },
    AgentMessage {
        id: Option<String>,
        project_id: String,
        sender_agent_id: Option<String>,
        recipient_agent_id: String,
        body: String,
    },
    AgentInbox {
        project_id: String,
        agent_id: String,
        after_id: Option<String>,
        limit: u32,
    },
    AgentDelete {
        project_id: String,
        agent_id: String,
    },
    RunList {
        project_id: String,
        after_id: Option<String>,
        limit: u32,
    },
    RunStop {
        project_id: String,
        run_id: String,
        grace_ms: u64,
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
    if let CliCommand::Help(text) = command {
        println!("{text}");
        return Ok(0);
    }
    if matches!(command, CliCommand::Usage) {
        return Ok(usage::run());
    }
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
    if let CliCommand::Attach {
        project_id,
        session_id,
        since_offset,
    } = command
    {
        return attach::run(&client, &project_id, &session_id, since_offset);
    }
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

    if let CliCommand::AgentProfileSet {
        project_id,
        agent_id,
        model,
        permission_mode,
        instructions_file,
        memory_file,
    } = command
    {
        let frame = agent_profile_set_frame(
            &client,
            project_id,
            agent_id,
            model,
            permission_mode,
            instructions_file,
            memory_file,
        )?;
        write_frame(&mut output, &frame)?;
        return Ok(if is_error(&frame) { 2 } else { 0 });
    }

    let frame = client
        .request(request_for(command)?)
        .map_err(|error| error.to_string())?;
    write_frame(&mut output, &frame)?;
    Ok(if is_error(&frame) { 2 } else { 0 })
}

/// Applies `--model`/`--permission-mode`/`--instructions-file`/
/// `--memory-file` as a patch over the agent's current profile: any flag
/// left unset carries the currently stored value forward unchanged, so
/// `agent profile set` cannot silently clear standing guidance or memory it
/// was not asked to change.
fn agent_profile_set_frame(
    client: &Client,
    project_id: String,
    agent_id: String,
    model: Option<String>,
    permission_mode: Option<String>,
    instructions_file: Option<String>,
    memory_file: Option<String>,
) -> Result<ServerFrame, String> {
    let project_id: factory_core::ProjectId = parse_id(project_id, "project")?;
    let agent_id: factory_core::AgentId = parse_id(agent_id, "agent")?;
    let current = client
        .request(LocalRequest::GetAgent {
            project_id: project_id.clone(),
            agent_id: agent_id.clone(),
        })
        .map_err(|error| error.to_string())?;
    let ServerFrame::Response {
        response: LocalResponse::Agent { agent },
        ..
    } = current
    else {
        return Ok(current);
    };
    let instructions = match instructions_file {
        Some(path) => read_guidance_file(&path)?,
        None => agent.profile.instructions,
    };
    let memory = match memory_file {
        Some(path) => read_guidance_file(&path)?,
        None => agent.profile.memory,
    };
    client
        .request(LocalRequest::UpdateAgentProfile {
            project_id,
            agent_id,
            model: model.or(agent.profile.model),
            permission_mode: permission_mode.or(agent.profile.permission_mode),
            instructions,
            memory,
        })
        .map_err(|error| error.to_string())
}

fn read_guidance_file(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|error| format!("cannot read {path}: {error}"))
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

fn is_help_flag(value: &str) -> bool {
    value == "--help" || value == "-h"
}

fn wants_help(args: &[String]) -> bool {
    args.iter().any(|argument| is_help_flag(argument))
}

fn parse_args(mut args: Vec<String>) -> Result<(Option<String>, CliCommand), String> {
    let socket = take_option(&mut args, "--socket")?;
    if args.is_empty() {
        return Err(USAGE.into());
    }

    let command = args.remove(0);
    if command == "help" || is_help_flag(&command) {
        return Ok((socket, CliCommand::Help(HELP)));
    }
    match command.as_str() {
        "health" => {
            if wants_help(&args) {
                return Ok((socket, CliCommand::Help(HEALTH_HELP)));
            }
            require_empty(&args)?;
            Ok((socket, CliCommand::Health))
        }
        "usage" => {
            if wants_help(&args) {
                return Ok((socket, CliCommand::Help(USAGE_HELP)));
            }
            require_empty(&args)?;
            Ok((socket, CliCommand::Usage))
        }
        "project" => parse_project(args).map(|command| (socket, command)),
        "task" => parse_task(args).map(|command| (socket, command)),
        "attach" => {
            if wants_help(&args) {
                return Ok((socket, CliCommand::Help(ATTACH_HELP)));
            }
            parse_attach(args).map(|command| (socket, command))
        }
        "agent" => parse_agent(args).map(|command| (socket, command)),
        "run" => parse_run(args).map(|command| (socket, command)),
        "events" => {
            if wants_help(&args) {
                return Ok((socket, CliCommand::Help(EVENTS_HELP)));
            }
            parse_events(args).map(|command| (socket, command))
        }
        _ => Err(format!("unknown command {command:?}; {USAGE}")),
    }
}

fn parse_project(mut args: Vec<String>) -> Result<CliCommand, String> {
    if args.is_empty() || is_help_flag(&args[0]) {
        return Ok(CliCommand::Help(PROJECT_HELP));
    }
    let action = take_action(&mut args, "project")?;
    if wants_help(&args) {
        return Ok(CliCommand::Help(match action.as_str() {
            "add" => PROJECT_ADD_HELP,
            "list" => PROJECT_LIST_HELP,
            "delete" => PROJECT_DELETE_HELP,
            "get" => PROJECT_GET_HELP,
            "guidance" => PROJECT_GUIDANCE_HELP,
            _ => PROJECT_HELP,
        }));
    }
    match action.as_str() {
        "add" => {
            let id = take_option(&mut args, "--id")?;
            let name = required_option(&mut args, "--name")?;
            let root = required_option(&mut args, "--root")?;
            require_empty(&args)?;
            Ok(CliCommand::ProjectAdd { id, name, root })
        }
        "list" => {
            let after_id = take_option(&mut args, "--after")?;
            let (limit, _) = take_limit(&mut args, PROJECT_LIST_LIMIT, PROJECT_LIST_LIMIT)?;
            require_empty(&args)?;
            Ok(CliCommand::ProjectList { after_id, limit })
        }
        "delete" => {
            let project_id = required_option(&mut args, "--project")?;
            require_empty(&args)?;
            Ok(CliCommand::ProjectDelete { project_id })
        }
        "get" => {
            let project_id = required_option(&mut args, "--project")?;
            require_empty(&args)?;
            Ok(CliCommand::ProjectGet { project_id })
        }
        "guidance" => {
            let sub_action = take_action(&mut args, "project guidance")?;
            match sub_action.as_str() {
                "set" => {
                    let project_id = required_option(&mut args, "--project")?;
                    let file = required_option(&mut args, "--file")?;
                    require_empty(&args)?;
                    Ok(CliCommand::ProjectGuidanceSet { project_id, file })
                }
                _ => Err(format!("unknown project guidance action {sub_action:?}")),
            }
        }
        _ => Err(format!("unknown project action {action:?}")),
    }
}

fn parse_task(mut args: Vec<String>) -> Result<CliCommand, String> {
    if args.is_empty() || is_help_flag(&args[0]) {
        return Ok(CliCommand::Help(TASK_HELP));
    }
    let action = take_action(&mut args, "task")?;
    if wants_help(&args) {
        return Ok(CliCommand::Help(match action.as_str() {
            "add" => TASK_ADD_HELP,
            "list" => TASK_LIST_HELP,
            "get" => TASK_GET_HELP,
            "start" => TASK_START_HELP,
            "retry" => TASK_RETRY_HELP,
            "assign" => TASK_ASSIGN_HELP,
            "cancel" => TASK_CANCEL_HELP,
            "update" => TASK_UPDATE_HELP,
            "delete" => TASK_DELETE_HELP,
            _ => TASK_HELP,
        }));
    }
    match action.as_str() {
        "add" => {
            let id = take_option(&mut args, "--id")?;
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
                id,
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
        "start" => {
            let project_id = required_option(&mut args, "--project")?;
            let task_id = required_option(&mut args, "--task")?;
            let agent_id = required_option(&mut args, "--agent")?;
            let parent_run_id = take_option(&mut args, "--parent-run")?;
            let worktree = required_option(&mut args, "--worktree")?;
            require_empty(&args)?;
            Ok(CliCommand::TaskStart {
                project_id,
                task_id,
                agent_id,
                parent_run_id,
                worktree,
            })
        }
        "retry" => {
            let project_id = required_option(&mut args, "--project")?;
            let task_id = required_option(&mut args, "--task")?;
            require_empty(&args)?;
            Ok(CliCommand::TaskRetry {
                project_id,
                task_id,
            })
        }
        "assign" => {
            let project_id = required_option(&mut args, "--project")?;
            let task_id = required_option(&mut args, "--task")?;
            let agent_id = take_option(&mut args, "--agent")?;
            require_empty(&args)?;
            Ok(CliCommand::TaskAssign {
                project_id,
                task_id,
                agent_id,
            })
        }
        "get" => {
            let project_id = required_option(&mut args, "--project")?;
            let task_id = required_option(&mut args, "--task")?;
            require_empty(&args)?;
            Ok(CliCommand::TaskGet {
                project_id,
                task_id,
            })
        }
        "cancel" => {
            let project_id = required_option(&mut args, "--project")?;
            let task_id = required_option(&mut args, "--task")?;
            require_empty(&args)?;
            Ok(CliCommand::TaskCancel {
                project_id,
                task_id,
            })
        }
        "update" => {
            let project_id = required_option(&mut args, "--project")?;
            let task_id = required_option(&mut args, "--task")?;
            let title = take_option(&mut args, "--title")?;
            let body = take_option(&mut args, "--body")?;
            require_empty(&args)?;
            if title.is_none() && body.is_none() {
                return Err("task update requires --title or --body".into());
            }
            Ok(CliCommand::TaskUpdate {
                project_id,
                task_id,
                title,
                body,
            })
        }
        "delete" => {
            let project_id = required_option(&mut args, "--project")?;
            let task_id = required_option(&mut args, "--task")?;
            require_empty(&args)?;
            Ok(CliCommand::TaskDelete {
                project_id,
                task_id,
            })
        }
        _ => Err(format!("unknown task action {action:?}")),
    }
}

fn parse_attach(mut args: Vec<String>) -> Result<CliCommand, String> {
    let project_id = required_option(&mut args, "--project")?;
    // `--run` is a deprecated alias kept during the transition to resident
    // sessions (a session's id is currently its run's id; see
    // `local_api.rs`'s `resolve_transitional_run_id`).
    let session = take_option(&mut args, "--session")?;
    let run = take_option(&mut args, "--run")?;
    let session_id = match (session, run) {
        (Some(session_id), None) => session_id,
        (None, Some(run_id)) => run_id,
        (Some(_), Some(_)) => {
            return Err("--session and --run may not both be provided".into());
        }
        (None, None) => return Err("--session is required".into()),
    };
    let since_offset = take_option(&mut args, "--since-offset")?
        .map(|value| parse_number(&value, "--since-offset"))
        .transpose()?
        .unwrap_or(0);
    require_empty(&args)?;
    Ok(CliCommand::Attach {
        project_id,
        session_id,
        since_offset,
    })
}

fn parse_agent(mut args: Vec<String>) -> Result<CliCommand, String> {
    if args.is_empty() || is_help_flag(&args[0]) {
        return Ok(CliCommand::Help(AGENT_HELP));
    }
    let action = take_action(&mut args, "agent")?;
    if wants_help(&args) {
        return Ok(CliCommand::Help(match action.as_str() {
            "add" => AGENT_ADD_HELP,
            "list" => AGENT_LIST_HELP,
            "delete" => AGENT_DELETE_HELP,
            "get" => AGENT_GET_HELP,
            "profile" => AGENT_PROFILE_HELP,
            "message" => AGENT_MESSAGE_HELP,
            "inbox" => AGENT_INBOX_HELP,
            _ => AGENT_HELP,
        }));
    }
    match action.as_str() {
        "add" => {
            let id = take_option(&mut args, "--id")?;
            let project_id = required_option(&mut args, "--project")?;
            let parent_agent_id = take_option(&mut args, "--parent")?;
            let role = match required_option(&mut args, "--role")?.as_str() {
                "orchestrator" => AgentRole::Orchestrator,
                "worker" => AgentRole::Worker,
                _ => return Err("--role must be orchestrator or worker".into()),
            };
            let provider = match required_option(&mut args, "--provider")?.as_str() {
                "claude" | "claude-code" => Provider::ClaudeCode,
                "codex" => Provider::Codex,
                _ => return Err("--provider must be claude or codex".into()),
            };
            let model = take_option(&mut args, "--model")?;
            require_empty(&args)?;
            Ok(CliCommand::AgentAdd {
                id,
                project_id,
                parent_agent_id,
                role,
                provider,
                model,
            })
        }
        "list" => {
            let project_id = required_option(&mut args, "--project")?;
            let after_id = take_option(&mut args, "--after")?;
            let (limit, _) = take_limit(&mut args, AGENT_LIST_LIMIT, MAX_AGENT_PAGE_ITEMS)?;
            require_empty(&args)?;
            Ok(CliCommand::AgentList {
                project_id,
                after_id,
                limit,
            })
        }
        "get" => {
            let project_id = required_option(&mut args, "--project")?;
            let agent_id = required_option(&mut args, "--agent")?;
            require_empty(&args)?;
            Ok(CliCommand::AgentGet {
                project_id,
                agent_id,
            })
        }
        "profile" => {
            let sub_action = take_action(&mut args, "agent profile")?;
            match sub_action.as_str() {
                "set" => {
                    let project_id = required_option(&mut args, "--project")?;
                    let agent_id = required_option(&mut args, "--agent")?;
                    let model = take_option(&mut args, "--model")?;
                    let permission_mode = take_option(&mut args, "--permission-mode")?;
                    let instructions_file = take_option(&mut args, "--instructions-file")?;
                    let memory_file = take_option(&mut args, "--memory-file")?;
                    require_empty(&args)?;
                    Ok(CliCommand::AgentProfileSet {
                        project_id,
                        agent_id,
                        model,
                        permission_mode,
                        instructions_file,
                        memory_file,
                    })
                }
                _ => Err(format!("unknown agent profile action {sub_action:?}")),
            }
        }
        "message" => {
            let id = take_option(&mut args, "--id")?;
            let project_id = required_option(&mut args, "--project")?;
            let sender_agent_id = take_option(&mut args, "--from")?;
            let recipient_agent_id = required_option(&mut args, "--to")?;
            let body = required_option(&mut args, "--body")?;
            require_empty(&args)?;
            Ok(CliCommand::AgentMessage {
                id,
                project_id,
                sender_agent_id,
                recipient_agent_id,
                body,
            })
        }
        "inbox" => {
            let project_id = required_option(&mut args, "--project")?;
            let agent_id = required_option(&mut args, "--agent")?;
            let after_id = take_option(&mut args, "--after")?;
            let (limit, _) = take_limit(&mut args, AGENT_LIST_LIMIT, MAX_AGENT_PAGE_ITEMS)?;
            require_empty(&args)?;
            Ok(CliCommand::AgentInbox {
                project_id,
                agent_id,
                after_id,
                limit,
            })
        }
        "delete" => {
            let project_id = required_option(&mut args, "--project")?;
            let agent_id = required_option(&mut args, "--agent")?;
            require_empty(&args)?;
            Ok(CliCommand::AgentDelete {
                project_id,
                agent_id,
            })
        }
        _ => Err(format!("unknown agent action {action:?}")),
    }
}

fn parse_run(mut args: Vec<String>) -> Result<CliCommand, String> {
    if args.is_empty() || is_help_flag(&args[0]) {
        return Ok(CliCommand::Help(RUN_HELP));
    }
    let action = take_action(&mut args, "run")?;
    if wants_help(&args) {
        return Ok(CliCommand::Help(match action.as_str() {
            "list" => RUN_LIST_HELP,
            "stop" => RUN_STOP_HELP,
            _ => RUN_HELP,
        }));
    }
    match action.as_str() {
        "list" => {
            let project_id = required_option(&mut args, "--project")?;
            let after_id = take_option(&mut args, "--after")?;
            let (limit, _) = take_limit(&mut args, RUN_LIST_LIMIT, MAX_RUN_PAGE_ITEMS)?;
            require_empty(&args)?;
            Ok(CliCommand::RunList {
                project_id,
                after_id,
                limit,
            })
        }
        "stop" => {
            let project_id = required_option(&mut args, "--project")?;
            let run_id = required_option(&mut args, "--run")?;
            let grace_ms = take_option(&mut args, "--grace-ms")?
                .map(|value| parse_number(&value, "--grace-ms"))
                .transpose()?
                .unwrap_or(0u64);
            require_empty(&args)?;
            Ok(CliCommand::RunStop {
                project_id,
                run_id,
                grace_ms,
            })
        }
        _ => Err(format!("unknown run action {action:?}")),
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
        CliCommand::Help(_) => Err("help is not a daemon request".into()),
        CliCommand::Health => Ok(LocalRequest::Health),
        CliCommand::Usage => Err("usage is handled before local requests".into()),
        CliCommand::ProjectAdd { id, name, root } => Ok(LocalRequest::CreateProject {
            id: id
                .map(|id| parse_id(id, "project"))
                .transpose()?
                .unwrap_or(generated_id()?),
            name,
            root,
        }),
        CliCommand::ProjectList { after_id, limit } => Ok(LocalRequest::ListProjects {
            after_id: after_id
                .map(|id| parse_id(id, "project cursor"))
                .transpose()?,
            limit,
        }),
        CliCommand::ProjectDelete { project_id } => Ok(LocalRequest::DeleteProject {
            project_id: parse_id(project_id, "project")?,
        }),
        CliCommand::ProjectGet { project_id } => Ok(LocalRequest::GetProject {
            project_id: parse_id(project_id, "project")?,
        }),
        CliCommand::ProjectGuidanceSet { project_id, file } => {
            Ok(LocalRequest::UpdateProjectGuidance {
                project_id: parse_id(project_id, "project")?,
                text: read_guidance_file(&file)?,
            })
        }
        CliCommand::TaskAdd {
            id,
            project_id,
            parent_task_id,
            title,
            body,
            priority,
        } => Ok(LocalRequest::CreateTask {
            id: id
                .map(|id| parse_id(id, "task"))
                .transpose()?
                .unwrap_or(generated_id()?),
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
        CliCommand::TaskStart {
            project_id,
            task_id,
            agent_id,
            parent_run_id,
            worktree,
        } => Ok(LocalRequest::StartTask {
            project_id: parse_id(project_id, "project")?,
            task_id: parse_id(task_id, "task")?,
            agent_id: parse_id(agent_id, "agent")?,
            parent_run_id: parent_run_id
                .map(|id| parse_id(id, "parent run"))
                .transpose()?,
            worktree: Some(worktree),
        }),
        CliCommand::Attach { .. } => Err("attach is handled before local requests".into()),
        CliCommand::TaskRetry {
            project_id,
            task_id,
        } => Ok(LocalRequest::RetryTask {
            project_id: parse_id(project_id, "project")?,
            task_id: parse_id(task_id, "task")?,
        }),
        CliCommand::TaskAssign {
            project_id,
            task_id,
            agent_id,
        } => Ok(LocalRequest::AssignTask {
            project_id: parse_id(project_id, "project")?,
            task_id: parse_id(task_id, "task")?,
            agent_id: agent_id.map(|id| parse_id(id, "agent")).transpose()?,
        }),
        CliCommand::TaskGet {
            project_id,
            task_id,
        } => Ok(LocalRequest::GetTask {
            project_id: parse_id(project_id, "project")?,
            task_id: parse_id(task_id, "task")?,
        }),
        CliCommand::TaskCancel {
            project_id,
            task_id,
        } => Ok(LocalRequest::CancelTask {
            project_id: parse_id(project_id, "project")?,
            task_id: parse_id(task_id, "task")?,
        }),
        CliCommand::TaskUpdate {
            project_id,
            task_id,
            title,
            body,
        } => Ok(LocalRequest::UpdateTask {
            project_id: parse_id(project_id, "project")?,
            task_id: parse_id(task_id, "task")?,
            title,
            body,
        }),
        CliCommand::TaskDelete {
            project_id,
            task_id,
        } => Ok(LocalRequest::DeleteTask {
            project_id: parse_id(project_id, "project")?,
            task_id: parse_id(task_id, "task")?,
        }),
        CliCommand::AgentAdd {
            id,
            project_id,
            parent_agent_id,
            role,
            provider,
            model,
        } => Ok(LocalRequest::CreateAgent {
            id: id
                .map(|id| parse_id(id, "agent"))
                .transpose()?
                .unwrap_or(generated_id()?),
            project_id: parse_id(project_id, "project")?,
            parent_agent_id: parent_agent_id
                .map(|id| parse_id(id, "parent agent"))
                .transpose()?,
            role,
            provider,
            model,
            worktree: None,
        }),
        CliCommand::AgentList {
            project_id,
            after_id,
            limit,
        } => Ok(LocalRequest::ListAgents {
            project_id: parse_id(project_id, "project")?,
            after_id: after_id
                .map(|id| parse_id(id, "agent cursor"))
                .transpose()?,
            limit,
        }),
        CliCommand::AgentGet {
            project_id,
            agent_id,
        } => Ok(LocalRequest::GetAgent {
            project_id: parse_id(project_id, "project")?,
            agent_id: parse_id(agent_id, "agent")?,
        }),
        CliCommand::AgentProfileSet { .. } => {
            Err("agent profile set is resolved before the daemon request".into())
        }
        CliCommand::AgentMessage {
            id,
            project_id,
            sender_agent_id,
            recipient_agent_id,
            body,
        } => Ok(LocalRequest::SendAgentMessage {
            id: id
                .map(|id| parse_id(id, "message"))
                .transpose()?
                .unwrap_or(generated_id()?),
            project_id: parse_id(project_id, "project")?,
            sender_agent_id: sender_agent_id
                .map(|id| parse_id(id, "sender agent"))
                .transpose()?,
            recipient_agent_id: parse_id(recipient_agent_id, "recipient agent")?,
            body,
        }),
        CliCommand::AgentInbox {
            project_id,
            agent_id,
            after_id,
            limit,
        } => Ok(LocalRequest::ListAgentMessages {
            project_id: parse_id(project_id, "project")?,
            agent_id: parse_id(agent_id, "agent")?,
            after_id: after_id
                .map(|id| parse_id(id, "message cursor"))
                .transpose()?,
            limit,
        }),
        CliCommand::AgentDelete {
            project_id,
            agent_id,
        } => Ok(LocalRequest::DeleteAgent {
            project_id: parse_id(project_id, "project")?,
            agent_id: parse_id(agent_id, "agent")?,
        }),
        CliCommand::RunList {
            project_id,
            after_id,
            limit,
        } => Ok(LocalRequest::ListRuns {
            project_id: parse_id(project_id, "project")?,
            after_id: after_id.map(|id| parse_id(id, "run cursor")).transpose()?,
            limit,
        }),
        CliCommand::RunStop {
            project_id,
            run_id,
            grace_ms,
        } => Ok(LocalRequest::StopRun {
            project_id: parse_id(project_id, "project")?,
            run_id: parse_id(run_id, "run")?,
            grace_ms,
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

    use factory_core::{AgentRole, Provider, local::LocalRequest};
    use uuid::Uuid;

    use super::*;

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
    fn help_is_available_without_a_daemon_connection() {
        assert_eq!(
            parse_args(args(&["--help"])).unwrap(),
            (None, CliCommand::Help(HELP))
        );
        assert_eq!(
            parse_args(args(&["help"])).unwrap(),
            (None, CliCommand::Help(HELP))
        );
        assert_eq!(
            parse_args(args(&["-h"])).unwrap(),
            (None, CliCommand::Help(HELP))
        );
    }

    #[test]
    fn every_group_and_subcommand_has_its_own_help_text() {
        assert_eq!(
            parse_args(args(&["health", "--help"])).unwrap().1,
            CliCommand::Help(HEALTH_HELP)
        );
        assert_eq!(
            parse_args(args(&["usage", "--help"])).unwrap().1,
            CliCommand::Help(USAGE_HELP)
        );
        assert_eq!(
            parse_args(args(&["events", "--help"])).unwrap().1,
            CliCommand::Help(EVENTS_HELP)
        );
        assert_eq!(
            parse_args(args(&["attach", "--help"])).unwrap().1,
            CliCommand::Help(ATTACH_HELP)
        );

        assert_eq!(
            parse_args(args(&["project"])).unwrap().1,
            CliCommand::Help(PROJECT_HELP)
        );
        assert_eq!(
            parse_args(args(&["project", "--help"])).unwrap().1,
            CliCommand::Help(PROJECT_HELP)
        );
        assert_eq!(
            parse_args(args(&["project", "add", "--help"])).unwrap().1,
            CliCommand::Help(PROJECT_ADD_HELP)
        );
        assert_eq!(
            parse_args(args(&["project", "list", "-h"])).unwrap().1,
            CliCommand::Help(PROJECT_LIST_HELP)
        );
        assert_eq!(
            parse_args(args(&["project", "delete", "--help"]))
                .unwrap()
                .1,
            CliCommand::Help(PROJECT_DELETE_HELP)
        );
        assert_eq!(
            parse_args(args(&["project", "get", "--help"])).unwrap().1,
            CliCommand::Help(PROJECT_GET_HELP)
        );
        assert_eq!(
            parse_args(args(&["project", "guidance", "--help"]))
                .unwrap()
                .1,
            CliCommand::Help(PROJECT_GUIDANCE_HELP)
        );
        assert_eq!(
            parse_args(args(&["project", "guidance", "set", "--help"]))
                .unwrap()
                .1,
            CliCommand::Help(PROJECT_GUIDANCE_HELP)
        );

        assert_eq!(
            parse_args(args(&["task"])).unwrap().1,
            CliCommand::Help(TASK_HELP)
        );
        for (action, expected) in [
            ("add", TASK_ADD_HELP),
            ("list", TASK_LIST_HELP),
            ("get", TASK_GET_HELP),
            ("start", TASK_START_HELP),
            ("retry", TASK_RETRY_HELP),
            ("assign", TASK_ASSIGN_HELP),
            ("cancel", TASK_CANCEL_HELP),
            ("update", TASK_UPDATE_HELP),
            ("delete", TASK_DELETE_HELP),
        ] {
            assert_eq!(
                parse_args(args(&["task", action, "--help"])).unwrap().1,
                CliCommand::Help(expected),
                "task {action} --help"
            );
        }

        assert_eq!(
            parse_args(args(&["agent"])).unwrap().1,
            CliCommand::Help(AGENT_HELP)
        );
        for (action, expected) in [
            ("add", AGENT_ADD_HELP),
            ("list", AGENT_LIST_HELP),
            ("delete", AGENT_DELETE_HELP),
            ("get", AGENT_GET_HELP),
            ("profile", AGENT_PROFILE_HELP),
            ("message", AGENT_MESSAGE_HELP),
            ("inbox", AGENT_INBOX_HELP),
        ] {
            assert_eq!(
                parse_args(args(&["agent", action, "--help"])).unwrap().1,
                CliCommand::Help(expected),
                "agent {action} --help"
            );
        }
        assert_eq!(
            parse_args(args(&["agent", "profile", "set", "--help"]))
                .unwrap()
                .1,
            CliCommand::Help(AGENT_PROFILE_HELP)
        );

        assert_eq!(
            parse_args(args(&["run"])).unwrap().1,
            CliCommand::Help(RUN_HELP)
        );
        for (action, expected) in [("list", RUN_LIST_HELP), ("stop", RUN_STOP_HELP)] {
            assert_eq!(
                parse_args(args(&["run", action, "--help"])).unwrap().1,
                CliCommand::Help(expected),
                "run {action} --help"
            );
        }
    }

    #[test]
    fn help_does_not_require_otherwise_required_options() {
        // --help must short-circuit before required-option validation.
        let (_, command) = parse_args(args(&["task", "add", "--help"])).unwrap();
        assert_eq!(command, CliCommand::Help(TASK_ADD_HELP));
        let (_, command) = parse_args(args(&["run", "stop", "--help"])).unwrap();
        assert_eq!(command, CliCommand::Help(RUN_STOP_HELP));
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
                    id: None,
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
                    id: None,
                    project_id: "project-1".into(),
                    parent_task_id: Some("task-0".into()),
                    title: "Build client".into(),
                    body: "Use the socket".into(),
                    priority: 7,
                }
            )
        );
        assert_eq!(
            parse_args(args(&[
                "task",
                "assign",
                "--project",
                "project-1",
                "--task",
                "task-1",
                "--agent",
                "curie",
            ]))
            .unwrap(),
            (
                None,
                CliCommand::TaskAssign {
                    project_id: "project-1".into(),
                    task_id: "task-1".into(),
                    agent_id: Some("curie".into()),
                }
            )
        );
    }

    #[test]
    fn parses_explicit_agent_creation_and_task_start_commands() {
        assert_eq!(
            parse_args(args(&[
                "agent",
                "add",
                "--project",
                "project-1",
                "--parent",
                "agent-parent",
                "--role",
                "worker",
                "--provider",
                "codex",
                "--model",
                "gpt-5-codex",
            ]))
            .unwrap(),
            (
                None,
                CliCommand::AgentAdd {
                    id: None,
                    project_id: "project-1".into(),
                    parent_agent_id: Some("agent-parent".into()),
                    role: AgentRole::Worker,
                    provider: Provider::Codex,
                    model: Some("gpt-5-codex".into()),
                }
            )
        );
        assert_eq!(
            parse_args(args(&[
                "task",
                "start",
                "--project",
                "project-1",
                "--task",
                "task-1",
                "--agent",
                "agent-1",
                "--worktree",
                "/work/agent-1",
                "--parent-run",
                "run-parent",
            ]))
            .unwrap(),
            (
                None,
                CliCommand::TaskStart {
                    project_id: "project-1".into(),
                    task_id: "task-1".into(),
                    agent_id: "agent-1".into(),
                    parent_run_id: Some("run-parent".into()),
                    worktree: "/work/agent-1".into(),
                }
            )
        );

        assert_eq!(
            parse_args(args(&[
                "agent",
                "add",
                "--project",
                "project-1",
                "--role",
                "god",
            ]))
            .unwrap_err(),
            "--role must be orchestrator or worker"
        );
    }

    #[test]
    fn agent_get_and_project_get_are_bounded_local_reads() {
        assert_eq!(
            parse_args(args(&["project", "get", "--project", "factory"])).unwrap(),
            (
                None,
                CliCommand::ProjectGet {
                    project_id: "factory".into(),
                }
            )
        );
        assert_eq!(
            parse_args(args(&[
                "agent",
                "get",
                "--project",
                "factory",
                "--agent",
                "god"
            ]))
            .unwrap(),
            (
                None,
                CliCommand::AgentGet {
                    project_id: "factory".into(),
                    agent_id: "god".into(),
                }
            )
        );
        let (_, request) = parse_args(args(&["project", "get", "--project", "factory"])).unwrap();
        assert!(matches!(
            request_for(request).unwrap(),
            LocalRequest::GetProject { project_id } if project_id == "factory".try_into().unwrap()
        ));
    }

    #[test]
    fn attach_parses_project_session_and_defaults_since_offset_to_zero() {
        assert_eq!(
            parse_args(args(&[
                "attach",
                "--project",
                "project-1",
                "--session",
                "session-1"
            ]))
            .unwrap(),
            (
                None,
                CliCommand::Attach {
                    project_id: "project-1".into(),
                    session_id: "session-1".into(),
                    since_offset: 0,
                }
            )
        );
        assert_eq!(
            parse_args(args(&[
                "attach",
                "--project",
                "project-1",
                "--session",
                "session-1",
                "--since-offset",
                "4096",
            ]))
            .unwrap(),
            (
                None,
                CliCommand::Attach {
                    project_id: "project-1".into(),
                    session_id: "session-1".into(),
                    since_offset: 4096,
                }
            )
        );
        assert!(parse_args(args(&["attach", "--session", "session-1"])).is_err());
        assert!(
            request_for(CliCommand::Attach {
                project_id: "project-1".into(),
                session_id: "session-1".into(),
                since_offset: 0,
            })
            .is_err()
        );
    }

    #[test]
    fn attach_accepts_run_as_a_deprecated_alias_for_session() {
        assert_eq!(
            parse_args(args(&[
                "attach",
                "--project",
                "project-1",
                "--run",
                "run-1"
            ]))
            .unwrap(),
            (
                None,
                CliCommand::Attach {
                    project_id: "project-1".into(),
                    session_id: "run-1".into(),
                    since_offset: 0,
                }
            )
        );
        assert!(
            parse_args(args(&[
                "attach",
                "--project",
                "project-1",
                "--session",
                "session-1",
                "--run",
                "run-1"
            ]))
            .is_err()
        );
    }

    #[test]
    fn project_guidance_set_reads_the_file_into_the_local_request() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "# Project\n\nBuild the thing.\n").unwrap();
        let path = file.path().to_str().unwrap().to_owned();
        let (_, command) = parse_args(args(&[
            "project",
            "guidance",
            "set",
            "--project",
            "factory",
            "--file",
            &path,
        ]))
        .unwrap();
        assert_eq!(
            command,
            CliCommand::ProjectGuidanceSet {
                project_id: "factory".into(),
                file: path,
            }
        );
        assert!(matches!(
            request_for(command).unwrap(),
            LocalRequest::UpdateProjectGuidance { project_id, text }
                if project_id == "factory".try_into().unwrap()
                    && text == "# Project\n\nBuild the thing.\n"
        ));
    }

    #[test]
    fn agent_profile_set_parses_every_optional_flag() {
        let instructions = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(instructions.path(), "Coordinate the team.").unwrap();
        let memory = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(memory.path(), "Prefer small, reversible slices.").unwrap();
        assert_eq!(
            parse_args(args(&[
                "agent",
                "profile",
                "set",
                "--project",
                "factory",
                "--agent",
                "god",
                "--model",
                "gpt-5-codex",
                "--permission-mode",
                "on-request",
                "--instructions-file",
                instructions.path().to_str().unwrap(),
                "--memory-file",
                memory.path().to_str().unwrap(),
            ]))
            .unwrap(),
            (
                None,
                CliCommand::AgentProfileSet {
                    project_id: "factory".into(),
                    agent_id: "god".into(),
                    model: Some("gpt-5-codex".into()),
                    permission_mode: Some("on-request".into()),
                    instructions_file: Some(instructions.path().to_str().unwrap().into()),
                    memory_file: Some(memory.path().to_str().unwrap().into()),
                }
            )
        );
        assert_eq!(
            parse_args(args(&[
                "agent",
                "profile",
                "set",
                "--project",
                "factory",
                "--agent",
                "god",
            ]))
            .unwrap(),
            (
                None,
                CliCommand::AgentProfileSet {
                    project_id: "factory".into(),
                    agent_id: "god".into(),
                    model: None,
                    permission_mode: None,
                    instructions_file: None,
                    memory_file: None,
                }
            )
        );
    }

    #[test]
    fn agent_message_and_inbox_commands_use_the_shared_local_channel() {
        let (_, message) = parse_args(args(&[
            "agent",
            "message",
            "--project",
            "factory",
            "--from",
            "god",
            "--to",
            "worker",
            "--body",
            "Please report your result.",
        ]))
        .unwrap();
        let request = request_for(message).unwrap();
        assert!(matches!(
            request,
            LocalRequest::SendAgentMessage {
                sender_agent_id: Some(sender),
                recipient_agent_id,
                body,
                ..
            } if sender == "god".try_into().unwrap()
                && recipient_agent_id == "worker".try_into().unwrap()
                && body == "Please report your result."
        ));

        let (_, inbox) = parse_args(args(&[
            "agent",
            "inbox",
            "--project",
            "factory",
            "--agent",
            "worker",
        ]))
        .unwrap();
        assert!(matches!(
            request_for(inbox).unwrap(),
            LocalRequest::ListAgentMessages { .. }
        ));
    }

    #[test]
    fn agent_ids_are_client_generated_but_run_ids_are_daemon_generated() {
        let request = request_for(CliCommand::AgentAdd {
            id: None,
            project_id: "project-1".into(),
            parent_agent_id: None,
            role: AgentRole::Orchestrator,
            provider: Provider::Codex,
            model: None,
        })
        .unwrap();
        let LocalRequest::CreateAgent { id, role, .. } = request else {
            panic!("expected create agent request");
        };
        assert!(Uuid::parse_str(id.as_str()).is_ok());
        assert_eq!(role, AgentRole::Orchestrator);

        let request = request_for(CliCommand::TaskStart {
            project_id: "project-1".into(),
            task_id: "task-1".into(),
            agent_id: "agent-1".into(),
            parent_run_id: None,
            worktree: "/work/agent-1".into(),
        })
        .unwrap();
        assert!(matches!(request, LocalRequest::StartTask { .. }));
    }

    #[test]
    fn task_assignment_command_maps_agent_and_operator_queue() {
        let (_, assigned) = parse_args(args(&[
            "task",
            "assign",
            "--project",
            "project-1",
            "--task",
            "task-1",
            "--agent",
            "curie",
        ]))
        .unwrap();
        assert_eq!(
            request_for(assigned).unwrap(),
            LocalRequest::AssignTask {
                project_id: "project-1".try_into().unwrap(),
                task_id: "task-1".try_into().unwrap(),
                agent_id: Some("curie".try_into().unwrap()),
            }
        );

        let (_, unassigned) = parse_args(args(&[
            "task",
            "assign",
            "--project",
            "project-1",
            "--task",
            "task-1",
        ]))
        .unwrap();
        assert_eq!(
            request_for(unassigned).unwrap(),
            LocalRequest::AssignTask {
                project_id: "project-1".try_into().unwrap(),
                task_id: "task-1".try_into().unwrap(),
                agent_id: None,
            }
        );
    }

    #[test]
    fn events_follow_is_an_explicit_subscription() {
        let (_, command) = parse_args(args(&["events", "--after", "12", "--follow"])).unwrap();
        assert_eq!(
            command,
            CliCommand::Events {
                after_sequence: 12,
                limit: EVENT_LIST_LIMIT,
                follow: true,
            }
        );
    }

    #[test]
    fn usage_parses_with_no_arguments() {
        let (_, command) = parse_args(args(&["usage"])).unwrap();
        assert_eq!(command, CliCommand::Usage);
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

        assert!(
            parse_args(args(&[
                "project",
                "list",
                "--limit",
                &(MAX_PROJECT_PAGE_ITEMS + 1).to_string(),
            ]))
            .is_err()
        );
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
            id: None,
            name: "Dark Factory".into(),
            root: "/work/dark-factory".into(),
        })
        .unwrap();

        let LocalRequest::CreateProject { id, .. } = request else {
            panic!("expected create project request");
        };
        assert!(Uuid::parse_str(id.as_str()).is_ok());
    }

    #[test]
    fn task_control_commands_parse_and_map_to_new_requests() {
        let (_, command) = parse_args(args(&[
            "task",
            "cancel",
            "--project",
            "project-1",
            "--task",
            "task-1",
        ]))
        .unwrap();
        assert_eq!(
            request_for(command).unwrap(),
            LocalRequest::CancelTask {
                project_id: "project-1".try_into().unwrap(),
                task_id: "task-1".try_into().unwrap(),
            }
        );

        let (_, command) = parse_args(args(&[
            "task",
            "get",
            "--project",
            "project-1",
            "--task",
            "task-1",
        ]))
        .unwrap();
        assert_eq!(
            request_for(command).unwrap(),
            LocalRequest::GetTask {
                project_id: "project-1".try_into().unwrap(),
                task_id: "task-1".try_into().unwrap(),
            }
        );

        let (_, command) = parse_args(args(&[
            "task",
            "delete",
            "--project",
            "project-1",
            "--task",
            "task-1",
        ]))
        .unwrap();
        assert_eq!(
            request_for(command).unwrap(),
            LocalRequest::DeleteTask {
                project_id: "project-1".try_into().unwrap(),
                task_id: "task-1".try_into().unwrap(),
            }
        );

        let (_, command) = parse_args(args(&[
            "task",
            "update",
            "--project",
            "project-1",
            "--task",
            "task-1",
            "--title",
            "New",
        ]))
        .unwrap();
        assert_eq!(
            request_for(command).unwrap(),
            LocalRequest::UpdateTask {
                project_id: "project-1".try_into().unwrap(),
                task_id: "task-1".try_into().unwrap(),
                title: Some("New".into()),
                body: None,
            }
        );

        let error = parse_args(args(&[
            "task",
            "update",
            "--project",
            "project-1",
            "--task",
            "task-1",
        ]))
        .unwrap_err();
        assert_eq!(error, "task update requires --title or --body");
    }

    #[test]
    fn agent_and_project_delete_commands_parse_and_map_to_new_requests() {
        let (_, command) = parse_args(args(&[
            "agent",
            "delete",
            "--project",
            "project-1",
            "--agent",
            "agent-1",
        ]))
        .unwrap();
        assert_eq!(
            request_for(command).unwrap(),
            LocalRequest::DeleteAgent {
                project_id: "project-1".try_into().unwrap(),
                agent_id: "agent-1".try_into().unwrap(),
            }
        );

        let (_, command) =
            parse_args(args(&["project", "delete", "--project", "project-1"])).unwrap();
        assert_eq!(
            request_for(command).unwrap(),
            LocalRequest::DeleteProject {
                project_id: "project-1".try_into().unwrap(),
            }
        );
    }

    #[test]
    fn run_stop_command_parses_grace_and_defaults_to_zero() {
        let (_, command) = parse_args(args(&[
            "run",
            "stop",
            "--project",
            "project-1",
            "--run",
            "run-1",
        ]))
        .unwrap();
        assert_eq!(
            request_for(command).unwrap(),
            LocalRequest::StopRun {
                project_id: "project-1".try_into().unwrap(),
                run_id: "run-1".try_into().unwrap(),
                grace_ms: 0,
            }
        );

        let (_, command) = parse_args(args(&[
            "run",
            "stop",
            "--project",
            "project-1",
            "--run",
            "run-1",
            "--grace-ms",
            "2500",
        ]))
        .unwrap();
        assert_eq!(
            request_for(command).unwrap(),
            LocalRequest::StopRun {
                project_id: "project-1".try_into().unwrap(),
                run_id: "run-1".try_into().unwrap(),
                grace_ms: 2500,
            }
        );
    }
}
