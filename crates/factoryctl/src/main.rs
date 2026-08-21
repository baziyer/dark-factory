use std::{env, io::Write, path::PathBuf, process};

use factory_core::local::{
    GuidanceHealthState, LocalRequest, LocalResponse, MAX_AGENT_PAGE_ITEMS, MAX_CHANGE_PAGE_ITEMS,
    MAX_LEGACY_SOURCE_PAGE_ITEMS, MAX_PROJECT_PAGE_ITEMS, MAX_RUN_PAGE_ITEMS, MAX_TASK_PAGE_ITEMS,
    RequestCredential, ServerFrame,
};
use factory_core::{AgentRole, Provider, ProviderHookEvent};
use factoryctl::{Client, capacity};
use uuid::Uuid;

mod doctor;
mod events;
mod init;
mod status;
mod update_command;
mod usage;

const ATTEMPT_TOKEN_FILE_ENV: &str = "DARK_FACTORY_ATTEMPT_TOKEN_FILE";

const USAGE: &str = "usage: factoryctl [--socket PATH] <health|status|auto|capacity|init|doctor|update|version|usage|project|task|agent|run|change|hook|events> ...";
const HELP: &str = "Dark Factory local control plane

Run the daemon separately (launchd keeps it alive), then run `factory-tui` in a persistent terminal.

Commands:
  health                                      Check the daemon
  status [--json]                             The whole fleet at one instant: attempts, queues, attention, active-attempt cap
  auto on|off|status                         Set or show the factory-wide provider bypass default
  capacity status|set N                     Show or change the operator-owned active-attempt capacity (1..=64)
  init [--yes] [--no-launchd]                 Guided install: create the home, install these binaries, load the launchd job
  doctor [--json]                             Diagnose the install, one line each; exit 1 if any fail
  update [--install]                          Check for a newer release; --install downloads, verifies, and activates it
  version                                     Print this factoryctl's version
  usage                                       Probe Codex subscription usage on demand
  project add|list|delete|get|guidance        Manage projects and their guidance file
  task add|list|get|start|retry|assign|cancel|update|delete|done|blocked
                                               Manage and run tasks
  agent add|list|delete|get|profile|message|inbox|pause|resume
                                               Manage agents, their guidance files, and their durable messages
  run list|stop                               List and stop process attempts
  change list|remove|legacy-list|forget-legacy
                                               Inspect retained source and forget legacy metadata
  hook --token-file PATH <Event>              Forward one provider hook invocation to the daemon
  events [--follow]                           Read durable events

Run `factoryctl <command> --help` or `factoryctl <command> <action> --help`
for action-specific options.

Every `--project` may be omitted if `$DARK_FACTORY_PROJECT` is set (as it is
inside an attempt's own environment).

Options:
  --socket PATH                      Use an explicit local socket
  -h, --help                         Show this help";
const HEALTH_HELP: &str = "usage: factoryctl health

Check that the daemon is reachable and responding.";
const STATUS_HELP: &str = "usage: factoryctl status [--json]

A concise human summary of the whole daemon at one instant: projects,
agents, attempts, project backlogs, assigned worker queues, Changes, and
anything needing attention.
factory-tui reads the same request. For history, use the list commands.

Options:
  --json                       Print the complete protocol response frame
  -h, --help                   Show this help";

const CAPACITY_HELP: &str = "usage: factoryctl capacity <status|set N>

The capacity is a finite daemon-wide active-attempt bound. `set` is operator-only
(attempt shell policy denies this mutation),
requires the managed launchd job, shows that launchd will restart only factoryd
while preserving runner processes/attempt state, and rolls the plist back if the
reload or health check fails. Valid values are 1 through 64.";
const INIT_HELP: &str = "usage: factoryctl init [--yes] [--no-launchd]

Guided first install on this machine:
  1. create $DARK_FACTORY_HOME (default ~/.dark-factory) and its logs/ dir, mode 0700
  2. report whether claude, codex, and git resolve on PATH, and their versions
  3. install the binaries next to this factoryctl as $DARK_FACTORY_HOME/bin/<version>/
     and point bin/current at them (a bin/<version> holding a different build of the
     same version is refused, never overwritten)
  4. state that Dark Factory writes outside its home only for the launchd job, and
     ask before touching launchd
  5. render ~/Library/LaunchAgents/com.dark-factory.factoryd.plist with a PATH that
     can find those CLIs, load it, and wait for the daemon to answer health with
     this version
  6. show first-project next steps for an empty fleet, or status and the TUI when
     projects already exist

Re-running is safe: an existing job keeps its extra daemon arguments and
environment (its PATH is repaired if a provider CLI moved), an installed
version is not overwritten, and the daemon is restarted only when the job
is (re)loaded. A daemon started by hand on the same socket is refused --
stop it first -- so launchd's copy can't crash-loop behind it.

Options:
  --yes                      Skip the consent prompt (needed when stdin is not a terminal)
  --no-launchd               Install and activate the binaries only
  -h, --help                 Show this help";
const DOCTOR_HELP: &str = "usage: factoryctl doctor [--json]

Diagnostic checks, one line each: the home directory and its permissions,
the installed release under bin/, the daemon (reachable? same version as
this factoryctl?), the launchd job (installed, loaded, PATH covers claude/
codex?), claude/codex/git on PATH with versions, Codex credential seeding,
every configured project's source root, and
whether a newer release exists (may refresh the cached result, at most one
fetch per hour). This command does not repair or reconfigure the installation.
Exits 1 if any check fails; warnings don't change the exit code.

Options:
  --json                     One JSON object instead of text lines
  -h, --help                 Show this help";
const UPDATE_HELP: &str = "usage: factoryctl update [--install] [--json]

Fetch the newest release's manifest and report the invoking factoryctl,
the active bin/current runtime, and whether the release is newer than the
active runtime. Human-readable output is the default; --json preserves the
machine-readable object. The manifest result is also cached in
$DARK_FACTORY_HOME/update-check.json, which factory-tui's status line reads at
most hourly). With no active runtime, compare with the invoking factoryctl.

With --install: download that release for this platform, verify its SHA-256
against the manifest, unpack it into $DARK_FACTORY_HOME/bin/<version>/, and
repoint $DARK_FACTORY_HOME/bin/current at it. If this user's launchd job
(~/Library/LaunchAgents/com.dark-factory.factoryd.plist) exists it is
rewritten to run from bin/current (keeping its other arguments and
environment; PATH gains the provider CLIs' directories if missing) and
reloaded -- only the daemon restarts; every running attempt survives. The
job must already run with this $DARK_FACTORY_HOME (a scratch home is
refused rather than moving the job). Without a launchd job, restart the
daemon yourself afterwards. Nothing is deleted: a failed reload rolls
bin/current back; to roll back by hand, repoint bin/current at the
previous version directory and restart the daemon.

Exit status: 0 when up to date, or when the new daemon answers health with
the new version; 1 when the manifest can't be fetched (private repository,
offline) or the restarted daemon doesn't answer in time.

Options:
  --install                  Download, verify, and activate the latest needed release
  --json                     One JSON object instead of text lines
  -h, --help                 Show this help";
const USAGE_HELP: &str = "usage: factoryctl usage

Run a local Codex JSON-RPC probe against `codex` on PATH and print the
result. No daemon or socket is involved and nothing is persisted; Claude's
usage is read by running `/usage` inside Claude's own interactive terminal.";
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

Delete a project that has no non-terminal run and no retained managed Change
or legacy-source metadata. Cascades to delete every remaining task, agent, and
run in the project.

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
    "usage: factoryctl task <add|list|get|start|retry|assign|cancel|update|delete|done|blocked> [options]

Manage tasks within a project.

Actions:
  add       Create a new task
  list      List tasks in a project
  get       Fetch one task
  start     Start a queued task on an agent
  retry     Requeue a blocked, failed, or cancelled task
  reorder   Change a queued task's priority/order
  assign    Assign or return a queued task; assignment wakes delivery
  cancel    Cancel a queued or blocked task
  update    Edit a queued task's title or body
  delete    Delete a task that has no active run
  done      Request success for the authenticated attempt
  blocked   Request a block for the authenticated attempt

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
  --priority N             Priority (default: 0)
  --agent ID               Create directly in this agent's queue (atomic)
  -h, --help                 Show this help";
const TASK_LIST_HELP: &str = "usage: factoryctl task list --project ID [options]

List the active assigned queue in daemon-defined order. Use --history to
show terminal task history as a separate view.

Required:
  --project ID           Project to list tasks from

Options:
  --after ID               Resume after this task ID (requires --revision)
  --revision N             Revision returned with the previous page (requires --after)
  --agent ID               Show only tasks assigned to this agent
  --history                Include terminal task history
  --limit N                  Page size (default and max: 10)
  -h, --help                   Show this help";
const TASK_GET_HELP: &str = "usage: factoryctl task get --project ID --task ID

Fetch one task by ID.

Required:
  --project ID           Project the task belongs to
  --task ID              Task to fetch

Options:
  -h, --help              Show this help";
const TASK_START_HELP: &str = "usage: factoryctl task start --project ID --task ID --agent ID

Start a queued task on an idle agent.

Required:
  --project ID           Project the task belongs to
  --task ID              Task to start
  --agent ID             Agent to run it

Options:
  -h, --help                 Show this help";
const TASK_RETRY_HELP: &str = "usage: factoryctl task retry --project ID --task ID

Requeue a blocked, failed, or cancelled task.

Required:
  --project ID           Project the task belongs to
  --task ID              Task to retry

