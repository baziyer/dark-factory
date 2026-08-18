use std::{
    env,
    io::Write,
    path::{Path, PathBuf},
    process,
};

use factory_core::local::{
    LocalRequest, LocalResponse, MAX_AGENT_PAGE_ITEMS, MAX_EVENT_PAGE_ITEMS,
    MAX_PROJECT_PAGE_ITEMS, MAX_RUN_PAGE_ITEMS, MAX_SESSION_PAGE_ITEMS, MAX_TASK_PAGE_ITEMS,
    ServerFrame,
};
use factory_core::{AgentRole, Provider, ProviderHookEvent};
use factoryctl::{Client, capacity};
use uuid::Uuid;

mod attach;
mod doctor;
mod init;
mod outbox;
mod status;
mod update_command;
mod usage;

/// A session's own guidance directory, exported in every session's
/// environment (`runner_process::SESSION_ENVIRONMENT_NAMES`) — see
/// `docs/providers.md`'s "Sandboxed providers: the outbox".
const AGENT_DIR_ENV: &str = "DARK_FACTORY_AGENT_DIR";
/// Set to `1` to force the outbox fallback even when the daemon socket is
/// reachable -- exists purely so a test can exercise the sandboxed-connect-
/// failure path deterministically, without needing to actually break the
/// socket's permissions. Checked only by the outbox-eligible commands
/// (`task done`/`task blocked`/`agent message`).
const FORCE_OUTBOX_ENV: &str = "DARK_FACTORY_FORCE_OUTBOX";
const SESSION_TOKEN_FILE_ENV: &str = "DARK_FACTORY_SESSION_TOKEN_FILE";

use attach::AttachTarget;

const USAGE: &str = "usage: factoryctl [--socket PATH] <health|status|auto|capacity|init|doctor|update|version|usage|project|task|agent|git|pr|run|session|hook|attach|events> ...";
const HELP: &str = "Dark Factory local control plane

Run the daemon separately (launchd keeps it alive), then run `factory-tui` in a persistent terminal.

Commands:
  health                                      Check the daemon
  status [--json]                             The whole fleet at one instant: sessions, queues, attention, live-session cap
  auto on|off|status                         Set or show the factory-wide provider bypass default
  capacity status|set N                     Show or change the operator-owned live-session capacity (1..=64)
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
  git status|diff|commit|push                  Session-authenticated daemon-owned Git operations
  pr open|update                               Session-authenticated daemon-owned pull requests
  run list|stop                               List and stop process attempts
  session list|stop                           List and stop resident provider sessions
  hook --token-file PATH <Event>              Forward one provider hook invocation to the daemon
  attach --project P (--session S | --agent A) Attach to a session's PTY, by ID or by its agent
  events [--follow]                           Read durable events

Run `factoryctl <command> --help` or `factoryctl <command> <action> --help`
for action-specific options.

Every `--project` may be omitted if `$DARK_FACTORY_PROJECT` is set (as it is
inside a session's own environment); `agent message --from` similarly
defaults to `$DARK_FACTORY_AGENT` when unset.

Options:
  --socket PATH                      Use an explicit local socket
  -h, --help                         Show this help";
const HEALTH_HELP: &str = "usage: factoryctl health

Check that the daemon is reachable and responding.";
const STATUS_HELP: &str = "usage: factoryctl status [--json]

A concise human summary of the whole daemon at one instant: projects,
agents, sessions, project backlogs, assigned worker queues, worktrees, and
anything needing attention.
factory-tui reads the same request. For history, use the list commands.

Options:
  --json                       Print the complete protocol response frame
  -h, --help                   Show this help";

const CAPACITY_HELP: &str = "usage: factoryctl capacity <status|set N>

The capacity is a finite daemon-wide live-session bound. `set` is operator-only
(provider-session shell policy denies this mutation),
requires the managed launchd job, shows that launchd will restart only factoryd
while preserving runner processes/session state, and rolls the plist back if the
reload or health check fails. Valid values are 1 through 64.";
const INIT_HELP: &str = "usage: factoryctl init [--yes] [--no-launchd]

Guided first install on this machine:
  1. create $DARK_FACTORY_HOME (default ~/.dark-factory) and its logs/ dir, mode 0700
  2. report whether claude, codex, and git resolve on PATH, and their versions
  3. install the binaries next to this factoryctl as $DARK_FACTORY_HOME/bin/<version>/
     and point bin/current at them (a bin/<version> holding a different build of the
     same version is refused, never overwritten)
  4. state what Dark Factory writes outside its home (the launchd job, worktree
     pre-trust entries in ~/.claude.json, an agent/<id> branch per agent in each
     project's repository) and ask before touching launchd
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
codex?), claude/codex/git on PATH with versions, ~/.claude.json (worktree
pre-trust), every project's root and stale worktree directories, and
whether a newer release exists (may refresh the cached result, at most one
fetch per hour). This command does not repair or reconfigure the installation.
Exits 1 if any check fails; warnings don't change the exit code.

Options:
  --json                     One JSON object instead of text lines
  -h, --help                 Show this help";
const UPDATE_HELP: &str = "usage: factoryctl update [--install]

Fetch the newest release's manifest and report the invoking factoryctl,
the active bin/current runtime, and whether the release is newer than the
active runtime (JSON on stdout; the manifest result is also cached in
$DARK_FACTORY_HOME/update-check.json, which factory-tui's status line reads at
most hourly). With no active runtime, compare with the invoking factoryctl.

With --install: download that release for this platform, verify its SHA-256
against the manifest, unpack it into $DARK_FACTORY_HOME/bin/<version>/, and
repoint $DARK_FACTORY_HOME/bin/current at it. If this user's launchd job
(~/Library/LaunchAgents/com.dark-factory.factoryd.plist) exists it is
rewritten to run from bin/current (keeping its other arguments and
environment; PATH gains the provider CLIs' directories if missing) and
reloaded -- only the daemon restarts; every running session survives. The
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
  -h, --help                 Show this help";
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

const GIT_HELP: &str = "usage: factoryctl git <status|diff|commit|push> [options]

Run Git through factoryd for the calling session's exact managed worktree and
agent/<id> branch. Identity comes only from DARK_FACTORY_SESSION_TOKEN_FILE.
The daemon serializes operations and never accepts a path, branch, or remote.

Actions:
  status                    Show short branch/worktree status
  diff [--staged]           Show an unstaged or staged diff
  commit --message TEXT     Stage all worktree changes and commit them
  push                      Push the managed branch to origin without force";
const PR_HELP: &str = "usage: factoryctl pr <open|update> [options]

Open or update a pull request through factoryd for the calling session branch.
The daemon verifies an updated PR has that exact head branch.

Actions:
  open --title TEXT (--body TEXT | --body-file PATH)
  update --number N --title TEXT (--body TEXT | --body-file PATH)";

const PROJECT_HELP: &str =
    "usage: factoryctl project <add|list|delete|get|guidance|repository> [options]

Manage projects.

Actions:
  add       Create a new project
  list      List projects
  delete    Delete a project that has no non-terminal run
  get       Fetch one project, including its guidance file path
  guidance  Manage a project's standing guidance file
  repository  Set the daemon-owned remote and PR base used by agent requests

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
const PROJECT_REPOSITORY_HELP: &str =
    "usage: factoryctl project repository set --project ID --remote URL --base BRANCH

Set the daemon-owned remote and PR base for a project. This authority is
write-once and can be set only while the factory has no live sessions in any
project; later retarget attempts are rejected.

Required:
  --project ID           Project to configure
  --remote URL           Exact Git remote URL agents may push to
  --base BRANCH          Pull-request base branch

Options:
  -h, --help              Show this help";

const TASK_HELP: &str =
    "usage: factoryctl task <add|list|get|start|retry|assign|cancel|update|delete|done|blocked> [options]

Manage tasks within a project.

Actions:
  add       Create a new task
  list      List tasks in a project
  get       Fetch one task
  start     Start a queued task on an agent
  retry     Requeue a failed or cancelled task
  reorder   Change a queued task's priority/order
  assign    Assign or return a queued task; assignment wakes delivery
  cancel    Cancel a queued or blocked task
  update    Edit a queued task's title or body
  delete    Delete a task that has no active run
  done      Mark the task's open episode succeeded, from inside a session
  blocked   Mark the task's open episode blocked, from inside a session

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

Assigning to an agent wakes automatic delivery and may start its session.
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

Delete a task that has no non-terminal run, no subtasks, and no run that is
a parent of another run. Also deletes its terminal runs and any rows that
reference it (questions, dependencies, webhook capabilities).

Required:
  --project ID           Project the task belongs to
  --task ID              Task to delete

Options:
  -h, --help              Show this help";
const TASK_DONE_HELP: &str =
    "usage: factoryctl task done --project ID --task ID (--result TEXT | --result-file PATH)

Marks a task's open episode succeeded from inside its own session: the run
closes with closed_by=task_done and the task becomes succeeded. Intended to
be called by the agent itself once it finishes its work, using the
DARK_FACTORY_AGENT/DARK_FACTORY_SESSION_TOKEN_FILE identity in its session
environment; it does not take an --agent flag.

Required:
  --project ID           Project the task belongs to
  --task ID              Task to complete
  --result TEXT          Result text (mutually exclusive with --result-file)
  --result-file PATH     Local file to read the result text from

Options:
  -h, --help              Show this help";
const TASK_BLOCKED_HELP: &str =
    "usage: factoryctl task blocked --project ID --task ID --reason TEXT

Marks a task's open episode blocked from inside its own session: the run
closes with closed_by=task_blocked and the task becomes blocked. Like `task
done`, identity comes from the session environment, not an --agent flag.

Required:
  --project ID           Project the task belongs to
  --task ID              Task to block
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
  status    One agent's live picture: session, run, last hook, queue, inbox, worktree git state
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
  --worktree PATH              Absolute path to an existing git worktree,
                               overriding the daemon-managed default
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
live session -- or the most recent ended one, so a failure stays visible --
with state, activity, last hook event and time, and wait reason; its current
run; the queued tasks assigned to it (oldest first, first 10 listed, full
depth alongside); undelivered inbox messages; its attention level (and
whether that was read from the session or inferred from the run); and, when
it has a worktree, `git status` summarized (branch, changed files, dirty).

Required:
  --project ID           Project the agent belongs to
  --agent ID             Agent to inspect
Options:
  -h, --help             Show this help";
const AGENT_GET_HELP: &str = "usage: factoryctl agent get --project ID --agent ID

Fetch one agent, including the absolute paths of its `instructions.md` and
`memory.md` guidance files and their current contents.

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
  --from AGENT_ID             Sender agent ID (default: $DARK_FACTORY_AGENT if
                                set inside a session, else none/system)
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
messages into its session until `agent resume`. Its current session, if
any, keeps running; this only affects future delivery.

Required:
  --project ID           Project the agent belongs to
  --agent ID             Agent to pause

Options:
  -h, --help              Show this help";
const AGENT_RESUME_HELP: &str = "usage: factoryctl agent resume --project ID --agent ID

Undoes `agent pause`: the daemon resumes delivering queued work into this
agent's session.

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

const ATTACH_HELP: &str =
    "usage: factoryctl attach --project ID (--session ID | --agent ID) [--since-offset N]

Attach to a session's PTY: puts the local terminal in raw mode, replays
retained output from --since-offset (default 0, i.e. from the start), then
streams live output and forwards stdin as operator input. Resizes the
remote PTY to match the local terminal on attach and on every SIGWINCH.
Detach with Ctrl-] without affecting the session.

Required:
  --project ID               Project the session belongs to
  --session ID                Session to attach to (--run is accepted as an alias)
  --agent ID                  Attach to this agent's current live session
                               instead (resolved via `session list`; exactly
                               one of --session/--run/--agent is required)

Options:
  --since-offset N            Replay retained output from this byte offset (default 0)
  -h, --help                    Show this help";

const SESSION_HELP: &str = "usage: factoryctl session <list|stop> [options]

Inspect and control resident provider sessions (one per agent, PTY-backed,
spanning many task episodes).

Actions:
  list      List sessions in a project
  stop      Gracefully stop a session's provider process

Run `factoryctl session <action> --help` for action-specific options.";
const SESSION_LIST_HELP: &str = "usage: factoryctl session list --project ID [options]

List sessions in a project, ordered by ID.

Required:
  --project ID           Project to list sessions from

Options:
  --after ID               Resume after this session ID
  --limit N                  Page size (default and max: 1000)
  -h, --help                   Show this help";
const SESSION_STOP_HELP: &str =
    "usage: factoryctl session stop --project ID --session ID [--grace-ms N]

Gracefully stops a session's PTY-backed provider process group and waits for
the runner to confirm that the owned group is gone. Any open run (task
episode) closes with closed_by=operator_stop. Cleanup failure leaves the
session live and returns an error instead of claiming it stopped.

Required:
  --project ID           Project the session belongs to
  --session ID           Session to stop

Options:
  --grace-ms N              Grace period before a harder stop (default 0, max 60000)
  -h, --help                  Show this help";

const HOOK_HELP: &str = "usage: factoryctl hook --token-file PATH <Event>

Forwards one provider hook invocation (a Claude Code `--settings` hook or a
Codex `CODEX_HOME/config.toml` hook) to the daemon. Reads the hook's JSON
payload from stdin (bounded to 64 KiB), sends it as one `provider_hook`
request together with the token file's contents, and prints the daemon's
`reply` JSON verbatim to stdout so the provider can act on it (for example
`{\"decision\":\"block\",\"reason\":\"...\"}`).

Always exits 0 and prints `{}` on stdout if the token file cannot be read,
stdin is not valid bounded JSON, or the daemon is unreachable, errors, or is
slow (5 second timeout) — a broken or slow hook must never wedge the
operator's live Claude Code or Codex session. This command is meant to be
invoked by the provider itself, from a generated hook command line, not
typed by an operator.

Required:
  --token-file PATH        This session's private hook token file
  <Event>                     One of: SessionStart, UserPromptSubmit,
                               PreToolUse, PermissionRequest, PostToolUse,
                               Notification, Stop, SubagentStop, SessionEnd

Options:
  -h, --help                    Show this help";

const PROJECT_LIST_LIMIT: u32 = MAX_PROJECT_PAGE_ITEMS;
const TASK_LIST_LIMIT: u32 = MAX_TASK_PAGE_ITEMS;
const AGENT_LIST_LIMIT: u32 = MAX_AGENT_PAGE_ITEMS;
const RUN_LIST_LIMIT: u32 = MAX_RUN_PAGE_ITEMS;
const SESSION_LIST_LIMIT: u32 = MAX_SESSION_PAGE_ITEMS;
const EVENT_LIST_LIMIT: u32 = MAX_EVENT_PAGE_ITEMS;
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
    ProjectRepositorySet {
        project_id: String,
        remote_url: String,
        base_branch: String,
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
        parent_run_id: Option<String>,
        worktree: String,
    },
    Attach {
        project_id: String,
        target: AttachTarget,
        since_offset: u64,
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
        project_id: String,
        task_id: String,
        result: String,
    },
    TaskBlocked {
        project_id: String,
        task_id: String,
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
        worktree: Option<String>,
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
    AgentPause {
        project_id: String,
        agent_id: String,
    },
    AgentResume {
        project_id: String,
        agent_id: String,
    },
    GitStatus,
    GitDiff {
        staged: bool,
    },
    GitCommit {
        message: String,
    },
    GitPush,
    PrOpen {
        title: String,
        body: String,
    },
    PrUpdate {
        number: u64,
        title: String,
        body: String,
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
    SessionList {
        project_id: String,
        after_id: Option<String>,
        limit: u32,
    },
    SessionStop {
        project_id: String,
        session_id: String,
        grace_ms: u64,
    },
    Hook {
        token_file: String,
        event: ProviderHookEvent,
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
            "capacity: {previous} -> {requested} live sessions; launchd will restart only factoryd, preserving live runner processes and session state; higher values can increase concurrent provider/subscription use, lower values leave work queued"
        );
        let change = capacity::set_from_environment(&socket, *value)?;
        println!("{}", capacity_result(&change));
        return Ok(0);
    }
    if let CliCommand::Update { install } = command {
        return update_command::run(&update_command::Options { install }, &socket);
    }
    if let CliCommand::Init { yes, no_launchd } = command {
        return init::run(&init::Options { yes, no_launchd }, &socket);
    }
    if let CliCommand::Doctor { json } = command {
        return doctor::run(&doctor::Options { json }, &socket);
    }
    let client = Client::new(socket);
    if let CliCommand::Attach {
        project_id,
        target,
        since_offset,
    } = command
    {
        return attach::run(&client, &project_id, &target, since_offset);
    }
    if let CliCommand::Hook { token_file, event } = command {
        return Ok(run_hook(&client, &token_file, event));
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
    if is_outboxable(&request) {
        return run_outboxable(&client, request, &mut output);
    }
    let frame = client.request(request).map_err(|error| error.to_string())?;
    if human_status {
        match &frame {
            ServerFrame::Response {
                response: LocalResponse::FleetStatus { status },
                ..
            } => status::write(&mut output, status)?,
            _ => write_frame(&mut output, &frame)?,
        }
    } else {
        write_frame(&mut output, &frame)?;
    }
    Ok(if is_error(&frame) { 2 } else { 0 })
}

fn capacity_result(change: &capacity::CapacityChange) -> serde_json::Value {
    serde_json::json!({
        "previous": change.previous,
        "capacity": change.current,
        "launchd": "reloaded",
        "live_sessions_preserved": true,
    })
}

/// The agent-facing mutations a session's own `factoryctl` calls make on
/// itself -- `task done`, `task blocked`, `agent message` -- are the only
/// commands that fall back to the file outbox (`outbox` module) when the
/// daemon socket can't be reached. Every other command fails exactly as it
/// always has: silently papering over an unreachable daemon for, say,
/// `project add` would just hide a real operator-facing failure behind a
/// misleading "queued" message nothing is ever going to deliver from an
/// operator's own shell.
fn is_outboxable(request: &LocalRequest) -> bool {
    matches!(
        request,
        LocalRequest::CompleteTask { .. }
            | LocalRequest::BlockTask { .. }
            | LocalRequest::SendAgentMessage { .. }
    )
}

/// Sends one outbox-eligible request, falling back to `outbox::queue` on a
/// connect/send failure (or unconditionally when `DARK_FACTORY_FORCE_OUTBOX`
/// is set, so a test can exercise the fallback without breaking the socket
/// itself). See the `outbox` module and `docs/providers.md`'s "Sandboxed
/// providers: the outbox".
///
/// When `$DARK_FACTORY_AGENT_DIR` is unset, this behaves exactly as it did
/// before the outbox existed: a connect failure surfaces its original
/// error, and (the only way to reach this without ever attempting the
/// daemon) forcing the outbox with no agent directory to queue into is
/// reported explicitly rather than silently doing nothing.
fn run_outboxable(
    client: &Client,
    request: LocalRequest,
    output: &mut impl Write,
) -> Result<i32, String> {
    let force_outbox = env::var(FORCE_OUTBOX_ENV).ok().as_deref() == Some("1");
    if !force_outbox {
        match client.request(request.clone()) {
            Ok(frame) => {
                write_frame(output, &frame)?;
                return Ok(if is_error(&frame) { 2 } else { 0 });
            }
            Err(error) => {
                return match env::var(AGENT_DIR_ENV) {
                    Ok(agent_dir) => queue_to_outbox(&agent_dir, request, output),
                    Err(_) => Err(error.to_string()),
                };
            }
        }
    }
    match env::var(AGENT_DIR_ENV) {
        Ok(agent_dir) => queue_to_outbox(&agent_dir, request, output),
        Err(_) => Err(format!(
            "{FORCE_OUTBOX_ENV} is set but {AGENT_DIR_ENV} is not; nowhere to queue the request"
        )),
    }
}

fn queue_to_outbox(
    agent_dir: &str,
    request: LocalRequest,
    output: &mut impl Write,
) -> Result<i32, String> {
    let agent_dir = PathBuf::from(agent_dir);
    let path = outbox::queue(&agent_dir, &request).map_err(|error| error.to_string())?;
    let relative = path.strip_prefix(&agent_dir).unwrap_or(path.as_path());
    writeln!(
        output,
        "queued: {} (delivered on the next hook)",
        relative.display()
    )
    .map_err(|error| error.to_string())?;
    Ok(0)
}

/// Executes `factoryctl hook`: forwards one provider hook payload to the
/// daemon and prints its `reply` JSON verbatim. Lifecycle hooks fail open so
/// they cannot wedge a provider; `PreToolUse` fails closed because losing
/// the daemon must not silently remove the auto-mode policy gate.
///
/// Before sending the hook itself, drains `$DARK_FACTORY_AGENT_DIR/outbox/`
/// (a no-op when the variable is unset or the directory doesn't exist) —
/// see the `outbox` module and `docs/providers.md`'s "Sandboxed providers:
/// the outbox". This runs on every hook event, not just `Stop`, so a
/// queued request is carried at the very next opportunity rather than
/// waiting specifically for the end of a turn.
fn run_hook(client: &Client, token_file: &str, event: ProviderHookEvent) -> i32 {
    if let Ok(agent_dir) = env::var(AGENT_DIR_ENV) {
        outbox::drain(client, Path::new(&agent_dir));
    }
    let reply = hook_reply(client, token_file, event).unwrap_or_else(|| {
        if event == ProviderHookEvent::PreToolUse {
            serde_json::json!({"hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": "Dark Factory policy unavailable"
            }})
        } else {
            serde_json::json!({})
        }
    });
    println!("{reply}");
    0
}

fn hook_reply(
    client: &Client,
    token_file: &str,
    event: ProviderHookEvent,
) -> Option<serde_json::Value> {
    let token = std::fs::read_to_string(token_file).ok()?;
    let token = token.trim().to_owned();
    if token.is_empty() {
        return None;
    }
    let payload = read_bounded_stdin_json(HOOK_PAYLOAD_LIMIT_BYTES)?;
    let frame = client
        .request_with_timeout(
            LocalRequest::ProviderHook {
                token,
                event,
                payload,
            },
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
        None => agent.profile.instructions,
    };
    let memory = match memory_file {
        Some(path) => read_guidance_file(&path)?,
        None => agent.profile.memory,
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
            require_empty(&args)?;
            Ok((socket, CliCommand::Update { install }))
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
        "git" => parse_git(args).map(|command| (socket, command)),
        "pr" => parse_pr(args).map(|command| (socket, command)),
        "run" => parse_run(args).map(|command| (socket, command)),
        "session" => parse_session(args).map(|command| (socket, command)),
        "hook" => {
            if wants_help(&args) {
                return Ok((socket, CliCommand::Help(HOOK_HELP)));
            }
            parse_hook(args).map(|command| (socket, command))
        }
        "events" => {
            if wants_help(&args) {
                return Ok((socket, CliCommand::Help(EVENTS_HELP)));
            }
            parse_events(args).map(|command| (socket, command))
        }
        _ => Err(format!("unknown command {command:?}; {USAGE}")),
    }
}

fn parse_git(mut args: Vec<String>) -> Result<CliCommand, String> {
    if args.is_empty() || is_help_flag(&args[0]) {
        return Ok(CliCommand::Help(GIT_HELP));
    }
    let action = take_action(&mut args, "git")?;
    if wants_help(&args) {
        return Ok(CliCommand::Help(GIT_HELP));
    }
    match action.as_str() {
        "status" => {
            require_empty(&args)?;
            Ok(CliCommand::GitStatus)
        }
        "diff" => {
            let staged = take_flag(&mut args, "--staged")?;
            require_empty(&args)?;
            Ok(CliCommand::GitDiff { staged })
        }
        "commit" => {
            let message = required_option(&mut args, "--message")?;
            require_empty(&args)?;
            Ok(CliCommand::GitCommit { message })
        }
        "push" => {
            require_empty(&args)?;
            Ok(CliCommand::GitPush)
        }
        _ => Err(format!("unknown git action {action:?}")),
    }
}

fn parse_pr(mut args: Vec<String>) -> Result<CliCommand, String> {
    if args.is_empty() || is_help_flag(&args[0]) {
        return Ok(CliCommand::Help(PR_HELP));
    }
    let action = take_action(&mut args, "pr")?;
    if wants_help(&args) {
        return Ok(CliCommand::Help(PR_HELP));
    }
    let title = required_option(&mut args, "--title")?;
    let body = take_option(&mut args, "--body")?;
    let body_file = take_option(&mut args, "--body-file")?;
    let body = match (body, body_file) {
        (Some(body), None) => body,
        (None, Some(path)) => read_guidance_file(&path)?,
        (Some(_), Some(_)) => return Err("--body and --body-file are mutually exclusive".into()),
        (None, None) => return Err("--body or --body-file is required".into()),
    };
    match action.as_str() {
        "open" => {
            require_empty(&args)?;
            Ok(CliCommand::PrOpen { title, body })
        }
        "update" => {
            let number = parse_number(&required_option(&mut args, "--number")?, "--number")?;
            if number == 0 {
                return Err("--number must be positive".into());
            }
            require_empty(&args)?;
            Ok(CliCommand::PrUpdate {
                number,
                title,
                body,
            })
        }
        _ => Err(format!("unknown pr action {action:?}")),
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
            "repository" => PROJECT_REPOSITORY_HELP,
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
        "repository" => {
            let sub_action = take_action(&mut args, "project repository")?;
            if sub_action != "set" {
                return Err(format!("unknown project repository action {sub_action:?}"));
            }
            let project_id = required_project(&mut args)?;
            let remote_url = required_option(&mut args, "--remote")?;
            let base_branch = required_option(&mut args, "--base")?;
            require_empty(&args)?;
            Ok(CliCommand::ProjectRepositorySet {
                project_id,
                remote_url,
                base_branch,
            })
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
            let project_id = required_project(&mut args)?;
            let task_id = required_option(&mut args, "--task")?;
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
            Ok(CliCommand::TaskDone {
                project_id,
                task_id,
                result,
            })
        }
        "blocked" => {
            let project_id = required_project(&mut args)?;
            let task_id = required_option(&mut args, "--task")?;
            let reason = required_option(&mut args, "--reason")?;
            require_empty(&args)?;
            Ok(CliCommand::TaskBlocked {
                project_id,
                task_id,
                reason,
            })
        }
        _ => Err(format!("unknown task action {action:?}")),
    }
}

fn parse_attach(mut args: Vec<String>) -> Result<CliCommand, String> {
    let project_id = required_project(&mut args)?;
    // `--run` is a deprecated alias kept during the transition to resident
    // sessions (a session's id is currently its run's id; see
    // `local_api.rs`'s `resolve_transitional_run_id`).
    let session = take_option(&mut args, "--session")?;
    let run = take_option(&mut args, "--run")?;
    let agent = take_option(&mut args, "--agent")?;
    let target = match (session, run, agent) {
        (Some(session_id), None, None) => AttachTarget::Session(session_id),
        (None, Some(run_id), None) => AttachTarget::Session(run_id),
        (None, None, Some(agent_id)) => AttachTarget::Agent(agent_id),
        (None, None, None) => return Err("--session or --agent is required".into()),
        _ => return Err("--session, --run, and --agent may not be combined".into()),
    };
    let since_offset = take_option(&mut args, "--since-offset")?
        .map(|value| parse_number(&value, "--since-offset"))
        .transpose()?
        .unwrap_or(0);
    require_empty(&args)?;
    Ok(CliCommand::Attach {
        project_id,
        target,
        since_offset,
    })
}

fn parse_session(mut args: Vec<String>) -> Result<CliCommand, String> {
    if args.is_empty() || is_help_flag(&args[0]) {
        return Ok(CliCommand::Help(SESSION_HELP));
    }
    let action = take_action(&mut args, "session")?;
    if wants_help(&args) {
        return Ok(CliCommand::Help(match action.as_str() {
            "list" => SESSION_LIST_HELP,
            "stop" => SESSION_STOP_HELP,
            _ => SESSION_HELP,
        }));
    }
    match action.as_str() {
        "list" => {
            let project_id = required_project(&mut args)?;
            let after_id = take_option(&mut args, "--after")?;
            let (limit, _) = take_limit(&mut args, SESSION_LIST_LIMIT, MAX_SESSION_PAGE_ITEMS)?;
            require_empty(&args)?;
            Ok(CliCommand::SessionList {
                project_id,
                after_id,
                limit,
            })
        }
        "stop" => {
            let project_id = required_project(&mut args)?;
            let session_id = required_option(&mut args, "--session")?;
            let grace_ms = take_option(&mut args, "--grace-ms")?
                .map(|value| parse_number(&value, "--grace-ms"))
                .transpose()?
                .unwrap_or(0u64);
            require_empty(&args)?;
            Ok(CliCommand::SessionStop {
                project_id,
                session_id,
                grace_ms,
            })
        }
        _ => Err(format!("unknown session action {action:?}")),
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
            let worktree = take_option(&mut args, "--worktree")?;
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
                worktree,
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
            let sender_agent_id = take_option_or_env(&mut args, "--from", "DARK_FACTORY_AGENT")?;
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
        CliCommand::ProjectRepositorySet {
            project_id,
            remote_url,
            base_branch,
        } => Ok(LocalRequest::SetProjectRepositoryAuthority {
            project_id: parse_id(project_id, "project")?,
            remote_url,
            base_branch,
        }),
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
        CliCommand::TaskDone {
            project_id,
            task_id,
            result,
        } => Ok(LocalRequest::CompleteTask {
            project_id: parse_id(project_id, "project")?,
            task_id: parse_id(task_id, "task")?,
            result,
        }),
        CliCommand::TaskBlocked {
            project_id,
            task_id,
            reason,
        } => Ok(LocalRequest::BlockTask {
            project_id: parse_id(project_id, "project")?,
            task_id: parse_id(task_id, "task")?,
            reason,
        }),
        CliCommand::AgentAdd {
            id,
            project_id,
            parent_agent_id,
            role,
            provider,
            model,
            reasoning_effort,
            model_selection_reason,
            worktree,
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
            worktree,
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
        CliCommand::GitStatus => Ok(LocalRequest::GitStatus {
            token: session_token()?,
        }),
        CliCommand::GitDiff { staged } => Ok(LocalRequest::GitDiff {
            token: session_token()?,
            staged,
        }),
        CliCommand::GitCommit { message } => Ok(LocalRequest::GitCommit {
            token: session_token()?,
            message,
        }),
        CliCommand::GitPush => Ok(LocalRequest::GitPush {
            token: session_token()?,
        }),
        CliCommand::PrOpen { title, body } => Ok(LocalRequest::PrOpen {
            token: session_token()?,
            title,
            body,
        }),
        CliCommand::PrUpdate {
            number,
            title,
            body,
        } => Ok(LocalRequest::PrUpdate {
            token: session_token()?,
            number,
            title,
            body,
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
        CliCommand::SessionList {
            project_id,
            after_id,
            limit,
        } => Ok(LocalRequest::ListSessions {
            project_id: parse_id(project_id, "project")?,
            after_id: after_id
                .map(|id| parse_id(id, "session cursor"))
                .transpose()?,
            limit: Some(usize::try_from(limit).unwrap_or(usize::MAX)),
        }),
        CliCommand::SessionStop {
            project_id,
            session_id,
            grace_ms,
        } => Ok(LocalRequest::StopSession {
            project_id: parse_id(project_id, "project")?,
            session_id: parse_id(session_id, "session")?,
            grace_ms,
        }),
        CliCommand::Hook { .. } => Err("hook is handled before local requests".into()),
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

fn session_token() -> Result<String, String> {
    let path = env::var(SESSION_TOKEN_FILE_ENV).map_err(|_| {
        format!("{SESSION_TOKEN_FILE_ENV} is required; this command only works inside a session")
    })?;
    let token = std::fs::read_to_string(&path)
        .map_err(|error| format!("cannot read session token file: {error}"))?;
    let token = token.trim().to_owned();
    if token.is_empty() {
        return Err("session token file is empty".into());
    }
    Ok(token)
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

/// `--project`, falling back to `$DARK_FACTORY_PROJECT` when the flag is
/// absent (so a command run from inside a session's own environment does
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
    take_option_or_env(args, name, env_var)?
        .ok_or_else(|| format!("{name} is required (or set ${env_var})"))
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

    #[test]
    fn repository_commands_expose_no_target_or_force_selectors() {
        let (_, command) = parse_args(vec![
            "git".into(),
            "commit".into(),
            "--message".into(),
            "small fix".into(),
        ])
        .unwrap();
        assert!(matches!(command, CliCommand::GitCommit { ref message } if message == "small fix"));

        let (_, command) = parse_args(vec![
            "pr".into(),
            "update".into(),
            "--number".into(),
            "7".into(),
            "--title".into(),
            "Revised".into(),
            "--body".into(),
            "Details".into(),
        ])
        .unwrap();
        assert!(matches!(command, CliCommand::PrUpdate { number: 7, .. }));

        assert!(parse_args(vec!["git".into(), "push".into(), "--force".into()]).is_err());
        assert!(
            parse_args(vec![
                "git".into(),
                "push".into(),
                "--branch".into(),
                "main".into()
            ])
            .is_err()
        );
        assert!(
            parse_args(vec![
                "pr".into(),
                "open".into(),
                "--repo".into(),
                "other/repo".into()
            ])
            .is_err()
        );
    }

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
    fn capacity_result_promises_live_session_preservation_in_the_cli_contract() {
        let value = capacity_result(&capacity::CapacityChange {
            previous: 4,
            current: 8,
        });
        assert_eq!(value["previous"], 4);
        assert_eq!(value["capacity"], 8);
        assert_eq!(value["live_sessions_preserved"], true);
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
            CliCommand::Update { install: false }
        );
        assert_eq!(
            parse_args(args(&["update", "--install"])).unwrap().1,
            CliCommand::Update { install: true }
        );
        assert!(parse_args(args(&["update", "--force"])).is_err());
        assert_eq!(
            parse_args(args(&["version"])).unwrap().1,
            CliCommand::Version
        );
        assert_eq!(
            parse_args(args(&["--version"])).unwrap().1,
            CliCommand::Version
        );
        assert!(request_for(CliCommand::Update { install: false }).is_err());
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
            parse_args(args(&["project", "repository", "--help"]))
                .unwrap()
                .1,
            CliCommand::Help(PROJECT_REPOSITORY_HELP)
        );
        assert_eq!(
            parse_args(args(&["project", "repository", "set", "--help"]))
                .unwrap()
                .1,
            CliCommand::Help(PROJECT_REPOSITORY_HELP)
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

        for required in ["--project ID", "--remote URL", "--base BRANCH"] {
            assert!(
                PROJECT_REPOSITORY_HELP.contains(required),
                "repository help must name required flag {required}"
            );
        }
        assert!(PROJECT_REPOSITORY_HELP.contains("write-once"));
        assert!(PROJECT_REPOSITORY_HELP.contains("no live sessions in any\nproject"));
        assert!(TASK_ASSIGN_HELP.contains("wakes automatic delivery"));
        assert!(TASK_ASSIGN_HELP.contains("may start its session"));
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
            parse_args(args(&["session"])).unwrap().1,
            CliCommand::Help(SESSION_HELP)
        );
        for (action, expected) in [("list", SESSION_LIST_HELP), ("stop", SESSION_STOP_HELP)] {
            assert_eq!(
                parse_args(args(&["session", action, "--help"])).unwrap().1,
                CliCommand::Help(expected),
                "session {action} --help"
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
                    worktree: None,
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
                "worker",
                "--provider",
                "shell",
                "--worktree",
                "/abs/worktree",
            ]))
            .unwrap()
            .1,
            CliCommand::AgentAdd {
                id: None,
                project_id: "project-1".into(),
                parent_agent_id: None,
                role: AgentRole::Worker,
                provider: Provider::Shell,
                model: None,
                reasoning_effort: None,
                model_selection_reason: None,
                worktree: Some("/abs/worktree".into()),
            }
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
                    target: AttachTarget::Session("session-1".into()),
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
                    target: AttachTarget::Session("session-1".into()),
                    since_offset: 4096,
                }
            )
        );
        let missing_project = parse_args(args(&["attach", "--session", "session-1"]));
        if env::var_os("DARK_FACTORY_PROJECT").is_some() {
            assert!(missing_project.is_ok());
        } else {
            assert!(missing_project.is_err());
        }
        assert!(
            request_for(CliCommand::Attach {
                project_id: "project-1".into(),
                target: AttachTarget::Session("session-1".into()),
                since_offset: 0,
            })
            .is_err()
        );
    }

    #[test]
    fn attach_resolves_agent_as_an_alternative_target_to_session() {
        assert_eq!(
            parse_args(args(&[
                "attach",
                "--project",
                "project-1",
                "--agent",
                "agent-1"
            ]))
            .unwrap(),
            (
                None,
                CliCommand::Attach {
                    project_id: "project-1".into(),
                    target: AttachTarget::Agent("agent-1".into()),
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
                "--agent",
                "agent-1"
            ]))
            .is_err()
        );
        assert!(parse_args(args(&["attach", "--project", "project-1"])).is_err());
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
                    target: AttachTarget::Session("run-1".into()),
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
            reasoning_effort: None,
            model_selection_reason: None,
            worktree: None,
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
    }

    #[test]
    fn task_done_and_blocked_commands_parse_and_map_to_new_requests() {
        let (_, command) = parse_args(args(&[
            "task",
            "done",
            "--project",
            "project-1",
            "--task",
            "task-1",
            "--result",
            "all good",
        ]))
        .unwrap();
        assert_eq!(
            request_for(command).unwrap(),
            LocalRequest::CompleteTask {
                project_id: "project-1".try_into().unwrap(),
                task_id: "task-1".try_into().unwrap(),
                result: "all good".into(),
            }
        );

        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "result from a file\n").unwrap();
        let (_, command) = parse_args(args(&[
            "task",
            "done",
            "--project",
            "project-1",
            "--task",
            "task-1",
            "--result-file",
            file.path().to_str().unwrap(),
        ]))
        .unwrap();
        assert_eq!(
            request_for(command).unwrap(),
            LocalRequest::CompleteTask {
                project_id: "project-1".try_into().unwrap(),
                task_id: "task-1".try_into().unwrap(),
                result: "result from a file\n".into(),
            }
        );

        let error = parse_args(args(&[
            "task",
            "done",
            "--project",
            "project-1",
            "--task",
            "task-1",
            "--result",
            "a",
            "--result-file",
            file.path().to_str().unwrap(),
        ]))
        .unwrap_err();
        assert_eq!(error, "--result and --result-file may not both be provided");

        let error = parse_args(args(&[
            "task",
            "done",
            "--project",
            "project-1",
            "--task",
            "task-1",
        ]))
        .unwrap_err();
        assert_eq!(error, "task done requires --result or --result-file");

        let (_, command) = parse_args(args(&[
            "task",
            "blocked",
            "--project",
            "project-1",
            "--task",
            "task-1",
            "--reason",
            "waiting on review",
        ]))
        .unwrap();
        assert_eq!(
            request_for(command).unwrap(),
            LocalRequest::BlockTask {
                project_id: "project-1".try_into().unwrap(),
                task_id: "task-1".try_into().unwrap(),
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
    fn session_list_and_stop_commands_parse_and_map_to_new_requests() {
        let (_, command) =
            parse_args(args(&["session", "list", "--project", "project-1"])).unwrap();
        assert_eq!(
            request_for(command).unwrap(),
            LocalRequest::ListSessions {
                project_id: "project-1".try_into().unwrap(),
                after_id: None,
                limit: Some(SESSION_LIST_LIMIT as usize),
            }
        );

        let (_, command) = parse_args(args(&[
            "session",
            "list",
            "--project",
            "project-1",
            "--after",
            "session-1",
            "--limit",
            "5",
        ]))
        .unwrap();
        assert_eq!(
            request_for(command).unwrap(),
            LocalRequest::ListSessions {
                project_id: "project-1".try_into().unwrap(),
                after_id: Some("session-1".try_into().unwrap()),
                limit: Some(5),
            }
        );

        let (_, command) = parse_args(args(&[
            "session",
            "stop",
            "--project",
            "project-1",
            "--session",
            "session-1",
            "--grace-ms",
            "1500",
        ]))
        .unwrap();
        assert_eq!(
            request_for(command).unwrap(),
            LocalRequest::StopSession {
                project_id: "project-1".try_into().unwrap(),
                session_id: "session-1".try_into().unwrap(),
                grace_ms: 1500,
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
            "SubagentStop",
        ]))
        .unwrap();
        assert_eq!(
            command,
            CliCommand::Hook {
                token_file: "/runs/session-1/hook.token".into(),
                event: ProviderHookEvent::SubagentStop,
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