Options:
  -h, --help              Show this help";
const TASK_ASSIGN_HELP: &str = "usage: factoryctl task assign --project ID --task ID [--agent ID]

Assigning to an agent wakes automatic admission and may start an attempt.
Omit --agent to return the task to the operator.

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

Delete a task that has no non-terminal run, no retained Change, no subtasks,
and no run that is a parent of another run. Also deletes its terminal runs and
any rows that reference it (questions, dependencies, webhook capabilities).

Required:
  --project ID           Project the task belongs to
  --task ID              Task to delete

Options:
  -h, --help              Show this help";
const TASK_DONE_HELP: &str = "usage: factoryctl task done (--result TEXT | --result-file PATH)

Requests success for the exact running attempt identified by
DARK_FACTORY_ATTEMPT_TOKEN_FILE. The caller cannot select another task or run.

Required:
  --result TEXT          Result text (mutually exclusive with --result-file)
  --result-file PATH     Local file to read the result text from

Options:
  -h, --help              Show this help";
const TASK_BLOCKED_HELP: &str = "usage: factoryctl task blocked --reason TEXT

Requests a block for the exact running attempt identified by
DARK_FACTORY_ATTEMPT_TOKEN_FILE. The caller cannot select another task or run.

Required:
  --reason TEXT          Why the task is blocked (at most 4096 bytes)

Options:
  -h, --help              Show this help";

const AGENT_HELP: &str =
    "usage: factoryctl agent <add|list|delete|get|status|profile|budget|message|inbox|pause|resume> [options]

Manage agents within a project and their durable messages.

Actions:
  add       Create a new agent
  list      List agents in a project
  delete    Delete an agent that has no open run
  get       Fetch one agent, including its guidance file paths
  status    One agent's live picture: attempt, queue, inbox, budget, and attention
  profile   Manage an agent's model, permission mode, and guidance files
  budget    Show, set, or reset the agent's durable provider budget
  message   Send a durable message from one agent to another
  inbox     List an agent's durable messages
  pause     Durably hold an agent's queue: stop delivering new work into it
  resume    Undo `pause`

Run `factoryctl agent <action> --help` for action-specific options.";
const AGENT_ADD_HELP: &str =
    "usage: factoryctl agent add --project ID --role <orchestrator|worker> --provider <claude|codex|shell> [options]

Create a new agent.

Required:
  --project ID           Project the agent belongs to
  --role ROLE              orchestrator or worker
  --provider PROVIDER      claude (or claude-code), codex, or shell (minimal example provider)

Options:
  --id ID                    Explicit agent ID (default: generated UUID)
  --parent PARENT_ID         Parent agent ID
  --model MODEL               Provider model identifier for this agent
                               (shell provider: a command to run under
                               `sh -lc`, e.g. an absolute path to a script;
                               omitted means a plain interactive shell)
  --reasoning-effort TIER      Codex reasoning tier (none|low|medium|high|xhigh|max)
  --model-reason REASON        Auditable reason for an explicit model or escalation
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
const AGENT_STATUS_HELP: &str = "usage: factoryctl agent status --project ID --agent ID

One agent at one instant: its snapshot and profile (as `agent get`), its
active attempt and most recent terminal attempt with phase, outcome, observer
health, activity, and wait reason; the queued tasks assigned to it (first 10
listed, full depth alongside); undelivered inbox messages; and structured
bounded attention reasons with source IDs, age, and safe actions (the same
projection shown by `factoryctl status` and `factory-tui`). Memory is projected with
bounded health (`ok`, `near_limit`, `oversized`, `invalid_utf8`, or
`path_error`); unhealthy content is omitted, never allowed to hide status.

Required:
  --project ID           Project the agent belongs to
  --agent ID             Agent to inspect
Options:
  -h, --help             Show this help";
const AGENT_GET_HELP: &str = "usage: factoryctl agent get --project ID --agent ID

Fetch one agent, including the absolute paths of its `instructions.md` and
`memory.md`, plus current memory health.
Healthy memory content is included; oversized or invalid content is omitted
so the mechanical lookup still succeeds.

Required:
  --project ID           Project the agent belongs to
  --agent ID             Agent to fetch

Options:
  -h, --help              Show this help";
const AGENT_BUDGET_HELP: &str = "usage: factoryctl agent budget <status|set|reset> --project ID --agent ID [--max-tool-calls N|unlimited]

Tool calls are counted from authenticated PreToolUse hooks. The default is
1000 per agent. Monetary spend is unavailable because shipped providers do
not report trustworthy per-agent cost; Dark Factory never estimates it.";
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
  --reasoning-effort TIER            Codex reasoning tier (none|low|medium|high|xhigh|max)
  --model-reason REASON              Auditable reason for an explicit model or escalation
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
const AGENT_PAUSE_HELP: &str = "usage: factoryctl agent pause --project ID --agent ID

Durably holds this agent's queue: the daemon stops delivering new tasks or
messages into an attempt until `agent resume`. Its current attempt, if
any, keeps running; this only affects future delivery.

Required:
  --project ID           Project the agent belongs to
  --agent ID             Agent to pause

Options:
  -h, --help              Show this help";
const AGENT_RESUME_HELP: &str = "usage: factoryctl agent resume --project ID --agent ID

Undoes `agent pause`: the daemon resumes delivering queued work into this
agent's future attempt.

Required:
  --project ID           Project the agent belongs to
  --agent ID             Agent to resume

Options:
  -h, --help              Show this help";

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

const CHANGE_HELP: &str =
    "usage: factoryctl change <list|remove|legacy-list|forget-legacy> [options]

Inspect retained daemon-owned Change source trees, request exact removal, or
forget metadata for a source path retained by the pre-kernel architecture.

Run `factoryctl change <action> --help` for action-specific options.";
const CHANGE_LIST_HELP: &str = "usage: factoryctl change list --project ID [options]

List retained Changes, the project's last measured allocated bytes, and the
factory-wide retained count/cap.

Required:
  --project ID           Project to inspect

Options:
  --after ID             Resume after this Change ID
  --limit N              Page size (default and max: 16)
  -h, --help             Show this help";
const CHANGE_REMOVE_HELP: &str =
    "usage: factoryctl change remove --project ID --change ID --revision N

Request identity-safe removal of one retained Change. Removal is refused while
an attempt leases it and when the inventory revision is stale. No path can be
supplied by the caller.

Required:
  --project ID           Project owning the Change
  --change ID            Change to remove
  --revision N           Exact revision returned by `change list`

Options:
  -h, --help             Show this help";
const CHANGE_LEGACY_LIST_HELP: &str = "usage: factoryctl change legacy-list --project ID [options]

List metadata for pre-kernel source paths. These paths are quarantined
evidence: factoryd never stats, measures, leases, renames, or deletes them.

Required:
  --project ID           Project whose legacy metadata to inspect

Options:
  --after ID             Resume after this legacy-source ID
  --limit N              Page size (default and max: 16)
  -h, --help             Show this help";
const CHANGE_FORGET_LEGACY_HELP: &str =
    "usage: factoryctl change forget-legacy --project ID --legacy-source ID

Forget exactly one pre-kernel source metadata row. This never touches the
recorded filesystem path; any preserved directory remains operator-owned.

Required:
  --project ID           Project owning the metadata row
  --legacy-source ID     Typed legacy-source ID from `change legacy-list`

Options:
  -h, --help             Show this help";

const HOOK_HELP: &str = "usage: factoryctl hook --token-file PATH PreToolUse

Forwards one provider hook invocation (a Claude Code `--settings` hook or a
Codex `CODEX_HOME/config.toml` hook) to the daemon. Reads the hook's JSON
payload from stdin (bounded to 64 KiB), sends it as one `provider_hook`
request together with the token file's contents, and prints the daemon's
`reply` JSON verbatim to stdout so the provider can act on it (for example
`{\"decision\":\"block\",\"reason\":\"...\"}`).

Always exits 0, but fails closed with a provider denial if the token file
cannot be read, stdin is not valid bounded JSON, or the daemon is unreachable,
errors, or is slow (5 second timeout). This command is generated for the
provider; it is not an operator command.

Required:
  --token-file PATH        This attempt's private credential file
  PreToolUse                 The sole supported authority hook

Options:
  -h, --help                    Show this help";

const PROJECT_LIST_LIMIT: u32 = MAX_PROJECT_PAGE_ITEMS;
const TASK_LIST_LIMIT: u32 = MAX_TASK_PAGE_ITEMS;
const AGENT_LIST_LIMIT: u32 = MAX_AGENT_PAGE_ITEMS;
const RUN_LIST_LIMIT: u32 = MAX_RUN_PAGE_ITEMS;
const CHANGE_LIST_LIMIT: u32 = MAX_CHANGE_PAGE_ITEMS;
const LEGACY_SOURCE_LIST_LIMIT: u32 = MAX_LEGACY_SOURCE_PAGE_ITEMS;
/// Hard bound on `factoryctl hook`'s stdin payload, matching
/// `LocalRequest::ProviderHook`'s documented 64 KiB payload limit
/// (`factory-core/src/local.rs`).
const HOOK_PAYLOAD_LIMIT_BYTES: usize = 64 * 1024;
/// `factoryctl hook`'s fail-open budget: long enough for a healthy daemon
/// under normal load, short enough that a wedged daemon never visibly
/// stalls the operator's live Claude Code or Codex session.
const HOOK_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, Eq, PartialEq)]
enum CliCommand {
    Help(&'static str),
    Health,
    Status {
        json: bool,
    },
    SetAutoMode {
        enabled: bool,
    },
    CapacityStatus,
    CapacitySet {
        value: usize,
    },
    Usage,
    Version,
    Update {
        install: bool,
        json: bool,
    },
    Init {
        yes: bool,
        no_launchd: bool,
    },
    Doctor {
        json: bool,
    },
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
        agent_id: Option<String>,
    },
    TaskList {
        project_id: String,
        after_id: Option<String>,
        queue_revision: Option<i64>,
        agent_id: Option<String>,
        history: bool,
        limit: u32,
    },
    TaskStart {
        project_id: String,
        task_id: String,
        agent_id: String,
    },
    TaskRetry {
        project_id: String,
        task_id: String,
    },
    TaskReorder {
        project_id: String,
        task_id: String,
        priority: i32,
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
    TaskDone {
        result: String,
    },
    TaskBlocked {
        reason: String,
    },
    AgentAdd {
        id: Option<String>,
        project_id: String,
        parent_agent_id: Option<String>,
        role: AgentRole,
        provider: Provider,
        model: Option<String>,
        reasoning_effort: Option<String>,
        model_selection_reason: Option<String>,
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
    AgentStatus {
        project_id: String,
        agent_id: String,
    },
    AgentBudgetSet {
        project_id: String,
        agent_id: String,
        max_tool_calls: Option<u64>,
    },
    AgentBudgetReset {
        project_id: String,
        agent_id: String,
    },
    AgentProfileSet {
        project_id: String,
        agent_id: String,
        model: Option<String>,
        reasoning_effort: Option<String>,
        model_selection_reason: Option<String>,
        permission_mode: Option<String>,
        instructions_file: Option<String>,
        memory_file: Option<String>,
    },
    AgentMessage {
        id: Option<String>,
        project_id: String,
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
    AgentPause {
        project_id: String,
        agent_id: String,
    },
    AgentResume {
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
    ChangeList {
        project_id: String,
        after_id: Option<String>,
        limit: u32,
    },
    ChangeRemove {
        project_id: String,
        change_id: String,
        expected_revision: i64,
    },
    ChangeLegacyList {
        project_id: String,
        after_id: Option<String>,
        limit: u32,
    },
    ChangeForgetLegacy {
        project_id: String,
        legacy_source_id: String,
    },
    Hook {
        token_file: String,
        event: ProviderHookEvent,
    },
    Events(events::Command),
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
    if env::var_os(ATTEMPT_TOKEN_FILE_ENV).is_some() && host_level_command(&command) {
        return Err("this host-level operation is unavailable inside an attempt".into());
    }
    if matches!(command, CliCommand::Usage) {
        return Ok(usage::run());
    }
    if matches!(command, CliCommand::Version) {
        println!("factoryctl {}", factoryctl::update::CURRENT_VERSION);
        return Ok(0);
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
    if matches!(&command, CliCommand::CapacityStatus) {
        let status = capacity::status_from_environment()?;
        println!(
            "{}",
            serde_json::json!({
                "capacity": status.configured,
                "launchd_loaded": status.launchd_loaded,
            })
        );
        return Ok(0);
    }
    if let CliCommand::CapacitySet { value } = &command {
        let previous = capacity::status_from_environment()?.configured;
        let requested = capacity::validate(*value)?;
        eprintln!(
            "capacity: {previous} -> {requested} active attempts; launchd will restart only factoryd, preserving live runner processes and attempt state; higher values can increase concurrent provider/subscription use, lower values leave work queued"
        );
        let change = capacity::set_from_environment(&socket, *value)?;
        println!("{}", capacity_result(&change));
        return Ok(0);
    }
    if let CliCommand::Update { install, json } = command {
        return update_command::run(&update_command::Options { install, json }, &socket);
    }
    if let CliCommand::Init { yes, no_launchd } = command {
        return init::run(&init::Options { yes, no_launchd }, &socket);
    }
    if let CliCommand::Doctor { json } = command {
        return doctor::run(&doctor::Options { json }, &socket);
    }
    let attempt_scoped = command_requires_attempt_credential(&command);
    let ambient_attempt = if env::var_os(ATTEMPT_TOKEN_FILE_ENV).is_some() {
        Some(attempt_credential()?)
    } else if attempt_scoped {
        return Err(format!(
            "{ATTEMPT_TOKEN_FILE_ENV} is required; this command only works inside an attempt"
        ));
    } else {
        None
    };
    let client = if matches!(&command, CliCommand::Health | CliCommand::Hook { .. }) {
        Client::new(socket)
    } else if let Some(credential) = ambient_attempt {
        // A provider cannot accidentally cross into operator authority by
        // invoking an operator-shaped command. The daemon will evaluate the
        // request against this exact attempt and reject it if unauthorized.
        Client::authenticated(socket, credential)
    } else {
        let resolved_factory_home = resolve_factory_home(factory_home.as_deref(), home.as_deref())?;
        Client::authenticated(socket, operator_credential(&resolved_factory_home)?)
    };
    if let CliCommand::Hook { token_file, event } = command {
        return Ok(run_hook(&client, &token_file, event));
    }
    let stdout = std::io::stdout();
    let mut output = stdout.lock();

    if let CliCommand::Events(command) = &command {
        if let Some(exit_code) = events::run_follow(&client, command, &mut output)? {
            return Ok(exit_code);
        }
    }

    if let CliCommand::AgentProfileSet {
        project_id,
        agent_id,
        model,
        reasoning_effort,
        model_selection_reason,
        permission_mode,
        instructions_file,
        memory_file,
    } = command
    {
        let frame = agent_profile_set_frame(
            &client,
            project_id,
            agent_id,
            AgentProfileSetOptions {
                model,
                reasoning_effort,
                model_selection_reason,
                permission_mode,
                instructions_file,
                memory_file,
            },
        )?;
        write_frame(&mut output, &frame)?;
        return Ok(if is_error(&frame) { 2 } else { 0 });
    }

    let human_status = matches!(&command, CliCommand::Status { json: false });
    let request = request_for(command)?;
    let frame = client.request(request).map_err(|error| error.to_string())?;
    if human_status {
        match &frame {
            ServerFrame::Response {
                response: LocalResponse::FleetStatus { status },
                ..
            } => {
                let daemon_version =
                    client
                        .request(LocalRequest::Health)
                        .ok()
                        .and_then(|frame| match frame {
                            ServerFrame::Response {
                                response: LocalResponse::Health { version, .. },
                                ..
                            } => Some(version),
                            _ => None,
                        });
                status::write_with_daemon_version(
                    &mut output,
                    status,
                    daemon_version
                        .as_deref()
                        .filter(|version| !version.is_empty()),
                )?
            }
            _ => write_frame(&mut output, &frame)?,
        }
    } else {
        write_frame(&mut output, &frame)?;
    }
    Ok(if is_error(&frame) { 2 } else { 0 })
}

fn host_level_command(command: &CliCommand) -> bool {
    matches!(
        command,
        CliCommand::Usage
            | CliCommand::CapacityStatus
            | CliCommand::CapacitySet { .. }
            | CliCommand::Init { .. }
            | CliCommand::Doctor { .. }
            | CliCommand::Update { .. }
    )
}

fn capacity_result(change: &capacity::CapacityChange) -> serde_json::Value {
    serde_json::json!({
        "previous": change.previous,
        "capacity": change.current,
        "launchd": "reloaded",
        "active_attempts_preserved": true,
    })
}

fn command_requires_attempt_credential(command: &CliCommand) -> bool {
    matches!(
        command,
        CliCommand::TaskDone { .. } | CliCommand::TaskBlocked { .. }
    )
}

/// Executes the sole authority hook. Losing the daemon must not silently
/// remove the attempt policy gate, so every local failure denies the tool.
fn run_hook(client: &Client, token_file: &str, event: ProviderHookEvent) -> i32 {
    let reply = hook_reply(client, token_file, event).unwrap_or_else(|| {
        serde_json::json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": "Dark Factory policy unavailable"
        }})
    });
    println!("{reply}");
    0
}

fn hook_reply(
    client: &Client,
    token_file: &str,
    event: ProviderHookEvent,
) -> Option<serde_json::Value> {
    let credential = credential_from_file(token_file).ok()?;
    let payload = read_bounded_stdin_json(HOOK_PAYLOAD_LIMIT_BYTES)?;
    let frame = client
        .request_with_timeout_authenticated(
            LocalRequest::ProviderHook { event, payload },
            credential,
            HOOK_REQUEST_TIMEOUT,
        )
        .ok()?;
    match frame {
        ServerFrame::Response {
            response: LocalResponse::ProviderHookReply { reply },
            ..
        } => Some(reply),
        _ => None,
    }
}

/// Reads at most `limit` bytes of stdin and parses them as one JSON value.
/// Returns `None` (never an error the caller must format) if stdin exceeds
/// the bound or is not valid JSON; the event-specific caller chooses the
/// fail-open or fail-closed reply.
fn read_bounded_stdin_json(limit: usize) -> Option<serde_json::Value> {
    use std::io::Read;
    let mut buffer = Vec::new();
    let read = std::io::stdin()
        .take(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut buffer)
        .ok()?;
    if read > limit {
        return None;
    }
    serde_json::from_slice(&buffer).ok()
}

/// Applies `--model`/`--permission-mode`/`--instructions-file`/
/// `--memory-file` as a patch over the agent's current profile: any flag
/// left unset carries the currently stored value forward unchanged, so
/// `agent profile set` cannot silently clear standing guidance or memory it
/// was not asked to change.
struct AgentProfileSetOptions {
    model: Option<String>,
    reasoning_effort: Option<String>,
    model_selection_reason: Option<String>,
    permission_mode: Option<String>,
    instructions_file: Option<String>,
    memory_file: Option<String>,
}

fn agent_profile_set_frame(
    client: &Client,
    project_id: String,
    agent_id: String,
    options: AgentProfileSetOptions,
) -> Result<ServerFrame, String> {
    let AgentProfileSetOptions {
        model,
        reasoning_effort,
        model_selection_reason,
        permission_mode,
        instructions_file,
        memory_file,
    } = options;
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
        None => reusable_guidance(
            &agent.profile.instructions,
            agent.instructions_health.state,
            "instructions",
            "--instructions-file",
        )?,
    };
    let memory = match memory_file {
        Some(path) => read_guidance_file(&path)?,
        None => reusable_guidance(
            &agent.profile.memory,
            agent.memory_health.state,
            "memory",
            "--memory-file",
        )?,
    };
    let selected_model = model.clone().or_else(|| agent.profile.model.clone());
    let model_changed = model.is_some() && selected_model != agent.profile.model;
    let selection_reason = if model_changed {
        model_selection_reason
    } else {
        model_selection_reason.or(agent.profile.model_selection_reason.clone())
    };
    client
        .request(LocalRequest::UpdateAgentProfile {
            project_id,
            agent_id,
            model: selected_model,
            reasoning_effort: reasoning_effort.or(agent.profile.reasoning_effort),
            model_selection_reason: selection_reason,
            permission_mode: permission_mode.or(agent.profile.permission_mode),
            instructions,
            memory,
        })
        .map_err(|error| error.to_string())
}

fn reusable_guidance(
    content: &str,
    state: GuidanceHealthState,
    label: &str,
    replacement_option: &str,
) -> Result<String, String> {
    if matches!(
        state,
        GuidanceHealthState::Ok | GuidanceHealthState::NearLimit
    ) {
        Ok(content.to_owned())
    } else {
        Err(format!(
            "{label} is {state:?}; supply {replacement_option} to replace it instead of clearing it"
        ))
    }
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
        "status" => {
            if wants_help(&args) {
                return Ok((socket, CliCommand::Help(STATUS_HELP)));
            }
            let json = take_flag(&mut args, "--json")?;
            require_empty(&args)?;
            Ok((socket, CliCommand::Status { json }))
        }
        "auto" => {
            if wants_help(&args) {
                return Ok((
                    socket,
                    CliCommand::Help(
                        "usage: factoryctl auto <on|off|status>\n\nSet the durable factory-wide provider bypass default, or show fleet status containing its current value.",
                    ),
                ));
            }
            let action = take_action(&mut args, "auto")?;
            require_empty(&args)?;
            match action.as_str() {
                "on" => Ok((socket, CliCommand::SetAutoMode { enabled: true })),
                "off" => Ok((socket, CliCommand::SetAutoMode { enabled: false })),
                // Keep this compatibility alias machine-readable; the
                // human-first command is `factoryctl status`.
                "status" => Ok((socket, CliCommand::Status { json: true })),
                _ => Err("auto action must be `on`, `off`, or `status`".into()),
            }
        }
        "capacity" => {
            if wants_help(&args) {
                return Ok((socket, CliCommand::Help(CAPACITY_HELP)));
            }
            parse_capacity(args).map(|command| (socket, command))
        }
        "version" | "--version" | "-V" => {
            require_empty(&args)?;
            Ok((socket, CliCommand::Version))
        }
        "init" => {
            if wants_help(&args) {
                return Ok((socket, CliCommand::Help(INIT_HELP)));
            }
            let yes = take_flag(&mut args, "--yes")?;
            let no_launchd = take_flag(&mut args, "--no-launchd")?;
            require_empty(&args)?;
            Ok((socket, CliCommand::Init { yes, no_launchd }))
        }
        "doctor" => {
            if wants_help(&args) {
                return Ok((socket, CliCommand::Help(DOCTOR_HELP)));
            }
            let json = take_flag(&mut args, "--json")?;
            require_empty(&args)?;
            Ok((socket, CliCommand::Doctor { json }))
        }
        "update" => {
            if wants_help(&args) {
                return Ok((socket, CliCommand::Help(UPDATE_HELP)));
            }
            let install = take_flag(&mut args, "--install")?;
            let json = take_flag(&mut args, "--json")?;
            require_empty(&args)?;
            Ok((socket, CliCommand::Update { install, json }))
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
        "agent" => parse_agent(args).map(|command| (socket, command)),
        "run" => parse_run(args).map(|command| (socket, command)),
        "change" => parse_change(args).map(|command| (socket, command)),
        "hook" => {
            if wants_help(&args) {
                return Ok((socket, CliCommand::Help(HOOK_HELP)));
            }
            parse_hook(args).map(|command| (socket, command))
        }
        "events" => events::parse(args).map(|command| (socket, command)),
        _ => Err(format!("unknown command {command:?}; {USAGE}")),
    }
}

fn parse_capacity(mut args: Vec<String>) -> Result<CliCommand, String> {
    let action = take_action(&mut args, "capacity")?;
    match action.as_str() {
        "status" => {
            require_empty(&args)?;
            Ok(CliCommand::CapacityStatus)
        }
        "set" => {
            if args.len() != 1 {
                return Err("capacity set requires one integer value".into());
            }
            let value = args
                .pop()
                .and_then(|value| value.parse::<usize>().ok())
                .ok_or("capacity set requires one positive integer value")?;
            Ok(CliCommand::CapacitySet { value })
        }
        _ => Err("capacity action must be `status` or `set`".into()),
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
            let project_id = required_project(&mut args)?;
            require_empty(&args)?;
            Ok(CliCommand::ProjectDelete { project_id })
        }
        "get" => {
            let project_id = required_project(&mut args)?;
            require_empty(&args)?;
            Ok(CliCommand::ProjectGet { project_id })
        }
        "guidance" => {
            let sub_action = take_action(&mut args, "project guidance")?;
            match sub_action.as_str() {
                "set" => {
                    let project_id = required_project(&mut args)?;
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
            "done" => TASK_DONE_HELP,
            "blocked" => TASK_BLOCKED_HELP,
            _ => TASK_HELP,
        }));
    }
    match action.as_str() {
        "add" => {
            let id = take_option(&mut args, "--id")?;
            let project_id = required_project(&mut args)?;
            let parent_task_id = take_option(&mut args, "--parent")?;
            let title = required_option(&mut args, "--title")?;
            let body = required_option(&mut args, "--body")?;
            let priority = take_option(&mut args, "--priority")?
                .map(|value| parse_number(&value, "--priority"))
                .transpose()?
                .unwrap_or(0);
            let agent_id = take_option(&mut args, "--agent")?;
            require_empty(&args)?;
            Ok(CliCommand::TaskAdd {
                id,
                project_id,
                parent_task_id,
                title,
                body,
                priority,
                agent_id,
            })
        }
        "list" => {
            let project_id = required_project(&mut args)?;
            let after_id = take_option(&mut args, "--after")?;
            let queue_revision = take_option(&mut args, "--revision")?
                .map(|value| parse_number(&value, "--revision"))
                .transpose()?;
            let agent_id = take_option(&mut args, "--agent")?;
            if after_id.is_some() != queue_revision.is_some() {
                return Err("--after and --revision must be supplied together".into());
            }
            let history = take_flag(&mut args, "--history")?;
            let (limit, _) = take_limit(&mut args, TASK_LIST_LIMIT, TASK_LIST_LIMIT)?;
            require_empty(&args)?;
            Ok(CliCommand::TaskList {
                project_id,
                after_id,
                queue_revision,
                agent_id,
                history,
                limit,
            })
        }
        "start" => {
            let project_id = required_project(&mut args)?;
            let task_id = required_option(&mut args, "--task")?;
            let agent_id = required_option(&mut args, "--agent")?;
            require_empty(&args)?;
            Ok(CliCommand::TaskStart {
                project_id,
                task_id,
                agent_id,
            })
        }
        "retry" => {
            let project_id = required_project(&mut args)?;
            let task_id = required_option(&mut args, "--task")?;
            require_empty(&args)?;
            Ok(CliCommand::TaskRetry {
                project_id,
                task_id,
            })
        }
        "reorder" => {
            let project_id = required_project(&mut args)?;
            let task_id = required_option(&mut args, "--task")?;
            let priority = required_option(&mut args, "--priority")?
                .parse::<i32>()
                .map_err(|_| "--priority must be a signed integer".to_owned())?;
            require_empty(&args)?;
            Ok(CliCommand::TaskReorder {
                project_id,
                task_id,
                priority,
            })
        }
        "assign" => {
            let project_id = required_project(&mut args)?;
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
            let project_id = required_project(&mut args)?;
            let task_id = required_option(&mut args, "--task")?;
            require_empty(&args)?;
            Ok(CliCommand::TaskGet {
                project_id,
                task_id,
            })
        }
        "cancel" => {
            let project_id = required_project(&mut args)?;
            let task_id = required_option(&mut args, "--task")?;
            require_empty(&args)?;
            Ok(CliCommand::TaskCancel {
                project_id,
                task_id,
            })
        }
        "update" => {
            let project_id = required_project(&mut args)?;
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
            let project_id = required_project(&mut args)?;
            let task_id = required_option(&mut args, "--task")?;
            require_empty(&args)?;
            Ok(CliCommand::TaskDelete {
                project_id,
                task_id,
            })
        }
        "done" => {
            let result = take_option(&mut args, "--result")?;
            let result_file = take_option(&mut args, "--result-file")?;
            let result = match (result, result_file) {
                (Some(_), Some(_)) => {
                    return Err("--result and --result-file may not both be provided".into());
                }
                (Some(result), None) => result,
                (None, Some(file)) => read_guidance_file(&file)?,
                (None, None) => return Err("task done requires --result or --result-file".into()),
            };
            require_empty(&args)?;
            Ok(CliCommand::TaskDone { result })
        }
        "blocked" => {
            let reason = required_option(&mut args, "--reason")?;
            require_empty(&args)?;
            Ok(CliCommand::TaskBlocked { reason })
        }
        _ => Err(format!("unknown task action {action:?}")),
    }
}

fn parse_hook(mut args: Vec<String>) -> Result<CliCommand, String> {
    let token_file = required_option(&mut args, "--token-file")?;
    if args.is_empty() {
        return Err("hook requires an event name".into());
    }
    let event_name = args.remove(0);
    let event = ProviderHookEvent::parse_provider_event_name(&event_name)
        .ok_or_else(|| format!("unknown hook event {event_name:?}"))?;
    require_empty(&args)?;
    Ok(CliCommand::Hook { token_file, event })
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
            "status" => AGENT_STATUS_HELP,
            "profile" => AGENT_PROFILE_HELP,
            "budget" => AGENT_BUDGET_HELP,
            "message" => AGENT_MESSAGE_HELP,
            "inbox" => AGENT_INBOX_HELP,
            "pause" => AGENT_PAUSE_HELP,
            "resume" => AGENT_RESUME_HELP,
            _ => AGENT_HELP,
        }));
    }
    match action.as_str() {
        "add" => {
            let id = take_option(&mut args, "--id")?;
            let project_id = required_project(&mut args)?;
            let parent_agent_id = take_option(&mut args, "--parent")?;
            let role = match required_option(&mut args, "--role")?.as_str() {
                "orchestrator" => AgentRole::Orchestrator,
                "worker" => AgentRole::Worker,
                _ => return Err("--role must be orchestrator or worker".into()),
            };
            let provider = match required_option(&mut args, "--provider")?.as_str() {
                "claude" | "claude-code" => Provider::ClaudeCode,
                "codex" => Provider::Codex,
                "shell" => Provider::Shell,
                _ => return Err("--provider must be claude, codex, or shell".into()),
            };
            let model = take_option(&mut args, "--model")?;
            let reasoning_effort = take_option(&mut args, "--reasoning-effort")?;
            let model_selection_reason = take_option(&mut args, "--model-reason")?;
            require_empty(&args)?;
            Ok(CliCommand::AgentAdd {
                id,
                project_id,
                parent_agent_id,
                role,
                provider,
                model,
                reasoning_effort,
                model_selection_reason,
            })
        }
        "list" => {
            let project_id = required_project(&mut args)?;
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
            let project_id = required_project(&mut args)?;
            let agent_id = required_option(&mut args, "--agent")?;
            require_empty(&args)?;
            Ok(CliCommand::AgentGet {
                project_id,
                agent_id,
            })
        }
        "status" => {
            let project_id = required_project(&mut args)?;
            let agent_id = required_option(&mut args, "--agent")?;
            require_empty(&args)?;
            Ok(CliCommand::AgentStatus {
                project_id,
                agent_id,
            })
        }
        "budget" => {
            let sub_action = take_action(&mut args, "agent budget")?;
            let project_id = required_project(&mut args)?;
            let agent_id = required_option(&mut args, "--agent")?;
            match sub_action.as_str() {
                "status" => {
                    require_empty(&args)?;
                    Ok(CliCommand::AgentStatus {
                        project_id,
                        agent_id,
                    })
                }
                "set" => {
                    let value = required_option(&mut args, "--max-tool-calls")?;
                    let max_tool_calls = if value == "unlimited" {
                        None
                    } else {
                        let parsed = parse_number(&value, "--max-tool-calls")?;
                        if parsed == 0 {
                            return Err(
                                "--max-tool-calls must be greater than zero or unlimited".into()
                            );
                        }
                        Some(parsed)
                    };
                    require_empty(&args)?;
                    Ok(CliCommand::AgentBudgetSet {
                        project_id,
                        agent_id,
                        max_tool_calls,
                    })
                }
                "reset" => {
                    require_empty(&args)?;
                    Ok(CliCommand::AgentBudgetReset {
                        project_id,
                        agent_id,
                    })
                }
                _ => Err(format!("unknown agent budget action {sub_action:?}")),
            }
        }
        "profile" => {
            let sub_action = take_action(&mut args, "agent profile")?;
            match sub_action.as_str() {
                "set" => {
                    let project_id = required_project(&mut args)?;
                    let agent_id = required_option(&mut args, "--agent")?;
                    let model = take_option(&mut args, "--model")?;
                    let reasoning_effort = take_option(&mut args, "--reasoning-effort")?;
                    let model_selection_reason = take_option(&mut args, "--model-reason")?;
                    let permission_mode = take_option(&mut args, "--permission-mode")?;
                    let instructions_file = take_option(&mut args, "--instructions-file")?;
                    let memory_file = take_option(&mut args, "--memory-file")?;
                    require_empty(&args)?;
                    Ok(CliCommand::AgentProfileSet {
                        project_id,
                        agent_id,
                        model,
                        reasoning_effort,
                        model_selection_reason,
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
            let project_id = required_project(&mut args)?;
            let recipient_agent_id = required_option(&mut args, "--to")?;
            let body = required_option(&mut args, "--body")?;
            require_empty(&args)?;
            Ok(CliCommand::AgentMessage {
                id,
                project_id,
                recipient_agent_id,
                body,
            })
        }
        "inbox" => {
            let project_id = required_project(&mut args)?;
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
            let project_id = required_project(&mut args)?;
            let agent_id = required_option(&mut args, "--agent")?;
            require_empty(&args)?;
            Ok(CliCommand::AgentDelete {
                project_id,
                agent_id,
            })
        }
        "pause" => {
            let project_id = required_project(&mut args)?;
            let agent_id = required_option(&mut args, "--agent")?;
            require_empty(&args)?;
            Ok(CliCommand::AgentPause {
                project_id,
                agent_id,
            })
        }
        "resume" => {
            let project_id = required_project(&mut args)?;
            let agent_id = required_option(&mut args, "--agent")?;
            require_empty(&args)?;
            Ok(CliCommand::AgentResume {
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
            let project_id = required_project(&mut args)?;
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
            let project_id = required_project(&mut args)?;
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

fn parse_change(mut args: Vec<String>) -> Result<CliCommand, String> {
    if args.is_empty() || is_help_flag(&args[0]) {
        return Ok(CliCommand::Help(CHANGE_HELP));
    }
    let action = take_action(&mut args, "change")?;
    if wants_help(&args) {
        return Ok(CliCommand::Help(match action.as_str() {
            "list" => CHANGE_LIST_HELP,
            "remove" => CHANGE_REMOVE_HELP,
            "legacy-list" => CHANGE_LEGACY_LIST_HELP,
            "forget-legacy" => CHANGE_FORGET_LEGACY_HELP,
            _ => CHANGE_HELP,
        }));
    }
    match action.as_str() {
        "list" => {
            let project_id = required_project(&mut args)?;
            let after_id = take_option(&mut args, "--after")?;
            let (limit, _) = take_limit(&mut args, CHANGE_LIST_LIMIT, MAX_CHANGE_PAGE_ITEMS)?;
            require_empty(&args)?;
            Ok(CliCommand::ChangeList {
                project_id,
                after_id,
                limit,
            })
        }
        "remove" => {
            let project_id = required_project(&mut args)?;
            let change_id = required_option(&mut args, "--change")?;
            let expected_revision =
                parse_number::<i64>(&required_option(&mut args, "--revision")?, "--revision")?;
            if expected_revision < 0 {
                return Err("--revision must be a non-negative integer".into());
            }
            require_empty(&args)?;
            Ok(CliCommand::ChangeRemove {
                project_id,
                change_id,
                expected_revision,
            })
        }
        "legacy-list" => {
            let project_id = required_project(&mut args)?;
            let after_id = take_option(&mut args, "--after")?;
            let (limit, _) = take_limit(
                &mut args,
                LEGACY_SOURCE_LIST_LIMIT,
                MAX_LEGACY_SOURCE_PAGE_ITEMS,
            )?;
            require_empty(&args)?;
            Ok(CliCommand::ChangeLegacyList {
                project_id,
                after_id,
                limit,
            })
        }
        "forget-legacy" => {
            let project_id = required_project(&mut args)?;
            let legacy_source_id = required_option(&mut args, "--legacy-source")?;
            require_empty(&args)?;
            Ok(CliCommand::ChangeForgetLegacy {
                project_id,
                legacy_source_id,
            })
        }
        _ => Err(format!("unknown change action {action:?}")),
    }
}

fn request_for(command: CliCommand) -> Result<LocalRequest, String> {
    match command {
        CliCommand::Help(_) => Err("help is not a daemon request".into()),
        CliCommand::Health => Ok(LocalRequest::Health),
        CliCommand::Status { .. } => Ok(LocalRequest::FleetStatus),
        CliCommand::SetAutoMode { enabled } => Ok(LocalRequest::SetAutoMode { enabled }),
        CliCommand::CapacityStatus | CliCommand::CapacitySet { .. } => {
            Err("capacity is handled outside the daemon protocol".into())
        }
        CliCommand::Usage
        | CliCommand::Version
        | CliCommand::Update { .. }
        | CliCommand::Init { .. }
        | CliCommand::Doctor { .. } => Err("handled before local requests".into()),
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
            agent_id,
        } => {
            let request = LocalRequest::CreateTask {
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
                agent_id: agent_id.map(|id| parse_id(id, "agent")).transpose()?,
            };
            Ok(request)
        }
        CliCommand::TaskList {
            project_id,
            after_id,
            queue_revision,
            agent_id,
            history,
            limit,
        } => {
            if after_id.is_some() != queue_revision.is_some() {
                return Err("--after and --revision must be supplied together".into());
            }
            Ok(LocalRequest::ListTasks {
                project_id: parse_id(project_id, "project")?,
                after_id: after_id.map(|id| parse_id(id, "task cursor")).transpose()?,
                agent_id: agent_id.map(|id| parse_id(id, "agent")).transpose()?,
                queue_revision,
                history,
                limit,
            })
        }
        CliCommand::TaskStart {
            project_id,
            task_id,
            agent_id,
        } => Ok(LocalRequest::StartTask {
            project_id: parse_id(project_id, "project")?,
            task_id: parse_id(task_id, "task")?,
            agent_id: parse_id(agent_id, "agent")?,
        }),
        CliCommand::TaskRetry {
            project_id,
            task_id,
        } => Ok(LocalRequest::RetryTask {
            project_id: parse_id(project_id, "project")?,
            task_id: parse_id(task_id, "task")?,
        }),
        CliCommand::TaskReorder {
            project_id,
            task_id,
            priority,
        } => Ok(LocalRequest::UpdateTask {
            project_id: parse_id(project_id, "project")?,
            task_id: parse_id(task_id, "task")?,
            title: None,
            body: None,
            priority: Some(priority),
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
            priority: None,
        }),
        CliCommand::TaskDelete {
            project_id,
            task_id,
        } => Ok(LocalRequest::DeleteTask {
            project_id: parse_id(project_id, "project")?,
            task_id: parse_id(task_id, "task")?,
        }),
        CliCommand::TaskDone { result } => Ok(LocalRequest::CompleteAttempt { result }),
        CliCommand::TaskBlocked { reason } => Ok(LocalRequest::BlockAttempt { reason }),
        CliCommand::AgentAdd {
            id,
            project_id,
            parent_agent_id,
            role,
            provider,
            model,
            reasoning_effort,
            model_selection_reason,
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
            reasoning_effort,
            model_selection_reason,
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
        CliCommand::AgentStatus {
            project_id,
            agent_id,
        } => Ok(LocalRequest::AgentStatus {
            project_id: parse_id(project_id, "project")?,
            agent_id: parse_id(agent_id, "agent")?,
        }),
        CliCommand::AgentBudgetSet {
            project_id,
            agent_id,
            max_tool_calls,
        } => Ok(LocalRequest::SetAgentBudget {
            project_id: parse_id(project_id, "project")?,
            agent_id: parse_id(agent_id, "agent")?,
            max_tool_calls,
        }),
        CliCommand::AgentBudgetReset {
            project_id,
            agent_id,
        } => Ok(LocalRequest::ResetAgentBudget {
            project_id: parse_id(project_id, "project")?,
            agent_id: parse_id(agent_id, "agent")?,
        }),
        CliCommand::AgentProfileSet { .. } => {
            Err("agent profile set is resolved before the daemon request".into())
        }
        CliCommand::AgentMessage {
            id,
            project_id,
            recipient_agent_id,
            body,
        } => Ok(LocalRequest::SendAgentMessage {
            id: id
                .map(|id| parse_id(id, "message"))
                .transpose()?
                .unwrap_or(generated_id()?),
            project_id: parse_id(project_id, "project")?,
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
        CliCommand::AgentPause {
            project_id,
            agent_id,
        } => Ok(LocalRequest::PauseAgent {
            project_id: parse_id(project_id, "project")?,
            agent_id: parse_id(agent_id, "agent")?,
        }),
        CliCommand::AgentResume {
            project_id,
            agent_id,
        } => Ok(LocalRequest::ResumeAgent {
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
        } => Ok(LocalRequest::CancelRun {
            project_id: parse_id(project_id, "project")?,
            run_id: parse_id(run_id, "run")?,
            grace_ms,
        }),
        CliCommand::ChangeList {
            project_id,
            after_id,
            limit,
        } => Ok(LocalRequest::ListChanges {
            project_id: parse_id(project_id, "project")?,
            after_id: after_id
                .map(|id| parse_id(id, "change cursor"))
                .transpose()?,
            limit,
        }),
        CliCommand::ChangeRemove {
            project_id,
            change_id,
            expected_revision,
        } => Ok(LocalRequest::RemoveChange {
            project_id: parse_id(project_id, "project")?,
            change_id: parse_id(change_id, "change")?,
            expected_revision,
        }),
        CliCommand::ChangeLegacyList {
            project_id,
            after_id,
            limit,
        } => Ok(LocalRequest::ListLegacySources {
            project_id: parse_id(project_id, "project")?,
            after_id: after_id
                .map(|id| parse_id(id, "legacy source cursor"))
                .transpose()?,
            limit,
        }),
        CliCommand::ChangeForgetLegacy {
            project_id,
            legacy_source_id,
        } => Ok(LocalRequest::ForgetLegacySource {
            project_id: parse_id(project_id, "project")?,
            legacy_source_id: parse_id(legacy_source_id, "legacy source")?,
        }),
        CliCommand::Hook { .. } => Err("hook is handled before local requests".into()),
        CliCommand::Events(command) => Ok(events::request(command)),
    }
}

fn attempt_credential() -> Result<RequestCredential, String> {
    let path = env::var(ATTEMPT_TOKEN_FILE_ENV).map_err(|_| {
        format!("{ATTEMPT_TOKEN_FILE_ENV} is required; this command only works inside an attempt")
    })?;
    credential_from_file(&path)
}

fn credential_from_file(path: &str) -> Result<RequestCredential, String> {
    let token = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read attempt credential file: {error}"))?;
    let token = token.trim().to_owned();
    RequestCredential::new(token).map_err(str::to_owned)
}

fn operator_credential(factory_home: &std::path::Path) -> Result<RequestCredential, String> {
    credential_from_file(
        factory_home
            .join("operator.token")
            .to_str()
            .ok_or("operator credential path is not valid UTF-8")?,
    )
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

fn resolve_factory_home(factory_home: Option<&str>, home: Option<&str>) -> Result<PathBuf, String> {
    if let Some(path) = factory_home.filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    home.filter(|path| !path.is_empty())
        .map(|path| PathBuf::from(path).join(".dark-factory"))
        .ok_or_else(|| "DARK_FACTORY_HOME and HOME are unavailable".into())
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

/// `--project`, falling back to `$DARK_FACTORY_PROJECT` when the flag is
/// absent (so a command run from inside an attempt's own environment does
/// not have to repeat `--project` the daemon already told it). Behavior for
/// every existing call is unchanged unless that environment variable is
/// set: the flag still wins when both are present.
fn required_project(args: &mut Vec<String>) -> Result<String, String> {
    required_option_or_env(args, "--project", "DARK_FACTORY_PROJECT")
}

fn required_option_or_env(
    args: &mut Vec<String>,
    name: &str,
    env_var: &str,
) -> Result<String, String> {
    require_option_or_env_value(take_option_or_env(args, name, env_var)?, name, env_var)
}

fn require_option_or_env_value(
    value: Option<String>,
    name: &str,
    env_var: &str,
) -> Result<String, String> {
    value.ok_or_else(|| format!("{name} is required (or set ${env_var})"))
}

fn take_option_or_env(
    args: &mut Vec<String>,
    name: &str,
    env_var: &str,
) -> Result<Option<String>, String> {
    let explicit = take_option(args, name)?;
    Ok(resolve_or_env(explicit, env::var(env_var).ok()))
}

/// An explicit flag value always wins; otherwise falls back to an
/// already-looked-up environment value, treating an empty string as "unset"
/// (a stray `export DARK_FACTORY_PROJECT=` must not silently win over a
/// clear "flag is required" error). Split out from [`take_option_or_env`]
/// so the merge logic is testable without mutating real process
/// environment — `std::env::set_var` is `unsafe` (and this workspace
/// forbids `unsafe_code`), so tests cannot set the variable themselves.
fn resolve_or_env(explicit: Option<String>, env_value: Option<String>) -> Option<String> {
    explicit.or_else(|| env_value.filter(|value| !value.is_empty()))
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
    fn capacity_commands_are_operator_setting_commands() {
        let (_, status) = parse_args(args(&["capacity", "status"])).unwrap();
        assert!(matches!(status, CliCommand::CapacityStatus));
        let (_, set) = parse_args(args(&["capacity", "set", "8"])).unwrap();
        assert!(matches!(set, CliCommand::CapacitySet { value: 8 }));
        assert!(parse_args(args(&["capacity", "set"])).is_err());
        assert!(parse_args(args(&["capacity", "set", "8", "9"])).is_err());
    }

    #[test]
    fn capacity_result_promises_active_attempt_preservation_in_the_cli_contract() {
        let value = capacity_result(&capacity::CapacityChange {
            previous: 4,
            current: 8,
        });
        assert_eq!(value["previous"], 4);
        assert_eq!(value["capacity"], 8);
        assert_eq!(value["active_attempts_preserved"], true);
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
    fn update_and_version_parse_without_a_daemon_request() {
        assert_eq!(
            parse_args(args(&["update"])).unwrap().1,
            CliCommand::Update {
                install: false,
                json: false,
            }
        );
        assert_eq!(
            parse_args(args(&["update", "--install"])).unwrap().1,
            CliCommand::Update {
                install: true,
                json: false,
            }
        );
        assert_eq!(
            parse_args(args(&["update", "--json"])).unwrap().1,
            CliCommand::Update {
                install: false,
                json: true,
            }
        );
        assert!(parse_args(args(&["update", "--force"])).is_err());
        assert!(UPDATE_HELP.contains("update [--install] [--json]"));
        assert!(
            UPDATE_HELP
                .contains("--json                     One JSON object instead of text lines")
        );
        assert_eq!(
            parse_args(args(&["version"])).unwrap().1,
            CliCommand::Version
        );
        assert_eq!(
            parse_args(args(&["--version"])).unwrap().1,
            CliCommand::Version
        );
        assert!(
            request_for(CliCommand::Update {
                install: false,
                json: false,
            })
            .is_err()
        );
        assert_eq!(
            parse_args(args(&["init", "--yes", "--no-launchd"]))
                .unwrap()
                .1,
            CliCommand::Init {
                yes: true,
                no_launchd: true
            }
        );
        assert_eq!(
            parse_args(args(&["doctor", "--json"])).unwrap().1,
            CliCommand::Doctor { json: true }
        );
        assert!(parse_args(args(&["doctor", "--yes"])).is_err());
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
            parse_args(args(&["status", "--help"])).unwrap().1,
            CliCommand::Help(STATUS_HELP)
        );
        assert_eq!(
            parse_args(args(&["update", "--help"])).unwrap().1,
            CliCommand::Help(UPDATE_HELP)
        );
        assert_eq!(
            parse_args(args(&["init", "--help"])).unwrap().1,
            CliCommand::Help(INIT_HELP)
        );
        assert_eq!(
            parse_args(args(&["doctor", "--help"])).unwrap().1,
            CliCommand::Help(DOCTOR_HELP)
        );
        assert_eq!(
            parse_args(args(&["events", "--help"])).unwrap().1,
            CliCommand::Help(events::HELP)
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
            ("done", TASK_DONE_HELP),
            ("blocked", TASK_BLOCKED_HELP),
        ] {
            assert_eq!(
                parse_args(args(&["task", action, "--help"])).unwrap().1,
                CliCommand::Help(expected),
                "task {action} --help"
            );
        }

        assert!(TASK_ASSIGN_HELP.contains("wakes automatic admission"));
        assert!(TASK_ASSIGN_HELP.contains("may start an attempt"));
        assert!(!TASK_ASSIGN_HELP.contains("without starting"));

        assert_eq!(
            parse_args(args(&["agent"])).unwrap().1,
            CliCommand::Help(AGENT_HELP)
        );
        for (action, expected) in [
            ("add", AGENT_ADD_HELP),
            ("list", AGENT_LIST_HELP),
            ("delete", AGENT_DELETE_HELP),
            ("get", AGENT_GET_HELP),
            ("status", AGENT_STATUS_HELP),
            ("profile", AGENT_PROFILE_HELP),
            ("message", AGENT_MESSAGE_HELP),
            ("inbox", AGENT_INBOX_HELP),
            ("pause", AGENT_PAUSE_HELP),
            ("resume", AGENT_RESUME_HELP),
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
        assert_eq!(
            parse_args(args(&["change"])).unwrap().1,
            CliCommand::Help(CHANGE_HELP)
        );
        for (action, expected) in [
            ("list", CHANGE_LIST_HELP),
            ("remove", CHANGE_REMOVE_HELP),
            ("legacy-list", CHANGE_LEGACY_LIST_HELP),
            ("forget-legacy", CHANGE_FORGET_LEGACY_HELP),
        ] {
            assert_eq!(
                parse_args(args(&["change", action, "--help"])).unwrap().1,
                CliCommand::Help(expected),
                "change {action} --help"
            );
        }

        assert_eq!(
            parse_args(args(&["hook", "--help"])).unwrap().1,
            CliCommand::Help(HOOK_HELP)
        );
    }

    #[test]
    fn help_does_not_require_otherwise_required_options() {
        // --help must short-circuit before required-option validation.
        let (_, command) = parse_args(args(&["task", "add", "--help"])).unwrap();
        assert_eq!(command, CliCommand::Help(TASK_ADD_HELP));
        let (_, command) = parse_args(args(&["run", "stop", "--help"])).unwrap();
        assert_eq!(command, CliCommand::Help(RUN_STOP_HELP));
        let (_, command) = parse_args(args(&["change", "remove", "--help"])).unwrap();
        assert_eq!(command, CliCommand::Help(CHANGE_REMOVE_HELP));
        let (_, command) = parse_args(args(&["change", "forget-legacy", "--help"])).unwrap();
        assert_eq!(command, CliCommand::Help(CHANGE_FORGET_LEGACY_HELP));
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
                    agent_id: None,
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
                    reasoning_effort: None,
                    model_selection_reason: None,
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
            ]))
            .unwrap(),
            (
                None,
                CliCommand::TaskStart {
                    project_id: "project-1".into(),
                    task_id: "task-1".into(),
                    agent_id: "agent-1".into(),
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
    fn status_and_agent_status_map_to_the_status_requests() {
        assert_eq!(
            request_for(parse_args(args(&["auto", "off"])).unwrap().1).unwrap(),
            LocalRequest::SetAutoMode { enabled: false }
        );
        assert_eq!(
            request_for(parse_args(args(&["auto", "status"])).unwrap().1).unwrap(),
            LocalRequest::FleetStatus
        );
        assert_eq!(
            request_for(parse_args(args(&["status"])).unwrap().1).unwrap(),
            LocalRequest::FleetStatus
        );
        assert_eq!(
            parse_args(args(&["status"])).unwrap().1,
            CliCommand::Status { json: false }
        );
        assert_eq!(
            parse_args(args(&["status", "--json"])).unwrap().1,
            CliCommand::Status { json: true }
        );
        assert_eq!(
            request_for(parse_args(args(&["status", "--json"])).unwrap().1).unwrap(),
            LocalRequest::FleetStatus
        );
        assert!(parse_args(args(&["status", "--json", "--json"])).is_err());
        let (_, command) = parse_args(args(&[
            "agent",
            "status",
            "--project",
            "factory",
            "--agent",
            "god",
        ]))
        .unwrap();
        assert!(matches!(
            request_for(command).unwrap(),
            LocalRequest::AgentStatus { project_id, agent_id }
                if project_id == "factory".try_into().unwrap() && agent_id == "god".try_into().unwrap()
        ));
        assert!(parse_args(args(&["agent", "status", "--project", "factory"])).is_err());
    }

    #[test]
    fn status_help_keeps_the_human_default_and_json_escape_hatch() {
        assert!(STATUS_HELP.contains("A concise human summary"));
        assert!(STATUS_HELP.contains("--json"));
        assert!(!STATUS_HELP.contains("One JSON frame"));
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
    fn model_policy_flags_are_carried_to_the_shared_request() {
        let (_, command) = parse_args(args(&[
            "agent",
            "add",
            "--project",
            "factory",
            "--role",
            "worker",
            "--provider",
            "codex",
            "--reasoning-effort",
            "xhigh",
            "--model-reason",
            "release integration after a failed attempt",
        ]))
        .unwrap();
        let request = request_for(command).unwrap();
        let LocalRequest::CreateAgent {
            model,
            reasoning_effort,
            model_selection_reason,
            ..
        } = request
        else {
            panic!("expected create agent request");
        };
        assert_eq!(model, None);
        assert_eq!(reasoning_effort.as_deref(), Some("xhigh"));
        assert_eq!(
            model_selection_reason.as_deref(),
            Some("release integration after a failed attempt")
        );
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
                    reasoning_effort: None,
                    model_selection_reason: None,
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
                    reasoning_effort: None,
                    model_selection_reason: None,
                    permission_mode: None,
                    instructions_file: None,
                    memory_file: None,
                }
            )
        );
    }

    #[test]
    fn unhealthy_guidance_cannot_be_silently_cleared_by_profile_update() {
        let error = reusable_guidance(
            "",
            GuidanceHealthState::Oversized,
            "memory",
            "--memory-file",
        )
        .unwrap_err();
        assert!(error.contains("memory is Oversized"));
        assert!(error.contains("--memory-file"));
    }

    #[test]
    fn agent_message_and_inbox_commands_use_the_shared_local_channel() {
        let (_, message) = parse_args(args(&[
            "agent",
            "message",
            "--project",
            "factory",
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
                recipient_agent_id,
                body,
                ..
            } if recipient_agent_id == "worker".try_into().unwrap()
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
            reasoning_effort: None,
            model_selection_reason: None,
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
    fn task_add_agent_uses_atomic_assigned_create_and_task_list_filters_agent() {
        let (_, add) = parse_args(args(&[
            "task",
            "add",
            "--project",
            "project-1",
            "--agent",
            "curie",
            "--title",
            "Work",
            "--body",
            "Do it",
        ]))
        .unwrap();
        assert!(matches!(
            request_for(add).unwrap(),
            LocalRequest::CreateTask { agent_id, .. }
                if agent_id == Some("curie".try_into().unwrap())
        ));

        let (_, list) = parse_args(args(&[
            "task",
            "list",
            "--project",
            "project-1",
            "--agent",
            "curie",
        ]))
        .unwrap();
        assert_eq!(
            request_for(list).unwrap(),
            LocalRequest::ListTasks {
                project_id: "project-1".try_into().unwrap(),
                after_id: None,
                agent_id: Some("curie".try_into().unwrap()),
                queue_revision: None,
                history: false,
                limit: 10,
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
                "--revision",
                "12",
            ]))
            .unwrap(),
            (
                None,
                CliCommand::TaskList {
                    project_id: "project-1".into(),
                    after_id: Some("task-9".into()),
                    queue_revision: Some(12),
                    agent_id: None,
                    history: false,
                    limit: 10,
                }
            )
        );

        assert!(
            parse_args(args(&[
                "task",
                "list",
                "--project",
                "project-1",
                "--after",
                "task-9",
            ]))
            .is_err()
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
                priority: None,
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
            LocalRequest::CancelRun {
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
            LocalRequest::CancelRun {
                project_id: "project-1".try_into().unwrap(),
                run_id: "run-1".try_into().unwrap(),
                grace_ms: 2500,
            }
        );
    }

    #[test]
    fn attempt_context_cannot_select_host_level_operations() {
        for command in [
            CliCommand::Usage,
            CliCommand::CapacityStatus,
            CliCommand::CapacitySet { value: 8 },
            CliCommand::Init {
                yes: true,
                no_launchd: true,
            },
            CliCommand::Doctor { json: false },
            CliCommand::Update {
                install: false,
                json: false,
            },
        ] {
            assert!(host_level_command(&command));
        }
        assert!(!host_level_command(&CliCommand::Health));
    }

    #[test]
    fn change_commands_select_only_typed_identity_and_revision() {
        let (_, list) = parse_args(args(&[
            "change",
            "list",
            "--project",
            "project-1",
            "--after",
            "change-1",
        ]))
        .unwrap();
        assert_eq!(
            request_for(list).unwrap(),
            LocalRequest::ListChanges {
                project_id: "project-1".try_into().unwrap(),
                after_id: Some("change-1".try_into().unwrap()),
                limit: MAX_CHANGE_PAGE_ITEMS,
            }
        );

        let (_, remove) = parse_args(args(&[
            "change",
            "remove",
            "--project",
            "project-1",
            "--change",
            "change-1",
            "--revision",
            "7",
        ]))
        .unwrap();
        assert_eq!(
            request_for(remove).unwrap(),
            LocalRequest::RemoveChange {
                project_id: "project-1".try_into().unwrap(),
                change_id: "change-1".try_into().unwrap(),
                expected_revision: 7,
            }
        );
        let (_, remove_newly_reserved) = parse_args(args(&[
            "change",
            "remove",
            "--project",
            "project-1",
            "--change",
            "change-1",
            "--revision",
            "0",
        ]))
        .unwrap();
        assert_eq!(
            request_for(remove_newly_reserved).unwrap(),
            LocalRequest::RemoveChange {
                project_id: "project-1".try_into().unwrap(),
                change_id: "change-1".try_into().unwrap(),
                expected_revision: 0,
            }
        );
        assert!(
            parse_args(args(&[
                "change",
                "remove",
                "--project",
                "project-1",
                "--change",
                "change-1",
                "--revision",
                "-1",
            ]))
            .is_err()
        );

        let (_, list_legacy) =
            parse_args(args(&["change", "legacy-list", "--project", "project-1"])).unwrap();
        assert_eq!(
            request_for(list_legacy).unwrap(),
            LocalRequest::ListLegacySources {
                project_id: "project-1".try_into().unwrap(),
                after_id: None,
                limit: MAX_LEGACY_SOURCE_PAGE_ITEMS,
            }
        );

        let (_, forget_legacy) = parse_args(args(&[
            "change",
            "forget-legacy",
            "--project",
            "project-1",
            "--legacy-source",
            "legacy-1",
        ]))
        .unwrap();
        assert_eq!(
            request_for(forget_legacy).unwrap(),
            LocalRequest::ForgetLegacySource {
                project_id: "project-1".try_into().unwrap(),
                legacy_source_id: "legacy-1".try_into().unwrap(),
            }
        );
    }

    #[test]
    fn resolve_or_env_prefers_the_explicit_value_and_treats_empty_env_as_unset() {
        assert_eq!(
            resolve_or_env(Some("explicit".into()), Some("from-env".into())),
            Some("explicit".into())
        );
        assert_eq!(
            resolve_or_env(None, Some("from-env".into())),
            Some("from-env".into())
        );
        assert_eq!(resolve_or_env(None, Some(String::new())), None);
        assert_eq!(resolve_or_env(None, None), None);
        assert_eq!(
            require_option_or_env_value(None, "--project", "DARK_FACTORY_PROJECT").unwrap_err(),
            "--project is required (or set $DARK_FACTORY_PROJECT)"
        );
    }

    #[test]
    fn task_done_and_blocked_commands_parse_and_map_to_new_requests() {
        let (_, command) = parse_args(args(&["task", "done", "--result", "all good"])).unwrap();
        assert_eq!(
            request_for(command).unwrap(),
            LocalRequest::CompleteAttempt {
                result: "all good".into(),
            }
        );

        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "result from a file\n").unwrap();
        let (_, command) = parse_args(args(&[
            "task",
            "done",
            "--result-file",
            file.path().to_str().unwrap(),
        ]))
        .unwrap();
        assert_eq!(
            request_for(command).unwrap(),
            LocalRequest::CompleteAttempt {
                result: "result from a file\n".into(),
            }
        );

        let error = parse_args(args(&[
            "task",
            "done",
            "--result",
            "a",
            "--result-file",
            file.path().to_str().unwrap(),
        ]))
        .unwrap_err();
        assert_eq!(error, "--result and --result-file may not both be provided");

        let error = parse_args(args(&["task", "done"])).unwrap_err();
        assert_eq!(error, "task done requires --result or --result-file");

        let (_, command) =
            parse_args(args(&["task", "blocked", "--reason", "waiting on review"])).unwrap();
        assert_eq!(
            request_for(command).unwrap(),
            LocalRequest::BlockAttempt {
                reason: "waiting on review".into(),
            }
        );
    }

    #[test]
    fn agent_pause_and_resume_commands_parse_and_map_to_new_requests() {
        let (_, command) = parse_args(args(&[
            "agent",
            "pause",
            "--project",
            "project-1",
            "--agent",
            "agent-1",
        ]))
        .unwrap();
        assert_eq!(
            request_for(command).unwrap(),
            LocalRequest::PauseAgent {
                project_id: "project-1".try_into().unwrap(),
                agent_id: "agent-1".try_into().unwrap(),
            }
        );

        let (_, command) = parse_args(args(&[
            "agent",
            "resume",
            "--project",
            "project-1",
            "--agent",
            "agent-1",
        ]))
        .unwrap();
        assert_eq!(
            request_for(command).unwrap(),
            LocalRequest::ResumeAgent {
                project_id: "project-1".try_into().unwrap(),
                agent_id: "agent-1".try_into().unwrap(),
            }
        );
    }

    #[test]
    fn agent_budget_commands_are_cli_first_and_explicit() {
        let (_, command) = parse_args(args(&[
            "agent",
            "budget",
            "set",
            "--project",
            "project-1",
            "--agent",
            "agent-1",
            "--max-tool-calls",
            "25",
        ]))
        .unwrap();
        assert_eq!(
            request_for(command).unwrap(),
            LocalRequest::SetAgentBudget {
                project_id: "project-1".try_into().unwrap(),
                agent_id: "agent-1".try_into().unwrap(),
                max_tool_calls: Some(25),
            }
        );
        let (_, command) = parse_args(args(&[
            "agent",
            "budget",
            "reset",
            "--project",
            "project-1",
            "--agent",
            "agent-1",
        ]))
        .unwrap();
        assert_eq!(
            request_for(command).unwrap(),
            LocalRequest::ResetAgentBudget {
                project_id: "project-1".try_into().unwrap(),
                agent_id: "agent-1".try_into().unwrap(),
            }
        );
        assert!(
            parse_args(args(&[
                "agent",
                "budget",
                "set",
                "--project",
                "project-1",
                "--agent",
                "agent-1",
                "--max-tool-calls",
                "0"
            ]))
            .is_err()
        );
    }

    #[test]
    fn hook_command_parses_the_token_file_and_the_exact_event_name() {
        let (_, command) = parse_args(args(&[
            "hook",
            "--token-file",
            "/runs/session-1/hook.token",
            "PreToolUse",
        ]))
        .unwrap();
        assert_eq!(
            command,
            CliCommand::Hook {
                token_file: "/runs/session-1/hook.token".into(),
                event: ProviderHookEvent::PreToolUse,
            }
        );
        assert_eq!(
            request_for(command).unwrap_err(),
            "hook is handled before local requests"
        );

        let error = parse_args(args(&[
            "hook",
            "--token-file",
            "/runs/session-1/hook.token",
            "SubagentStop",
        ]))
        .unwrap_err();
        assert_eq!(error, "unknown hook event \"SubagentStop\"");

        let error = parse_args(args(&[
            "hook",
            "--token-file",
            "/runs/session-1/hook.token",
            "NotARealEvent",
        ]))
        .unwrap_err();
        assert_eq!(error, "unknown hook event \"NotARealEvent\"");

        let error = parse_args(args(&[
            "hook",
            "--token-file",
            "/runs/session-1/hook.token",
        ]))
        .unwrap_err();
        assert_eq!(error, "hook requires an event name");
    }
}
